// Output that is already in the OS pipe when the 250 ms post-exit budget is
// spent. The stdout reader is parked on a gate instead of being slept, and the
// budget is spent by backdating the wait worker's stamp, so no step depends on
// scheduler timing. Included into `mod tests` to reuse `controlled_process`.

use std::sync::mpsc as sync_mpsc;

const GATE_SAFETY: Duration = Duration::from_secs(10);
const COMPLETE: &str = "READY native-mock-session\nTURN\nTEXT deterministic progress\nDONE\n";
const QUOTA: &str = "READY native-mock-session\nTURN\nTEXT deterministic progress\nLIMIT quota\n";
const PREFIX_LEN: usize = "READY native-mock-session\nTURN\nTEXT deterministic progress\n".len();

type Trace = Arc<Mutex<Vec<String>>>;

fn note(trace: &Trace, line: impl Into<String>) {
    trace
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push(line.into());
}

#[derive(Debug)]
enum Signal {
    Parked(usize),
    ReaderEnded { reads: usize, saw_eof: bool },
}

struct GatedReader {
    inner: std::io::PipeReader,
    gate_at: usize,
    cap_before_gate: Option<usize>,
    reads: usize,
    saw_eof: bool,
    release: sync_mpsc::Receiver<()>,
    signals: sync_mpsc::Sender<Signal>,
    trace: Trace,
}

impl Read for GatedReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let index = self.reads;
        self.reads += 1;
        let mut limit = buf.len();
        if index == self.gate_at {
            note(&self.trace, format!("reader: parked before read #{index}"));
            let _ = self.signals.send(Signal::Parked(index));
            if self.release.recv_timeout(GATE_SAFETY) == Err(sync_mpsc::RecvTimeoutError::Timeout) {
                return Err(std::io::Error::other("reader gate was never released"));
            }
            note(&self.trace, format!("reader: gate open for read #{index}"));
        } else if index < self.gate_at
            && let Some(cap) = self.cap_before_gate
        {
            limit = limit.min(cap);
        }
        let count = self.inner.read(&mut buf[..limit])?;
        note(
            &self.trace,
            format!(
                "reader: read #{index} returned {count} bytes {:?}",
                String::from_utf8_lossy(&buf[..count])
            ),
        );
        self.saw_eof |= count == 0;
        Ok(count)
    }
}

impl Drop for GatedReader {
    fn drop(&mut self) {
        note(
            &self.trace,
            format!(
                "reader: thread ended after {} reads, eof_seen={}",
                self.reads, self.saw_eof
            ),
        );
        let _ = self.signals.send(Signal::ReaderEnded {
            reads: self.reads,
            saw_eof: self.saw_eof,
        });
    }
}

struct Scenario {
    name: &'static str,
    stream: &'static str,
    exit_code: i32,
    /// Index of the `read()` call that waits on the gate.
    gate_at: usize,
    /// Byte cap for the reads before the gate, so a prefix is forwarded first.
    cap_before_gate: Option<usize>,
    /// Keep the pipe's write end open, as an inheriting descendant would.
    hold_writer: bool,
    /// Open the gate and let the reader finish before the budget is spent.
    release_first: bool,
    /// Time since the wait worker stamped the clock when the consumer starts.
    elapsed: Duration,
    /// Keep the gate shut until the consumer has seen the final frame.
    hold_gate_until_exit: bool,
}

const STALLED_COMPLETE: Scenario = Scenario {
    name: "complete",
    stream: COMPLETE,
    exit_code: 0,
    gate_at: 0,
    cap_before_gate: None,
    hold_writer: false,
    release_first: false,
    elapsed: Duration::from_secs(1),
    hold_gate_until_exit: false,
};

struct Observed {
    frames: Vec<Frame>,
    trace: Vec<String>,
    reads: usize,
    saw_eof: bool,
    verified: bool,
    wall: Duration,
}

fn clock_state(clock: &Mutex<DrainClock>) -> String {
    let clock = clock.lock().unwrap_or_else(|p| p.into_inner());
    format!(
        "updated={} spent={:?} blocked={} readers={} eof_readers={}",
        clock.updated.is_some(),
        clock.spent,
        clock.blocked,
        clock.readers,
        clock.eof_readers
    )
}

