//! The provider adapter contract.
//!
//! An adapter **describes** how to run one provider. It never spawns a
//! process, writes a file, or owns a transport. The runtime executes the
//! [`LaunchPlan`] an adapter returns, feeds transport frames to the adapter's
//! [`EventDecoder`], and the supervisor decides what happens next.
//!
//! All methods are synchronous. Host access during detection goes through the
//! read-only [`HostProbe`] trait, which the runtime implements.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::capability::{Capability, CapabilitySet};
use crate::error::{AdapterError, ErrorClass};
use crate::event::DecodedEvent;
use crate::handoff::{HandoffPackage, SessionCapture};
use crate::ids::{NativeSessionId, ProviderId};
use crate::permission::{PermissionMapping, PermissionPolicy, ValidatedPermissions};
use crate::session::RunContext;
use crate::transport::{ExitInfo, TerminalSize, TransportFrame, TransportKind, TransportSpec};
use crate::version::{CONTRACT_VERSION, ContractVersion};

/// Static identity of an adapter.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderDescriptor {
    /// Stable provider identifier.
    pub id: ProviderId,
    /// Human readable name.
    pub display_name: String,
    /// Contract version the adapter was built against.
    pub contract: ContractVersion,
    /// Transports the adapter can describe launches for, in preference order.
    pub transports: Vec<TransportKind>,
}

impl ProviderDescriptor {
    /// Creates a descriptor using the contract version of this build.
    pub fn new(
        id: ProviderId,
        display_name: impl Into<String>,
        transports: Vec<TransportKind>,
    ) -> Self {
        Self {
            id,
            display_name: display_name.into(),
            contract: CONTRACT_VERSION,
            transports,
        }
    }

    /// Whether the adapter supports `transport`.
    pub fn supports_transport(&self, transport: TransportKind) -> bool {
        self.transports.contains(&transport)
    }
}

/// Host operating system family.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Platform {
    /// Windows.
    Windows,
    /// macOS.
    MacOs,
    /// Linux.
    Linux,
    /// Anything else.
    Other,
}

impl Platform {
    /// Platform this binary was compiled for.
    pub const fn current() -> Self {
        if cfg!(target_os = "windows") {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else {
            Self::Other
        }
    }
}

/// Output of a probe command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeOutput {
    /// How the command exited.
    pub exit: ExitInfo,
    /// Captured standard output.
    pub stdout: Vec<u8>,
    /// Captured standard error.
    pub stderr: Vec<u8>,
}

/// Failure of a probe operation.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeError {
    /// The program does not exist.
    NotFound,
    /// The program did not finish within the timeout and was killed.
    TimedOut,
    /// The host refused the operation.
    Denied(String),
    /// Any other I/O failure.
    Io(String),
}

impl fmt::Display for ProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => f.write_str("program not found"),
            Self::TimedOut => f.write_str("probe timed out"),
            Self::Denied(msg) => write!(f, "probe denied: {msg}"),
            Self::Io(msg) => write!(f, "probe failed: {msg}"),
        }
    }
}

impl std::error::Error for ProbeError {}

impl From<ProbeError> for AdapterError {
    fn from(err: ProbeError) -> Self {
        let class = match err {
            ProbeError::NotFound => ErrorClass::NotInstalled,
            ProbeError::TimedOut => ErrorClass::Transient,
            ProbeError::Denied(_) => ErrorClass::PermissionDenied,
            ProbeError::Io(_) => ErrorClass::Internal,
        };
        AdapterError::new(class, err.to_string())
    }
}

/// Read-only access to the host, provided by the runtime during detection.
///
/// There is deliberately no method that writes files or changes
/// configuration. Adapters must not read credential files either; see
/// `docs/PROVIDER_SDK.md`.
pub trait HostProbe: Send + Sync {
    /// Resolves an executable name on the search path. On Windows the
    /// implementation must honor `PATHEXT` so `.cmd` shims are found.
    fn find_executable(&self, name: &str) -> Option<PathBuf>;

