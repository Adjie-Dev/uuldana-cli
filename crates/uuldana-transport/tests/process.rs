//! Real-process boundary tests; no installed agent is used.
#[path = "../../uuldana-runtime/tests/fixture_paths/mod.rs"]
mod fixture_paths;
#[path = "../../uuldana-runtime/tests/process_assertions/mod.rs"]
mod process_assertions;
use std::time::Duration;
use uuldana_core::*;
use uuldana_transport::{Frame, Process};

fn plan(mode: &str, pty: bool) -> (LaunchPlan, LaunchRequest) {
    let cwd = std::env::current_dir().unwrap();
    let permissions = validate_mapping(
        &PermissionPolicy::default(),
        &PermissionMapping::new(PermissionPolicy::default()),
    )
    .unwrap();
    let kind = if pty {
        TransportKind::NativePty
    } else {
        TransportKind::CustomCli
    };
    let request = LaunchRequest::new(cwd.clone(), kind, permissions);
    let spec = if pty {
        TransportSpec::NativePty {
            initial_size: TerminalSize::DEFAULT,
        }
    } else {
        TransportSpec::CustomCli {
            stdout: StreamFraming::Lines,
            stdin: StdinMode::Piped,
        }
    };
    (
        LaunchPlan::new(
            std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("examples")
                .join(format!(
                    "uuldana-test-child{}",
                    std::env::consts::EXE_SUFFIX
                )),
            cwd,
            spec,
        )
        .arg(mode),
        request,
    )
}

/// Failure diagnostics: the frame order the consumer saw. Only call it from
/// an assert message, so a passing run never formats anything.
fn describe_frames(frames: &[Frame]) -> String {
    use std::fmt::Write;
    let mut out = format!("{} frames\n", frames.len());
    let mut records = 0usize;
    for (index, frame) in frames.iter().enumerate() {
        if matches!(frame, Frame::Message(text) if text.starts_with("record-")) {
            records += 1;
            continue;
        }
        if records > 0 {
            let _ = writeln!(out, "  ... {records} record-* messages");
            records = 0;
        }
        match frame {
            Frame::Output(bytes) => {
                let _ = writeln!(out, "  #{index} Output({} bytes)", bytes.len());
            }
            Frame::Stderr(bytes) => {
                let _ = writeln!(out, "  #{index} Stderr({} bytes)", bytes.len());
            }
            other => {
                let _ = writeln!(out, "  #{index} {other:?}");
            }
        }
    }
    if records > 0 {
        let _ = writeln!(out, "  ... {records} record-* messages");
    }
    out
}

async fn collect(process: &mut Process) -> Vec<Frame> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut frames = Vec::new();
        while let Some(frame) = process.next_frame().await.unwrap() {
            frames.push(frame);
        }
        frames
    })
    .await
    .expect("child must exit and output must drain")
}

#[tokio::test]
async fn delayed_pipe_consumer_preserves_large_pre_exit_output() {
    for (mode, terminal) in [("burst-done", "DONE"), ("burst-limit", "LIMIT quota")] {
        let (plan, request) = plan(mode, false);
        let mut process = Process::spawn(&plan, &request).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        let frames = collect(&mut process).await;
        let records: Vec<_> = frames
            .iter()
            .filter_map(|frame| match frame {
                Frame::Message(record) if record.starts_with("record-") => Some(record.as_str()),
                _ => None,
            })
            .collect();
        let expected: Vec<_> = (0..200).map(|index| format!("record-{index:03}")).collect();
        assert_eq!(
            records,
            expected.iter().map(String::as_str).collect::<Vec<_>>(),
            "{mode}\n{}",
            describe_frames(&frames)
        );
        assert_eq!(
            frames
                .iter()
                .filter(|frame| matches!(frame, Frame::Message(value) if value == terminal))
                .count(),
            1,
            "{mode}\n{}",
            describe_frames(&frames)
        );
        assert_eq!(
            frames
                .iter()
                .filter(|frame| matches!(frame, Frame::Exited(ExitInfo { code: Some(0), .. })))
                .count(),
            1,
            "{mode}\n{}",
            describe_frames(&frames)
        );
    }
}

