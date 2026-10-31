//! Permission policy contracts.
//!
//! Two independent concerns are modeled separately:
//!
//! - **Approval**: when the provider asks a human before acting
//!   ([`ApprovalMode`]).
//! - **Sandbox**: what the provider is technically allowed to touch
//!   ([`SandboxPolicy`], split into filesystem and network).
//!
//! Every dimension is ordered from most restrictive to most permissive.
//! A policy may only ever move towards *more* restrictive values when it is
//! handed to an adapter or carried into a new run. There is no escalation
//! path.

use std::fmt;

/// When the provider asks a human before acting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ApprovalMode {
    /// Ask before every action that is not read-only. Most restrictive.
    AskEveryAction,
    /// Ask for actions the provider classifies as risky.
    AskForRisky,
    /// Never ask. Actions run within the sandbox without confirmation.
    NeverAsk,
}

impl ApprovalMode {
    const fn rank(self) -> u8 {
        match self {
            Self::AskEveryAction => 0,
            Self::AskForRisky => 1,
            Self::NeverAsk => 2,
        }
    }

    /// Whether `self` is at most as permissive as `other`.
    pub const fn is_at_most(self, other: Self) -> bool {
        self.rank() <= other.rank()
    }
}

/// What the provider may do on the filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FilesystemAccess {
    /// Read only. Most restrictive.
    ReadOnly,
    /// Write inside the session workspace only.
    WorkspaceWrite,
    /// No filesystem restriction.
    Unrestricted,
}

impl FilesystemAccess {
    const fn rank(self) -> u8 {
        match self {
            Self::ReadOnly => 0,
            Self::WorkspaceWrite => 1,
            Self::Unrestricted => 2,
        }
    }

    /// Whether `self` is at most as permissive as `other`.
    pub const fn is_at_most(self, other: Self) -> bool {
        self.rank() <= other.rank()
    }
}

/// Whether the provider's tools may use the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NetworkAccess {
    /// No network access for tools. Most restrictive.
    Denied,
    /// Network access allowed.
    Allowed,
}

impl NetworkAccess {
    const fn rank(self) -> u8 {
        match self {
            Self::Denied => 0,
            Self::Allowed => 1,
        }
    }

    /// Whether `self` is at most as permissive as `other`.
    pub const fn is_at_most(self, other: Self) -> bool {
        self.rank() <= other.rank()
    }
}

/// Technical restrictions on the provider's tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SandboxPolicy {
    /// Filesystem restriction.
    pub filesystem: FilesystemAccess,
    /// Network restriction.
    pub network: NetworkAccess,
}

/// One dimension of a [`PermissionPolicy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PermissionDimension {
    /// [`ApprovalMode`].
    Approval,
    /// [`FilesystemAccess`].
    Filesystem,
    /// [`NetworkAccess`].
    Network,
}

impl PermissionDimension {
    /// Stable snake_case name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Approval => "approval",
            Self::Filesystem => "filesystem",
            Self::Network => "network",
        }
    }
}

impl fmt::Display for PermissionDimension {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Approval behavior plus sandbox restrictions.
///
/// The default is the most restrictive policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PermissionPolicy {
    /// When to ask a human.
    pub approval: ApprovalMode,
    /// What tools may touch.
    pub sandbox: SandboxPolicy,
}

impl PermissionPolicy {
    /// Ask for everything, read-only filesystem, no network.
    pub const fn most_restrictive() -> Self {
        Self {
            approval: ApprovalMode::AskEveryAction,
            sandbox: SandboxPolicy {
                filesystem: FilesystemAccess::ReadOnly,
                network: NetworkAccess::Denied,
            },
        }
    }

    /// Never ask, no filesystem or network restriction.
    pub const fn unrestricted() -> Self {
        Self {
            approval: ApprovalMode::NeverAsk,
            sandbox: SandboxPolicy {
                filesystem: FilesystemAccess::Unrestricted,
                network: NetworkAccess::Allowed,
            },
        }
    }

