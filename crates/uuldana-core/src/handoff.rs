//! Types reserved for cross-provider context handoff.
//!
//! This crate defines the shape only. Building and injecting handoff packages
//! belongs to the runtime. The key rule is encoded here: a package
//! always lists what it does **not** carry, so no consumer can assume hidden
//! reasoning or native provider state was transferred.
//!
//! Native session capture is best effort: provider session stores are
//! provider-owned, usually undocumented, may change between versions, and
//! ids may rotate during a run. A capture may legitimately find nothing.

use std::path::PathBuf;

use crate::capability::Support;
use crate::event::{Fidelity, Role};
use crate::ids::NativeSessionId;
use crate::session::{NativeSessionRef, RunContext};

/// Information that never crosses a provider boundary.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NonTransferable {
    /// Reasoning the provider did not display.
    HiddenReasoning,
    /// Provider memory, caches and learned preferences.
    NativeMemory,
    /// Approvals granted inside the provider (for example "allow always").
    ProviderApprovals,
    /// Any other undocumented provider-internal state.
    ProviderInternalState,
    /// Tool calls that were still running when the handoff was prepared.
    InFlightToolCalls,
}

impl NonTransferable {
    /// Every known non-transferable category.
    pub const ALL: [NonTransferable; 5] = [
        Self::HiddenReasoning,
        Self::NativeMemory,
        Self::ProviderApprovals,
        Self::ProviderInternalState,
        Self::InFlightToolCalls,
    ];
}

/// One visible transcript entry carried in a handoff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptEntry {
    /// Author.
    pub role: Role,
    /// Visible text.
    pub text: String,
    /// Fidelity of the source event.
    pub fidelity: Fidelity,
}

/// Context that may be offered to the next provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffPackage {
    /// Absolute workspace path.
    pub workspace: PathBuf,
    /// The user's objective, if known.
    pub objective: Option<String>,
    /// Visible transcript excerpt.
    pub transcript: Vec<TranscriptEntry>,
    /// Native session the context was taken from, if known.
    pub source: Option<NativeSessionRef>,
    not_transferred: Vec<NonTransferable>,
}

impl HandoffPackage {
    /// Creates an empty package. The non-transferable list is always the full
    /// [`NonTransferable::ALL`] set and cannot be cleared.
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            objective: None,
            transcript: Vec::new(),
            source: None,
            not_transferred: NonTransferable::ALL.to_vec(),
        }
    }

    /// Creates an empty package bound to the workspace and current native
    /// session of `context`.
    pub fn for_run(context: &RunContext<'_>) -> Self {
        let mut package = Self::new(context.workspace().to_path_buf());
        package.source = context.native_session();
        package
    }

    /// What this package does not carry.
    pub fn not_transferred(&self) -> &[NonTransferable] {
        &self.not_transferred
    }
}

/// What an adapter could learn about a finished or running native session.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCapture {
    /// Native session id, if found.
    pub native: Option<NativeSessionId>,
    /// Whether the native session can be resumed.
    pub resumable: Support,
    /// Native session `native` was derived from (fork or branch), if the
    /// provider records it.
    pub parent: Option<NativeSessionId>,
}

impl SessionCapture {
    /// Creates a capture result without a parent.
    pub fn new(native: Option<NativeSessionId>, resumable: Support) -> Self {
        Self {
            native,
            resumable,
            parent: None,
        }
    }

    /// Sets the parent native session.
    pub fn with_parent(mut self, parent: NativeSessionId) -> Self {
        self.parent = Some(parent);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_always_declares_non_transferable_state() {
        let package = HandoffPackage::new(std::env::temp_dir());
        assert_eq!(package.not_transferred(), &NonTransferable::ALL);
        assert!(
            package
                .not_transferred()
                .contains(&NonTransferable::HiddenReasoning)
        );
    }

    #[test]
    fn session_capture_defaults_are_explicit() {
        let capture = SessionCapture::new(None, Support::Unknown);
        assert_eq!(capture.resumable, Support::Unknown);
        assert!(capture.native.is_none());
        assert!(capture.parent.is_none());
        let forked = SessionCapture::new(
            Some(NativeSessionId::new("child").unwrap()),
            Support::Supported,
        )
        .with_parent(NativeSessionId::new("parent").unwrap());
        assert_eq!(forked.parent.unwrap().as_str(), "parent");
    }

    #[test]
    fn package_for_run_is_bound_to_its_workspace_and_native_session() {
        use crate::ids::{ProviderId, RunId, SessionId};
        use crate::permission::{PermissionMapping, PermissionPolicy, validate_mapping};
        use crate::session::{NewRun, Session};
        use crate::time::Timestamp;
        use crate::transport::TransportKind;

        let policy = PermissionPolicy::default();
        let run = RunId::new("r").unwrap();
        let package_in = |dir: &str, native: Option<&str>| {
            let mut session = Session::new(
                SessionId::new("s").unwrap(),
                std::env::temp_dir().join(dir),
                policy,
                Timestamp::from_unix_millis(0),
            )
            .unwrap();
            session
                .begin_run(NewRun {
                    id: run.clone(),
                    provider: ProviderId::new("example-agent").unwrap(),
                    transport: TransportKind::NativePty,
                    permissions: validate_mapping(&policy, &PermissionMapping::new(policy))
                        .unwrap(),
                    at: Timestamp::from_unix_millis(1),
                })
                .unwrap();
            if let Some(native) = native {
                session
                    .record_native_session(&run, NativeSessionId::new(native).unwrap())
                    .unwrap();
            }
            HandoffPackage::for_run(&session.run_context(&run).unwrap())
        };
        let a = package_in("uuldana-handoff-a", Some("n1"));
        let b = package_in("uuldana-handoff-b", Some("n1"));
        assert_ne!(a.workspace, b.workspace);
        assert!(a.workspace.ends_with("uuldana-handoff-a"));
        assert_eq!(a.source.as_ref().unwrap().id.as_str(), "n1");
        assert_eq!(a.not_transferred(), &NonTransferable::ALL);
        assert!(package_in("uuldana-handoff-c", None).source.is_none());
    }
}
