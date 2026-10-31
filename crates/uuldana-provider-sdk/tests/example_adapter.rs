//! A fictional third-party provider written only against `uuldana-provider-sdk`.
//!
//! This is an SDK extensibility check, not the runtime mock provider: nothing
//! here spawns a process. It proves a new provider can be described, detected,
//! permission-mapped, launched (as a plan) and decoded without touching the
//! core contracts.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use uuldana_provider_sdk::prelude::*;
use uuldana_provider_sdk::{
    ApprovalMode, ExitInfo, FilesystemAccess, LimitKind, LimitSignal, NativeSessionBinding,
    NativeSessionId, NativeSessionLink, NetworkAccess, PermissionError, Platform, ProbeError,
    ProbeOutput, Role, SandboxPolicy, SessionCapture, StdinMode, StreamFraming, validate_mapping,
};

/// Fictional provider. Its CLI has `--approval ask|risky|never`, a
/// `--sandbox read-only|workspace` flag, no network sandbox, and a line-based
/// structured mode enabled with `--lines`.
struct ExampleAgent {
    descriptor: ProviderDescriptor,
}

impl ExampleAgent {
    fn new() -> Self {
        Self {
            descriptor: ProviderDescriptor::new(
                ProviderId::new("example-agent").unwrap(),
                "Example Agent",
                vec![TransportKind::NativePty, TransportKind::CustomCli],
            ),
        }
    }
}

impl ProviderAdapter for ExampleAgent {
    fn descriptor(&self) -> &ProviderDescriptor {
        &self.descriptor
    }

    fn detect(&self, probe: &dyn HostProbe) -> Result<Installation, AdapterError> {
        let executable = probe.find_executable("example-agent").ok_or_else(|| {
            AdapterError::new(ErrorClass::NotInstalled, "example-agent not found")
        })?;
        let output = probe.run(&executable, &["--version"], Duration::from_secs(5))?;
        let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        Ok(Installation::new(executable).with_version(version))
    }

    fn capabilities(&self, _installation: &Installation) -> CapabilitySet {
        CapabilitySet::new()
            .with(
                Capability::NativeTerminalUi,
                Support::Supported,
                Evidence::Declared,
            )
            .with(
                Capability::ApprovalModeControl,
                Support::Supported,
                Evidence::Declared,
            )
            .with(
                Capability::FilesystemSandboxControl,
                Support::Supported,
                Evidence::Declared,
            )
            .with(
                Capability::NetworkSandboxControl,
                Support::Unsupported,
                Evidence::Declared,
            )
            .with(Capability::ResumeById, Support::Unknown, Evidence::Declared)
    }

    fn map_permissions(
        &self,
        _installation: &Installation,
        requested: &PermissionPolicy,
    ) -> PermissionMapping {
        let filesystem = match requested.sandbox.filesystem {
            FilesystemAccess::ReadOnly => FilesystemAccess::ReadOnly,
            // No "unrestricted" mode exists, so fall back to the stricter one.
            _ => FilesystemAccess::WorkspaceWrite,
        };
        PermissionMapping::new(PermissionPolicy {
            approval: requested.approval,
            sandbox: SandboxPolicy {
                filesystem,
                // The provider cannot block network access: report reality.
                network: NetworkAccess::Allowed,
            },
        })
        .with_note("example-agent has no network sandbox")
    }

    fn launch_plan(
        &self,
        installation: &Installation,
        request: &LaunchRequest,
    ) -> Result<LaunchPlan, AdapterError> {
        let effective = request.permissions.effective();
        let approval = match effective.approval {
            ApprovalMode::AskEveryAction => "ask",
            ApprovalMode::AskForRisky => "risky",
            ApprovalMode::NeverAsk => "never",
        };
        let sandbox = match effective.sandbox.filesystem {
            FilesystemAccess::ReadOnly => "read-only",
            _ => "workspace",
        };
        let transport = match request.transport {
            TransportKind::NativePty => TransportSpec::NativePty {
                initial_size: request.terminal_size.unwrap_or_default(),
            },
            TransportKind::CustomCli => TransportSpec::CustomCli {
                stdout: StreamFraming::Lines,
                stdin: StdinMode::Closed,
            },
            other => {
                return Err(AdapterError::new(
                    ErrorClass::Unsupported,
                    format!("transport {other} is not supported"),
                ));
            }
        };
        let mut plan = LaunchPlan::new(
            installation.executable.clone(),
            request.workspace.clone(),
            transport,
        )
        .args(["--approval", approval, "--sandbox", sandbox])
        .native_session(NativeSessionBinding::DiscoverFromEvents);
        if request.transport == TransportKind::CustomCli {
            plan = plan.arg("--lines");
        }
        Ok(plan)
    }

    fn event_decoder(&self, transport: TransportKind) -> Option<Box<dyn EventDecoder>> {
        (transport == TransportKind::CustomCli)
            .then(|| Box::new(LineDecoder) as Box<dyn EventDecoder>)
    }

