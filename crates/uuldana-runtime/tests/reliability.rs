//! Regression coverage for finalization, recovery, and external liveness.
mod common;
mod process_assertions;
mod support;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use support::*;
use uuldana_core::*;
use uuldana_runtime::StartOptions;

async fn run_case(mode: &'static str, decoder: DecoderMode) -> uuldana_runtime::Supervisor {
    let mut runtime = runtime(Arc::new(FixtureAdapter::new(mode, decoder)));
    runtime
        .start(StartOptions::new(
            ProviderId::new("test.fixture").unwrap(),
            RunId::new("r").unwrap(),
        ))
        .await
        .unwrap();
    common::drain_to_end(&mut runtime, Duration::from_secs(15)).await;
    assert_eq!(
        runtime
            .events()
            .iter()
            .filter(|e| matches!(e.event, AgentEvent::ProcessExited(_)))
            .count(),
        1,
        "{mode}\n{}",
        common::describe_run(&runtime)
    );
    assert_eq!(
        runtime
            .events()
            .iter()
            .filter(|e| matches!(
                e.event,
                AgentEvent::RunStateChanged {
                    to: RunState::Ended(_),
                    ..
                }
            ))
            .count(),
        1,
        "{mode}\n{}",
        common::describe_run(&runtime)
    );
    runtime
}

#[tokio::test]
async fn delayed_consumer_preserves_200_pre_exit_records_and_done() {
    delayed_burst("burst-complete", RunEnd::Completed).await;
}

#[tokio::test]
async fn delayed_consumer_preserves_200_pre_exit_records_and_limit() {
    delayed_burst("burst-quota", RunEnd::LimitReached(LimitKind::UsageQuota)).await;
}

