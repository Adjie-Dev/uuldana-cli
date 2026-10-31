//! Validated identifier newtypes.
//!
//! Identifiers owned by Uuldana ([`SessionId`], [`RunId`]) and identifiers
//! owned by a provider ([`NativeSessionId`], [`ToolCallId`],
//! [`PermissionRequestId`]) are opaque strings. Only [`ProviderId`] has a
//! restricted alphabet because it is used in configuration, logs and paths.

use std::fmt;
use std::str::FromStr;

/// Error returned when an identifier fails validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdError {
    kind: &'static str,
    reason: &'static str,
}

impl IdError {
    const fn new(kind: &'static str, reason: &'static str) -> Self {
        Self { kind, reason }
    }

    /// Name of the identifier type that failed validation.
    pub fn kind(&self) -> &'static str {
        self.kind
    }

    /// Human readable reason for the failure.
    pub fn reason(&self) -> &'static str {
        self.reason
    }
}

impl fmt::Display for IdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid {}: {}", self.kind, self.reason)
    }
}

impl std::error::Error for IdError {}

/// Stable identifier of a provider adapter, for example `example-agent`.
///
/// Rules: 1 to 64 characters, lowercase ASCII letters, digits, `-`, `_` or
/// `.`, starting with a letter or digit, and never containing `..`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProviderId(String);

impl ProviderId {
    /// Maximum length in bytes.
    pub const MAX_LEN: usize = 64;

    /// Validates and creates a provider identifier.
    pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
        const KIND: &str = "provider id";
        let value = value.into();
        if value.is_empty() {
            return Err(IdError::new(KIND, "must not be empty"));
        }
        if value.len() > Self::MAX_LEN {
            return Err(IdError::new(KIND, "must be at most 64 bytes"));
        }
        let first = value.chars().next().unwrap_or_default();
        if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
            return Err(IdError::new(
                KIND,
                "must start with a lowercase letter or digit",
            ));
        }
        if !value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
        {
            return Err(IdError::new(
                KIND,
                "may only contain lowercase ASCII letters, digits, '-', '_' or '.'",
            ));
        }
        if value.contains("..") {
            return Err(IdError::new(KIND, "must not contain '..'"));
        }
        Ok(Self(value))
    }

    /// Returns the identifier as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for ProviderId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl FromStr for ProviderId {
    type Err = IdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

/// Maximum length in bytes of an opaque identifier.
pub const OPAQUE_ID_MAX_LEN: usize = 512;

fn validate_opaque(kind: &'static str, value: &str) -> Result<(), IdError> {
    if value.is_empty() {
        return Err(IdError::new(kind, "must not be empty"));
    }
    if value.len() > OPAQUE_ID_MAX_LEN {
        return Err(IdError::new(kind, "must be at most 512 bytes"));
    }
    if value.chars().any(char::is_control) {
        return Err(IdError::new(kind, "must not contain control characters"));
    }
    Ok(())
}

macro_rules! opaque_id {
    ($(#[$meta:meta])* $name:ident, $kind:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);

        impl $name {
            /// Validates and creates the identifier.
            ///
            /// The value must be non-empty, at most [`OPAQUE_ID_MAX_LEN`]
            /// bytes, and free of control characters.
            pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
                let value = value.into();
                validate_opaque($kind, &value)?;
                Ok(Self(value))
            }

            /// Returns the identifier as a string slice.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl FromStr for $name {
            type Err = IdError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::new(s)
            }
        }
    };
}

opaque_id!(
    /// Uuldana-owned identifier of a supervised session (the unified history
    /// record that may span several provider runs).
    SessionId,
    "session id"
);

opaque_id!(
    /// Uuldana-owned identifier of one provider run inside a session.
    RunId,
    "run id"
);

opaque_id!(
    /// Provider-owned session identifier. Uuldana stores it as an opaque
    /// reference and never interprets or rewrites the native state behind it.
    NativeSessionId,
    "native session id"
);

opaque_id!(
    /// Identifier of a tool call, as reported by the provider.
    ToolCallId,
    "tool call id"
);

opaque_id!(
    /// Identifier of a permission request raised by a provider.
    PermissionRequestId,
    "permission request id"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_id_accepts_simple_names() {
        for ok in ["example-agent", "a", "agent2", "vendor.cli", "x_y", "0day"] {
            assert!(ProviderId::new(ok).is_ok(), "{ok} should be valid");
        }
    }

    #[test]
    fn provider_id_rejects_invalid_names() {
        let too_long = "a".repeat(ProviderId::MAX_LEN + 1);
        for bad in [
            "",
            "Upper",
            "with space",
            "-leading",
            ".hidden",
            "a..b",
            "a/b",
            "a\\b",
            "emoji\u{1F600}",
            too_long.as_str(),
        ] {
            assert!(ProviderId::new(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn provider_id_round_trips_through_display_and_from_str() {
        let id: ProviderId = "example-agent".parse().unwrap();
        assert_eq!(id.to_string(), "example-agent");
        assert_eq!(id.as_str(), "example-agent");
    }

    #[test]
    fn opaque_ids_accept_provider_formats() {
        assert!(NativeSessionId::new("550e8400-e29b-41d4-a716-446655440000").is_ok());
        assert!(NativeSessionId::new("auth refactor").is_ok());
        assert!(SessionId::new("s-1").is_ok());
        assert!(ToolCallId::new("call_001").is_ok());
    }

    #[test]
    fn opaque_ids_reject_empty_control_and_oversized_values() {
        assert!(RunId::new("").is_err());
        assert!(NativeSessionId::new("line\nbreak").is_err());
        assert!(PermissionRequestId::new("nul\0").is_err());
        assert!(SessionId::new("x".repeat(OPAQUE_ID_MAX_LEN + 1)).is_err());
    }

    #[test]
    fn id_error_reports_kind_and_reason() {
        let err = RunId::new("").unwrap_err();
        assert_eq!(err.kind(), "run id");
        assert_eq!(err.to_string(), "invalid run id: must not be empty");
    }
}