    fn capture_session(
        &self,
        context: &RunContext<'_>,
        _probe: &dyn HostProbe,
    ) -> Result<SessionCapture, AdapterError> {
        // The fictional provider files sessions per project directory. A real
        // adapter would look them up read-only under `context.workspace()`.
        let history = context.run().native_sessions();
        let mut capture =
            SessionCapture::new(history.last().map(|e| e.id.clone()), Support::Unknown);
        if let [.., parent, current] = history
            && current.link == NativeSessionLink::Forked
        {
            capture = capture.with_parent(parent.id.clone());
        }
        Ok(capture)
    }
}

/// Decodes `session <id>`, `fork <id>`, `say <text>` and `limit quota` lines.
struct LineDecoder;

impl EventDecoder for LineDecoder {
    fn decode(&mut self, frame: TransportFrame<'_>, out: &mut Vec<DecodedEvent>) {
        match frame {
            TransportFrame::Message(line) => {
                let event = match line.split_once(' ') {
                    Some(("session", id)) => match NativeSessionId::new(id) {
                        Ok(native) => AgentEvent::NativeSessionIdentified { native },
                        Err(err) => AgentEvent::ProviderError {
                            class: ErrorClass::ProtocolViolation,
                            message: err.to_string(),
                        },
                    },
                    Some(("fork", id)) => match NativeSessionId::new(id) {
                        Ok(native) => AgentEvent::NativeSessionChanged {
                            native,
                            link: NativeSessionLink::Forked,
                        },
                        Err(err) => AgentEvent::ProviderError {
                            class: ErrorClass::ProtocolViolation,
                            message: err.to_string(),
                        },
                    },
                    Some(("say", text)) => AgentEvent::MessageChunk {
                        role: Role::Agent,
                        text: text.to_owned(),
                    },
                    Some(("limit", "quota")) => AgentEvent::LimitReached(LimitSignal {
                        kind: LimitKind::UsageQuota,
                        resets_at: None,
                        message: None,
                    }),
                    _ => AgentEvent::Diagnostic {
                        message: format!("unrecognized line: {line}"),
                    },
                };
                out.push(DecodedEvent::native(event));
            }
            TransportFrame::Exited(exit) => {
                out.push(DecodedEvent::native(AgentEvent::ProcessExited(exit)));
            }
            _ => {}
        }
    }
}

struct FakeProbe {
    root: PathBuf,
    installed: bool,
}

impl HostProbe for FakeProbe {
    fn find_executable(&self, name: &str) -> Option<PathBuf> {
        self.installed.then(|| self.root.join(name))
    }

    fn run(
        &self,
        _program: &Path,
        args: &[&str],
        _timeout: Duration,
    ) -> Result<ProbeOutput, ProbeError> {
        assert_eq!(args, ["--version"], "detection must only run --version");
        Ok(ProbeOutput {
            exit: ExitInfo::from_code(0),
            stdout: b"example-agent 3.1.4\n".to_vec(),
            stderr: Vec::new(),
        })
    }

    fn env_var(&self, _name: &str) -> Option<OsString> {
        None
    }

    fn platform(&self) -> Platform {
        Platform::current()
    }
}

fn probe(installed: bool) -> FakeProbe {
    FakeProbe {
        root: std::env::temp_dir().join("uuldana-sdk-test-bin"),
        installed,
    }
}

fn workspace() -> PathBuf {
    std::env::temp_dir().join("uuldana-sdk-test-workspace")
}

fn policy(network: NetworkAccess) -> PermissionPolicy {
    PermissionPolicy {
        approval: ApprovalMode::AskForRisky,
        sandbox: SandboxPolicy {
            filesystem: FilesystemAccess::WorkspaceWrite,
            network,
        },
    }
}

#[test]
fn registers_and_discovers_through_the_core_registry() {
    let mut registry = uuldana_core::ProviderRegistry::new();
    registry.register(Arc::new(ExampleAgent::new())).unwrap();

    let reports = registry.discover(&probe(true));
    assert_eq!(reports.len(), 1);
    let found = reports[0].outcome.as_ref().unwrap();
    assert_eq!(
        found.installation.version.as_deref(),
        Some("example-agent 3.1.4")
    );
    assert_eq!(
        found
            .capabilities
            .support(&Capability::NetworkSandboxControl),
        Support::Unsupported
    );

    let missing = registry.discover(&probe(false));
    assert_eq!(
        missing[0].outcome.as_ref().unwrap_err().class(),
        ErrorClass::NotInstalled
    );
}

#[test]
fn honest_mapping_blocks_unenforceable_network_restriction() {
    let adapter = ExampleAgent::new();
    let installation = adapter.detect(&probe(true)).unwrap();
    let requested = policy(NetworkAccess::Denied);
    let mapping = adapter.map_permissions(&installation, &requested);
    assert!(matches!(
        validate_mapping(&requested, &mapping),
        Err(PermissionError::WouldEscalate { .. })
    ));
}

