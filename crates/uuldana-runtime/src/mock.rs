//! Harmless SDK adapter and documented deterministic newline protocol.
//! It has no tools, filesystem mutation, credentials, or network access.
use std::{path::PathBuf, time::Duration};
use uuldana_provider_sdk::{
    LimitKind, LimitSignal, NativeSessionId, Role, ShutdownPlan, ShutdownStep, StdinMode,
    StopReason, StreamFraming, prelude::*,
};

/// Deterministic process scenarios used by lifecycle and future failover tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scenario {
    /// Successful startup and clean completion.
    Startup,
    /// A normal visible turn completes.
    NormalCompletion,
    /// Account quota exhausted.
    UsageQuota,
    /// Temporary request throttling.
    RateLimit,
    /// Context no longer fits.
    ContextWindow,
    /// User authentication required.
    AuthenticationFailure,
    /// Temporary network failure; no real network call is made.
    NetworkFailure,
    /// Unclassified provider failure.
    UnknownError,
    /// Nonzero exit without a semantic error message.
    UnexpectedTermination,
    /// Wait for graceful cancellation on stdin.
    Interrupted,
    /// Native resume is unavailable.
    UnsupportedResume,
    /// Requested restrictions cannot be represented.
    UnsupportedPermissionPolicy,
    /// Ignore graceful commands to exercise force termination.
    Unresponsive,
    /// A second turn starts but terminates without EndTurn.
    IncompleteSecondTurn,
}
impl Scenario {
    /// Stable command-line spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::NormalCompletion => "complete",
            Self::UsageQuota => "quota",
            Self::RateLimit => "rate",
            Self::ContextWindow => "context",
            Self::AuthenticationFailure => "auth",
            Self::NetworkFailure => "network",
            Self::UnknownError => "unknown",
            Self::UnexpectedTermination => "crash",
            Self::Interrupted => "interrupted",
            Self::UnsupportedResume => "no-resume",
            Self::UnsupportedPermissionPolicy => "no-policy",
            Self::Unresponsive => "unresponsive",
            Self::IncompleteSecondTurn => "incomplete-second-turn",
        }
    }
}

