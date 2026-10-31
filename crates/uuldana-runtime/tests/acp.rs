//! ACP transport and client conformance against a real local ACP agent process.
mod common;
use std::{path::PathBuf, sync::Arc, time::Duration};
use uuldana_core::*;
use uuldana_runtime::{
    StartOptions, Supervisor,
    acp::{AcpClient, AcpError},
};
use uuldana_transport::Frame;

struct AcpFixture {
    descriptor: ProviderDescriptor,
    executable: PathBuf,
    mode: &'static str,
}

impl AcpFixture {
    fn new(mode: &'static str) -> Self {
        Self {
            descriptor: ProviderDescriptor::new(
                ProviderId::new("acp.fixture").unwrap(),
                "ACP fixture agent",
                vec![TransportKind::Acp],
            ),
            executable: common::acp_fixture(),
            mode,
        }
    }
}

impl ProviderAdapter for AcpFixture {
    fn descriptor(&self) -> &ProviderDescriptor {
        &self.descriptor
    }
    fn detect(&self, probe: &dyn HostProbe) -> Result<Installation, AdapterError> {
        probe.run(&self.executable, &["--version"], Duration::from_secs(5))?;
        Ok(Installation::new(self.executable.clone()))
    }
    fn capabilities(&self, _: &Installation) -> CapabilitySet {
        CapabilitySet::new()
    }
    fn map_permissions(&self, _: &Installation, requested: &PermissionPolicy) -> PermissionMapping {
        PermissionMapping::new(*requested)
    }
    fn launch_plan(
        &self,
        installation: &Installation,
        request: &LaunchRequest,
    ) -> Result<LaunchPlan, AdapterError> {
        Ok(LaunchPlan::new(
            installation.executable.clone(),
            request.workspace.clone(),
            TransportSpec::Acp {
                versions: AcpVersionRange::V1,
            },
        )
        .arg(self.mode))
    }
    fn event_decoder(&self, _: TransportKind) -> Option<Box<dyn EventDecoder>> {
        // No decoder: the runtime must supply the generic ACP mapping.
        None
    }
    fn shutdown_plan(&self, _: TransportKind) -> ShutdownPlan {
        ShutdownPlan {
            steps: vec![ShutdownStep::CloseInput],
            grace: Duration::from_secs(3),
        }
    }
}

async fn started(mode: &'static str) -> Supervisor {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(AcpFixture::new(mode))).unwrap();
    let session = Session::new(
        SessionId::new("acp-test").unwrap(),
        std::env::current_dir().unwrap(),
        PermissionPolicy::default(),
        Timestamp::from_unix_millis(0),
    )
    .unwrap();
    let mut runtime = Supervisor::new(
        registry,
        Arc::new(common::DiagnosticHost::default()),
        session,
    );
    let mut options = StartOptions::new(
        ProviderId::new("acp.fixture").unwrap(),
        RunId::new("run-1").unwrap(),
    );
    options.transport = Some(TransportKind::Acp);
    runtime.start(options).await.unwrap();
    runtime
}

/// Pumps frames until `done` approves one, feeding every message to `client`.
async fn pump(
    runtime: &mut Supervisor,
    client: &mut AcpClient,
    mut done: impl FnMut(&Supervisor) -> bool,
) {
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match runtime.next_frame().await.unwrap() {
                Some(Frame::Message(line)) => client.observe(&line),
                Some(_) => {}
                None => return,
            }
            if done(runtime) {
                return;
            }
        }
    })
    .await;
    assert!(
        result.is_ok(),
        "pump timed out\n{}",
        common::describe_run(runtime)
    );
}

fn has(runtime: &Supervisor, predicate: impl Fn(&AgentEvent) -> bool) -> bool {
    runtime.events().iter().any(|e| predicate(&e.event))
}

fn native_session(runtime: &Supervisor) -> NativeSessionId {
    runtime
        .events()
        .iter()
        .find_map(|e| match &e.event {
            AgentEvent::NativeSessionIdentified { native } => Some(native.clone()),
            _ => None,
        })
        .expect("native session identified")
}

async fn handshake(runtime: &mut Supervisor, client: &mut AcpClient) -> NativeSessionId {
    runtime.send_line(&client.initialize()).await.unwrap();
    pump(runtime, client, |r| {
        has(
            r,
            |e| matches!(e, AgentEvent::Extension { name, .. } if name == "initialized"),
        )
    })
    .await;
    let cwd = runtime.session().workspace().to_path_buf();
    runtime.send_line(&client.new_session(&cwd)).await.unwrap();
    pump(runtime, client, |r| {
        has(r, |e| {
            matches!(e, AgentEvent::NativeSessionIdentified { .. })
        })
    })
    .await;
    native_session(runtime)
}

fn ended_turn(runtime: &Supervisor, wanted: StopReason) -> bool {
    has(
        runtime,
        |e| matches!(e, AgentEvent::TurnEnded { reason } if *reason == wanted),
    )
}