#[tokio::test]
async fn delayed_pty_consumer_preserves_large_pre_exit_output() {
    let (plan, request) = plan("burst-large", true);
    let mut process = Process::spawn(&plan, &request).await.unwrap();
    // Only ConPTY asks the terminal for its cursor position at start. A Unix
    // pseudo terminal never asks, and waiting for the question there would
    // spin on a finished child without ever yielding to the timeout.
    #[cfg(windows)]
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut bytes = Vec::new();
        loop {
            if let Some(Frame::Output(output)) = process.next_frame().await.unwrap() {
                bytes.extend(output);
            }
            if bytes.windows(4).any(|window| window == b"\x1b[6n") {
                process.write_input(b"\x1b[1;1R").await.unwrap();
                break;
            }
        }
    })
    .await
    .expect("answer the ConPTY cursor-size handshake");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let frames = collect(&mut process).await;
    let bytes: Vec<_> = frames
        .iter()
        .filter_map(|frame| match frame {
            Frame::Output(bytes) => Some(bytes.as_slice()),
            _ => None,
        })
        .flatten()
        .copied()
        .collect();
    let output = String::from_utf8_lossy(&bytes);
    let mut records = Vec::new();
    let mut remaining = output.as_ref();
    while let Some((_, tail)) = remaining.split_once("record-") {
        if tail.len() < 3 {
            break;
        }
        if let Ok(index) = tail[..3].parse::<u16>() {
            records.push(index);
        }
        remaining = tail;
    }
    assert_eq!(
        records,
        (0..200).collect::<Vec<_>>(),
        "{}",
        describe_frames(&frames)
    );
    assert!(
        output.contains("DONE"),
        "PTY terminal record missing\n{}",
        describe_frames(&frames)
    );
    assert_eq!(
        frames
            .iter()
            .filter(|frame| matches!(frame, Frame::Exited(ExitInfo { code: Some(0), .. })))
            .count(),
        1,
        "{}",
        describe_frames(&frames)
    );
}

#[cfg(windows)]
#[tokio::test]
async fn delayed_cmd_echo_done_is_delivered_before_successful_exit() {
    let (mut plan, request) = plan("records", false);
    plan.program = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
        .join("System32")
        .join("cmd.exe");
    plan.args = vec!["/d".into(), "/c".into(), "echo DONE".into()];
    let mut process = Process::spawn(&plan, &request).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let frames = collect(&mut process).await;
    assert_eq!(
        frames
            .iter()
            .filter_map(|f| if let Frame::Message(s) = f {
                Some(s.as_str())
            } else {
                None
            })
            .collect::<Vec<_>>(),
        ["DONE"],
        "{}",
        describe_frames(&frames)
    );
    assert_eq!(
        frames.last(),
        Some(&Frame::Exited(ExitInfo::from_code(0))),
        "{}",
        describe_frames(&frames)
    );
    assert_eq!(
        frames
            .iter()
            .filter(|f| matches!(f, Frame::Exited(_)))
            .count(),
        1,
        "{}",
        describe_frames(&frames)
    );
}

