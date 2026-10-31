//! Explicit, tri-state capability negotiation.
//!
//! Every capability is `Supported`, `Unsupported` or `Unknown`. A capability
//! that nobody has stated anything about is `Unknown`, and `Unknown` is never
//! treated as `Supported`. Evidence ranks decide which statement wins when an
//! adapter's static declaration disagrees with what was probed or negotiated.

use std::collections::BTreeMap;
use std::fmt;

use crate::transport::TransportKind;

/// Tri-state support level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Support {
    /// Known to work.
    Supported,
    /// Known not to work, or deliberately disabled.
    Unsupported,
    /// Not known. Callers must not rely on the capability.
    Unknown,
}

impl Support {
    /// Stable snake_case name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Unsupported => "unsupported",
            Self::Unknown => "unknown",
        }
    }

    /// `true` only for [`Support::Supported`].
    pub const fn is_supported(self) -> bool {
        matches!(self, Self::Supported)
    }
}

impl fmt::Display for Support {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a capability statement came from. Later sources outrank earlier ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Evidence {
    /// Stated by the adapter without looking at the installed provider.
    Declared,
    /// Derived from probing the installed provider (version, help output).
    Probed,
    /// Reported by the provider at connect time, for example in an ACP
    /// `initialize` response.
    Negotiated,
}

impl Evidence {
    /// Precedence rank. Higher wins in [`CapabilitySet::refine`].
    pub const fn rank(self) -> u8 {
        match self {
            Self::Declared => 0,
            Self::Probed => 1,
            Self::Negotiated => 2,
        }
    }
}

/// Error for an invalid [`CustomCapability`] name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomCapabilityError {
    name: String,
}

impl fmt::Display for CustomCapabilityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid custom capability {:?}: expected a namespaced name like \"vendor.feature\" \
             using lowercase ASCII letters, digits, '-', '_' and '.'",
            self.name
        )
    }
}

impl std::error::Error for CustomCapabilityError {}

/// A third-party capability with a namespaced name such as `vendor.feature`.
///
/// Custom capabilities let adapters advertise behavior the core does not
/// model without changing the core contract.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CustomCapability(String);

impl CustomCapability {
    /// Maximum length in bytes.
    pub const MAX_LEN: usize = 128;

    /// Validates a namespaced capability name.
    pub fn new(name: impl Into<String>) -> Result<Self, CustomCapabilityError> {
        let name = name.into();
        let valid_chars = name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'));
        let valid = !name.is_empty()
            && name.len() <= Self::MAX_LEN
            && valid_chars
            && name.contains('.')
            && !name.starts_with('.')
            && !name.ends_with('.')
            && !name.contains("..");
        if valid {
            Ok(Self(name))
        } else {
            Err(CustomCapabilityError { name })
        }
    }

    /// Returns the name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A capability Uuldana may rely on.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Capability {
    /// The provider's own terminal UI is shown to the user unchanged.
    NativeTerminalUi,
    /// The provider emits machine-readable events (not screen text).
    StructuredEvents,
    /// A previous native session can be resumed by its identifier.
    ResumeById,
    /// The most recent native session can be resumed without an identifier.
    ResumeLatest,
    /// Uuldana can choose the native session identifier at launch.
    PreassignSessionId,
    /// Resuming replays prior conversation history to the client.
    SessionReplay,
    /// Native sessions can be listed.
    SessionList,
    /// A running turn can be cancelled without killing the process.
    Cancellation,
    /// Permission requests reach Uuldana as structured requests it can answer.
    PermissionRequests,
    /// The approval mode can be set at launch.
    ApprovalModeControl,
    /// Filesystem sandboxing can be set at launch and is enforced by the provider.
    FilesystemSandboxControl,
    /// Network sandboxing can be set at launch and is enforced by the provider.
    NetworkSandboxControl,
    /// Token or context usage is reported.
    UsageReporting,
    /// Usage or rate limits are reported in a recognizable way.
    LimitSignals,
    /// The model can be selected at launch.
    ModelSelection,
    /// An initial prompt can be supplied at launch.
    InitialPrompt,
    /// A reference to the native session can be captured after a run.
    SessionCapture,
    /// The adapter can contribute to a cross-provider handoff package.
    HandoffCapture,
    /// A namespaced third-party capability.
    Custom(CustomCapability),
}

