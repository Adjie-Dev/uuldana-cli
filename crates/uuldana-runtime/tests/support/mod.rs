use crate::common::fixture;
use std::{sync::Arc, time::Duration};
use uuldana_core::*;
use uuldana_runtime::{
    Supervisor,
    mock::{MockProvider, Scenario},
};

pub fn runtime(adapter: Arc<dyn ProviderAdapter>) -> Supervisor {
    let mut registry = ProviderRegistry::new();
    registry.register(adapter).unwrap();
    Supervisor::new(
        registry,
        Arc::new(crate::common::DiagnosticHost::default()),
        Session::new(
            SessionId::new("test").unwrap(),
            std::env::current_dir().unwrap(),
            PermissionPolicy::default(),
            Timestamp::from_unix_millis(0),
        )
        .unwrap(),
    )
}

#[derive(Clone, Copy)]
#[allow(dead_code)]
pub enum DecoderMode {
    Normal,
    RotateAtExit,
    PanicAtExit,
    ErrorAtExit,
}

pub struct FixtureAdapter {
    pub base: MockProvider,
    pub mode: &'static str,
    pub decoder: DecoderMode,
    pub shutdown: Option<ShutdownPlan>,
    pub descriptor: ProviderDescriptor,
    pub args: Vec<std::ffi::OsString>,
}
impl FixtureAdapter {
    pub fn new(mode: &'static str, decoder: DecoderMode) -> Self {
        let base = MockProvider::new(
            ProviderId::new("test.fixture").unwrap(),
            fixture(),
            Scenario::NormalCompletion,
        );
        Self {
            descriptor: base.descriptor().clone(),
            args: Vec::new(),
            base,
            mode,
            decoder,
            shutdown: None,
        }
    }
}
impl ProviderAdapter for FixtureAdapter {
    fn descriptor(&self) -> &ProviderDescriptor {
        &self.descriptor
    }
    fn detect(&self, p: &dyn HostProbe) -> Result<Installation, AdapterError> {
        self.base.detect(p)
    }
    fn capabilities(&self, i: &Installation) -> CapabilitySet {
        self.base.capabilities(i)
    }
    fn map_permissions(&self, i: &Installation, p: &PermissionPolicy) -> PermissionMapping {
        self.base.map_permissions(i, p)
    }
    fn launch_plan(&self, i: &Installation, r: &LaunchRequest) -> Result<LaunchPlan, AdapterError> {
        let mut plan = if r.transport == TransportKind::NativePty {
            LaunchPlan::new(
                i.executable.clone(),
                r.workspace.clone(),
                TransportSpec::NativePty {
                    initial_size: TerminalSize::DEFAULT,
                },
            )
        } else {
            self.base.launch_plan(i, r)?
        };
        plan.args = vec![self.mode.into()];
        plan.args.extend(self.args.clone());
        if r.transport == TransportKind::NativePty {
            plan.transport = TransportSpec::NativePty {
                initial_size: TerminalSize::DEFAULT,
            };
        }
        Ok(plan)
    }
    fn event_decoder(&self, t: TransportKind) -> Option<Box<dyn EventDecoder>> {
        if t == TransportKind::NativePty {
            return None;
        }
        Some(Box::new(Decoder {
            inner: self.base.event_decoder(t).unwrap(),
            mode: self.decoder,
        }))
    }
    fn shutdown_plan(&self, _: TransportKind) -> ShutdownPlan {
        self.shutdown.clone().unwrap_or(ShutdownPlan {
            steps: vec![
                ShutdownStep::WriteInput(b"quit\n".to_vec()),
                ShutdownStep::CloseInput,
            ],
            grace: Duration::from_secs(2),
        })
    }
}
struct Decoder {
    inner: Box<dyn EventDecoder>,
    mode: DecoderMode,
}
impl EventDecoder for Decoder {
    fn decode(&mut self, frame: TransportFrame<'_>, out: &mut Vec<DecodedEvent>) {
        if matches!(frame, TransportFrame::Exited(_)) {
            match self.mode {
                DecoderMode::RotateAtExit => {
                    out.push(DecodedEvent::native(AgentEvent::NativeSessionIdentified {
                        native: NativeSessionId::new("exit-a").unwrap(),
                    }));
                    out.push(DecodedEvent::native(AgentEvent::NativeSessionChanged {
                        native: NativeSessionId::new("exit-b").unwrap(),
                        link: NativeSessionLink::Forked,
                    }));
                }
                DecoderMode::PanicAtExit => panic!("intentional exit decoder panic"),
                DecoderMode::ErrorAtExit => {
                    out.push(DecodedEvent::native(AgentEvent::ProviderError {
                        class: ErrorClass::ProtocolViolation,
                        message: "malformed final provider record".into(),
                    }))
                }
                DecoderMode::Normal => {}
            }
        }
        if matches!(frame, TransportFrame::Message("INFERRED_QUOTA")) {
            out.push(DecodedEvent::inferred(AgentEvent::LimitReached(
                LimitSignal {
                    kind: LimitKind::UsageQuota,
                    resets_at: None,
                    message: Some("provisional text match".into()),
                },
            )));
        } else {
            self.inner.decode(frame, out);
        }
    }
}