#[tokio::test]
async fn lines_stderr_and_exit_are_delivered_before_one_final_exit() {
    let (plan, request) = plan("records", false);
    let mut process = Process::spawn(&plan, &request).await.unwrap();
    let frames = collect(&mut process).await;
    let messages: Vec<_> = frames
        .iter()
        .filter_map(|f| {
            if let Frame::Message(s) = f {
                Some(s.as_str())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(messages, ["first", "second", "tail"]);
    assert!(
        frames
            .iter()
            .any(|f| matches!(f, Frame::Stderr(s) if s == b"error-stream"))
    );
    assert_eq!(frames.last(), Some(&Frame::Exited(ExitInfo::from_code(0))));
    assert_eq!(
        frames
            .iter()
            .filter(|f| matches!(f, Frame::Exited(_)))
            .count(),
        1
    );
}

#[tokio::test]
async fn pty_forwards_input_ansi_and_child_observes_resize() {
    let (plan, request) = plan("interactive", true);
    let mut process = Process::spawn(&plan, &request).await.unwrap();
    let mut output = Vec::new();
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        let mut cursor_replied = false;
        loop {
            if let Some(Frame::Output(bytes)) = process.next_frame().await.unwrap() {
                output.extend(bytes);
            }
            if !cursor_replied && output.windows(4).any(|b| b == b"\x1b[6n") {
                process.write_input(b"\x1b[1;1R").await.unwrap();
                cursor_replied = true;
            }
            if String::from_utf8_lossy(&output).contains("READY") {
                break;
            }
        }
        process.resize(TerminalSize::new(37, 111)).unwrap();
        process.write_input(b"size\r").await.unwrap();
        loop {
            if let Some(Frame::Output(bytes)) = process.next_frame().await.unwrap() {
                output.extend(bytes);
            }
            if String::from_utf8_lossy(&output).contains("SIZE=37x111") {
                break;
            }
        }
        process.write_input(b"hello\r").await.unwrap();
        process.write_input(b"quit\r").await.unwrap();
        for frame in collect(&mut process).await {
            if let Frame::Output(bytes) = frame {
                output.extend(bytes);
            }
        }
    })
    .await;
    assert!(
        result.is_ok(),
        "PTY timeout; output={:?}",
        String::from_utf8_lossy(&output)
    );
    let text = String::from_utf8_lossy(&output);
    assert!(text.contains("ECHO=hello"), "{text}");
    assert!(text.contains("error-stream"), "PTY combines stderr: {text}");
    assert!(output.contains(&0x1b), "ANSI must reach terminal consumer");
}

#[tokio::test]
async fn invalid_plan_is_rejected() {
    let (mut plan, request) = plan("records", true);
    plan.cwd = std::path::PathBuf::from("relative");
    assert!(Process::spawn(&plan, &request).await.is_err());
}

#[cfg(windows)]
#[tokio::test]
async fn oversized_conpty_dimensions_are_rejected() {
    let (mut plan, request) = plan("records", true);
    plan.transport = TransportSpec::NativePty {
        initial_size: TerminalSize::new(24, 40000),
    };
    assert!(Process::spawn(&plan, &request).await.is_err());
}

#[tokio::test]
async fn kill_reaps_direct_child_and_cannot_kill_after_exit() {
    let (plan, request) = plan("wait", false);
    let mut process = Process::spawn(&plan, &request).await.unwrap();
    assert!(process.force_terminate().unwrap());
    let frames = collect(&mut process).await;
    assert!(matches!(frames.last(), Some(Frame::Exited(info)) if !info.success));
    assert!(!process.force_terminate().unwrap());
}

#[tokio::test]
async fn pty_force_termination_drains_and_observes_exit() {
    let (plan, request) = plan("interactive", true);
    let mut process = Process::spawn(&plan, &request).await.unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut bytes = Vec::<u8>::new();
        let mut cursor = false;
        loop {
            if let Some(Frame::Output(b)) = process.next_frame().await.unwrap() {
                bytes.extend(b);
            }
            if !cursor && bytes.windows(4).any(|b| b == b"\x1b[6n") {
                process.write_input(b"\x1b[1;1R").await.unwrap();
                cursor = true;
            }
            if String::from_utf8_lossy(&bytes).contains("READY") {
                break;
            }
        }
        assert!(process.force_terminate().unwrap());
        let frames = collect(&mut process).await;
        assert!(matches!(frames.last(), Some(Frame::Exited(exit)) if !exit.success));
        assert!(!process.force_terminate().unwrap());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn pty_ctrl_c_interrupts_the_native_child_and_preserves_exit_status() {
    let (plan, request) = plan("interruptible", true);
    let mut process = Process::spawn(&plan, &request).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut bytes = Vec::new();
        let mut cursor = false;
        loop {
            match process.next_frame().await.unwrap() {
                Some(Frame::Output(output)) => bytes.extend(output),
                other => panic!("unexpected frame before readiness: {other:?}"),
            }
            if !cursor && bytes.windows(4).any(|b| b == b"\x1b[6n") {
                process.write_input(b"\x1b[1;1R").await.unwrap();
                cursor = true;
            }
            if String::from_utf8_lossy(&bytes).contains("READY") {
                break;
            }
        }
        process
            .shutdown_step(&ShutdownStep::Interrupt)
            .await
            .unwrap();
        let frames = collect(&mut process).await;
        assert!(matches!(frames.last(), Some(Frame::Exited(exit)) if !exit.success));
        assert_eq!(
            frames
                .iter()
                .filter(|f| matches!(f, Frame::Exited(_)))
                .count(),
            1
        );
        assert!(!process.force_terminate().unwrap());
    })
    .await
    .expect("Ctrl+C must stop the native child without force termination");
}

/// Git Bash hands every child an inherited "ignore CTRL+C" attribute. A process
/// created in its own process group carries the same attribute, so re-running the
/// Ctrl+C test inside one shows the regression under any launcher.
#[cfg(windows)]
#[test]
fn pty_ctrl_c_works_when_the_creator_ignores_ctrl_c() {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x200;
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "pty_ctrl_c_interrupts_the_native_child_and_preserves_exit_status",
            "--exact",
            "--test-threads=1",
        ])
        .creation_flags(CREATE_NEW_PROCESS_GROUP)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed; 0 failed"),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The 32-bit system directory of this host; the images there are x86 builds.
#[cfg(windows)]
fn syswow64(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
        .join("SysWOW64")
        .join(name)
}

/// The COFF machine field of a PE image: 0x014c is x86, 0x8664 is x64.
#[cfg(windows)]
fn image_machine(path: &std::path::Path) -> u16 {
    let bytes = std::fs::read(path).unwrap();
    let pe = u32::from_le_bytes(bytes[0x3c..0x40].try_into().unwrap()) as usize;
    assert_eq!(&bytes[pe..pe + 4], b"PE\0\0", "{path:?} is not a PE image");
    u16::from_le_bytes(bytes[pe + 4..pe + 6].try_into().unwrap())
}

/// Runs the test-only process discovery helper (`uuldana-test-procinfo`, a
/// separate crate outside the workspace). A helper that is missing or cannot
/// run is a test failure, never an empty or "running" answer.
#[cfg(windows)]
fn procinfo(args: &[&str]) -> std::process::Output {
    let helper = std::env::var_os("UULDANA_TEST_PROCINFO")
        .expect("UULDANA_TEST_PROCINFO must name the uuldana-test-procinfo helper");
    std::process::Command::new(helper)
        .args(args)
        .output()
        .unwrap()
}