impl Capability {
    /// Stable snake_case name, or the custom name for [`Capability::Custom`].
    pub fn as_str(&self) -> &str {
        match self {
            Self::NativeTerminalUi => "native_terminal_ui",
            Self::StructuredEvents => "structured_events",
            Self::ResumeById => "resume_by_id",
            Self::ResumeLatest => "resume_latest",
            Self::PreassignSessionId => "preassign_session_id",
            Self::SessionReplay => "session_replay",
            Self::SessionList => "session_list",
            Self::Cancellation => "cancellation",
            Self::PermissionRequests => "permission_requests",
            Self::ApprovalModeControl => "approval_mode_control",
            Self::FilesystemSandboxControl => "filesystem_sandbox_control",
            Self::NetworkSandboxControl => "network_sandbox_control",
            Self::UsageReporting => "usage_reporting",
            Self::LimitSignals => "limit_signals",
            Self::ModelSelection => "model_selection",
            Self::InitialPrompt => "initial_prompt",
            Self::SessionCapture => "session_capture",
            Self::HandoffCapture => "handoff_capture",
            Self::Custom(custom) => custom.as_str(),
        }
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One capability statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityEntry {
    /// Support level.
    pub support: Support,
    /// Source of the statement.
    pub evidence: Evidence,
    /// Optional human readable explanation.
    pub note: Option<String>,
}

/// Returned by [`CapabilitySet::require`] when a capability is not `Supported`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityError {
    /// The capability that was required.
    pub capability: Capability,
    /// Its actual support level, `Unsupported` or `Unknown`.
    pub actual: Support,
}

impl fmt::Display for CapabilityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "capability `{}` is required but its support is {}",
            self.capability, self.actual
        )
    }
}

impl std::error::Error for CapabilityError {}

/// A set of capability statements keyed by capability.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapabilitySet {
    entries: BTreeMap<Capability, CapabilityEntry>,
}

impl CapabilitySet {
    /// Creates an empty set. Every capability starts as `Unknown`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a statement, replacing any previous one for `capability`.
    pub fn declare(&mut self, capability: Capability, support: Support, evidence: Evidence) {
        self.entries.insert(
            capability,
            CapabilityEntry {
                support,
                evidence,
                note: None,
            },
        );
    }

    /// Records a statement with an explanatory note.
    pub fn declare_with_note(
        &mut self,
        capability: Capability,
        support: Support,
        evidence: Evidence,
        note: impl Into<String>,
    ) {
        self.entries.insert(
            capability,
            CapabilityEntry {
                support,
                evidence,
                note: Some(note.into()),
            },
        );
    }

    /// Builder form of [`CapabilitySet::declare`].
    pub fn with(mut self, capability: Capability, support: Support, evidence: Evidence) -> Self {
        self.declare(capability, support, evidence);
        self
    }

    /// The full statement for `capability`, if any.
    pub fn entry(&self, capability: &Capability) -> Option<&CapabilityEntry> {
        self.entries.get(capability)
    }

    /// Support level for `capability`. Absent capabilities are `Unknown`.
    pub fn support(&self, capability: &Capability) -> Support {
        self.entries
            .get(capability)
            .map_or(Support::Unknown, |entry| entry.support)
    }

    /// Succeeds only when `capability` is `Supported`.
    pub fn require(&self, capability: &Capability) -> Result<(), CapabilityError> {
        match self.support(capability) {
            Support::Supported => Ok(()),
            actual => Err(CapabilityError {
                capability: capability.clone(),
                actual,
            }),
        }
    }

    /// Merges `observed` into `self`. An observed statement replaces the
    /// current one when its evidence rank is greater than or equal to the
    /// current rank. Lower-ranked observations are ignored.
    pub fn refine(&mut self, observed: &CapabilitySet) {
        for (capability, incoming) in &observed.entries {
            let replace = self
                .entries
                .get(capability)
                .is_none_or(|current| incoming.evidence.rank() >= current.evidence.rank());
            if replace {
                self.entries.insert(capability.clone(), incoming.clone());
            }
        }
    }

    /// Applies structural rules of `transport`. Non-PTY transports never carry
    /// the provider's native terminal UI, whatever the adapter declared.
    pub fn constrain_to_transport(&mut self, transport: TransportKind) {
        if !transport.carries_native_terminal_ui() {
            self.declare_with_note(
                Capability::NativeTerminalUi,
                Support::Unsupported,
                Evidence::Negotiated,
                format!("the {transport} transport does not carry the provider's terminal UI"),
            );
        }
    }

    /// Computes the effective set for one connection: start from `declared`,
    /// refine with `observed`, then apply the rules of `transport` last.
    pub fn negotiate(
        declared: &CapabilitySet,
        observed: &CapabilitySet,
        transport: TransportKind,
    ) -> CapabilitySet {
        let mut effective = declared.clone();
        effective.refine(observed);
        effective.constrain_to_transport(transport);
        effective
    }

    /// Iterates over explicit statements in a stable order.
    pub fn iter(&self) -> impl Iterator<Item = (&Capability, &CapabilityEntry)> {
        self.entries.iter()
    }