    /// Runs a short, side-effect free command (for example `--version`) and
    /// captures its output. The process is killed after `timeout`.
    fn run(
        &self,
        program: &Path,
        args: &[&str],
        timeout: Duration,
    ) -> Result<ProbeOutput, ProbeError>;

    /// Reads an environment variable of the Uuldana process.
    fn env_var(&self, name: &str) -> Option<OsString>;

    /// Host platform.
    fn platform(&self) -> Platform;
}

/// A detected provider installation.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installation {
    /// Absolute path of the provider executable.
    pub executable: PathBuf,
    /// Raw version string reported by the provider, if any.
    pub version: Option<String>,
    /// Detection notes for diagnostics.
    pub notes: Vec<String>,
}

impl Installation {
    /// Creates an installation record.
    pub fn new(executable: PathBuf) -> Self {
        Self {
            executable,
            version: None,
            notes: Vec::new(),
        }
    }

    /// Sets the raw version string.
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }

    /// Adds a note.
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }
}

/// What the supervisor asks an adapter to launch.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchRequest {
    /// Absolute workspace path. The provider runs with this as its cwd.
    pub workspace: PathBuf,
    /// Chosen transport.
    pub transport: TransportKind,
    /// Validated permissions. The adapter must launch with
    /// `permissions.effective()` and nothing more permissive.
    pub permissions: ValidatedPermissions,
    /// Initial terminal size for PTY launches.
    pub terminal_size: Option<TerminalSize>,
    /// Optional first prompt.
    pub initial_prompt: Option<String>,
    /// Continue the most recent conversation of this workspace instead of
    /// starting a new one, for providers that can do that without a session id.
    pub continue_latest: bool,
}

impl LaunchRequest {
    /// Creates a request without terminal size or prompt.
    pub fn new(
        workspace: PathBuf,
        transport: TransportKind,
        permissions: ValidatedPermissions,
    ) -> Self {
        Self {
            workspace,
            transport,
            permissions,
            terminal_size: None,
            initial_prompt: None,
            continue_latest: false,
        }
    }
}

/// A request to resume a native session.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeRequest {
    /// Launch parameters.
    pub launch: LaunchRequest,
    /// Native session to resume.
    pub native: NativeSessionId,
}

impl ResumeRequest {
    /// Creates a resume request.
    pub fn new(launch: LaunchRequest, native: NativeSessionId) -> Self {
        Self { launch, native }
    }
}

/// An environment change applied to the child process only.
///
/// `Debug` output redacts values so secrets do not leak into logs.
#[derive(Clone, PartialEq, Eq)]
pub enum EnvChange {
    /// Set a variable.
    Set {
        /// Variable name.
        key: OsString,
        /// Value.
        value: OsString,
    },
    /// Remove an inherited variable.
    Remove {
        /// Variable name.
        key: OsString,
    },
}

impl fmt::Debug for EnvChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Set { key, .. } => f
                .debug_struct("Set")
                .field("key", key)
                .field("value", &"<redacted>")
                .finish(),
            Self::Remove { key } => f.debug_struct("Remove").field("key", key).finish(),
        }
    }
}

/// How the native session id of a launch becomes known.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum NativeSessionBinding {
    /// The adapter chose the id up front (the provider accepts it at launch).
    Preassigned(NativeSessionId),
    /// The decoder will emit `AgentEvent::NativeSessionIdentified`.
    DiscoverFromEvents,
    /// The id cannot be known for this launch.
    #[default]
    Unavailable,
}

/// A complete, inert description of how to start a provider.
///
/// It contains no file writes. Configuration and authentication files of
/// the provider are never touched; only arguments, environment and working
/// directory of the child process are described.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    /// Absolute path of the program to run.
    pub program: PathBuf,
    /// Arguments, without the program name.
    pub args: Vec<OsString>,
    /// Environment changes on top of the inherited environment.
    pub env: Vec<EnvChange>,
    /// Absolute working directory. Must equal the request workspace.
    pub cwd: PathBuf,
    /// Transport parameters.
    pub transport: TransportSpec,
    /// How the native session id becomes known.
    pub native_session: NativeSessionBinding,
}