    /// Dimensions in which `self` is more permissive than `ceiling`.
    pub fn escalations_over(&self, ceiling: &PermissionPolicy) -> Vec<PermissionDimension> {
        let mut out = Vec::new();
        if !self.approval.is_at_most(ceiling.approval) {
            out.push(PermissionDimension::Approval);
        }
        if !self
            .sandbox
            .filesystem
            .is_at_most(ceiling.sandbox.filesystem)
        {
            out.push(PermissionDimension::Filesystem);
        }
        if !self.sandbox.network.is_at_most(ceiling.sandbox.network) {
            out.push(PermissionDimension::Network);
        }
        out
    }

    /// Whether `self` is at most as permissive as `ceiling` in every dimension.
    pub fn is_within(&self, ceiling: &PermissionPolicy) -> bool {
        self.escalations_over(ceiling).is_empty()
    }
}

impl Default for PermissionPolicy {
    fn default() -> Self {
        Self::most_restrictive()
    }
}

/// An adapter's honest report of what a requested policy becomes on its provider.
///
/// If the provider cannot enforce a dimension (for example it has no network
/// sandbox), `effective` must state the permissive value that will really
/// apply. Validation then rejects the launch instead of silently running
/// with more permission than the user asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionMapping {
    /// The policy that will actually apply.
    pub effective: PermissionPolicy,
    /// Human readable notes, for example which native mode was chosen.
    pub notes: Vec<String>,
}

impl PermissionMapping {
    /// A mapping whose effective policy is `effective`.
    pub fn new(effective: PermissionPolicy) -> Self {
        Self {
            effective,
            notes: Vec::new(),
        }
    }

    /// Adds a note.
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }
}

/// A permission pair that passed [`validate_mapping`].
///
/// Fields are private, so the only way to obtain a value is through
/// validation. Launch requests carry this type, which means an adapter can
/// never receive a request whose effective policy exceeds what was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedPermissions {
    requested: PermissionPolicy,
    effective: PermissionPolicy,
    notes: Vec<String>,
}

impl ValidatedPermissions {
    /// The policy that was requested.
    pub fn requested(&self) -> &PermissionPolicy {
        &self.requested
    }

    /// The policy that will apply. Never more permissive than `requested`.
    pub fn effective(&self) -> &PermissionPolicy {
        &self.effective
    }

    /// Notes copied from the adapter mapping.
    pub fn notes(&self) -> &[String] {
        &self.notes
    }
}

/// Checks that `mapping` does not grant more than `requested`.
pub fn validate_mapping(
    requested: &PermissionPolicy,
    mapping: &PermissionMapping,
) -> Result<ValidatedPermissions, PermissionError> {
    let dimensions = mapping.effective.escalations_over(requested);
    if !dimensions.is_empty() {
        return Err(PermissionError::WouldEscalate { dimensions });
    }
    Ok(ValidatedPermissions {
        requested: *requested,
        effective: mapping.effective,
        notes: mapping.notes.clone(),
    })
}

/// Answer to a single permission request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PermissionDecision {
    /// Allow this one action.
    AllowOnce,
    /// Allow this kind of action for the rest of the provider session.
    AllowAlways,
    /// Reject this one action.
    RejectOnce,
    /// Reject this kind of action for the rest of the provider session.
    RejectAlways,
    /// The request was withdrawn, for example because the turn was cancelled.
    Cancelled,
}

impl PermissionDecision {
    /// Whether the decision allows the action.
    pub const fn is_allow(self) -> bool {
        matches!(self, Self::AllowOnce | Self::AllowAlways)
    }
}

/// Who made a permission decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DecisionSource {
    /// A human answered.
    User,
    /// An automatic rule answered.
    Policy,
    /// The request was cancelled with its turn.
    Cancellation,
}

