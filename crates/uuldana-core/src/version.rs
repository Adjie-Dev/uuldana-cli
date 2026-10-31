//! Contract versioning for provider adapters.
//!
//! The contract version describes the shape of the public traits and types in
//! this crate. It is independent of the crate's semver version so that
//! adapters built out of tree can be checked for compatibility explicitly.

use std::fmt;

/// Version of the provider contract implemented by this build of `uuldana-core`.
///
/// Bump rules are documented in `docs/adr/0007-crate-layout-and-contract-versioning.md`.
pub const CONTRACT_VERSION: ContractVersion = ContractVersion::new(0, 2);

/// A `major.minor` contract version.
///
/// Compatibility rules:
/// - Majors must match.
/// - While the major is `0`, every minor is breaking, so minors must match exactly.
/// - From major `1` on, an adapter built against an older minor is accepted by
///   a newer host, never the other way around.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContractVersion {
    /// Major component. A change here is always breaking.
    pub major: u16,
    /// Minor component. Breaking while `major == 0`, additive afterwards.
    pub minor: u16,
}

impl ContractVersion {
    /// Creates a contract version.
    pub const fn new(major: u16, minor: u16) -> Self {
        Self { major, minor }
    }

    /// Returns `true` when an adapter declaring `self` can be hosted by `host`.
    pub const fn is_compatible_with(self, host: ContractVersion) -> bool {
        if self.major != host.major {
            return false;
        }
        if self.major == 0 {
            self.minor == host.minor
        } else {
            self.minor <= host.minor
        }
    }
}

impl fmt::Display for ContractVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_major_requires_exact_minor() {
        let host = ContractVersion::new(0, 2);
        assert!(ContractVersion::new(0, 2).is_compatible_with(host));
        assert!(!ContractVersion::new(0, 1).is_compatible_with(host));
        assert!(!ContractVersion::new(0, 3).is_compatible_with(host));
    }

    #[test]
    fn stable_major_accepts_older_minor_only() {
        let host = ContractVersion::new(1, 4);
        assert!(ContractVersion::new(1, 0).is_compatible_with(host));
        assert!(ContractVersion::new(1, 4).is_compatible_with(host));
        assert!(!ContractVersion::new(1, 5).is_compatible_with(host));
    }

    #[test]
    fn different_major_is_incompatible() {
        assert!(!ContractVersion::new(2, 0).is_compatible_with(ContractVersion::new(1, 9)));
        assert!(!ContractVersion::new(1, 0).is_compatible_with(ContractVersion::new(0, 0)));
    }

    #[test]
    fn current_contract_is_self_compatible() {
        assert!(CONTRACT_VERSION.is_compatible_with(CONTRACT_VERSION));
        assert_eq!(CONTRACT_VERSION.to_string(), "0.2");
    }
}
