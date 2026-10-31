//! Runtime/SDK conformance using a real harmless local child.
mod common;
use std::{sync::Arc, time::Duration};
use uuldana_core::*;
use uuldana_runtime::{
    StartOptions, Supervisor,
    mock::{MockProvider, Scenario},
};

fn supervisor(scenario: Scenario) -> (Supervisor, ProviderId) {
    let id = ProviderId::new("third-party.fixture").unwrap();
    let adapter = MockProvider::new(id.clone(), common::fixture(), scenario);
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(adapter)).unwrap();
    let session = Session::new(
        SessionId::new("test").unwrap(),
        std::env::current_dir().unwrap(),
        PermissionPolicy::default(),
        Timestamp::from_unix_millis(0),
    )
    .unwrap();
    (
        Supervisor::new(
            registry,
            Arc::new(common::DiagnosticHost::default()),
            session,
        ),
        id,
    )
}

async fn drain(runtime: &mut Supervisor) {
    common::drain_to_end(runtime, Duration::from_secs(15)).await;
}

async fn ready_to_stop(runtime: &mut Supervisor) {
    let ready = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match runtime.next_frame().await.unwrap() {
                Some(uuldana_transport::Frame::Message(message)) if message == "WAITING" => break,
                Some(_) => {}
                None => return false,
            }
        }
        true
    })
    .await;
    match ready {
        Ok(true) => {}
        Ok(false) => panic!(
            "fixture exited before readiness handshake
{}",
            common::describe_run(runtime)
        ),
        Err(_) => panic!(
            "readiness handshake exceeded 15s
{}",
            common::describe_run(runtime)
        ),
    }
}

#[tokio::test]
async fn required_scenarios_have_distinct_typed_outcomes_and_ordered_history() {
    let scenarios = [
        (Scenario::Startup, RunEnd::Completed),
        (
            Scenario::IncompleteSecondTurn,
            RunEnd::Exited { code: Some(0) },
        ),
        (Scenario::NormalCompletion, RunEnd::Completed),
        (
            Scenario::UsageQuota,
            RunEnd::LimitReached(LimitKind::UsageQuota),
        ),
        (
            Scenario::RateLimit,
            RunEnd::LimitReached(LimitKind::RateLimit),
        ),
        (
            Scenario::ContextWindow,
            RunEnd::LimitReached(LimitKind::ContextWindow),
        ),
        (
            Scenario::AuthenticationFailure,
            RunEnd::Failed(ErrorClass::AuthenticationRequired),
        ),
        (
            Scenario::NetworkFailure,
            RunEnd::Failed(ErrorClass::Transient),
        ),
        (Scenario::UnknownError, RunEnd::Failed(ErrorClass::Internal)),
        (
            Scenario::UnexpectedTermination,
            RunEnd::Exited { code: Some(23) },
        ),
    ];
    for (scenario, end) in scenarios {
        let (mut runtime, id) = supervisor(scenario);
        let run = RunId::new("run").unwrap();
        runtime
            .start(StartOptions::new(id.clone(), run.clone()))
            .await
            .unwrap();
        assert_eq!(
            runtime.session().run(&run).unwrap().state(),
            RunState::Running
        );
        drain(&mut runtime).await;
        assert_eq!(
            runtime.session().run(&run).unwrap().state(),
            RunState::Ended(end),
            "{scenario:?}; observed events: {:?}",
            runtime.events()
        );
        let events = runtime.events();
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e.event, AgentEvent::ProcessExited(_)))
                .count(),
            1
        );
        let exits: Vec<_> = events
            .iter()
            .enumerate()
            .filter(|(_, e)| matches!(e.event, AgentEvent::ProcessExited(_)))
            .collect();
        let terminals: Vec<_> = events
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                matches!(
                    e.event,
                    AgentEvent::RunStateChanged {
                        to: RunState::Ended(_),
                        ..
                    }
                )
            })
            .collect();
        assert_eq!(terminals.len(), 1, "{scenario:?}; {events:?}");
        assert_eq!(terminals[0].0, exits[0].0 + 1, "{scenario:?}; {events:?}");
        assert_eq!(terminals[0].0, events.len() - 1, "{scenario:?}; {events:?}");
        if end == RunEnd::Completed {
            let done = events
                .iter()
                .position(|e| {
                    matches!(
                        e.event,
                        AgentEvent::TurnEnded {
                            reason: StopReason::EndTurn
                        }
                    )
                })
                .expect("Completed requires a decoded EndTurn");
            assert!(done < exits[0].0, "{scenario:?}; {events:?}");
            assert!(matches!(exits[0].1.event, AgentEvent::ProcessExited(exit) if exit.success));
        }
        for (seq, event) in events.iter().enumerate() {
            assert_eq!(event.seq, seq as u64);
            assert_eq!(event.provider, id);
            assert_eq!(event.run, run);
        }
        assert!(
            runtime
                .session()
                .run(&run)
                .unwrap()
                .native_session()
                .is_some()
        );
        assert_eq!(
            runtime.session().runs().len(),
            1,
            "a run that ends on its own must never switch providers by itself"
        );
        let before = runtime.events().to_vec();
        assert!(runtime.next_frame().await.unwrap().is_none());
        assert!(runtime.next_frame().await.unwrap().is_none());
        assert_eq!(runtime.events(), before, "terminal history must be stable");
    }
}