impl LaunchPlan {
    /// Creates a plan with no arguments or environment changes.
    pub fn new(program: PathBuf, cwd: PathBuf, transport: TransportSpec) -> Self {
        Self {
            program,
            args: Vec::new(),
            env: Vec::new(),
            cwd,
            transport,
            native_session: NativeSessionBinding::Unavailable,
        }
    }

    /// Appends an argument.
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Appends arguments.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Sets an environment variable for the child.
    pub fn env_set(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push(EnvChange::Set {
            key: key.into(),
            value: value.into(),
        });
        self
    }

    /// Removes an inherited environment variable for the child.
    pub fn env_remove(mut self, key: impl Into<OsString>) -> Self {
        self.env.push(EnvChange::Remove { key: key.into() });
        self
    }

    /// Sets the native session binding.
    pub fn native_session(mut self, binding: NativeSessionBinding) -> Self {
        self.native_session = binding;
        self
    }

    /// Checks structural rules against the request it answers.
    pub fn validate(&self, request: &LaunchRequest) -> Result<(), LaunchPlanError> {
        if !self.program.is_absolute() {
            return Err(LaunchPlanError::ProgramNotAbsolute(self.program.clone()));
        }
        if !self.cwd.is_absolute() {
            return Err(LaunchPlanError::CwdNotAbsolute(self.cwd.clone()));
        }
        if self.cwd != request.workspace {
            return Err(LaunchPlanError::CwdMismatch {
                plan: self.cwd.clone(),
                workspace: request.workspace.clone(),
            });
        }
        if self.transport.kind() != request.transport {
            return Err(LaunchPlanError::TransportMismatch {
                plan: self.transport.kind(),
                requested: request.transport,
            });
        }
        if let TransportSpec::NativePty { initial_size } = self.transport
            && !initial_size.is_valid()
        {
            return Err(LaunchPlanError::InvalidTerminalSize(initial_size));
        }
        if let Some(index) = self
            .args
            .iter()
            .position(|arg| arg.as_encoded_bytes().contains(&0))
        {
            return Err(LaunchPlanError::InvalidArgument { index });
        }
        for change in &self.env {
            let (key, value) = match change {
                EnvChange::Set { key, value } => (key, Some(value)),
                EnvChange::Remove { key } => (key, None),
            };
            let key_bytes = key.as_encoded_bytes();
            let bad_key =
                key_bytes.is_empty() || key_bytes.contains(&b'=') || key_bytes.contains(&0);
            let bad_value = value.is_some_and(|v| v.as_encoded_bytes().contains(&0));
            if bad_key || bad_value {
                return Err(LaunchPlanError::InvalidEnv {
                    key: key.to_string_lossy().into_owned(),
                });
            }
        }
        Ok(())
    }
}

/// A launch plan that breaks a structural rule.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchPlanError {
    /// The program path is relative.
    ProgramNotAbsolute(PathBuf),
    /// The working directory is relative.
    CwdNotAbsolute(PathBuf),
    /// The working directory differs from the requested workspace.
    CwdMismatch {
        /// Plan cwd.
        plan: PathBuf,
        /// Requested workspace.
        workspace: PathBuf,
    },
    /// The plan uses a different transport than requested.
    TransportMismatch {
        /// Plan transport.
        plan: TransportKind,
        /// Requested transport.
        requested: TransportKind,
    },
    /// The PTY size has a zero dimension.
    InvalidTerminalSize(TerminalSize),
    /// An argument contains a NUL character.
    InvalidArgument {
        /// Index into `args`.
        index: usize,
    },
    /// An environment key is empty or contains `=` or NUL, or a value
    /// contains NUL.
    InvalidEnv {
        /// The offending key, lossily converted.
        key: String,
    },
}