async fn run_scenario(scenario: &Scenario) -> Observed {
    let trace: Trace = Arc::default();
    let (pipe_reader, mut pipe_writer) = std::io::pipe().unwrap();
    pipe_writer.write_all(scenario.stream.as_bytes()).unwrap();
    note(
        &trace,
        format!(
            "pipe: {} bytes written before exit: {:?}",
            scenario.stream.len(),
            scenario.stream
        ),
    );
    let held_writer = scenario.hold_writer.then_some(pipe_writer);

    let (release_tx, release_rx) = sync_mpsc::channel();
    let (signal_tx, signal_rx) = sync_mpsc::channel();
    let clock = Arc::new(Mutex::new(DrainClock::default()));
    let (tx, mut process) = controlled_process();
    process.drain_clock = Some(clock.clone());
    start_reader(
        Box::new(GatedReader {
            inner: pipe_reader,
            gate_at: scenario.gate_at,
            cap_before_gate: scenario.cap_before_gate,
            reads: 0,
            saw_eof: false,
            release: release_rx,
            signals: signal_tx,
            trace: trace.clone(),
        }),
        tx,
        StreamFraming::Lines,
        false,
        false,
        clock.clone(),
    )
    .unwrap();
    match signal_rx.recv_timeout(GATE_SAFETY) {
        Ok(Signal::Parked(index)) => assert_eq!(index, scenario.gate_at, "setup: wrong gate"),
        other => panic!("setup: reader never reached its gate: {other:?}"),
    }

    let mut release = Some(release_tx);
    let mut ended = None;
    if scenario.release_first {
        note(&trace, "test: gate opened before the budget is spent");
        release.take().unwrap().send(()).unwrap();
        ended = Some(signal_rx.recv_timeout(GATE_SAFETY).unwrap());
        // The reader signals from its own drop, slightly before the thread
        // releases its sender. Wait for the channel itself to report closed.
        tokio::time::timeout(GATE_SAFETY, async {
            while !process.frames.is_closed() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("setup: finished reader must release its sender");
    }

    // The wait worker stamped the clock a second ago and the reader has not
    // forwarded anything since: the 250 ms budget is spent.
    let stamped = std::time::Instant::now()
        .checked_sub(scenario.elapsed)
        .unwrap();
    clock.lock().unwrap().updated = Some(stamped);
    note(&trace, format!("clock before wait: {}", clock_state(&clock)));
    process
        .accept_wait(WaitResult {
            completed_at: stamped,
            exit: Ok(ExitInfo::from_code(scenario.exit_code)),
            diagnostics: vec![],
        })
        .unwrap();
    note(&trace, format!("clock after wait: {}", clock_state(&clock)));

    let mut frames = Vec::new();
    let started = std::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let mut next = std::pin::pin!(process.next_frame());
            let polled = std::future::poll_fn(|cx| {
                let polled = next.as_mut().poll(cx);
                if !scenario.hold_gate_until_exit
                    && let Some(release) = release.take()
                {
                    note(
                        &trace,
                        format!(
                            "consumer: first next_frame poll ready={}, opening reader gate",
                            polled.is_ready()
                        ),
                    );
                    release.send(()).unwrap();
                }
                polled
            })
            .await;
            match polled.unwrap() {
                Some(frame) => {
                    note(&trace, format!("consumer: frame {frame:?}"));
                    frames.push(frame);
                }
                None => break,
            }
        }
    })
    .await
    .expect("transport must reach its terminal frame within a bound");
    let wall = started.elapsed();
    let verified = process.output_verified();
    note(&trace, format!("clock at end: {}", clock_state(&clock)));
    if let Some(release) = release.take() {
        note(&trace, "test: consumer saw the final frame, opening reader gate");
        release.send(()).unwrap();
    }

    drop(held_writer);
    let ended = ended.unwrap_or_else(|| {
        signal_rx
            .recv_timeout(GATE_SAFETY)
            .expect("reader thread must end once its pipe is closed")
    });
    let Signal::ReaderEnded { reads, saw_eof } = ended else {
        panic!("unexpected signal: {ended:?}");
    };
    let trace = trace.lock().unwrap().clone();
    Observed {
        frames,
        trace,
        reads,
        saw_eof,
        verified,
        wall,
    }
}

fn report(scenario: &Scenario, seen: &Observed) -> String {
    format!(
        "scenario {}\nframes: {:#?}\nreader: reads={} eof_seen={} output_verified={} wall={:?}\ntrace:\n  {}",
        scenario.name,
        seen.frames,
        seen.reads,
        seen.saw_eof,
        seen.verified,
        seen.wall,
        seen.trace.join("\n  ")
    )
}

