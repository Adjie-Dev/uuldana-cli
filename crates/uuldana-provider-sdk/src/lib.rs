//! Stable entry point for writing **Uuldana** provider adapters.
//!
//! Adapter crates should depend on this crate rather than on `uuldana-core`
//! directly. It re-exports exactly the surface an adapter needs, so the core
//! can reorganize supervisor-side internals without breaking adapters.
//!
//! Start with [`prelude`] and implement [`ProviderAdapter`]. The full guide,
//! including the rules every adapter must follow, is `docs/PROVIDER_SDK.md`.
//!
//! ```
//! use uuldana_provider_sdk::prelude::*;
//!
//! let id = ProviderId::new("example-agent").unwrap();
//! let descriptor = ProviderDescriptor::new(id, "Example Agent", vec![TransportKind::NativePty]);
//! assert_eq!(descriptor.contract, CONTRACT_VERSION);
//! ```

pub use uuldana_core::adapter::{
    EnvChange, EventDecoder, HostProbe, Installation, LaunchPlan, LaunchPlanError, LaunchRequest,
    NativeSessionBinding, Platform, ProbeError, ProbeOutput, ProviderAdapter, ProviderDescriptor,
    ResumeRequest, ShutdownPlan, ShutdownStep,
};
pub use uuldana_core::capability::{
    Capability, CapabilityEntry, CapabilityError, CapabilitySet, CustomCapability,
    CustomCapabilityError, Evidence, Support,
};
pub use uuldana_core::error::{AdapterError, Disposition, ErrorClass};
pub use uuldana_core::event::{
    AgentEvent, DecodedEvent, Fidelity, LimitKind, LimitSignal, Role, StopReason, ToolKind,
    ToolStatus,
};
pub use uuldana_core::handoff::{HandoffPackage, NonTransferable, SessionCapture, TranscriptEntry};
pub use uuldana_core::ids::{
    IdError, NativeSessionId, PermissionRequestId, ProviderId, ToolCallId,
};
pub use uuldana_core::lifecycle::{RunEnd, RunState};
pub use uuldana_core::permission::{
    ApprovalMode, DecisionSource, FilesystemAccess, NetworkAccess, PermissionDecision,
    PermissionDimension, PermissionError, PermissionMapping, PermissionPolicy,
    PermissionResolution, SandboxPolicy, ValidatedPermissions, validate_mapping,
};
pub use uuldana_core::session::{
    NativeSessionEntry, NativeSessionLink, NativeSessionRef, RunContext, RunRecord,
};
pub use uuldana_core::time::Timestamp;
pub use uuldana_core::transport::{
    AcpVersionRange, ExitInfo, StdinMode, StreamFraming, TerminalSize, TransportFrame,
    TransportKind, TransportSpec,
};
pub use uuldana_core::version::{CONTRACT_VERSION, ContractVersion};

/// The items almost every adapter uses.
pub mod prelude {
    pub use crate::{
        AdapterError, AgentEvent, CONTRACT_VERSION, Capability, CapabilitySet, DecodedEvent,
        ErrorClass, EventDecoder, Evidence, Fidelity, HostProbe, Installation, LaunchPlan,
        LaunchRequest, PermissionMapping, PermissionPolicy, ProviderAdapter, ProviderDescriptor,
        ProviderId, ResumeRequest, RunContext, Support, TerminalSize, TransportFrame,
        TransportKind, TransportSpec,
    };
}