impl fmt::Display for LaunchPlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProgramNotAbsolute(p) => {
                write!(f, "program path is not absolute: {}", p.display())
            }
            Self::CwdNotAbsolute(p) => {
                write!(f, "working directory is not absolute: {}", p.display())
            }
            Self::CwdMismatch { plan, workspace } => write!(
                f,
                "working directory {} does not match workspace {}",
                plan.display(),
                workspace.display()
            ),
            Self::TransportMismatch { plan, requested } => {
                write!(
                    f,
                    "plan uses transport {plan} but {requested} was requested"
                )
            }
            Self::InvalidTerminalSize(size) => {
                write!(f, "invalid terminal size {}x{}", size.cols, size.rows)
            }
            Self::InvalidArgument { index } => write!(f, "argument {index} contains NUL"),
            Self::InvalidEnv { key } => write!(f, "invalid environment change for {key:?}"),
        }
    }
}

impl std::error::Error for LaunchPlanError {}

/// One graceful shutdown step. The runtime force-terminates the process if
/// it is still alive after [`ShutdownPlan::grace`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShutdownStep {
    /// Send an interrupt (Ctrl+C on a PTY, the platform equivalent otherwise).
    Interrupt,
    /// Close the child's input.
    CloseInput,
    /// Write bytes to the child's input, for example an exit command.
    WriteInput(Vec<u8>),
}

impl ShutdownStep {
    /// The step to try once instead when the active transport reports this
    /// one as unsupported. An interrupt falls back to closing input; other
    /// steps have no fallback.
    pub fn fallback(&self) -> Option<ShutdownStep> {
        match self {
            Self::Interrupt => Some(Self::CloseInput),
            Self::CloseInput | Self::WriteInput(_) => None,
        }
    }
}

/// How to stop a provider gracefully.
///
/// Execution contract for the runtime (ADR 0008):
///
/// 1. Run `steps` in order. Each step is bounded by `grace`.
/// 2. A step the transport reports as unsupported, or that fails, is recorded
///    as a diagnostic and its [`ShutdownStep::fallback`] is tried once. It
///    does not consume the grace period.
/// 3. After the steps, wait up to `grace` for the direct child to exit. The
///    run then ends `Cancelled`.
/// 4. Otherwise force-terminate the process. The run then ends `Terminated`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownPlan {
    /// Steps, in order.
    pub steps: Vec<ShutdownStep>,
    /// Time to wait after the last step before force termination.
    pub grace: Duration,
}

impl Default for ShutdownPlan {
    /// One interrupt, then five seconds of grace.
    fn default() -> Self {
        Self {
            steps: vec![ShutdownStep::Interrupt],
            grace: Duration::from_secs(5),
        }
    }
}

/// Converts transport frames into normalized events for one run.
///
/// Decoders must not panic on malformed input. Unparseable structured data
/// should produce an `AgentEvent::ProviderError` with
/// `ErrorClass::ProtocolViolation` or an `AgentEvent::Diagnostic`. Usage
/// limit detection happens here, labeled with the right [`crate::Fidelity`].
pub trait EventDecoder: Send {
    /// Decodes one frame, appending zero or more events to `out`.
    fn decode(&mut self, frame: TransportFrame<'_>, out: &mut Vec<DecodedEvent>);
}

/// The contract every provider adapter implements.
///
/// Typical call order, driven by the supervisor and runtime:
///
/// 1. [`descriptor`](Self::descriptor) at registration.
/// 2. [`detect`](Self::detect), then [`capabilities`](Self::capabilities).
/// 3. [`map_permissions`](Self::map_permissions), validated by the core.
/// 4. [`launch_plan`](Self::launch_plan) or [`resume_plan`](Self::resume_plan),
///    checked with [`LaunchPlan::validate`].
/// 5. [`event_decoder`](Self::event_decoder) for the chosen transport.
/// 6. [`shutdown_plan`](Self::shutdown_plan) when stopping.
/// 7. Optionally [`capture_session`](Self::capture_session) and
///    [`prepare_handoff`](Self::prepare_handoff).
pub trait ProviderAdapter: Send + Sync + 'static {
    /// Static identity.
    fn descriptor(&self) -> &ProviderDescriptor;