fn expected_frames(scenario: &Scenario) -> Vec<Frame> {
    scenario
        .stream
        .lines()
        .map(|line| Frame::Message(line.into()))
        .chain([Frame::Exited(ExitInfo::from_code(scenario.exit_code))])
        .collect()
}

fn assert_exit_contract(scenario: &Scenario, seen: &Observed) {
    let exits = seen
        .frames
        .iter()
        .filter(|f| matches!(f, Frame::Exited(_)))
        .count();
    assert_eq!(exits, 1, "{}", report(scenario, seen));
    assert!(
        matches!(seen.frames.last(), Some(Frame::Exited(_))),
        "{}",
        report(scenario, seen)
    );
}

async fn assert_terminal_record_survives(scenario: Scenario) -> Observed {
    let seen = run_scenario(&scenario).await;
    println!("{}", report(&scenario, &seen));
    assert_exit_contract(&scenario, &seen);
    let terminal = scenario.stream.lines().last().unwrap();
    assert!(
        seen.frames
            .iter()
            .any(|f| matches!(f, Frame::Message(m) if m == terminal)),
        "terminal record {terminal:?} was in the pipe before exit but never reached the \
         consumer\n{}",
        report(&scenario, &seen)
    );
    assert_eq!(
        seen.frames,
        expected_frames(&scenario),
        "{}",
        report(&scenario, &seen)
    );
    seen
}

// Recharacterized from the three original one-second-stall regression tests
// (kept in evidence as process_drain_regression.before.rs). Scenarios and
// inputs are unchanged: the reader is stalled past 250 ms plus the one-time
// 500 ms window and only then gets its data. The approved bounded-drain policy
// guarantees nothing in that case, so these tests pin the approved outcome:
// bounded termination, a flagged loss, and one finalization. Delivery inside the
// window is covered by the `..._past_the_first_bound` tests.
async fn assert_bounded_flagged_loss(scenario: Scenario, delivered: &str, reads: usize) {
    // Exact frames (delivered prefix, one "250 ms ... unverified" diagnostic, one
    // exit), the terminal record absent, and output_verified() false.
    let seen = assert_case(scenario, delivered, true, false).await;
    assert!(
        seen.wall < Duration::from_secs(2),
        "termination must stay bounded: {:?}",
        seen.wall
    );
    // The reader read every byte after the decision, but its first send was
    // rejected, so it never reached EOF: the loss is real, bounded, and flagged.
    assert_eq!((seen.reads, seen.saw_eof), (reads, false));
}

#[tokio::test]
async fn done_in_pipe_is_flagged_not_delivered_when_the_reader_stalls_past_every_bound() {
    assert_bounded_flagged_loss(STALLED_COMPLETE, "", 1).await;
}

#[tokio::test]
async fn limit_in_pipe_is_flagged_not_delivered_when_the_reader_stalls_past_every_bound() {
    assert_bounded_flagged_loss(
        Scenario {
            name: "quota",
            stream: QUOTA,
            exit_code: 1,
            ..STALLED_COMPLETE
        },
        "",
        1,
    )
    .await;
}

#[tokio::test]
async fn done_behind_a_prefix_is_flagged_not_delivered_when_the_reader_stalls_past_every_bound() {
    assert_bounded_flagged_loss(
        Scenario {
            name: "complete after forwarded prefix",
            gate_at: 1,
            cap_before_gate: Some(PREFIX_LEN),
            ..STALLED_COMPLETE
        },
        &COMPLETE[..PREFIX_LEN],
        2,
    )
    .await;
}

// Controls: the harness itself must not lose output.
#[tokio::test]
async fn reader_that_finished_before_the_budget_ran_out_delivers_everything() {
    assert_terminal_record_survives(Scenario {
        name: "control: reader released first",
        release_first: true,
        ..STALLED_COMPLETE
    })
    .await;
}

// Must hold before and after any correction: a pipe that never reaches EOF
// cannot hold the terminal frame back, and the exit stays single and last.
#[tokio::test]
async fn spent_drain_budget_with_held_pipe_stays_bounded_and_exits_once() {
    let scenario = Scenario {
        name: "control: write end held open",
        hold_writer: true,
        ..STALLED_COMPLETE
    };
    let seen = run_scenario(&scenario).await;
    println!("{}", report(&scenario, &seen));
    assert_exit_contract(&scenario, &seen);
    let expected = expected_frames(&scenario);
    let delivered: Vec<_> = seen
        .frames
        .iter()
        .filter(|f| matches!(f, Frame::Message(_)))
        .collect();
    assert!(
        expected.iter().zip(&delivered).all(|(want, got)| want == *got),
        "{}",
        report(&scenario, &seen)
    );
}