#[tokio::test]
async fn unsupported_resume_and_permission_policy_do_not_spawn() {
    let (mut runtime, id) = supervisor(Scenario::UnsupportedResume);
    let mut options = StartOptions::new(id, RunId::new("r").unwrap());
    options.native_resume = Some(NativeSessionId::new("native").unwrap());
    assert!(
        matches!(runtime.start(options).await, Err(uuldana_runtime::Error::Adapter(e)) if e.class() == ErrorClass::Unsupported && e.capability() == Some(&Capability::ResumeById))
    );
    assert!(runtime.session().runs().is_empty());
    let (mut runtime, id) = supervisor(Scenario::UnsupportedPermissionPolicy);
    assert!(matches!(
        runtime
            .start(StartOptions::new(id, RunId::new("r").unwrap()))
            .await,
        Err(uuldana_runtime::Error::Permission(
            PermissionError::WouldEscalate { .. }
        ))
    ));
    assert!(runtime.session().runs().is_empty());
}

#[tokio::test]
async fn interruption_is_graceful_and_records_stopping_before_cancelled() {
    let (mut runtime, id) = supervisor(Scenario::Interrupted);
    let run = RunId::new("r").unwrap();
    runtime
        .start(StartOptions::new(id, run.clone()))
        .await
        .unwrap();
    ready_to_stop(&mut runtime).await;
    runtime.stop().await.unwrap();
    assert_eq!(
        runtime.session().run(&run).unwrap().state(),
        RunState::Ended(RunEnd::Cancelled),
        "{}",
        common::describe_run(&runtime)
    );
    assert!(
        runtime.events().iter().any(|e| matches!(
            e.event,
            AgentEvent::RunStateChanged {
                to: RunState::Stopping,
                ..
            }
        )),
        "{}",
        common::describe_run(&runtime)
    );
    assert!(
        !runtime.last_stop_forced(),
        "{}",
        common::describe_run(&runtime)
    );
}

#[tokio::test]
async fn registry_rejects_duplicates_and_discovers_custom_id() {
    let id = ProviderId::new("vendor.any-agent").unwrap();
    let adapter = Arc::new(MockProvider::new(
        id.clone(),
        common::fixture(),
        Scenario::NormalCompletion,
    ));
    let mut registry = ProviderRegistry::new();
    registry.register(adapter.clone()).unwrap();
    assert!(matches!(
        registry.register(adapter),
        Err(RegistryError::Duplicate(_))
    ));
    let started = std::time::Instant::now();
    let discovered = registry.discover(&common::DiagnosticHost::default());
    assert_eq!(discovered[0].provider, id);
    assert!(
        discovered[0].outcome.is_ok(),
        "discovery elapsed={:?} outcome={:?}",
        started.elapsed(),
        discovered[0].outcome
    );
}

#[tokio::test]
async fn ignored_graceful_shutdown_is_forced_after_stopping() {
    let (mut runtime, id) = supervisor(Scenario::Unresponsive);
    let run = RunId::new("r").unwrap();
    runtime
        .start(StartOptions::new(id, run.clone()))
        .await
        .unwrap();
    ready_to_stop(&mut runtime).await;
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
        runtime.session().run(&run).unwrap().state(),
        RunState::Ended(RunEnd::Terminated),
        "{}",
        common::describe_run(&runtime)
    );
    assert!(
        runtime
            .events()
            .iter()
            .any(|e| matches!(e.event, AgentEvent::ProcessExited(info) if !info.success)),
        "{}",
        common::describe_run(&runtime)
    );
}

#[tokio::test]
async fn session_history_survives_sequential_runs_and_rejects_overlap() {
    let (mut runtime, id) = supervisor(Scenario::Interrupted);
    runtime
        .start(StartOptions::new(id.clone(), RunId::new("first").unwrap()))
        .await
        .unwrap();
    assert!(matches!(
        runtime
            .start(StartOptions::new(
                id.clone(),
                RunId::new("overlap").unwrap()
            ))
            .await,
        Err(uuldana_runtime::Error::Session(
            SessionError::RunAlreadyActive(_)
        ))
    ));
    ready_to_stop(&mut runtime).await;
    runtime.stop().await.unwrap();
    let prior = runtime.events().to_vec();
    runtime
        .start(StartOptions::new(id, RunId::new("second").unwrap()))
        .await
        .unwrap();
    ready_to_stop(&mut runtime).await;
    runtime.stop().await.unwrap();
    assert_eq!(runtime.session().runs().len(), 2);
    assert_eq!(&runtime.events()[..prior.len()], prior);
    assert_eq!(
        runtime.events().last().unwrap().seq,
        runtime.events().len() as u64 - 1
    );
}