    /// Number of explicit statements.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the set has no explicit statements.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_capability_is_unknown_and_not_required() {
        let set = CapabilitySet::new();
        assert_eq!(set.support(&Capability::ResumeById), Support::Unknown);
        let err = set.require(&Capability::ResumeById).unwrap_err();
        assert_eq!(err.actual, Support::Unknown);
    }

    #[test]
    fn unsupported_capability_fails_require() {
        let set = CapabilitySet::new().with(
            Capability::ResumeById,
            Support::Unsupported,
            Evidence::Declared,
        );
        assert_eq!(
            set.require(&Capability::ResumeById).unwrap_err().actual,
            Support::Unsupported
        );
    }

    #[test]
    fn supported_capability_passes_require() {
        let set =
            CapabilitySet::new().with(Capability::ResumeById, Support::Supported, Evidence::Probed);
        assert!(set.require(&Capability::ResumeById).is_ok());
    }

    #[test]
    fn higher_evidence_overrides_declaration() {
        let declared = CapabilitySet::new().with(
            Capability::SessionReplay,
            Support::Supported,
            Evidence::Declared,
        );
        let negotiated = CapabilitySet::new().with(
            Capability::SessionReplay,
            Support::Unsupported,
            Evidence::Negotiated,
        );
        let mut effective = declared;
        effective.refine(&negotiated);
        assert_eq!(
            effective.support(&Capability::SessionReplay),
            Support::Unsupported
        );
    }

    #[test]
    fn lower_evidence_does_not_override() {
        let mut effective = CapabilitySet::new().with(
            Capability::Cancellation,
            Support::Unsupported,
            Evidence::Negotiated,
        );
        let declared = CapabilitySet::new().with(
            Capability::Cancellation,
            Support::Supported,
            Evidence::Declared,
        );
        effective.refine(&declared);
        assert_eq!(
            effective.support(&Capability::Cancellation),
            Support::Unsupported
        );
    }

    #[test]
    fn explicit_unknown_with_higher_evidence_downgrades() {
        let mut effective = CapabilitySet::new().with(
            Capability::LimitSignals,
            Support::Supported,
            Evidence::Declared,
        );
        let probed =
            CapabilitySet::new().with(Capability::LimitSignals, Support::Unknown, Evidence::Probed);
        effective.refine(&probed);
        assert_eq!(
            effective.support(&Capability::LimitSignals),
            Support::Unknown
        );
    }

    #[test]
    fn non_pty_transports_never_claim_native_ui() {
        let declared = CapabilitySet::new().with(
            Capability::NativeTerminalUi,
            Support::Supported,
            Evidence::Declared,
        );
        let observed = CapabilitySet::new().with(
            Capability::NativeTerminalUi,
            Support::Supported,
            Evidence::Negotiated,
        );
        for transport in [TransportKind::Acp, TransportKind::CustomCli] {
            let effective = CapabilitySet::negotiate(&declared, &observed, transport);
            assert_eq!(
                effective.support(&Capability::NativeTerminalUi),
                Support::Unsupported,
                "{transport} must not claim native UI"
            );
        }
        let pty = CapabilitySet::negotiate(&declared, &observed, TransportKind::NativePty);
        assert_eq!(
            pty.support(&Capability::NativeTerminalUi),
            Support::Supported
        );
    }

    #[test]
    fn custom_capability_requires_namespace() {
        assert!(CustomCapability::new("vendor.feature").is_ok());
        assert!(CustomCapability::new("vendor.sub-feature_2").is_ok());
        for bad in [
            "", "feature", ".vendor", "vendor.", "a..b", "Vendor.x", "a b.c",
        ] {
            assert!(
                CustomCapability::new(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn custom_capabilities_are_tracked_like_builtins() {
        let cap = Capability::Custom(CustomCapability::new("vendor.voice").unwrap());
        let set = CapabilitySet::new().with(cap.clone(), Support::Supported, Evidence::Declared);
        assert_eq!(set.support(&cap), Support::Supported);
        assert_eq!(cap.as_str(), "vendor.voice");
    }

    #[test]
    fn builtin_names_are_unique() {
        let all = [
            Capability::NativeTerminalUi,
            Capability::StructuredEvents,
            Capability::ResumeById,
            Capability::ResumeLatest,
            Capability::PreassignSessionId,
            Capability::SessionReplay,
            Capability::SessionList,
            Capability::Cancellation,
            Capability::PermissionRequests,
            Capability::ApprovalModeControl,
            Capability::FilesystemSandboxControl,
            Capability::NetworkSandboxControl,
            Capability::UsageReporting,
            Capability::LimitSignals,
            Capability::ModelSelection,
            Capability::InitialPrompt,
            Capability::SessionCapture,
            Capability::HandoffCapture,
        ];
        let mut names: Vec<&str> = all.iter().map(Capability::as_str).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), all.len());
        // Builtin names never contain '.', so they cannot collide with custom ones.
        assert!(names.iter().all(|n| !n.contains('.')));
    }
}