// The reader stalls past POST_EXIT_DRAIN but inside the one-time final window.
const PAST_FIRST_BOUND: Duration = POST_EXIT_DRAIN;

#[tokio::test]
async fn done_in_pipe_survives_a_reader_stall_past_the_first_bound() {
    let seen = assert_terminal_record_survives(Scenario {
        name: "complete, stall past 250 ms",
        elapsed: PAST_FIRST_BOUND,
        ..STALLED_COMPLETE
    })
    .await;
    assert!(seen.verified);
}

#[tokio::test]
async fn limit_in_pipe_survives_a_reader_stall_past_the_first_bound() {
    let seen = assert_terminal_record_survives(Scenario {
        name: "quota, stall past 250 ms",
        stream: QUOTA,
        exit_code: 1,
        elapsed: PAST_FIRST_BOUND,
        ..STALLED_COMPLETE
    })
    .await;
    assert!(seen.verified);
}

#[tokio::test]
async fn done_behind_a_prefix_survives_a_reader_stall_past_the_first_bound() {
    let seen = assert_terminal_record_survives(Scenario {
        name: "complete after prefix, stall past 250 ms",
        gate_at: 1,
        cap_before_gate: Some(PREFIX_LEN),
        elapsed: PAST_FIRST_BOUND,
        ..STALLED_COMPLETE
    })
    .await;
    assert!(seen.verified);
}

#[tokio::test]
async fn reader_that_finished_in_time_is_verified() {
    let seen = assert_terminal_record_survives(Scenario {
        name: "control: reader released first, verified",
        release_first: true,
        ..STALLED_COMPLETE
    })
    .await;
    assert!(seen.verified);
}

// Past every bound the loss is bounded, flagged, and never reads as verified.
#[tokio::test]
async fn reader_stalled_past_every_bound_is_flagged_as_unverified() {
    let scenario = Scenario {
        name: "stall past 250 ms plus the final window",
        hold_gate_until_exit: true,
        ..STALLED_COMPLETE
    };
    let seen = run_scenario(&scenario).await;
    println!("{}", report(&scenario, &seen));
    assert_exit_contract(&scenario, &seen);
    assert!(!seen.verified, "{}", report(&scenario, &seen));
    assert_eq!(seen.frames.len(), 2, "{}", report(&scenario, &seen));
    assert!(
        matches!(&seen.frames[0], Frame::Diagnostic(m) if m.contains("250 ms") && m.contains("unverified")),
        "{}",
        report(&scenario, &seen)
    );
    assert_eq!((seen.reads, seen.saw_eof), (1, false));
}

// A pipe that never reaches EOF costs the two windows, once, and loses nothing
// the reader could deliver.
#[tokio::test]
async fn held_pipe_after_the_first_bound_is_bounded_unverified_and_loses_nothing() {
    let scenario = Scenario {
        name: "write end held open, stall past 250 ms",
        hold_writer: true,
        elapsed: PAST_FIRST_BOUND,
        ..STALLED_COMPLETE
    };
    let seen = run_scenario(&scenario).await;
    println!("{}", report(&scenario, &seen));
    assert_exit_contract(&scenario, &seen);
    assert!(!seen.verified, "{}", report(&scenario, &seen));
    assert!(
        seen.wall < Duration::from_secs(3),
        "{}",
        report(&scenario, &seen)
    );
    let delivered: Vec<_> = seen
        .frames
        .iter()
        .filter(|f| matches!(f, Frame::Message(_)))
        .cloned()
        .collect();
    let mut expected = expected_frames(&scenario);
    expected.pop();
    assert_eq!(delivered, expected, "{}", report(&scenario, &seen));
    assert_eq!(
        seen.frames
            .iter()
            .filter(|f| matches!(f, Frame::Diagnostic(_)))
            .count(),
        1
    );
}

#[test]
fn final_window_is_granted_once_and_adds_a_fixed_budget() {
    let mut clock = DrainClock::default();
    assert_eq!(clock.budget(), POST_EXIT_DRAIN);
    assert!(clock.grant_extension());
    assert_eq!(clock.budget(), POST_EXIT_DRAIN + FINAL_DRAIN);
    assert!(!clock.grant_extension());
    assert_eq!(clock.budget(), POST_EXIT_DRAIN + FINAL_DRAIN);
}