/// Start time of every descendant `child_pids` has reported, by process id. A later
/// `process_is_running` for that id then asks about that exact process, so an id the
/// system has recycled for an unrelated process is not mistaken for the descendant.
#[cfg(windows)]
static SEEN_STARTS: std::sync::Mutex<Vec<(u32, u64)>> = std::sync::Mutex::new(Vec::new());

/// Processes whose parent is `parent` and that started after it, read through
/// the OS rather than from the transport under test. The start-time check keeps
/// a recycled parent id from adopting an unrelated, older process.
#[cfg(windows)]
fn child_pids(parent: u32) -> Vec<(u32, String)> {
    let listing = procinfo(&["tree", &parent.to_string()]);
    assert!(
        listing.status.success(),
        "process listing failed: {}",
        String::from_utf8_lossy(&listing.stderr)
    );
    String::from_utf8_lossy(&listing.stdout)
        .lines()
        .map(|line| {
            let mut fields = line.split('\t');
            let pid: u32 = fields.next().unwrap().parse().unwrap();
            // Fields are: pid, start time, image name, image path, machine.
            let started: u64 = fields.next().unwrap().parse().unwrap();
            let name = fields.next().unwrap().to_string();
            if started != 0 {
                let mut seen = SEEN_STARTS.lock().unwrap();
                seen.retain(|(known, _)| *known != pid);
                seen.push((pid, started));
            }
            (pid, name)
        })
        .collect()
}

/// Whether a process with this id and image name is still alive.
#[cfg(windows)]
fn process_is_running(pid: u32, image: &str) -> bool {
    let started = SEEN_STARTS
        .lock()
        .unwrap()
        .iter()
        .find(|(known, _)| *known == pid)
        .map(|(_, started)| started.to_string());
    let mut args = vec!["alive".to_string(), pid.to_string(), image.to_string()];
    args.extend(started);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let answer = procinfo(&args);
    match answer.status.code() {
        Some(0) => true,
        Some(1) => false,
        _ => panic!(
            "process lookup failed: {}",
            String::from_utf8_lossy(&answer.stderr)
        ),
    }
}

/// Runs a real 32-bit program in the PTY, interrupts it with CTRL+C and checks
/// the whole lifecycle: native status, one exit, no forced stop, no leftovers.
/// Returns the terminal output so callers can check what the child printed.
#[cfg(windows)]
async fn interrupt_a_32_bit_child(
    program: std::path::PathBuf,
    args: &[&str],
    descendant: Option<&str>,
) -> String {
    const STATUS_CONTROL_C_EXIT: i32 = 0xC000_013Au32 as i32;
    assert_eq!(image_machine(&program), 0x014c, "{program:?} must be x86");
    let program_name = program.file_name().unwrap().to_string_lossy().into_owned();
    let cwd = std::env::current_dir().unwrap();
    let permissions = validate_mapping(
        &PermissionPolicy::default(),
        &PermissionMapping::new(PermissionPolicy::default()),
    )
    .unwrap();
    let request = LaunchRequest::new(cwd.clone(), TransportKind::NativePty, permissions);
    let plan = LaunchPlan::new(
        program,
        cwd,
        TransportSpec::NativePty {
            initial_size: TerminalSize::DEFAULT,
        },
    )
    .args(args.iter().copied());
    let mut process = Process::spawn(&plan, &request).await.unwrap();
    let pid = process.process_id().unwrap();
    let image = program_name;
    let mut descendants = Vec::new();
    let (frames, output) = tokio::time::timeout(Duration::from_secs(30), async {
        let mut bytes = Vec::new();
        let mut cursor = false;
        loop {
            match process.next_frame().await.unwrap() {
                Some(Frame::Output(output)) => bytes.extend(output),
                other => panic!("unexpected frame before readiness: {other:?}"),
            }
            if !cursor && bytes.windows(4).any(|b| b == b"\x1b[6n") {
                process.write_input(b"\x1b[1;1R").await.unwrap();
                cursor = true;
            }
            if String::from_utf8_lossy(&bytes).contains("Pinging") {
                break;
            }
        }
        descendants = child_pids(pid);
        process
            .shutdown_step(&ShutdownStep::Interrupt)
            .await
            .unwrap();
        let frames = collect(&mut process).await;
        for frame in &frames {
            if let Frame::Output(output) = frame {
                bytes.extend(output);
            }
        }
        (frames, String::from_utf8_lossy(&bytes).into_owned())
    })
    .await
    .expect("CTRL+C must stop the 32-bit child without force termination");
    let exits: Vec<_> = frames
        .iter()
        .filter_map(|f| match f {
            Frame::Exited(exit) => Some(*exit),
            _ => None,
        })
        .collect();
    assert_eq!(exits.len(), 1, "{}", describe_frames(&frames));
    assert!(matches!(frames.last(), Some(Frame::Exited(_))));
    assert_eq!(exits[0].code, Some(STATUS_CONTROL_C_EXIT), "{output:?}");
    assert!(!exits[0].success);
    assert_eq!(process.observed_exit(), Some(exits[0]));
    assert!(
        !process.force_terminate().unwrap(),
        "an interrupted child must not need force termination"
    );
    assert!(
        !frames.iter().any(|f| matches!(f, Frame::Diagnostic(_))),
        "{}",
        describe_frames(&frames)
    );
    assert!(
        !process_is_running(pid, &image),
        "the direct child must be gone"
    );
    if let Some(expected) = descendant {
        assert!(
            descendants
                .iter()
                .any(|(_, name)| name.eq_ignore_ascii_case(expected)),
            "{expected} was never seen below the child: {descendants:?}"
        );
    }
    for (descendant, name) in descendants {
        assert!(
            !process_is_running(descendant, &name),
            "descendant {descendant} ({name}) must be gone"
        );
    }
    output
}