async fn delayed_burst(mode: &'static str, expected: RunEnd) {
    let mut runtime = runtime(Arc::new(FixtureAdapter::new(mode, DecoderMode::Normal)));
    runtime
        .start(StartOptions::new(
            ProviderId::new("test.fixture").unwrap(),
            RunId::new("r").unwrap(),
        ))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    common::drain_to_end(&mut runtime, Duration::from_secs(15)).await;
    let messages: Vec<_> = runtime
        .events()
        .iter()
        .filter_map(|e| match &e.event {
            AgentEvent::MessageChunk { text, .. } if text.starts_with("record-") => {
                Some(text.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        messages,
        (0..200)
            .map(|i| format!("record-{i:03}"))
            .collect::<Vec<_>>(),
        "{mode}\n{}",
        common::describe_run(&runtime)
    );
    assert_eq!(
        runtime.session().runs()[0].state(),
        RunState::Ended(expected),
        "{mode}\n{}",
        common::describe_run(&runtime)
    );
    assert_eq!(
        runtime
            .events()
            .iter()
            .filter(|e| matches!(e.event, AgentEvent::ProcessExited(_)))
            .count(),
        1,
        "{mode}\n{}",
        common::describe_run(&runtime)
    );
    assert_eq!(
        runtime
            .events()
            .iter()
            .filter(|e| matches!(
                e.event,
                AgentEvent::RunStateChanged {
                    to: RunState::Ended(_),
                    ..
                }
            ))
            .count(),
        1,
        "{mode}\n{}",
        common::describe_run(&runtime)
    );
    assert!(runtime.next_frame().await.unwrap().is_none());
}

#[tokio::test]
async fn delayed_consumers_preserve_buffered_completion_limit_and_normal_exit_outcomes() {
    for (mode, expected) in [
        ("complete", RunEnd::Completed),
        ("quota", RunEnd::LimitReached(LimitKind::UsageQuota)),
        ("bare-exit", RunEnd::Exited { code: Some(0) }),
        ("crash", RunEnd::Exited { code: Some(23) }),
    ] {
        let mut runtime = runtime(Arc::new(FixtureAdapter::new(mode, DecoderMode::Normal)));
        runtime
            .start(StartOptions::new(
                ProviderId::new("test.fixture").unwrap(),
                RunId::new("r").unwrap(),
            ))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        common::drain_to_end(&mut runtime, Duration::from_secs(15)).await;
        assert_eq!(
            runtime.session().runs()[0].state(),
            RunState::Ended(expected),
            "{mode}\n{}",
            common::describe_run(&runtime)
        );
        assert_eq!(
            runtime
                .events()
                .iter()
                .filter(|e| matches!(e.event, AgentEvent::ProcessExited(_)))
                .count(),
            1,
            "{mode}\n{}",
            common::describe_run(&runtime)
        );
        assert_eq!(
            runtime
                .events()
                .iter()
                .filter(|e| matches!(
                    e.event,
                    AgentEvent::RunStateChanged {
                        to: RunState::Ended(_),
                        ..
                    }
                ))
                .count(),
            1,
            "{mode}\n{}",
            common::describe_run(&runtime)
        );
    }
}

#[tokio::test]
async fn protocol_rate_network_and_inferred_quota_recover_to_completed() {
    for mode in [
        "recover-protocol",
        "recover-rate",
        "recover-network",
        "recover-inferred-quota",
    ] {
        let runtime = run_case(mode, DecoderMode::Normal).await;
        assert_eq!(
            runtime.session().runs()[0].state(),
            RunState::Ended(RunEnd::Completed),
            "{mode}\n{}",
            common::describe_run(&runtime)
        );
    }
}

#[tokio::test]
async fn framing_errors_preserve_later_successful_records_and_exit() {
    for (mode, errors) in [
        ("invalid-then-valid", 1),
        ("huge-then-valid", 1),
        ("multiple-malformed", 3),
    ] {
        let runtime = run_case(mode, DecoderMode::Normal).await;
        assert_eq!(
            runtime.session().runs()[0].state(),
            RunState::Ended(RunEnd::Completed),
            "{mode}\n{}",
            common::describe_run(&runtime)
        );
        assert_eq!(
            runtime
                .events()
                .iter()
                .filter(|e| matches!(
                    e.event,
                    AgentEvent::ProviderError {
                        class: ErrorClass::ProtocolViolation,
                        ..
                    }
                ))
                .count(),
            errors,
            "{mode}\n{}",
            common::describe_run(&runtime)
        );
    }
}

#[tokio::test]
async fn native_ids_rotating_at_exit_preserve_fork_history_and_one_finalization() {
    let runtime = run_case("complete", DecoderMode::RotateAtExit).await;
    let run = &runtime.session().runs()[0];
    assert_eq!(
        run.state(),
        RunState::Ended(RunEnd::Completed),
        "{}",
        common::describe_run(&runtime)
    );
    assert_eq!(run.native_session().unwrap().as_str(), "exit-b");
    assert_eq!(run.native_sessions().len(), 3);
    assert_eq!(run.native_sessions()[2].link, NativeSessionLink::Forked);
}

#[tokio::test]
async fn malformed_decoder_event_at_exit_does_not_replace_completion() {
    let runtime = run_case("complete", DecoderMode::ErrorAtExit).await;
    assert_eq!(
        runtime.session().runs()[0].state(),
        RunState::Ended(RunEnd::Completed),
        "{}",
        common::describe_run(&runtime)
    );
}

#[tokio::test]
async fn decoder_panic_at_exit_preserves_status_and_aborts_once() {
    let runtime = run_case("complete", DecoderMode::PanicAtExit).await;
    assert_eq!(
        runtime.session().runs()[0].state(),
        RunState::Ended(RunEnd::Failed(ErrorClass::Internal)),
        "{}",
        common::describe_run(&runtime)
    );
    assert!(
        runtime
            .events()
            .iter()
            .any(|e| matches!(e.event,AgentEvent::ProcessExited(exit) if exit.success)),
        "{}",
        common::describe_run(&runtime)
    );
}

async fn wait_for(runtime: &mut uuldana_runtime::Supervisor, expected: &str) {
    let seen = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match runtime.next_frame().await.unwrap() {
                Some(uuldana_transport::Frame::Message(message)) if message == expected => break,
                Some(_) => {}
                None => return false,
            }
        }
        true
    })
    .await;
    match seen {
        Ok(true) => {}
        Ok(false) => panic!(
            "fixture exited before {expected}\n{}",
            common::describe_run(runtime)
        ),
        Err(_) => panic!(
            "{expected} not seen within 15s\n{}",
            common::describe_run(runtime)
        ),
    }
}

#[tokio::test]
async fn unsupported_interrupt_falls_back_to_eof_and_graceful_cancellation() {
    let mut adapter = FixtureAdapter::new("interrupted", DecoderMode::Normal);
    adapter.shutdown = Some(ShutdownPlan {
        steps: vec![ShutdownStep::Interrupt],
        grace: Duration::from_secs(2),
    });
    let mut runtime = runtime(Arc::new(adapter));
    runtime
        .start(StartOptions::new(
            ProviderId::new("test.fixture").unwrap(),
            RunId::new("r").unwrap(),
        ))
        .await
        .unwrap();
    wait_for(&mut runtime, "WAITING").await;
    runtime.stop().await.unwrap();
    assert_eq!(
        runtime.session().runs()[0].state(),
        RunState::Ended(RunEnd::Cancelled),
        "{}",
        common::describe_run(&runtime)
    );
    assert!(
        !runtime.last_stop_forced(),
        "{}",
        common::describe_run(&runtime)
    );
    assert!(
        runtime.events().iter().any(|e| matches!(&e.event,AgentEvent::Diagnostic { message } if message.contains("fallback CloseInput: Ok(Ok(()))"))),
        "{}",
        common::describe_run(&runtime)
    );
}