/// Adapter for a caller-selected local mock executable and provider id.
pub struct MockProvider {
    descriptor: ProviderDescriptor,
    executable: PathBuf,
    scenario: Scenario,
}
impl MockProvider {
    /// Creates an adapter; no subprocess is started here.
    pub fn new(id: ProviderId, executable: PathBuf, scenario: Scenario) -> Self {
        Self {
            descriptor: ProviderDescriptor::new(
                id,
                "Uuldana local mock",
                vec![TransportKind::CustomCli],
            ),
            executable,
            scenario,
        }
    }
}
impl ProviderAdapter for MockProvider {
    fn descriptor(&self) -> &ProviderDescriptor {
        &self.descriptor
    }
    fn detect(&self, probe: &dyn HostProbe) -> Result<Installation, AdapterError> {
        let result = probe.run(&self.executable, &["--version"], Duration::from_secs(3))?;
        if !result.exit.success || result.stdout != b"uuldana-mock 0.1\n" {
            return Err(AdapterError::new(
                ErrorClass::IncompatibleVersion,
                "unexpected mock version response",
            ));
        }
        Ok(Installation::new(self.executable.clone()).with_version("0.1"))
    }
    fn capabilities(&self, _: &Installation) -> CapabilitySet {
        CapabilitySet::new()
            .with(
                Capability::StructuredEvents,
                Support::Supported,
                Evidence::Declared,
            )
            .with(
                Capability::LimitSignals,
                Support::Supported,
                Evidence::Declared,
            )
            .with(
                Capability::ResumeById,
                Support::Unsupported,
                Evidence::Declared,
            )
    }
    fn map_permissions(&self, _: &Installation, requested: &PermissionPolicy) -> PermissionMapping {
        // Restrictions describe mock behavior, not an OS sandbox. This executable
        // implements no agent actions; it only prints fixed records and reads stdin.
        let effective = if self.scenario == Scenario::UnsupportedPermissionPolicy {
            PermissionPolicy::unrestricted()
        } else {
            *requested
        };
        PermissionMapping::new(effective)
    }
    fn launch_plan(
        &self,
        installation: &Installation,
        request: &LaunchRequest,
    ) -> Result<LaunchPlan, AdapterError> {
        if request.transport != TransportKind::CustomCli {
            return Err(AdapterError::new(
                ErrorClass::Unsupported,
                "mock requires custom CLI",
            ));
        }
        if request.initial_prompt.is_some() {
            return Err(AdapterError::unsupported(Capability::InitialPrompt));
        }
        Ok(LaunchPlan::new(
            installation.executable.clone(),
            request.workspace.clone(),
            TransportSpec::CustomCli {
                stdout: StreamFraming::Lines,
                stdin: StdinMode::Piped,
            },
        )
        .arg(self.scenario.as_str()))
    }
    fn event_decoder(&self, transport: TransportKind) -> Option<Box<dyn EventDecoder>> {
        (transport == TransportKind::CustomCli)
            .then(|| Box::new(MockDecoder) as Box<dyn EventDecoder>)
    }
    fn shutdown_plan(&self, _: TransportKind) -> ShutdownPlan {
        ShutdownPlan {
            steps: vec![
                ShutdownStep::WriteInput(b"quit\n".to_vec()),
                ShutdownStep::CloseInput,
            ],
            grace: Duration::from_secs(2),
        }
    }
}
struct MockDecoder;
impl EventDecoder for MockDecoder {
    fn decode(&mut self, frame: TransportFrame<'_>, out: &mut Vec<DecodedEvent>) {
        let message = match frame {
            TransportFrame::Message(s) => s,
            TransportFrame::Stderr(b) => {
                out.push(DecodedEvent::inferred(AgentEvent::Diagnostic {
                    message: String::from_utf8_lossy(b).into_owned(),
                }));
                return;
            }
            TransportFrame::Exited(_) => return,
            _ => {
                out.push(DecodedEvent::synthetic(AgentEvent::Diagnostic {
                    message: "mock decoder received unframed output".into(),
                }));
                return;
            }
        };
        let event = if let Some(id) = message.strip_prefix("READY ") {
            match NativeSessionId::new(id) {
                Ok(native) => AgentEvent::NativeSessionIdentified { native },
                Err(_) => malformed(message),
            }
        } else if let Some(text) = message.strip_prefix("TEXT ") {
            AgentEvent::MessageChunk {
                role: Role::Agent,
                text: text.into(),
            }
        } else {
            match message {
                "TURN" => AgentEvent::TurnStarted,
                "WAITING" => AgentEvent::Diagnostic {
                    message: "mock is ready for shutdown input".into(),
                },
                "DONE" => AgentEvent::TurnEnded {
                    reason: StopReason::EndTurn,
                },
                "CANCELLED" => AgentEvent::TurnEnded {
                    reason: StopReason::Cancelled,
                },
                "LIMIT quota" | "LIMIT rate" | "LIMIT context" => {
                    AgentEvent::LimitReached(LimitSignal {
                        kind: match message {
                            "LIMIT quota" => LimitKind::UsageQuota,
                            "LIMIT rate" => LimitKind::RateLimit,
                            _ => LimitKind::ContextWindow,
                        },
                        resets_at: None,
                        message: Some(message.into()),
                    })
                }
                "ERROR auth" => AgentEvent::ProviderError {
                    class: ErrorClass::AuthenticationRequired,
                    message: "mock authentication failure".into(),
                },
                "ERROR network" => AgentEvent::ProviderError {
                    class: ErrorClass::Transient,
                    message: "mock network failure".into(),
                },
                "ERROR unknown" => AgentEvent::ProviderError {
                    class: ErrorClass::Internal,
                    message: "unclassified mock failure".into(),
                },
                _ => malformed(message),
            }
        };
        out.push(DecodedEvent::native(event));
    }
}
fn malformed(message: &str) -> AgentEvent {
    AgentEvent::ProviderError {
        class: ErrorClass::ProtocolViolation,
        message: format!("invalid mock record: {message:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_records_report_protocol_errors_without_panicking() {
        let mut decoder = MockDecoder;
        let mut events = Vec::new();
        for record in [
            "",
            "READY ",
            "READY \0",
            "LIMIT something-else",
            "{broken-json",
        ] {
            decoder.decode(TransportFrame::Message(record), &mut events);
        }
        assert_eq!(events.len(), 5);
        assert!(events.iter().all(|e| matches!(
            e.event,
            AgentEvent::ProviderError {
                class: ErrorClass::ProtocolViolation,
                ..
            }
        )));
    }
}