// Terminal classification cases. Each test pins the exact frame sequence and
// `output_verified()` that the runtime classification matrix replays (see
// `supervisor.rs`, `classification_matrix`). Frames: delivered records, an
// optional bound diagnostic, then one exit.
const NO_DONE: &str = "READY native-mock-session\nTURN\nTEXT deterministic progress\n";

async fn assert_case(
    scenario: Scenario,
    delivered: &str,
    diagnostic: bool,
    verified: bool,
) -> Observed {
    let seen = run_scenario(&scenario).await;
    println!("{}", report(&scenario, &seen));
    let mut expected: Vec<Frame> = delivered
        .lines()
        .map(|line| Frame::Message(line.into()))
        .collect();
    let at = expected.len();
    if diagnostic {
        expected.push(Frame::Diagnostic(String::new()));
    }
    expected.push(Frame::Exited(ExitInfo::from_code(scenario.exit_code)));
    assert_eq!(seen.frames.len(), expected.len(), "{}", report(&scenario, &seen));
    for (index, (got, want)) in seen.frames.iter().zip(&expected).enumerate() {
        match want {
            Frame::Diagnostic(_) => assert!(
                index == at
                    && matches!(got, Frame::Diagnostic(m) if m.contains("250 ms") && m.contains("unverified")),
                "{}",
                report(&scenario, &seen)
            ),
            other => assert_eq!(got, other, "{}", report(&scenario, &seen)),
        }
    }
    assert_eq!(seen.verified, verified, "{}", report(&scenario, &seen));
    seen
}

#[tokio::test]
async fn case1_done_received_but_stdout_never_reaches_eof() {
    assert_case(
        Scenario {
            name: "case 1: DONE received, stdout never EOF",
            hold_writer: true,
            elapsed: PAST_FIRST_BOUND,
            ..STALLED_COMPLETE
        },
        COMPLETE,
        true,
        false,
    )
    .await;
}

#[tokio::test]
async fn case2_done_produced_but_discarded_after_the_final_bound() {
    assert_case(
        Scenario {
            name: "case 2: DONE discarded after final bound",
            hold_gate_until_exit: true,
            ..STALLED_COMPLETE
        },
        "",
        true,
        false,
    )
    .await;
}

#[tokio::test]
async fn case3_exit_without_done_while_an_inherited_handle_stays_open() {
    assert_case(
        Scenario {
            name: "case 3: no DONE produced, inherited handle open",
            stream: NO_DONE,
            hold_writer: true,
            elapsed: PAST_FIRST_BOUND,
            ..STALLED_COMPLETE
        },
        NO_DONE,
        true,
        false,
    )
    .await;
}

#[tokio::test]
async fn case4a_limit_received_before_the_bound_with_clean_eof() {
    assert_case(
        Scenario {
            name: "case 4a: LIMIT received, clean EOF",
            stream: QUOTA,
            exit_code: 1,
            release_first: true,
            ..STALLED_COMPLETE
        },
        QUOTA,
        false,
        true,
    )
    .await;
}

#[tokio::test]
async fn case4b_limit_received_before_the_bound_but_stdout_never_reaches_eof() {
    assert_case(
        Scenario {
            name: "case 4b: LIMIT received, stdout never EOF",
            stream: QUOTA,
            exit_code: 1,
            hold_writer: true,
            elapsed: PAST_FIRST_BOUND,
            ..STALLED_COMPLETE
        },
        QUOTA,
        true,
        false,
    )
    .await;
}

#[tokio::test]
async fn case5_limit_produced_but_discarded_after_the_final_bound() {
    assert_case(
        Scenario {
            name: "case 5: LIMIT discarded after final bound",
            stream: QUOTA,
            exit_code: 1,
            hold_gate_until_exit: true,
            ..STALLED_COMPLETE
        },
        "",
        true,
        false,
    )
    .await;
}

// Reality B: turn one completed, then turn two (TURN, a transient error) was
// produced before exit 0 but never delivered. The consumer sees exactly what it
// sees in case 1, where nothing was lost.
const STALE: &str =
    "READY native-mock-session\nTURN\nTEXT deterministic progress\nDONE\nTURN\nERROR network\n";

#[tokio::test]
async fn case_b_lost_second_turn_is_indistinguishable_from_case1() {
    assert_case(
        Scenario {
            name: "case B: completed turn, second turn lost",
            stream: STALE,
            gate_at: 1,
            cap_before_gate: Some(COMPLETE.len()),
            hold_gate_until_exit: true,
            ..STALLED_COMPLETE
        },
        COMPLETE,
        true,
        false,
    )
    .await;
}