    /// Finds the installed provider using read-only host access.
    /// Return [`ErrorClass::NotInstalled`] when it is absent.
    fn detect(&self, probe: &dyn HostProbe) -> Result<Installation, AdapterError>;

    /// Capabilities of the detected installation. Anything not stated is
    /// `Unknown`.
    fn capabilities(&self, installation: &Installation) -> CapabilitySet;

    /// Reports honestly what `requested` becomes on this provider. When the
    /// provider cannot enforce a dimension, report the permissive value that
    /// will really apply; the core then refuses the launch.
    fn map_permissions(
        &self,
        installation: &Installation,
        requested: &PermissionPolicy,
    ) -> PermissionMapping;

    /// Describes a fresh launch.
    fn launch_plan(
        &self,
        installation: &Installation,
        request: &LaunchRequest,
    ) -> Result<LaunchPlan, AdapterError>;

    /// Describes resuming a native session. Unsupported by default.
    fn resume_plan(
        &self,
        installation: &Installation,
        request: &ResumeRequest,
    ) -> Result<LaunchPlan, AdapterError> {
        let _ = (installation, request);
        Err(AdapterError::unsupported(Capability::ResumeById))
    }

    /// Decoder for `transport`. `None` means "no provider-specific decoding":
    /// for ACP the runtime then uses its generic ACP mapping; for other
    /// transports output is passed through without normalized events.
    fn event_decoder(&self, transport: TransportKind) -> Option<Box<dyn EventDecoder>>;

    /// Locates the native session of a run after the fact, best effort and
    /// read-only. `context` identifies the Uuldana session and the absolute
    /// workspace the run used. Unsupported by default.
    fn capture_session(
        &self,
        context: &RunContext<'_>,
        probe: &dyn HostProbe,
    ) -> Result<SessionCapture, AdapterError> {
        let _ = (context, probe);
        Err(AdapterError::unsupported(Capability::SessionCapture))
    }

    /// Contributes provider-specific visible context to a handoff, starting
    /// from [`HandoffPackage::for_run`]. Unsupported by default; the
    /// orchestrator in `uuldana-runtime` builds the handoff itself.
    fn prepare_handoff(&self, context: &RunContext<'_>) -> Result<HandoffPackage, AdapterError> {
        let _ = context;
        Err(AdapterError::unsupported(Capability::HandoffCapture))
    }