#[tokio::test]
async fn blocked_shutdown_write_has_a_deadline_and_forced_cancellation_semantics() {
    let mut adapter = FixtureAdapter::new("unresponsive", DecoderMode::Normal);
    adapter.shutdown = Some(ShutdownPlan {
        steps: vec![ShutdownStep::WriteInput(vec![b'x'; 8 * 1024 * 1024])],
        grace: Duration::from_millis(50),
    });
    let mut runtime = runtime(Arc::new(adapter));
    runtime
        .start(StartOptions::new(
            ProviderId::new("test.fixture").unwrap(),
            RunId::new("r").unwrap(),
        ))
        .await
        .unwrap();
    wait_for(&mut runtime, "WAITING").await;
    tokio::time::timeout(Duration::from_secs(10), runtime.stop())
        .await
        .unwrap()
        .unwrap();
    assert!(
        runtime.last_stop_forced(),
        "{}",
        common::describe_run(&runtime)
    );
    assert_eq!(
        runtime.session().runs()[0].state(),
        RunState::Ended(RunEnd::Terminated),
        "{}",
        common::describe_run(&runtime)
    );
    assert!(
        runtime.events().iter().any(
            |e| matches!(&e.event,AgentEvent::Diagnostic { message } if message.contains("Err(Elapsed"))
        ),
        "{}",
        common::describe_run(&runtime)
    );
    assert_eq!(
        runtime
            .events()
            .iter()
            .filter(|e| matches!(e.event, AgentEvent::ProcessExited(_)))
            .count(),
        1,
        "{}",
        common::describe_run(&runtime)
    );
}

#[tokio::test]
async fn default_transport_uses_descriptor_instead_of_mock_assumption() {
    let mut adapter = FixtureAdapter::new("complete", DecoderMode::Normal);
    adapter.descriptor.transports = vec![TransportKind::Acp, TransportKind::NativePty];
    let mut runtime = runtime(Arc::new(adapter));
    runtime
        .start(StartOptions::new(
            ProviderId::new("test.fixture").unwrap(),
            RunId::new("r").unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(
        runtime.session().runs()[0].transport(),
        TransportKind::NativePty
    );
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut bytes = Vec::new();
        let mut cursor = false;
        while let Some(frame) = runtime.next_frame().await.unwrap() {
            if let uuldana_transport::Frame::Output(output) = frame {
                bytes.extend(output);
            }
            if !cursor && bytes.windows(4).any(|b| b == b"\x1b[6n") {
                runtime.write_input(b"\x1b[1;1R").await.unwrap();
                cursor = true;
            }
        }
    })
    .await
    .unwrap();
    assert!(
        runtime.session().runs()[0].state().is_terminal(),
        "{}",
        common::describe_run(&runtime)
    );
}

#[tokio::test]
async fn unavailable_descriptor_transport_fails_before_run_or_spawn() {
    let mut adapter = FixtureAdapter::new("complete", DecoderMode::Normal);
    adapter.descriptor.transports = vec![TransportKind::Acp];
    let mut runtime = runtime(Arc::new(adapter));
    assert!(
        matches!(runtime.start(StartOptions::new(ProviderId::new("test.fixture").unwrap(),RunId::new("r").unwrap())).await,Err(uuldana_runtime::Error::Adapter(error)) if error.class() == ErrorClass::Unsupported)
    );
    assert!(runtime.session().runs().is_empty());
}