#[cfg(windows)]
#[tokio::test]
async fn pty_ctrl_c_interrupts_a_32_bit_child_and_preserves_exit_status() {
    interrupt_a_32_bit_child(syswow64("ping.exe"), &["-n", "30", "127.0.0.1"], None).await;
}

/// A 32-bit shell proves its own bitness at run time (a WOW64 process sees
/// PROCESSOR_ARCHITECTURE=x86) and owns a 32-bit descendant that must go too.
#[cfg(windows)]
#[tokio::test]
async fn pty_ctrl_c_interrupts_a_32_bit_shell_and_its_descendant() {
    let output = interrupt_a_32_bit_child(
        syswow64("cmd.exe"),
        &[
            "/d",
            "/c",
            "echo ARCH=%PROCESSOR_ARCHITECTURE%& ping -n 30 127.0.0.1",
        ],
        Some("PING.EXE"),
    )
    .await;
    assert!(output.contains("ARCH=x86"), "{output:?}");
}

/// The same two tests inside a process group, which carries the inherited
/// "ignore CTRL+C" attribute exactly as Git Bash passes it on.
#[cfg(windows)]
#[test]
fn pty_ctrl_c_reaches_32_bit_children_when_the_creator_ignores_ctrl_c() {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x200;
    for test in [
        "pty_ctrl_c_interrupts_a_32_bit_child_and_preserves_exit_status",
        "pty_ctrl_c_interrupts_a_32_bit_shell_and_its_descendant",
    ] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([test, "--exact", "--test-threads=1"])
            .creation_flags(CREATE_NEW_PROCESS_GROUP)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed; 0 failed"),
            "{test}\n{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[tokio::test]
async fn argv_is_preserved_without_a_shell() {
    for pty in [false, true] {
        let (plan, request) = plan("arguments", pty);
        let plan = plan.args(["space value", "quote\"value", "a&b", "%PATH%", "utf8-λ"]);
        let mut process = Process::spawn(&plan, &request).await.unwrap();
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(15), async {
            let mut cursor = false;
            while let Some(frame) = process.next_frame().await.unwrap() {
                match frame {
                    Frame::Message(s) => bytes.extend_from_slice(s.as_bytes()),
                    Frame::Output(b) => bytes.extend(b),
                    _ => {}
                }
                if !cursor && bytes.windows(4).any(|b| b == b"\x1b[6n") {
                    process.write_input(b"\x1b[1;1R").await.unwrap();
                    cursor = true;
                }
            }
        })
        .await
        .unwrap();
        let output = String::from_utf8_lossy(&bytes);
        for expected in ["space value", "quote\"value", "a&b", "%PATH%", "utf8-λ"] {
            assert!(output.contains(expected), "{output:?}");
        }
    }
}

#[tokio::test]
async fn closed_stdin_is_eof_and_cannot_accept_input() {
    let (mut plan, request) = plan("eof", false);
    plan.transport = TransportSpec::CustomCli {
        stdout: StreamFraming::Lines,
        stdin: StdinMode::Closed,
    };
    let mut process = Process::spawn(&plan, &request).await.unwrap();
    assert!(process.write_input(b"ignored").await.is_err());
    assert!(
        collect(&mut process)
            .await
            .contains(&Frame::Message("EOF=true".into()))
    );
}

#[tokio::test]
async fn concurrent_large_stdout_and_stderr_do_not_deadlock_or_truncate() {
    let (plan, request) = plan("flood", false);
    let mut process = Process::spawn(&plan, &request).await.unwrap();
    let frames = collect(&mut process).await;
    assert_eq!(
        frames
            .iter()
            .filter(|f| matches!(f, Frame::Message(s) if s.len() == 8191))
            .count(),
        256
    );
    assert_eq!(
        frames
            .iter()
            .filter_map(|f| if let Frame::Stderr(b) = f {
                Some(b.len())
            } else {
                None
            })
            .sum::<usize>(),
        256 * 8192
    );
    assert_eq!(frames.last(), Some(&Frame::Exited(ExitInfo::from_code(0))));
}

