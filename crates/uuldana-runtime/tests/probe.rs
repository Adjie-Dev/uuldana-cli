//! Bounded read-only subprocess probes.
mod common;
mod fixture_paths;
mod process_assertions;
use std::time::{Duration, Instant};
use uuldana_core::{HostProbe, ProbeError};

#[test]
fn version_probe_captures_stdout_and_status() {
    let result = common::DiagnosticHost::default()
        .run(&common::fixture(), &["--version"], Duration::from_secs(5))
        .unwrap();
    assert_eq!(result.stdout, b"uuldana-mock 0.1\n");
    assert!(result.stderr.is_empty());
    assert!(result.exit.success);
}
#[test]
fn timed_out_probe_is_killed_and_reaped() {
    let result = common::DiagnosticHost::default().run(
        &common::fixture(),
        &["unresponsive"],
        Duration::from_millis(150),
    );
    assert_eq!(result, Err(ProbeError::TimedOut));
}
#[test]
fn relative_probe_program_is_denied_and_missing_absolute_program_is_not_found() {
    assert!(matches!(
        common::DiagnosticHost::default().run(
            std::path::Path::new("relative"),
            &[],
            Duration::from_secs(1)
        ),
        Err(ProbeError::Denied(_))
    ));
    let absent = std::env::current_dir()
        .unwrap()
        .join("definitely-missing-uuldana-executable");
    assert_eq!(
        common::DiagnosticHost::default().run(&absent, &[], Duration::from_secs(1)),
        Err(ProbeError::NotFound)
    );
}

fn heartbeat_path() -> std::path::PathBuf {
    fixture_paths::heartbeat_path("uuldana-probe-heartbeat")
}

fn assert_descendant_stopped(path: &std::path::Path) {
    assert!(
        path.is_file(),
        "fixture descendant must have started; heartbeat={path:?}"
    );
    std::thread::sleep(Duration::from_millis(300));
    let before = std::fs::read(path).unwrap_or_else(|e| panic!("heartbeat={path:?}: {e}"));
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(
        std::fs::read(path).unwrap_or_else(|e| panic!("heartbeat={path:?}: {e}")),
        before,
        "probe descendant remained alive; heartbeat={path:?}"
    );
    std::fs::remove_file(path).unwrap();
}

#[test]
fn parallel_heartbeat_paths_are_unique() {
    // Atomic sequence uniqueness is deterministic, not a probabilistic birthday
    // test. Four simultaneous callers exercise the same race without multiplying
    // every official process into twelve CPU-heavy fixture workers.
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                (0..128)
                    .map(|_| fixture_paths::heartbeat_path_at("uuldana-probe-heartbeat", 0))
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let paths: Vec<_> = workers
        .into_iter()
        .flat_map(|w| w.join().unwrap())
        .collect();
    let mut unique = std::collections::HashSet::new();
    let mut collisions = Vec::new();
    for path in &paths {
        if !unique.insert(path) && collisions.len() < 10 {
            collisions.push(path);
        }
    }
    eprintln!(
        "heartbeat uniqueness: generated={} unique={} duplicate_examples={collisions:?}",
        paths.len(),
        unique.len()
    );
    assert_eq!(
        unique.len(),
        paths.len(),
        "parallel fixture identifiers collided"
    );
}