#[test]
fn validated_launch_produces_a_valid_plan() {
    let adapter = ExampleAgent::new();
    let installation = adapter.detect(&probe(true)).unwrap();
    let requested = policy(NetworkAccess::Allowed);
    let permissions = validate_mapping(
        &requested,
        &adapter.map_permissions(&installation, &requested),
    )
    .unwrap();

    for transport in [TransportKind::NativePty, TransportKind::CustomCli] {
        let request = LaunchRequest::new(workspace(), transport, permissions.clone());
        let plan = adapter.launch_plan(&installation, &request).unwrap();
        plan.validate(&request).unwrap();
        assert!(plan.args.iter().any(|a| a == "risky"));
    }

    let acp = LaunchRequest::new(workspace(), TransportKind::Acp, permissions);
    assert_eq!(
        adapter
            .launch_plan(&installation, &acp)
            .unwrap_err()
            .class(),
        ErrorClass::Unsupported
    );
}

#[test]
fn unimplemented_optional_methods_are_explicitly_unsupported() {
    let adapter = ExampleAgent::new();
    let installation = adapter.detect(&probe(true)).unwrap();
    let requested = policy(NetworkAccess::Allowed);
    let permissions = validate_mapping(
        &requested,
        &adapter.map_permissions(&installation, &requested),
    )
    .unwrap();
    let resume = ResumeRequest::new(
        LaunchRequest::new(workspace(), TransportKind::NativePty, permissions),
        NativeSessionId::new("abc").unwrap(),
    );
    let err = adapter.resume_plan(&installation, &resume).unwrap_err();
    assert_eq!(err.class(), ErrorClass::Unsupported);
    assert_eq!(err.capability(), Some(&Capability::ResumeById));
}

#[test]
fn line_decoder_normalizes_events() {
    let adapter = ExampleAgent::new();
    assert!(adapter.event_decoder(TransportKind::NativePty).is_none());
    let mut decoder = adapter.event_decoder(TransportKind::CustomCli).unwrap();
    let mut out = Vec::new();
    for line in ["session n-42", "say hello", "limit quota", "garbage"] {
        decoder.decode(TransportFrame::Message(line), &mut out);
    }
    decoder.decode(TransportFrame::Exited(ExitInfo::from_code(1)), &mut out);

    assert_eq!(out.len(), 5);
    assert!(out.iter().all(|e| e.fidelity == Fidelity::Native));
    assert_eq!(
        out[0].event,
        AgentEvent::NativeSessionIdentified {
            native: NativeSessionId::new("n-42").unwrap()
        }
    );
    assert!(matches!(
        &out[2].event,
        AgentEvent::LimitReached(LimitSignal {
            kind: LimitKind::UsageQuota,
            ..
        })
    ));
    assert!(matches!(&out[3].event, AgentEvent::Diagnostic { .. }));
    assert_eq!(
        ErrorClass::Limit(LimitKind::UsageQuota).disposition(),
        uuldana_provider_sdk::Disposition::FailoverEligible
    );
}

#[test]
fn forked_native_session_is_captured_through_run_context() {
    let adapter = ExampleAgent::new();
    let mut decoder = adapter.event_decoder(TransportKind::CustomCli).unwrap();
    let mut out = Vec::new();
    for line in ["session n-1", "fork n-2"] {
        decoder.decode(TransportFrame::Message(line), &mut out);
    }

    // Host side: the supervisor records what the decoder reported.
    let policy = policy(NetworkAccess::Allowed);
    let run = uuldana_core::RunId::new("run-1").unwrap();
    let mut session = uuldana_core::Session::new(
        uuldana_core::SessionId::new("session-1").unwrap(),
        workspace(),
        policy,
        uuldana_core::Timestamp::from_unix_millis(0),
    )
    .unwrap();
    session
        .begin_run(uuldana_core::NewRun {
            id: run.clone(),
            provider: adapter.descriptor().id.clone(),
            transport: TransportKind::CustomCli,
            permissions: validate_mapping(&policy, &PermissionMapping::new(policy)).unwrap(),
            at: uuldana_core::Timestamp::from_unix_millis(1),
        })
        .unwrap();
    for decoded in out {
        match decoded.event {
            AgentEvent::NativeSessionIdentified { native } => {
                session.record_native_session(&run, native).unwrap();
            }
            AgentEvent::NativeSessionChanged { native, link } => {
                session.observe_native_session(&run, native, link).unwrap();
            }
            other => panic!("unexpected event {other:?}"),
        }
    }

    let context = session.run_context(&run).unwrap();
    assert_eq!(context.workspace(), workspace());
    let capture = adapter.capture_session(&context, &probe(true)).unwrap();
    assert_eq!(capture.native.unwrap().as_str(), "n-2");
    assert_eq!(capture.parent.unwrap().as_str(), "n-1");
    assert_eq!(capture.resumable, Support::Unknown);
}