    /// How to stop the provider gracefully on `transport`.
    fn shutdown_plan(&self, transport: TransportKind) -> ShutdownPlan {
        let _ = transport;
        ShutdownPlan::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{Evidence, Support};
    use crate::event::AgentEvent;
    use crate::permission::validate_mapping;
    use crate::transport::{AcpVersionRange, StdinMode, StreamFraming};

    fn workspace() -> PathBuf {
        std::env::temp_dir().join("uuldana-plan-test")
    }

    fn program() -> PathBuf {
        std::env::temp_dir().join("bin").join("agent")
    }

    fn request(transport: TransportKind) -> LaunchRequest {
        let policy = PermissionPolicy::most_restrictive();
        let permissions = validate_mapping(&policy, &PermissionMapping::new(policy)).unwrap();
        LaunchRequest::new(workspace(), transport, permissions)
    }

    fn pty_plan() -> LaunchPlan {
        LaunchPlan::new(
            program(),
            workspace(),
            TransportSpec::NativePty {
                initial_size: TerminalSize::DEFAULT,
            },
        )
    }

    #[test]
    fn valid_plan_passes() {
        let plan = pty_plan()
            .arg("--flag")
            .args(["a", "b"])
            .env_set("AGENT_MODE", "1")
            .env_remove("NOISY");
        assert_eq!(plan.validate(&request(TransportKind::NativePty)), Ok(()));
        assert_eq!(plan.args.len(), 3);
    }

    #[test]
    fn relative_paths_are_rejected() {
        let mut plan = pty_plan();
        plan.program = PathBuf::from("agent");
        assert!(matches!(
            plan.validate(&request(TransportKind::NativePty)),
            Err(LaunchPlanError::ProgramNotAbsolute(_))
        ));
        let mut plan = pty_plan();
        plan.cwd = PathBuf::from("relative");
        assert!(matches!(
            plan.validate(&request(TransportKind::NativePty)),
            Err(LaunchPlanError::CwdNotAbsolute(_))
        ));
    }

    #[test]
    fn cwd_must_match_workspace() {
        let mut plan = pty_plan();
        plan.cwd = std::env::temp_dir().join("elsewhere");
        assert!(matches!(
            plan.validate(&request(TransportKind::NativePty)),
            Err(LaunchPlanError::CwdMismatch { .. })
        ));
    }

    #[test]
    fn transport_must_match_request() {
        let plan = LaunchPlan::new(
            program(),
            workspace(),
            TransportSpec::Acp {
                versions: AcpVersionRange::V1,
            },
        );
        assert_eq!(
            plan.validate(&request(TransportKind::NativePty)),
            Err(LaunchPlanError::TransportMismatch {
                plan: TransportKind::Acp,
                requested: TransportKind::NativePty,
            })
        );
        let cli = LaunchPlan::new(
            program(),
            workspace(),
            TransportSpec::CustomCli {
                stdout: StreamFraming::Lines,
                stdin: StdinMode::Closed,
            },
        );
        assert_eq!(cli.validate(&request(TransportKind::CustomCli)), Ok(()));
    }

    #[test]
    fn zero_sized_pty_is_rejected() {
        let plan = LaunchPlan::new(
            program(),
            workspace(),
            TransportSpec::NativePty {
                initial_size: TerminalSize::new(0, 80),
            },
        );
        assert!(matches!(
            plan.validate(&request(TransportKind::NativePty)),
            Err(LaunchPlanError::InvalidTerminalSize(_))
        ));
    }

    #[test]
    fn nul_and_malformed_env_are_rejected() {
        let req = request(TransportKind::NativePty);
        assert_eq!(
            pty_plan().arg("ok").arg("bad\0").validate(&req),
            Err(LaunchPlanError::InvalidArgument { index: 1 })
        );
        for plan in [
            pty_plan().env_set("", "v"),
            pty_plan().env_set("A=B", "v"),
            pty_plan().env_set("KEY", "v\0"),
            pty_plan().env_remove("K\0"),
        ] {
            assert!(matches!(
                plan.validate(&req),
                Err(LaunchPlanError::InvalidEnv { .. })
            ));
        }
    }

    #[test]
    fn env_debug_redacts_values() {
        let change = EnvChange::Set {
            key: "API_KEY".into(),
            value: "secret-value".into(),
        };
        let debug = format!("{change:?}");
        assert!(debug.contains("API_KEY"));
        assert!(!debug.contains("secret-value"));
        let plan_debug = format!("{:?}", pty_plan().env_set("TOKEN", "hunter2"));
        assert!(!plan_debug.contains("hunter2"));
    }

    #[test]
    fn probe_errors_map_to_classes() {
        assert_eq!(
            AdapterError::from(ProbeError::NotFound).class(),
            ErrorClass::NotInstalled
        );
        assert_eq!(
            AdapterError::from(ProbeError::TimedOut).class(),
            ErrorClass::Transient
        );
    }

    struct MinimalAdapter {
        descriptor: ProviderDescriptor,
    }

    impl ProviderAdapter for MinimalAdapter {
        fn descriptor(&self) -> &ProviderDescriptor {
            &self.descriptor
        }

        fn detect(&self, _probe: &dyn HostProbe) -> Result<Installation, AdapterError> {
            Ok(Installation::new(program()))
        }

        fn capabilities(&self, _installation: &Installation) -> CapabilitySet {
            CapabilitySet::new().with(
                Capability::NativeTerminalUi,
                Support::Supported,
                Evidence::Declared,
            )
        }

        fn map_permissions(
            &self,
            _installation: &Installation,
            requested: &PermissionPolicy,
        ) -> PermissionMapping {
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
                TransportSpec::NativePty {
                    initial_size: request.terminal_size.unwrap_or_default(),
                },
            ))
        }