#[test]
fn probe_direct_exit_cleans_output_holding_descendant() {
    let heartbeat = heartbeat_path();
    // Startup is independently bounded; it is not the EOF regression clock.
    // Allow the 30s probe, 3s failure cleanup, 3s runtime shutdown and 4s
    // scheduling margin. Cleanup assertions retain their separate 3s bound;
    // the official 60s process watchdog covers the entire test executable.
    let started = Instant::now();
    let watchdog = started + Duration::from_secs(40);
    let (tx, rx) = std::sync::mpsc::channel();
    let worker_path = heartbeat.clone();
    std::thread::spawn(move || {
        // Publish immediately after the real probe returns. Diagnostic sinks
        // can block; they must never sit inside the behavioral clock.
        let result = uuldana_runtime::LocalHost::new().run(
            &common::fixture(),
            &["tree-exit-synchronized", worker_path.to_str().unwrap()],
            Duration::from_secs(30),
        );
        let _ = tx.send((Instant::now(), result));
    });
    let ready = heartbeat.with_extension("ready");
    let startup_deadline = started + Duration::from_secs(15);
    while !ready.is_file() && Instant::now() < startup_deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let startup = started.elapsed();
    let parent = std::fs::read_to_string(&ready)
        .ok()
        .and_then(|s| s.parse::<u32>().ok());
    // Release even after a failed handshake so the bounded worker can clean up.
    std::fs::write(heartbeat.with_extension("release"), b"exit").unwrap();
    let released = Instant::now();
    let exit_deadline = released + Duration::from_secs(3);
    while parent.is_some_and(process_assertions::process_running) && Instant::now() < exit_deadline
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    let exited = Instant::now();
    let parent_exited = parent.is_some_and(|pid| !process_assertions::process_running(pid));
    // Native cleanup already has a 3s bound. Two additional seconds permit
    // test/worker scheduling; 5s remains far below the descendant's 60s life.
    let completion_deadline = exited + Duration::from_secs(5);
    let (completed, result) = rx
        .recv_timeout(watchdog.saturating_duration_since(Instant::now()))
        .expect("whole-probe watchdog exceeded 40s (including cleanup and runtime shutdown)");
    eprintln!(
        "direct-exit timing: startup={startup:?} release_to_exit={:?} exit_to_return={:?} total={:?} result={:?}",
        exited.duration_since(released),
        completed.saturating_duration_since(exited),
        completed.duration_since(started),
        result.as_ref().map(|output| output.exit),
    );
    // Check cleanup before timing assertions, including on probe failure.
    let pid = std::fs::read_to_string(heartbeat.with_extension("pid"))
        .expect("descendant PID receipt")
        .parse()
        .unwrap();
    process_assertions::assert_process_exited(pid);
    assert_descendant_stopped(&heartbeat);
    for extension in ["pid", "ready", "release"] {
        let _ = std::fs::remove_file(heartbeat.with_extension(extension));
    }
    assert!(
        parent.is_some(),
        "startup readiness exceeded 15s: {startup:?}"
    );
    assert!(
        parent_exited,
        "direct child did not exit within 3s of release"
    );
    assert!(
        completed <= completion_deadline,
        "completion waited more than 5s after observed direct exit; result={result:?}"
    );
    let result = result.unwrap_or_else(|e| {
        panic!("direct exit must not wait for descendant EOF; heartbeat={heartbeat:?}: {e:?}")
    });
    assert!(result.exit.success);
    let stdout = String::from_utf8_lossy(&result.stdout);
    let output_pid: u32 = stdout
        .lines()
        .find_map(|line| line.strip_prefix("DESCENDANT="))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(output_pid, pid);
}

#[test]
fn probe_timeout_cleans_descendant_before_returning() {
    let heartbeat = heartbeat_path();
    let result = common::DiagnosticHost::default().run(
        &common::fixture(),
        &["tree-force", heartbeat.to_str().unwrap()],
        Duration::from_secs(10),
    );
    let pid = std::fs::read_to_string(heartbeat.with_extension("pid"))
        .unwrap()
        .parse()
        .unwrap();
    process_assertions::assert_process_exited(pid);
    assert_descendant_stopped(&heartbeat);
    std::fs::remove_file(heartbeat.with_extension("pid")).unwrap();
    assert_eq!(result, Err(ProbeError::TimedOut), "heartbeat={heartbeat:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn synchronous_probe_can_run_from_inside_a_tokio_runtime() {
    let result = common::DiagnosticHost::default()
        .run(&common::fixture(), &["--version"], Duration::from_secs(5))
        .unwrap();
    assert_eq!(result.stdout, b"uuldana-mock 0.1\n");
    assert!(result.exit.success);
}

#[test]
fn probe_limits_stdout_and_stderr_independently() {
    for (mode, stream) in [
        ("probe-overflow-stdout", "stdout"),
        ("probe-overflow-stderr", "stderr"),
    ] {
        let result = common::DiagnosticHost::default().run(
            &common::fixture(),
            &[mode],
            Duration::from_secs(5),
        );
        assert!(
            matches!(result, Err(ProbeError::Io(ref message)) if message.contains(stream) && message.contains("1 MiB")),
            "{mode}: {result:?}"
        );
    }
}