fn agent_text(runtime: &Supervisor) -> Vec<String> {
    runtime
        .events()
        .iter()
        .filter_map(|e| match &e.event {
            AgentEvent::MessageChunk {
                role: Role::Agent,
                text,
            } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

async fn permission_id(runtime: &mut Supervisor, client: &mut AcpClient) -> PermissionRequestId {
    pump(runtime, client, |r| {
        has(r, |e| matches!(e, AgentEvent::PermissionRequested { .. }))
    })
    .await;
    runtime
        .events()
        .iter()
        .find_map(|e| match &e.event {
            AgentEvent::PermissionRequested { id, .. } => Some(id.clone()),
            _ => None,
        })
        .unwrap()
}

#[tokio::test]
async fn full_turn_maps_to_normalized_events_and_stops_gracefully() {
    let mut runtime = started("default").await;
    let mut client = AcpClient::new(AcpVersionRange::V1);
    let native = handshake(&mut runtime, &mut client).await;
    assert_eq!(client.negotiated_version(), Some(1));
    assert_eq!(native.as_str(), "acp-fixture-1");

    let prompt = client.prompt(&native, "perbaiki tes login");
    runtime.send_line(&prompt).await.unwrap();
    pump(&mut runtime, &mut client, |r| {
        ended_turn(r, StopReason::EndTurn)
    })
    .await;

    assert!(has(&runtime, |e| matches!(
        e,
        AgentEvent::ThoughtChunk { .. }
    )));
    assert!(has(&runtime, |e| matches!(
        e,
        AgentEvent::ToolCall {
            kind: ToolKind::Read,
            status: ToolStatus::InProgress,
            ..
        }
    )));
    assert!(has(&runtime, |e| matches!(
        e,
        AgentEvent::ToolCallUpdate {
            status: Some(ToolStatus::Completed),
            ..
        }
    )));
    assert!(has(&runtime, |e| matches!(
        e,
        AgentEvent::UsageUpdated {
            context_used: Some(1200),
            context_size: Some(200000)
        }
    )));
    assert_eq!(agent_text(&runtime), vec!["selesai: perbaiki tes login"]);
    assert_eq!(
        runtime.session().runs()[0]
            .native_session()
            .map(|n| n.as_str()),
        Some("acp-fixture-1")
    );

    runtime.stop().await.unwrap();
    assert!(!runtime.last_stop_forced());
    assert!(matches!(
        runtime.session().runs()[0].state(),
        RunState::Ended(RunEnd::Cancelled)
    ));
}

#[tokio::test]
async fn automatic_policy_can_reject_but_never_allow() {
    let mut runtime = started("permission").await;
    let mut client = AcpClient::new(AcpVersionRange::V1);
    let native = handshake(&mut runtime, &mut client).await;
    runtime
        .send_line(&client.prompt(&native, "ringkas README"))
        .await
        .unwrap();
    let id = permission_id(&mut runtime, &mut client).await;

    assert!(PermissionResolution::by_policy(PermissionDecision::AllowOnce).is_err());
    let reject = PermissionResolution::by_policy(PermissionDecision::RejectOnce).unwrap();
    let reply = client.respond_permission(&id, reject).unwrap();
    runtime.send_line(&reply).await.unwrap();
    pump(&mut runtime, &mut client, |r| {
        ended_turn(r, StopReason::EndTurn)
    })
    .await;
    assert_eq!(agent_text(&runtime), vec!["izin ditolak, langkah dilewati"]);
    runtime.stop().await.unwrap();
}

#[tokio::test]
async fn user_allow_reaches_the_agent_as_the_selected_option() {
    let mut runtime = started("permission").await;
    let mut client = AcpClient::new(AcpVersionRange::V1);
    let native = handshake(&mut runtime, &mut client).await;
    runtime
        .send_line(&client.prompt(&native, "ringkas README"))
        .await
        .unwrap();
    let id = permission_id(&mut runtime, &mut client).await;
    let allow = PermissionResolution::by_user(PermissionDecision::AllowOnce);
    runtime
        .send_line(&client.respond_permission(&id, allow).unwrap())
        .await
        .unwrap();
    pump(&mut runtime, &mut client, |r| {
        ended_turn(r, StopReason::EndTurn)
    })
    .await;
    assert_eq!(agent_text(&runtime), vec!["selesai: ringkas README"]);
    // The request is consumed; answering twice is an error, not a second allow.
    assert!(matches!(
        client.respond_permission(&id, allow),
        Err(AcpError::UnknownRequest(_))
    ));
    runtime.stop().await.unwrap();
}

#[tokio::test]
async fn cancel_answers_pending_permission_and_ends_the_turn_cancelled() {
    let mut runtime = started("permission").await;
    let mut client = AcpClient::new(AcpVersionRange::V1);
    let native = handshake(&mut runtime, &mut client).await;
    runtime
        .send_line(&client.prompt(&native, "ringkas README"))
        .await
        .unwrap();
    permission_id(&mut runtime, &mut client).await;
    for line in client.cancel(&native) {
        runtime.send_line(&line).await.unwrap();
    }
    pump(&mut runtime, &mut client, |r| {
        ended_turn(r, StopReason::Cancelled)
    })
    .await;
    runtime.stop().await.unwrap();
}

#[tokio::test]
async fn malformed_line_is_non_fatal_and_the_session_continues() {
    let mut runtime = started("garbage").await;
    let mut client = AcpClient::new(AcpVersionRange::V1);
    let native = handshake(&mut runtime, &mut client).await;
    assert!(has(&runtime, |e| matches!(
        e,
        AgentEvent::ProviderError {
            class: ErrorClass::ProtocolViolation,
            ..
        }
    )));
    runtime
        .send_line(&client.prompt(&native, "perbaiki tes login"))
        .await
        .unwrap();
    pump(&mut runtime, &mut client, |r| {
        ended_turn(r, StopReason::EndTurn)
    })
    .await;
    runtime.stop().await.unwrap();
}

#[tokio::test]
async fn unsupported_agent_version_is_reported_and_not_negotiated() {
    let mut runtime = started("old-version").await;
    let mut client = AcpClient::new(AcpVersionRange::V1);
    runtime.send_line(&client.initialize()).await.unwrap();
    pump(&mut runtime, &mut client, |r| {
        has(r, |e| {
            matches!(
                e,
                AgentEvent::ProviderError {
                    class: ErrorClass::IncompatibleVersion,
                    ..
                }
            )
        })
    })
    .await;
    assert_eq!(client.negotiated_version(), None);
    runtime.stop().await.unwrap();
}