#[tokio::test]
async fn framing_rejects_non_utf8_and_oversized_records() {
    for (mode, encoding) in [("invalid-utf8", true), ("huge-record", false)] {
        let (plan, request) = plan(mode, false);
        let mut process = Process::spawn(&plan, &request).await.unwrap();
        let error = tokio::time::timeout(Duration::from_secs(15), process.next_frame())
            .await
            .unwrap()
            .unwrap_err();
        assert!(if encoding {
            matches!(error, uuldana_transport::Error::InvalidEncoding)
        } else {
            matches!(error, uuldana_transport::Error::RecordTooLarge)
        });
        let frames = collect(&mut process).await;
        assert!(
            frames.contains(&Frame::Message("valid".into())),
            "{frames:?}"
        );
    }
}

#[tokio::test]
async fn child_environment_overlay_and_cwd_are_applied_without_changing_parent() {
    let parent = std::env::var_os("UULDANA_FIXTURE_SET");
    let (plan, request) = plan("environment", false);
    let plan = plan
        .env_set("UULDANA_FIXTURE_SET", "child-only")
        .env_set("UULDANA_FIXTURE_REMOVE", "temporary")
        .env_remove("UULDANA_FIXTURE_REMOVE");
    let mut process = Process::spawn(&plan, &request).await.unwrap();
    let frames = collect(&mut process).await;
    assert!(frames.contains(&Frame::Message("SET=child-only".into())));
    assert!(frames.contains(&Frame::Message("REMOVE=absent".into())));
    assert!(frames.contains(&Frame::Message(format!("CWD={}", plan.cwd.display()))));
    assert_eq!(std::env::var_os("UULDANA_FIXTURE_SET"), parent);
}