/// A permission decision together with its source.
///
/// Automatic rules may only reject or cancel. Only a human can allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PermissionResolution {
    decision: PermissionDecision,
    source: DecisionSource,
}

impl PermissionResolution {
    /// A decision made by a human.
    pub const fn by_user(decision: PermissionDecision) -> Self {
        Self {
            decision,
            source: DecisionSource::User,
        }
    }

    /// A decision made by an automatic rule. Allowing is refused.
    pub fn by_policy(decision: PermissionDecision) -> Result<Self, PermissionError> {
        if decision.is_allow() {
            return Err(PermissionError::AutomaticAllowForbidden { decision });
        }
        Ok(Self {
            decision,
            source: DecisionSource::Policy,
        })
    }

    /// The request was cancelled together with its turn.
    pub const fn cancelled() -> Self {
        Self {
            decision: PermissionDecision::Cancelled,
            source: DecisionSource::Cancellation,
        }
    }

    /// The decision.
    pub const fn decision(&self) -> PermissionDecision {
        self.decision
    }

    /// Who decided.
    pub const fn source(&self) -> DecisionSource {
        self.source
    }
}

/// Permission contract violations.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionError {
    /// An adapter mapping would grant more than was requested.
    WouldEscalate {
        /// Dimensions that would be more permissive.
        dimensions: Vec<PermissionDimension>,
    },
    /// A run requested more than the session's permission ceiling.
    ExceedsCeiling {
        /// Dimensions above the ceiling.
        dimensions: Vec<PermissionDimension>,
    },
    /// An automatic rule tried to allow an action.
    AutomaticAllowForbidden {
        /// The rejected decision.
        decision: PermissionDecision,
    },
}

fn join_dimensions(dimensions: &[PermissionDimension]) -> String {
    dimensions
        .iter()
        .map(|d| d.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

impl fmt::Display for PermissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WouldEscalate { dimensions } => write!(
                f,
                "provider mapping would escalate permissions in: {}",
                join_dimensions(dimensions)
            ),
            Self::ExceedsCeiling { dimensions } => write!(
                f,
                "requested permissions exceed the session ceiling in: {}",
                join_dimensions(dimensions)
            ),
            Self::AutomaticAllowForbidden { decision } => write!(
                f,
                "automatic policy may not allow actions (attempted {decision:?})"
            ),
        }
    }
}

