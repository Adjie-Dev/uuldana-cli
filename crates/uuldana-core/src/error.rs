//! Error classification shared by adapters, transports and the supervisor.
//!
//! [`ErrorClass`] says *what* went wrong in provider-neutral terms.
//! [`Disposition`] says what the supervisor may do about it. Dispositions are
//! advice; the orchestrator in `uuldana-runtime` is what acts on them.

use std::fmt;

use crate::capability::Capability;
use crate::event::LimitKind;

/// Provider-neutral error category.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorClass {
    /// The provider executable was not found.
    NotInstalled,
    /// The installed provider version is not supported by the adapter.
    IncompatibleVersion,
    /// The provider needs the user to sign in.
    AuthenticationRequired,
    /// A usage, rate, context or budget limit was hit.
    Limit(LimitKind),
    /// A temporary failure such as a network hiccup.
    Transient,
    /// The provider broke the wire protocol.
    ProtocolViolation,
    /// The requested operation needs a capability that is not supported.
    Unsupported,
    /// The provider or the user denied a permission.
    PermissionDenied,
    /// A Uuldana policy check refused the operation (for example escalation).
    PolicyViolation,
    /// The provider process crashed or exited unexpectedly.
    ProcessFailed,
    /// The operation was cancelled.
    Cancelled,
    /// Local configuration is invalid or incomplete.
    Configuration,
    /// A bug in Uuldana or an adapter.
    Internal,
}

/// What the supervisor may do after an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Disposition {
    /// Stop. Retrying or switching provider will not help.
    Fatal,
    /// A human must act (sign in, install, change settings, raise a budget).
    UserActionRequired,
    /// Retrying the same provider later may succeed.
    RetrySameProvider,
    /// Continuing on another provider is allowed, within the same permission
    /// ceiling. The orchestrator acts on it.
    FailoverEligible,
}

impl ErrorClass {
    /// Stable snake_case name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotInstalled => "not_installed",
            Self::IncompatibleVersion => "incompatible_version",
            Self::AuthenticationRequired => "authentication_required",
            Self::Limit(_) => "limit",
            Self::Transient => "transient",
            Self::ProtocolViolation => "protocol_violation",
            Self::Unsupported => "unsupported",
            Self::PermissionDenied => "permission_denied",
            Self::PolicyViolation => "policy_violation",
            Self::ProcessFailed => "process_failed",
            Self::Cancelled => "cancelled",
            Self::Configuration => "configuration",
            Self::Internal => "internal",
        }
    }

    /// Default disposition for this class.
    ///
    /// Only an exhausted usage quota is failover eligible. Permission and
    /// policy denials, cancellations and user-set budgets are never, because
    /// switching provider must not be a way around a decision.
    pub const fn disposition(self) -> Disposition {
        match self {
            Self::Limit(LimitKind::UsageQuota) => Disposition::FailoverEligible,
            Self::Limit(LimitKind::RateLimit) | Self::Transient => Disposition::RetrySameProvider,
            Self::Limit(_)
            | Self::NotInstalled
            | Self::IncompatibleVersion
            | Self::AuthenticationRequired
            | Self::PermissionDenied
            | Self::ProcessFailed
            | Self::Configuration => Disposition::UserActionRequired,
            Self::ProtocolViolation
            | Self::Unsupported
            | Self::PolicyViolation
            | Self::Cancelled
            | Self::Internal => Disposition::Fatal,
        }
    }
}

impl fmt::Display for ErrorClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Limit(kind) => write!(f, "limit({})", kind.as_str()),
            other => f.write_str(other.as_str()),
        }
    }
}

/// Error returned by provider adapters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterError {
    class: ErrorClass,
    message: String,
    capability: Option<Capability>,
}

impl AdapterError {
    /// Creates an error of `class` with a message.
    pub fn new(class: ErrorClass, message: impl Into<String>) -> Self {
        Self {
            class,
            message: message.into(),
            capability: None,
        }
    }

    /// The operation needs `capability`, which this adapter does not support.
    pub fn unsupported(capability: Capability) -> Self {
        Self {
            class: ErrorClass::Unsupported,
            message: format!("capability `{capability}` is not supported by this adapter"),
            capability: Some(capability),
        }
    }

    /// Attaches the capability involved.
    pub fn with_capability(mut self, capability: Capability) -> Self {
        self.capability = Some(capability);
        self
    }

    /// Error class.
    pub fn class(&self) -> ErrorClass {
        self.class
    }

    /// Human readable message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The capability involved, if any.
    pub fn capability(&self) -> Option<&Capability> {
        self.capability.as_ref()
    }

    /// Shortcut for `self.class().disposition()`.
    pub fn disposition(&self) -> Disposition {
        self.class.disposition()
    }
}

impl fmt::Display for AdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.class, self.message)
    }
}

impl std::error::Error for AdapterError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_usage_quota_is_failover_eligible() {
        let all = [
            ErrorClass::NotInstalled,
            ErrorClass::IncompatibleVersion,
            ErrorClass::AuthenticationRequired,
            ErrorClass::Limit(LimitKind::UsageQuota),
            ErrorClass::Limit(LimitKind::RateLimit),
            ErrorClass::Limit(LimitKind::ContextWindow),
            ErrorClass::Limit(LimitKind::Budget),
            ErrorClass::Limit(LimitKind::Unknown),
            ErrorClass::Transient,
            ErrorClass::ProtocolViolation,
            ErrorClass::Unsupported,
            ErrorClass::PermissionDenied,
            ErrorClass::PolicyViolation,
            ErrorClass::ProcessFailed,
            ErrorClass::Cancelled,
            ErrorClass::Configuration,
            ErrorClass::Internal,
        ];
        let eligible: Vec<_> = all
            .into_iter()
            .filter(|c| c.disposition() == Disposition::FailoverEligible)
            .collect();
        assert_eq!(eligible, vec![ErrorClass::Limit(LimitKind::UsageQuota)]);
    }

    #[test]
    fn denials_and_budgets_never_fail_over() {
        for class in [
            ErrorClass::PermissionDenied,
            ErrorClass::PolicyViolation,
            ErrorClass::Cancelled,
            ErrorClass::Limit(LimitKind::Budget),
        ] {
            assert_ne!(
                class.disposition(),
                Disposition::FailoverEligible,
                "{class}"
            );
        }
    }

    #[test]
    fn rate_limits_and_transient_errors_retry_same_provider() {
        assert_eq!(
            ErrorClass::Limit(LimitKind::RateLimit).disposition(),
            Disposition::RetrySameProvider
        );
        assert_eq!(
            ErrorClass::Transient.disposition(),
            Disposition::RetrySameProvider
        );
    }

    #[test]
    fn unsupported_error_names_capability() {
        let err = AdapterError::unsupported(Capability::ResumeById);
        assert_eq!(err.class(), ErrorClass::Unsupported);
        assert_eq!(err.capability(), Some(&Capability::ResumeById));
        assert_eq!(err.disposition(), Disposition::Fatal);
        assert!(err.to_string().contains("resume_by_id"));
    }

    #[test]
    fn display_includes_limit_kind() {
        assert_eq!(
            ErrorClass::Limit(LimitKind::UsageQuota).to_string(),
            "limit(usage_quota)"
        );
    }
}