#[tokio::test]
async fn dropping_a_live_process_terminates_the_direct_child() {
    let (plan, request) = plan("wait", false);
    let process = Process::spawn(&plan, &request).await.unwrap();
    let pid = process.process_id().unwrap();
    fn alive(pid: u32) -> bool {
        if cfg!(windows) {
            let out = std::process::Command::new("tasklist.exe")
                .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
        } else {
            std::process::Command::new("/bin/kill")
                .args(["-0", &pid.to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
                .success()
        }
    }
    assert!(alive(pid));
    drop(process);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while alive(pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "direct child leaked after drop"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn independent_input_and_output_handle_full_duplex_backpressure() {
    let (mut plan, request) = plan("duplex", false);
    plan.transport = TransportSpec::CustomCli {
        stdout: StreamFraming::Raw,
        stdin: StdinMode::Piped,
    };
    let mut process = Process::spawn(&plan, &request).await.unwrap();
    let input = process.input_handle().unwrap();
    let writer = tokio::spawn(async move { input.write(&vec![b'y'; 2 * 1024 * 1024]).await });
    let frames = collect(&mut process).await;
    tokio::time::timeout(Duration::from_secs(5), writer)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let bytes: Vec<_> = frames
        .iter()
        .filter_map(|f| {
            if let Frame::Output(b) = f {
                Some(b.as_slice())
            } else {
                None
            }
        })
        .flatten()
        .copied()
        .collect();
    assert_eq!(
        bytes.iter().filter(|b| **b == b'x').count(),
        2 * 1024 * 1024
    );
    assert!(bytes.ends_with(b"READ=2097152\n"));
    assert_eq!(frames.last(), Some(&Frame::Exited(ExitInfo::from_code(0))));
}

#[tokio::test]
async fn closing_input_disables_retained_handles_and_delivers_eof() {
    let (plan, request) = plan("eof", false);
    let mut process = Process::spawn(&plan, &request).await.unwrap();
    let input = process.input_handle().unwrap();
    process.close_input();
    assert!(input.write(b"late").await.is_err());
    assert!(
        collect(&mut process)
            .await
            .contains(&Frame::Message("EOF=true".into()))
    );
}

#[cfg(windows)]
#[tokio::test]
async fn windows_pty_exit_259_is_not_misclassified_as_still_active() {
    let (plan, request) = plan("exit259", true);
    let mut process = Process::spawn(&plan, &request).await.unwrap();
    let mut bytes = Vec::<u8>::new();
    let frames = tokio::time::timeout(Duration::from_secs(15), async {
        let mut cursor = false;
        let mut frames = Vec::new();
        while let Some(frame) = process.next_frame().await.unwrap() {
            if let Frame::Output(b) = &frame {
                bytes.extend(b);
            }
            if !cursor && bytes.windows(4).any(|b| b == b"\x1b[6n") {
                process.write_input(b"\x1b[1;1R").await.unwrap();
                cursor = true;
            }
            frames.push(frame);
        }
        frames
    })
    .await
    .unwrap();
    assert_eq!(
        frames.last(),
        Some(&Frame::Exited(ExitInfo::from_code(259)))
    );
}

#[tokio::test]
async fn multiple_bad_records_each_report_one_error_and_resynchronize() {
    let (plan, request) = plan("multiple-errors", false);
    let mut process = Process::spawn(&plan, &request).await.unwrap();
    let mut errors = Vec::new();
    let mut messages = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match process.next_frame().await {
                Err(uuldana_transport::Error::InvalidEncoding) => errors.push("encoding"),
                Err(uuldana_transport::Error::RecordTooLarge) => errors.push("size"),
                Ok(Some(Frame::Message(s))) => messages.push(s),
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(errors, ["encoding", "encoding", "size"]);
    assert_eq!(messages, ["first", "last"]);
}

async fn tree_fixture(mode: &str, pty: bool) -> (Process, std::path::PathBuf, u32) {
    let heartbeat = fixture_paths::heartbeat_path("uuldana-heartbeat");
    let (plan, request) = plan(mode, pty);
    let mut process = Process::spawn(&plan.arg(heartbeat.as_os_str()), &request)
        .await
        .unwrap();
    let pid = tokio::time::timeout(Duration::from_secs(10), async {
        let mut bytes = Vec::new();
        let mut cursor = false;
        loop {
            match process.next_frame().await.unwrap() {
                Some(Frame::Message(s)) if s.starts_with("DESCENDANT=") => {
                    break s.trim_start_matches("DESCENDANT=").parse::<u32>().unwrap();
                }
                Some(Frame::Output(b)) => bytes.extend(b),
                other => panic!("unexpected fixture frame: {other:?}"),
            }
            if !cursor && bytes.windows(4).any(|b| b == b"\x1b[6n") {
                process.write_input(b"\x1b[1;1R").await.unwrap();
                cursor = true;
            }
            let text = String::from_utf8_lossy(&bytes);
            if let Some(rest) = text.split("DESCENDANT=").nth(1)
                && let Some((line, _)) = rest.split_once('\n')
                && let Ok(pid) = line.trim().parse::<u32>()
            {
                break pid;
            }
        }
    })
    .await
    .unwrap();
    (process, heartbeat, pid)
}

async fn assert_heartbeat_stopped(path: &std::path::Path) {
    // Observe actual work cessation, independent of PID reuse or zombie state.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let before = std::fs::read(path).ok();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let after = std::fs::read(path);
    assert_eq!(
        after.as_ref().ok(),
        before.as_ref(),
        "descendant survived cleanup; heartbeat={path:?}; read_error={:?}",
        after.as_ref().err()
    );
    if path.exists() {
        std::fs::remove_file(path).unwrap();
    }
}

#[tokio::test]
async fn direct_exit_is_bounded_when_descendant_inherits_stdout() {
    let (mut process, heartbeat, pid) = tree_fixture("descendant-exit", false).await;
    let frames = tokio::time::timeout(Duration::from_secs(2), collect(&mut process))
        .await
        .expect("descendant stdout must not delay direct-child finalization");
    assert_eq!(frames.last(), Some(&Frame::Exited(ExitInfo::from_code(0))));
    process_assertions::assert_process_exited(pid);
    assert_heartbeat_stopped(&heartbeat).await;
}

#[tokio::test]
async fn force_termination_cleans_descendants_in_both_transports() {
    for pty in [false, true] {
        let (mut process, heartbeat, pid) = tree_fixture("descendant-wait", pty).await;
        assert!(process.force_terminate().unwrap());
        let frames = collect(&mut process).await;
        assert!(matches!(frames.last(), Some(Frame::Exited(e)) if !e.success));
        process_assertions::assert_process_exited(pid);
        assert_heartbeat_stopped(&heartbeat).await;
    }
}

#[tokio::test]
async fn dropping_owner_cleans_descendants_in_both_transports() {
    for pty in [false, true] {
        let (process, heartbeat, pid) = tree_fixture("descendant-wait", pty).await;
        drop(process);
        process_assertions::assert_process_exited(pid);
        assert_heartbeat_stopped(&heartbeat).await;
    }
}

#[cfg(windows)]
mod windows_containment {
    use super::*;
    use std::io::{Read, Write};

    fn marker() -> std::path::PathBuf {
        fixture_paths::heartbeat_path("uuldana-early")
    }

    fn suspended(
        path: &std::path::Path,
    ) -> (
        portable_pty::win::SuspendedChild,
        Box<dyn portable_pty::MasterPty + Send>,
    ) {
        let (plan, _) = plan("containment-early", true);
        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize::default())
            .unwrap();
        let mut reader = pair.master.try_clone_reader().unwrap();
        let mut writer = pair.master.take_writer().unwrap();
        // Service ConPTY's cursor request even while the child is suspended.
        // A mistakenly running child cannot hide behind the cursor handshake.
        std::thread::spawn(move || {
            let mut seen = Vec::new();
            let mut chunk = [0; 8192];
            while let Ok(n) = reader.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                seen.extend_from_slice(&chunk[..n]);
                if seen.windows(4).any(|b| b == b"\x1b[6n") {
                    let _ = writer.write_all(b"\x1b[1;1R");
                    let _ = writer.flush();
                    seen.clear();
                }
            }
        });
        let mut cmd = portable_pty::CommandBuilder::new(&plan.program);
        cmd.arg("containment-early");
        cmd.arg(path);
        cmd.cwd(plan.cwd);
        let child = pair.slave.spawn_command_suspended(cmd).unwrap();
        (child, pair.master)
    }

    fn wait_marker(path: &std::path::Path) -> (u32, u32) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(text) = std::fs::read_to_string(path) {
                let pids: Vec<u32> = text
                    .split_whitespace()
                    .filter_map(|s| s.parse().ok())
                    .collect();
                if pids.len() == 2 {
                    return (pids[0], pids[1]);
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "fixture did not execute after resume"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn suspended_pty_cannot_execute_before_assignment_and_descendant_joins_job() {
        let path = marker();
        let (guard, master) = suspended(&path);
        let parent = guard.process_id().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(!path.exists(), "child executed before assignment");
        let job = subc_jobobject::JobObject::new().unwrap();
        assert_eq!(job.process_count().unwrap(), 0);
        let child = guard.resume_in_job(&job).unwrap();
        let (recorded, descendant) = wait_marker(&path);
        assert_eq!(recorded, parent);
        assert_eq!(
            job.process_count().unwrap(),
            2,
            "immediately spawned descendant must inherit containment"
        );
        job.terminate().unwrap();
        process_assertions::assert_process_exited(parent);
        process_assertions::assert_process_exited(descendant);
        drop(child);
        drop(job);
        std::thread::spawn(move || drop(master));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn abandoned_suspended_pty_is_terminated_without_ever_executing() {
        let path = marker();
        let (guard, master) = suspended(&path);
        let status = guard.process_status().unwrap();
        drop(guard);
        assert!(status.has_exited().unwrap());
        assert!(!path.exists());
        std::thread::spawn(move || drop(master));
    }

    #[test]
    fn failed_job_assignment_terminates_suspended_pty_without_execution() {
        let path = marker();
        let (guard, master) = suspended(&path);
        let status = guard.process_status().unwrap();
        let other_path = marker();
        let (other, other_master) = suspended(&other_path);
        let other_status = other.process_status().unwrap();
        let first_job = subc_jobobject::JobObject::new().unwrap();
        let job = subc_jobobject::JobObject::new().unwrap();
        // A nonempty sibling Job cannot accept a process already in another
        // Job hierarchy. Both children remain alive and suspended at rejection.
        first_job.assign(&guard).unwrap();
        job.assign(&other).unwrap();
        assert!(!status.has_exited().unwrap());
        assert!(guard.resume_in_job(&job).is_err());
        assert!(status.has_exited().unwrap());
        assert!(!path.exists());
        drop(other);
        assert!(other_status.has_exited().unwrap());
        assert!(!other_path.exists());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while job.process_count().unwrap() != 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(job.process_count().unwrap(), 0);
        std::thread::spawn(move || drop(master));
        std::thread::spawn(move || drop(other_master));
    }

    async fn wait_running_marker(process: &mut Process, path: &std::path::Path) -> (u32, u32) {
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut bytes = Vec::new();
            let mut cursor = false;
            loop {
                if let Ok(text) = std::fs::read_to_string(path) {
                    let pids: Vec<u32> = text
                        .split_whitespace()
                        .filter_map(|s| s.parse().ok())
                        .collect();
                    if pids.len() == 2 {
                        return (pids[0], pids[1]);
                    }
                }
                tokio::select! {
                    frame = process.next_frame() => {
                        match frame.unwrap() {
                            Some(Frame::Output(output)) => bytes.extend(output),
                            Some(Frame::Diagnostic(_)) => {},
                            other => panic!("fixture exited before marker: {other:?}"),
                        }
                        if !cursor && bytes.windows(4).any(|b| b == b"\x1b[6n") {
                            process.write_input(b"\x1b[1;1R").await.unwrap();
                            cursor = true;
                        }
                    }
                    () = tokio::time::sleep(Duration::from_millis(10)) => {}
                }
            }
        })
        .await
        .expect("fixture must execute after contained resume")
    }

    #[tokio::test]
    async fn immediate_pty_descendants_are_contained_during_repeated_force_and_drop() {
        for index in 0..12 {
            let path = marker();
            let (plan, request) = plan("containment-early", true);
            let mut process = Process::spawn(&plan.arg(&path), &request).await.unwrap();
            let (parent, descendant) = wait_running_marker(&mut process, &path).await;
            assert_eq!(process.process_id(), Some(parent));
            if index % 2 == 0 {
                assert!(process.force_terminate().unwrap());
                let frames = collect(&mut process).await;
                assert!(matches!(frames.last(), Some(Frame::Exited(e)) if !e.success));
            }
            drop(process);
            process_assertions::assert_process_exited(parent);
            process_assertions::assert_process_exited(descendant);
            std::fs::remove_file(&path).unwrap();
            let _ = std::fs::remove_file(path.with_extension("heartbeat"));
        }
    }
}
