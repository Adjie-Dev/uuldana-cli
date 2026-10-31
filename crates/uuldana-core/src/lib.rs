//! Provider-agnostic contracts for **Uuldana**, a native AI coding agent
//! supervisor.
//!
//! Uuldana runs existing coding agents through their own interfaces (a native
//! terminal UI in a PTY, the Agent Client Protocol, or a provider-specific
//! CLI mode). It never replaces a provider's agent loop, tools, MCP servers,
//! skills, plugins, configuration or instruction-file discovery.
//!
//! This crate holds the contracts shared by the supervisor, the runtime and
//! provider adapters. It is synchronous, performs no I/O of its own, and has
//! no third-party dependencies.
//!
//! | Module | Responsibility |
//! | --- | --- |
//! | [`adapter`] | `ProviderAdapter` trait, launch plans, host probing, event decoders |
//! | [`capability`] | Tri-state capability negotiation |
//! | [`transport`] | Transport kinds, specs, frames, ACP version negotiation |
//! | [`event`] | Normalized event taxonomy with fidelity labels |
//! | [`permission`] | Approval and sandbox policies, escalation checks |
//! | [`session`] | Uuldana-owned session and run records |
//! | [`lifecycle`] | Run state machine |
//! | [`error`] | Error classification and dispositions |
//! | [`registry`] | Provider registration and discovery |
//! | [`handoff`] | Shapes reserved for cross-provider handoff |
//!
//! Architecture: `docs/ARCHITECTURE.md`. Adapter guide: `docs/PROVIDER_SDK.md`.

pub mod adapter;
pub mod capability;
pub mod error;
pub mod event;
pub mod handoff;
pub mod ids;
pub mod lifecycle;
pub mod permission;
pub mod registry;
pub mod session;
pub mod time;
pub mod transport;
pub mod version;

pub use adapter::{
    EnvChange, EventDecoder, HostProbe, Installation, LaunchPlan, LaunchPlanError, LaunchRequest,
    NativeSessionBinding, Platform, ProbeError, ProbeOutput, ProviderAdapter, ProviderDescriptor,
    ResumeRequest, ShutdownPlan, ShutdownStep,
};
pub use capability::{
    Capability, CapabilityEntry, CapabilityError, CapabilitySet, CustomCapability,
    CustomCapabilityError, Evidence, Support,
};
pub use error::{AdapterError, Disposition, ErrorClass};
pub use event::{
    AgentEvent, DecodedEvent, EventEnvelope, Fidelity, LimitKind, LimitSignal, Role, StopReason,
    ToolKind, ToolStatus,
};
pub use handoff::{HandoffPackage, NonTransferable, SessionCapture, TranscriptEntry};
pub use ids::{
    IdError, NativeSessionId, PermissionRequestId, ProviderId, RunId, SessionId, ToolCallId,
};
pub use lifecycle::{LifecycleError, RunEnd, RunFinish, RunOutcome, RunState, RunTransition};
pub use permission::{
    ApprovalMode, DecisionSource, FilesystemAccess, NetworkAccess, PermissionDecision,
    PermissionDimension, PermissionError, PermissionMapping, PermissionPolicy,
    PermissionResolution, SandboxPolicy, ValidatedPermissions, validate_mapping,
};
pub use registry::{Discovered, DiscoveryReport, ProviderRegistry, RegistryError};
pub use session::{
    NativeSessionEntry, NativeSessionLink, NativeSessionRef, NativeSessionUpdate, NewRun,
    RunContext, RunRecord, Session, SessionError,
};
pub use time::Timestamp;
pub use transport::{
    AcpVersionRange, ExitInfo, StdinMode, StreamFraming, TerminalSize, TransportError,
    TransportFrame, TransportKind, TransportSpec, negotiate_acp_version,
};
pub use version::{CONTRACT_VERSION, ContractVersion};