#[tokio::test]
async fn inherited_stdout_and_descendants_do_not_corrupt_exit_or_cancellation() {
    for pty in [false, true] {
        for (mode, end, forced) in [
            ("tree-exit", RunEnd::Exited { code: Some(0) }, false),
            ("tree-graceful", RunEnd::Cancelled, false),
            ("tree-force", RunEnd::Terminated, true),
        ] {
            let path = std::env::temp_dir().join(format!(
                "uuldana-runtime-tree-{}-{mode}-{pty}",
                std::process::id()
            ));
            assert!(!path.exists(), "fixture heartbeat must be unique");
            let mut adapter = FixtureAdapter::new(mode, DecoderMode::Normal);
            adapter.descriptor.transports =
                vec![TransportKind::CustomCli, TransportKind::NativePty];
            adapter.args.push(path.as_os_str().into());
            adapter.shutdown = Some(ShutdownPlan {
                // Closing ConPTY input can itself close the console and end
                // the child. Keep it open when testing an ignored text request
                // so the force case actually reaches force termination.
                steps: if pty {
                    vec![ShutdownStep::WriteInput(b"quit\r".to_vec())]
                } else {
                    vec![
                        ShutdownStep::WriteInput(b"quit\n".to_vec()),
                        ShutdownStep::CloseInput,
                    ]
                },
                grace: Duration::from_secs(1),
            });
            let mut runtime = runtime(Arc::new(adapter));
            let mut options = StartOptions::new(
                ProviderId::new("test.fixture").unwrap(),
                RunId::new("r").unwrap(),
            );
            options.transport = Some(if pty {
                TransportKind::NativePty
            } else {
                TransportKind::CustomCli
            });
            runtime.start(options).await.unwrap();
            if pty {
                tokio::time::timeout(Duration::from_secs(15), async {
                    let mut bytes = Vec::new();
                    let mut cursor = false;
                    loop {
                        match runtime.next_frame().await.unwrap() {
                            Some(uuldana_transport::Frame::Output(output)) => bytes.extend(output),
                            other => panic!("unexpected PTY frame before readiness: {other:?}"),
                        }
                        if !cursor && bytes.windows(4).any(|b| b == b"\x1b[6n") {
                            runtime.write_input(b"\x1b[1;1R").await.unwrap();
                            cursor = true;
                        }
                        if String::from_utf8_lossy(&bytes).contains("WAITING") {
                            break;
                        }
                    }
                })
                .await
                .unwrap();
            } else {
                wait_for(&mut runtime, "WAITING").await;
            }
            tokio::time::timeout(Duration::from_secs(10), async {
                if mode == "tree-exit" {
                    while runtime.next_frame().await.unwrap().is_some() {}
                } else {
                    runtime.stop().await.unwrap();
                }
            })
            .await
            .unwrap();
            assert_eq!(
                runtime.session().runs()[0].state(),
                RunState::Ended(end),
                "{mode} pty={pty}\n{}",
                common::describe_run(&runtime)
            );
            assert_eq!(
                runtime.last_stop_forced(),
                forced,
                "{mode} pty={pty}\n{}",
                common::describe_run(&runtime)
            );
            assert_eq!(
                runtime
                    .events()
                    .iter()
                    .filter(|e| matches!(e.event, AgentEvent::ProcessExited(_)))
                    .count(),
                1,
                "{mode} pty={pty}\n{}",
                common::describe_run(&runtime)
            );
            let pid: u32 = std::fs::read_to_string(path.with_extension("pid"))
                .unwrap()
                .parse()
                .unwrap();
            process_assertions::assert_process_exited(pid);
            tokio::time::sleep(Duration::from_millis(300)).await;
            let before = std::fs::read(&path).unwrap();
            tokio::time::sleep(Duration::from_millis(150)).await;
            assert_eq!(
                std::fs::read(&path).unwrap(),
                before,
                "descendant survived {mode}"
            );
            std::fs::remove_file(&path).unwrap();
            std::fs::remove_file(path.with_extension("pid")).unwrap();
        }
    }
}

// Parent test runs the child test in a separate OS process. A CPU-bound async
// regression cannot prevent this watchdog from enforcing its deadline.
#[test]
fn external_watchdog_guards_exit_finalization() {
    if std::env::var_os("UULDANA_WATCHDOG_EXIT").is_some() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                for decoder in [
                    DecoderMode::RotateAtExit,
                    DecoderMode::ErrorAtExit,
                    DecoderMode::PanicAtExit,
                ] {
                    let runtime = run_case("complete", decoder).await;
                    let expected = if matches!(decoder, DecoderMode::PanicAtExit) {
                        RunEnd::Failed(ErrorClass::Internal)
                    } else {
                        RunEnd::Completed
                    };
                    assert_eq!(
                        runtime.session().runs()[0].state(),
                        RunState::Ended(expected),
                        "{}",
                        common::describe_run(&runtime)
                    );
                }
            });
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "external_watchdog_guards_exit_finalization",
            "--nocapture",
        ])
        .env("UULDANA_WATCHDOG_EXIT", "1")
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("external watchdog detected executor starvation during finalization");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success(), "watchdog child assertions failed");
}