impl std::error::Error for PermissionError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(a: ApprovalMode, fs: FilesystemAccess, net: NetworkAccess) -> PermissionPolicy {
        PermissionPolicy {
            approval: a,
            sandbox: SandboxPolicy {
                filesystem: fs,
                network: net,
            },
        }
    }

    #[test]
    fn default_is_most_restrictive() {
        assert_eq!(
            PermissionPolicy::default(),
            PermissionPolicy::most_restrictive()
        );
        assert!(PermissionPolicy::most_restrictive().is_within(&PermissionPolicy::unrestricted()));
        assert!(!PermissionPolicy::unrestricted().is_within(&PermissionPolicy::most_restrictive()));
    }

    #[test]
    fn equal_policy_is_within_itself() {
        let p = policy(
            ApprovalMode::AskForRisky,
            FilesystemAccess::WorkspaceWrite,
            NetworkAccess::Denied,
        );
        assert!(p.is_within(&p));
    }

    #[test]
    fn each_dimension_escalation_is_detected() {
        let ceiling = policy(
            ApprovalMode::AskForRisky,
            FilesystemAccess::WorkspaceWrite,
            NetworkAccess::Denied,
        );
        let cases = [
            (
                policy(
                    ApprovalMode::NeverAsk,
                    FilesystemAccess::WorkspaceWrite,
                    NetworkAccess::Denied,
                ),
                PermissionDimension::Approval,
            ),
            (
                policy(
                    ApprovalMode::AskForRisky,
                    FilesystemAccess::Unrestricted,
                    NetworkAccess::Denied,
                ),
                PermissionDimension::Filesystem,
            ),
            (
                policy(
                    ApprovalMode::AskForRisky,
                    FilesystemAccess::WorkspaceWrite,
                    NetworkAccess::Allowed,
                ),
                PermissionDimension::Network,
            ),
        ];
        for (candidate, dimension) in cases {
            assert_eq!(candidate.escalations_over(&ceiling), vec![dimension]);
        }
    }

    #[test]
    fn stricter_in_one_dimension_does_not_compensate_for_another() {
        let ceiling = policy(
            ApprovalMode::AskForRisky,
            FilesystemAccess::WorkspaceWrite,
            NetworkAccess::Denied,
        );
        let mixed = policy(
            ApprovalMode::AskEveryAction,
            FilesystemAccess::ReadOnly,
            NetworkAccess::Allowed,
        );
        assert!(!mixed.is_within(&ceiling));
    }

    #[test]
    fn exact_and_stricter_mappings_validate() {
        let requested = policy(
            ApprovalMode::AskForRisky,
            FilesystemAccess::WorkspaceWrite,
            NetworkAccess::Allowed,
        );
        let exact = validate_mapping(&requested, &PermissionMapping::new(requested)).unwrap();
        assert_eq!(exact.effective(), &requested);

        let stricter = PermissionMapping::new(policy(
            ApprovalMode::AskEveryAction,
            FilesystemAccess::ReadOnly,
            NetworkAccess::Denied,
        ))
        .with_note("provider has no workspace-write mode; using read-only");
        let validated = validate_mapping(&requested, &stricter).unwrap();
        assert_eq!(validated.requested(), &requested);
        assert_eq!(validated.notes().len(), 1);
    }

    #[test]
    fn unenforceable_sandbox_is_rejected() {
        // The user asked for no network, but the provider cannot enforce it,
        // so the honest effective value is `Allowed`.
        let requested = policy(
            ApprovalMode::AskEveryAction,
            FilesystemAccess::ReadOnly,
            NetworkAccess::Denied,
        );
        let honest = PermissionMapping::new(policy(
            ApprovalMode::AskEveryAction,
            FilesystemAccess::ReadOnly,
            NetworkAccess::Allowed,
        ));
        assert_eq!(
            validate_mapping(&requested, &honest),
            Err(PermissionError::WouldEscalate {
                dimensions: vec![PermissionDimension::Network]
            })
        );
    }

    #[test]
    fn policy_cannot_auto_allow() {
        for decision in [
            PermissionDecision::AllowOnce,
            PermissionDecision::AllowAlways,
        ] {
            assert_eq!(
                PermissionResolution::by_policy(decision),
                Err(PermissionError::AutomaticAllowForbidden { decision })
            );
        }
        for decision in [
            PermissionDecision::RejectOnce,
            PermissionDecision::RejectAlways,
            PermissionDecision::Cancelled,
        ] {
            let resolution = PermissionResolution::by_policy(decision).unwrap();
            assert_eq!(resolution.source(), DecisionSource::Policy);
        }
    }

    #[test]
    fn user_may_allow_and_cancellation_is_tagged() {
        let allowed = PermissionResolution::by_user(PermissionDecision::AllowOnce);
        assert!(allowed.decision().is_allow());
        assert_eq!(allowed.source(), DecisionSource::User);
        let cancelled = PermissionResolution::cancelled();
        assert_eq!(cancelled.decision(), PermissionDecision::Cancelled);
        assert_eq!(cancelled.source(), DecisionSource::Cancellation);
    }

    #[test]
    fn error_messages_name_dimensions() {
        let err = PermissionError::ExceedsCeiling {
            dimensions: vec![
                PermissionDimension::Filesystem,
                PermissionDimension::Network,
            ],
        };
        assert_eq!(
            err.to_string(),
            "requested permissions exceed the session ceiling in: filesystem, network"
        );
    }
}