        fn event_decoder(&self, _transport: TransportKind) -> Option<Box<dyn EventDecoder>> {
            None
        }
    }

    fn minimal() -> MinimalAdapter {
        MinimalAdapter {
            descriptor: ProviderDescriptor::new(
                ProviderId::new("minimal").unwrap(),
                "Minimal",
                vec![TransportKind::NativePty],
            ),
        }
    }

    #[test]
    fn optional_methods_default_to_explicit_unsupported() {
        let adapter = minimal();
        let installation = Installation::new(program());
        let resume = ResumeRequest::new(
            request(TransportKind::NativePty),
            NativeSessionId::new("n1").unwrap(),
        );
        let err = adapter.resume_plan(&installation, &resume).unwrap_err();
        assert_eq!(err.class(), ErrorClass::Unsupported);
        assert_eq!(err.capability(), Some(&Capability::ResumeById));
        assert_eq!(
            adapter.shutdown_plan(TransportKind::NativePty),
            ShutdownPlan::default()
        );
        let session = session_in("uuldana-defaults");
        let context = session.run_context(&run_id()).unwrap();
        assert_eq!(
            adapter
                .capture_session(&context, &NoProbe)
                .unwrap_err()
                .capability(),
            Some(&Capability::SessionCapture)
        );
        assert_eq!(
            adapter.prepare_handoff(&context).unwrap_err().capability(),
            Some(&Capability::HandoffCapture)
        );
    }

    #[test]
    fn interrupt_falls_back_to_close_input() {
        assert_eq!(
            ShutdownStep::Interrupt.fallback(),
            Some(ShutdownStep::CloseInput)
        );
        assert_eq!(ShutdownStep::CloseInput.fallback(), None);
        assert_eq!(
            ShutdownStep::WriteInput(b"exit\n".to_vec()).fallback(),
            None
        );
        assert_eq!(ShutdownPlan::default().steps, [ShutdownStep::Interrupt]);
    }

    struct NoProbe;

    impl HostProbe for NoProbe {
        fn find_executable(&self, _name: &str) -> Option<PathBuf> {
            None
        }

        fn run(
            &self,
            _program: &Path,
            _args: &[&str],
            _timeout: Duration,
        ) -> Result<ProbeOutput, ProbeError> {
            Err(ProbeError::NotFound)
        }

        fn env_var(&self, _name: &str) -> Option<OsString> {
            None
        }

        fn platform(&self) -> Platform {
            Platform::current()
        }
    }

    fn run_id() -> crate::ids::RunId {
        crate::ids::RunId::new("r1").unwrap()
    }

    /// A session in `dir` with one run whose native session is the same in
    /// every workspace, so only the context can tell them apart.
    fn session_in(dir: &str) -> crate::session::Session {
        use crate::session::{NewRun, Session};
        use crate::time::Timestamp;
        let policy = PermissionPolicy::most_restrictive();
        let mut session = Session::new(
            crate::ids::SessionId::new("s").unwrap(),
            std::env::temp_dir().join(dir),
            policy,
            Timestamp::from_unix_millis(0),
        )
        .unwrap();
        session
            .begin_run(NewRun {
                id: run_id(),
                provider: ProviderId::new("minimal").unwrap(),
                transport: TransportKind::NativePty,
                permissions: validate_mapping(&policy, &PermissionMapping::new(policy)).unwrap(),
                at: Timestamp::from_unix_millis(1),
            })
            .unwrap();
        session
            .record_native_session(&run_id(), NativeSessionId::new("same").unwrap())
            .unwrap();
        session
    }

    /// Adapter whose capture result depends on the workspace, as a real
    /// provider whose session store is keyed by project directory would.
    struct WorkspaceAware(MinimalAdapter);

    impl ProviderAdapter for WorkspaceAware {
        fn descriptor(&self) -> &ProviderDescriptor {
            self.0.descriptor()
        }

        fn detect(&self, probe: &dyn HostProbe) -> Result<Installation, AdapterError> {
            self.0.detect(probe)
        }

        fn capabilities(&self, installation: &Installation) -> CapabilitySet {
            self.0.capabilities(installation)
        }

        fn map_permissions(
            &self,
            installation: &Installation,
            requested: &PermissionPolicy,
        ) -> PermissionMapping {
            self.0.map_permissions(installation, requested)
        }

        fn launch_plan(
            &self,
            installation: &Installation,
            request: &LaunchRequest,
        ) -> Result<LaunchPlan, AdapterError> {
            self.0.launch_plan(installation, request)
        }

        fn event_decoder(&self, transport: TransportKind) -> Option<Box<dyn EventDecoder>> {
            self.0.event_decoder(transport)
        }

        fn capture_session(
            &self,
            context: &RunContext<'_>,
            _probe: &dyn HostProbe,
        ) -> Result<SessionCapture, AdapterError> {
            let dir = context.workspace().file_name().unwrap().to_string_lossy();
            let native = NativeSessionId::new(format!("{dir}:{}", context.session_id())).unwrap();
            Ok(SessionCapture::new(
                Some(native),
                crate::capability::Support::Supported,
            ))
        }

        fn prepare_handoff(
            &self,
            context: &RunContext<'_>,
        ) -> Result<HandoffPackage, AdapterError> {
            Ok(HandoffPackage::for_run(context))
        }
    }

    #[test]
    fn callbacks_see_which_workspace_a_run_belongs_to() {
        let adapter = WorkspaceAware(minimal());
        let a = session_in("uuldana-callback-a");
        let b = session_in("uuldana-callback-b");
        let (ctx_a, ctx_b) = (
            a.run_context(&run_id()).unwrap(),
            b.run_context(&run_id()).unwrap(),
        );
        assert_eq!(ctx_a.run(), ctx_b.run(), "run records are identical");

        let capture_a = adapter.capture_session(&ctx_a, &NoProbe).unwrap();
        let capture_b = adapter.capture_session(&ctx_b, &NoProbe).unwrap();
        assert_ne!(capture_a.native, capture_b.native);
        assert_eq!(capture_a.native.unwrap().as_str(), "uuldana-callback-a:s");

        let handoff_a = adapter.prepare_handoff(&ctx_a).unwrap();
        let handoff_b = adapter.prepare_handoff(&ctx_b).unwrap();
        assert_eq!(handoff_a.workspace, a.workspace());
        assert_eq!(handoff_b.workspace, b.workspace());
        assert_eq!(handoff_a.source, handoff_b.source);
    }

    #[test]
    fn adapter_plan_round_trips_through_validation() {
        let adapter = minimal();
        let installation = Installation::new(program()).with_version("1.2.3");
        let req = request(TransportKind::NativePty);
        let plan = adapter.launch_plan(&installation, &req).unwrap();
        assert_eq!(plan.validate(&req), Ok(()));
        assert!(
            adapter
                .descriptor()
                .supports_transport(TransportKind::NativePty)
        );
        assert!(!adapter.descriptor().supports_transport(TransportKind::Acp));
    }

    struct EchoDecoder;

    impl EventDecoder for EchoDecoder {
        fn decode(&mut self, frame: TransportFrame<'_>, out: &mut Vec<DecodedEvent>) {
            match frame {
                TransportFrame::Exited(exit) => {
                    out.push(DecodedEvent::native(AgentEvent::ProcessExited(exit)));
                }
                TransportFrame::Message(text) => {
                    out.push(DecodedEvent::inferred(AgentEvent::Diagnostic {
                        message: text.to_owned(),
                    }))
                }
                _ => {}
            }
        }
    }

    #[test]
    fn decoder_is_object_safe_and_appends_events() {
        let mut decoder: Box<dyn EventDecoder> = Box::new(EchoDecoder);
        let mut out = Vec::new();
        decoder.decode(TransportFrame::Output(b"ignored"), &mut out);
        decoder.decode(TransportFrame::Message("hello"), &mut out);
        decoder.decode(TransportFrame::Exited(ExitInfo::from_code(0)), &mut out);
        assert_eq!(out.len(), 2);
    }
}
