//! Uuldana-owned session records.
//!
//! A [`Session`] is Uuldana's unified history record. It may span several
//! provider runs over time. Each [`RunRecord`] points at the provider's own
//! native session through an opaque [`NativeSessionId`]; the native
//! transcript and any hidden provider state stay owned by the provider.
//!
//! Invariants enforced here:
//! - at most one non-terminal run per session;
//! - every run's permissions are within the session's permission ceiling;
//! - run state only changes through the lifecycle state machine.
//!
//! Native session ids are provider-owned and may change during a run (reset,
//! fork, resume of another session). Every change is recorded in order and
//! none of them is an error (ADR 0009).

use std::fmt;
use std::path::{Path, PathBuf};

use crate::ids::{NativeSessionId, ProviderId, RunId, SessionId};
use crate::lifecycle::{LifecycleError, RunState, RunTransition};
use crate::permission::{PermissionError, PermissionPolicy, ValidatedPermissions};
use crate::time::Timestamp;
use crate::transport::TransportKind;

/// Reference to a provider's native session.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NativeSessionRef {
    /// Provider that owns the session.
    pub provider: ProviderId,
    /// Provider's identifier.
    pub id: NativeSessionId,
}

/// How a native session relates to the one recorded before it in the same
/// run (or to the run start, for the first entry).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NativeSessionLink {
    /// A fresh native session (first start, reset, clear, new conversation).
    New,
    /// An existing native session was continued (resume, load).
    Resumed,
    /// A new native session derived from the previous entry (fork, branch).
    Forked,
    /// The relationship is unknown.
    Unspecified,
}

/// One native session observed during a run.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NativeSessionEntry {
    /// Provider's identifier.
    pub id: NativeSessionId,
    /// Relationship to the previous entry.
    pub link: NativeSessionLink,
}

/// Result of [`Session::observe_native_session`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeSessionUpdate {
    /// The first native session of the run was recorded.
    First,
    /// The id equals the current one; nothing was recorded.
    Unchanged,
    /// A different id was recorded and is now current.
    Changed {
        /// The previously current id.
        previous: NativeSessionId,
    },
}

/// One provider run inside a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRecord {
    id: RunId,
    provider: ProviderId,
    transport: TransportKind,
    permissions: PermissionPolicy,
    natives: Vec<NativeSessionEntry>,
    state: RunState,
    started_at: Timestamp,
    ended_at: Option<Timestamp>,
}

impl RunRecord {
    /// Run identifier.
    pub fn id(&self) -> &RunId {
        &self.id
    }

    /// Provider of this run.
    pub fn provider(&self) -> &ProviderId {
        &self.provider
    }

    /// Transport used.
    pub fn transport(&self) -> TransportKind {
        self.transport
    }

    /// Effective permissions the run was launched with.
    pub fn permissions(&self) -> &PermissionPolicy {
        &self.permissions
    }

    /// Current (most recently recorded) native session identifier.
    pub fn native_session(&self) -> Option<&NativeSessionId> {
        self.natives.last().map(|entry| &entry.id)
    }

    /// Current native session reference.
    pub fn native_ref(&self) -> Option<NativeSessionRef> {
        self.native_session().map(|id| NativeSessionRef {
            provider: self.provider.clone(),
            id: id.clone(),
        })
    }

    /// Every native session observed during the run, oldest first. The entry
    /// before a [`NativeSessionLink::Forked`] entry is its parent.
    pub fn native_sessions(&self) -> &[NativeSessionEntry] {
        &self.natives
    }

    /// Current lifecycle state.
    pub fn state(&self) -> RunState {
        self.state
    }

    /// When the run was created.
    pub fn started_at(&self) -> Timestamp {
        self.started_at
    }

    /// When the run reached a terminal state.
    pub fn ended_at(&self) -> Option<Timestamp> {
        self.ended_at
    }
}

/// Parameters for [`Session::begin_run`].
#[derive(Debug, Clone)]
pub struct NewRun {
    /// Identifier for the new run.
    pub id: RunId,
    /// Provider to run.
    pub provider: ProviderId,
    /// Transport to use.
    pub transport: TransportKind,
    /// Validated permissions for the launch.
    pub permissions: ValidatedPermissions,
    /// Creation time.
    pub at: Timestamp,
}

/// Read-only view of one run together with the session it belongs to.
///
/// Adapter callbacks that look at a past or running native session receive
/// this instead of a bare [`RunRecord`], because identical run records can
/// come from different sessions and workspaces. It can only be obtained from
/// [`Session::run_context`], so the workspace always matches the run.
#[derive(Debug, Clone, Copy)]
pub struct RunContext<'a> {
    session: &'a SessionId,
    workspace: &'a Path,
    run: &'a RunRecord,
}

impl<'a> RunContext<'a> {
    /// Identifier of the Uuldana session.
    pub fn session_id(&self) -> &'a SessionId {
        self.session
    }

    /// Absolute workspace the run used as its working directory.
    pub fn workspace(&self) -> &'a Path {
        self.workspace
    }

    /// The run itself, including its native session history.
    pub fn run(&self) -> &'a RunRecord {
        self.run
    }

    /// Current native session of the run, if any.
    pub fn native_session(&self) -> Option<NativeSessionRef> {
        self.run.native_ref()
    }
}

/// A supervised session: Uuldana's unified history record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    id: SessionId,
    workspace: PathBuf,
    permission_ceiling: PermissionPolicy,
    created_at: Timestamp,
    runs: Vec<RunRecord>,
}

impl Session {
    /// Creates a session. `workspace` must be absolute. `permission_ceiling`
    /// is the most permissive policy any run of this session may use; it is
    /// chosen by the user and never raised automatically.
    pub fn new(
        id: SessionId,
        workspace: PathBuf,
        permission_ceiling: PermissionPolicy,
        created_at: Timestamp,
    ) -> Result<Self, SessionError> {
        if !workspace.is_absolute() {
            return Err(SessionError::RelativeWorkspace(workspace));
        }
        Ok(Self {
            id,
            workspace,
            permission_ceiling,
            created_at,
            runs: Vec::new(),
        })
    }

    /// Session identifier.
    pub fn id(&self) -> &SessionId {
        &self.id
    }

    /// Absolute workspace path.
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Permission ceiling for every run.
    pub fn permission_ceiling(&self) -> &PermissionPolicy {
        &self.permission_ceiling
    }

    /// Creation time.
    pub fn created_at(&self) -> Timestamp {
        self.created_at
    }

    /// All runs, oldest first.
    pub fn runs(&self) -> &[RunRecord] {
        &self.runs
    }

    /// The run that has not ended yet, if any.
    pub fn active_run(&self) -> Option<&RunRecord> {
        self.runs.iter().find(|run| !run.state.is_terminal())
    }

    /// Looks up a run.
    pub fn run(&self, id: &RunId) -> Option<&RunRecord> {
        self.runs.iter().find(|run| &run.id == id)
    }

    /// Context for adapter callbacks about run `id`.
    pub fn run_context(&self, id: &RunId) -> Option<RunContext<'_>> {
        self.run(id).map(|run| RunContext {
            session: &self.id,
            workspace: &self.workspace,
            run,
        })
    }

    /// Adds a new run in [`RunState::Pending`].
    pub fn begin_run(&mut self, new: NewRun) -> Result<&RunRecord, SessionError> {
        if let Some(active) = self.active_run() {
            return Err(SessionError::RunAlreadyActive(active.id.clone()));
        }
        if self.run(&new.id).is_some() {
            return Err(SessionError::DuplicateRun(new.id));
        }
        let dimensions = new
            .permissions
            .requested()
            .escalations_over(&self.permission_ceiling);
        if !dimensions.is_empty() {
            return Err(SessionError::Permission(PermissionError::ExceedsCeiling {
                dimensions,
            }));
        }
        let index = self.runs.len();
        self.runs.push(RunRecord {
            id: new.id,
            provider: new.provider,
            transport: new.transport,
            permissions: *new.permissions.effective(),
            natives: Vec::new(),
            state: RunState::Pending,
            started_at: new.at,
            ended_at: None,
        });
        Ok(&self.runs[index])
    }

    /// Applies a lifecycle transition to a run.
    pub fn transition_run(
        &mut self,
        id: &RunId,
        transition: RunTransition,
        at: Timestamp,
    ) -> Result<RunState, SessionError> {
        let run = self.run_mut(id)?;
        let next = run.state.apply(transition)?;
        run.state = next;
        if next.is_terminal() {
            run.ended_at = Some(at);
        }
        Ok(next)
    }

    /// Records a native session id observed for run `id`.
    ///
    /// An id equal to the current one is ignored. A different id is appended
    /// to the run's history with `link` and becomes current. Changes are never
    /// errors, and ended runs accept observations too (for example the result
    /// of a later session capture). The only error is an unknown run.
    pub fn observe_native_session(
        &mut self,
        id: &RunId,
        native: NativeSessionId,
        link: NativeSessionLink,
    ) -> Result<NativeSessionUpdate, SessionError> {
        let run = self.run_mut(id)?;
        let update = match run.natives.last() {
            None => NativeSessionUpdate::First,
            Some(current) if current.id == native => return Ok(NativeSessionUpdate::Unchanged),
            Some(current) => NativeSessionUpdate::Changed {
                previous: current.id.clone(),
            },
        };
        run.natives.push(NativeSessionEntry { id: native, link });
        Ok(update)
    }

    /// Shorthand for [`Session::observe_native_session`] with
    /// [`NativeSessionLink::Unspecified`], discarding the update kind.
    pub fn record_native_session(
        &mut self,
        id: &RunId,
        native: NativeSessionId,
    ) -> Result<(), SessionError> {
        self.observe_native_session(id, native, NativeSessionLink::Unspecified)
            .map(|_| ())
    }

    fn run_mut(&mut self, id: &RunId) -> Result<&mut RunRecord, SessionError> {
        self.runs
            .iter_mut()
            .find(|run| &run.id == id)
            .ok_or_else(|| SessionError::UnknownRun(id.clone()))
    }
}

/// Session invariant violations.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    /// The workspace path is not absolute.
    RelativeWorkspace(PathBuf),
    /// Another run is still active.
    RunAlreadyActive(RunId),
    /// A run with this id already exists.
    DuplicateRun(RunId),
    /// No run with this id.
    UnknownRun(RunId),
    /// A permission rule was violated.
    Permission(PermissionError),
    /// A lifecycle rule was violated.
    Lifecycle(LifecycleError),
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RelativeWorkspace(path) => {
                write!(f, "workspace path must be absolute: {}", path.display())
            }
            Self::RunAlreadyActive(id) => write!(f, "run {id} is still active"),
            Self::DuplicateRun(id) => write!(f, "run {id} already exists"),
            Self::UnknownRun(id) => write!(f, "unknown run {id}"),
            Self::Permission(err) => err.fmt(f),
            Self::Lifecycle(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for SessionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Permission(err) => Some(err),
            Self::Lifecycle(err) => Some(err),
            _ => None,
        }
    }
}

impl From<LifecycleError> for SessionError {
    fn from(err: LifecycleError) -> Self {
        Self::Lifecycle(err)
    }
}

impl From<PermissionError> for SessionError {
    fn from(err: PermissionError) -> Self {
        Self::Permission(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::RunEnd;
    use crate::permission::{
        ApprovalMode, FilesystemAccess, NetworkAccess, PermissionMapping, SandboxPolicy,
        validate_mapping,
    };

    fn workspace() -> PathBuf {
        std::env::temp_dir().join("uuldana-test-workspace")
    }

    fn ceiling() -> PermissionPolicy {
        PermissionPolicy {
            approval: ApprovalMode::AskForRisky,
            sandbox: SandboxPolicy {
                filesystem: FilesystemAccess::WorkspaceWrite,
                network: NetworkAccess::Denied,
            },
        }
    }

    fn session() -> Session {
        Session::new(
            SessionId::new("s1").unwrap(),
            workspace(),
            ceiling(),
            Timestamp::from_unix_millis(0),
        )
        .unwrap()
    }

    fn validated(policy: PermissionPolicy) -> ValidatedPermissions {
        validate_mapping(&policy, &PermissionMapping::new(policy)).unwrap()
    }

    fn new_run(id: &str, policy: PermissionPolicy) -> NewRun {
        NewRun {
            id: RunId::new(id).unwrap(),
            provider: ProviderId::new("example-agent").unwrap(),
            transport: TransportKind::NativePty,
            permissions: validated(policy),
            at: Timestamp::from_unix_millis(1),
        }
    }

    #[test]
    fn relative_workspace_is_rejected() {
        let err = Session::new(
            SessionId::new("s").unwrap(),
            PathBuf::from("relative/dir"),
            ceiling(),
            Timestamp::from_unix_millis(0),
        )
        .unwrap_err();
        assert!(matches!(err, SessionError::RelativeWorkspace(_)));
    }

    #[test]
    fn run_within_ceiling_starts_pending() {
        let mut s = session();
        let run = s.begin_run(new_run("r1", ceiling())).unwrap();
        assert_eq!(run.state(), RunState::Pending);
        assert_eq!(s.active_run().unwrap().id().as_str(), "r1");
    }

    #[test]
    fn run_above_ceiling_is_rejected() {
        let mut s = session();
        let mut wider = ceiling();
        wider.sandbox.network = NetworkAccess::Allowed;
        let err = s.begin_run(new_run("r1", wider)).unwrap_err();
        assert!(matches!(
            err,
            SessionError::Permission(PermissionError::ExceedsCeiling { .. })
        ));
        assert!(s.runs().is_empty());
    }

    #[test]
    fn only_one_active_run() {
        let mut s = session();
        s.begin_run(new_run("r1", ceiling())).unwrap();
        let err = s.begin_run(new_run("r2", ceiling())).unwrap_err();
        assert_eq!(
            err,
            SessionError::RunAlreadyActive(RunId::new("r1").unwrap())
        );
    }

    #[test]
    fn ended_run_allows_next_run_and_records_end_time() {
        let mut s = session();
        let r1 = RunId::new("r1").unwrap();
        s.begin_run(new_run("r1", ceiling())).unwrap();
        s.transition_run(&r1, RunTransition::Launch, Timestamp::from_unix_millis(2))
            .unwrap();
        s.transition_run(&r1, RunTransition::Started, Timestamp::from_unix_millis(3))
            .unwrap();
        s.transition_run(
            &r1,
            RunTransition::End(RunEnd::Completed),
            Timestamp::from_unix_millis(4),
        )
        .unwrap();
        assert_eq!(
            s.run(&r1).unwrap().ended_at(),
            Some(Timestamp::from_unix_millis(4))
        );
        assert!(s.active_run().is_none());
        s.begin_run(new_run("r2", PermissionPolicy::most_restrictive()))
            .unwrap();
        assert_eq!(s.runs().len(), 2);
    }

    #[test]
    fn duplicate_run_id_is_rejected() {
        let mut s = session();
        let r1 = RunId::new("r1").unwrap();
        s.begin_run(new_run("r1", ceiling())).unwrap();
        s.transition_run(
            &r1,
            RunTransition::End(RunEnd::Cancelled),
            Timestamp::from_unix_millis(2),
        )
        .unwrap();
        assert_eq!(
            s.begin_run(new_run("r1", ceiling())).unwrap_err(),
            SessionError::DuplicateRun(r1)
        );
    }

    #[test]
    fn invalid_transition_is_reported() {
        let mut s = session();
        let r1 = RunId::new("r1").unwrap();
        s.begin_run(new_run("r1", ceiling())).unwrap();
        let err = s
            .transition_run(&r1, RunTransition::Started, Timestamp::from_unix_millis(2))
            .unwrap_err();
        assert!(matches!(err, SessionError::Lifecycle(_)));
        assert_eq!(s.run(&r1).unwrap().state(), RunState::Pending);
    }

    fn native(id: &str) -> NativeSessionId {
        NativeSessionId::new(id).unwrap()
    }

    #[test]
    fn native_session_first_unchanged_and_changed() {
        let mut s = session();
        let r1 = RunId::new("r1").unwrap();
        s.begin_run(new_run("r1", ceiling())).unwrap();
        assert_eq!(
            s.observe_native_session(&r1, native("a"), NativeSessionLink::New),
            Ok(NativeSessionUpdate::First)
        );
        assert_eq!(
            s.observe_native_session(&r1, native("a"), NativeSessionLink::Unspecified),
            Ok(NativeSessionUpdate::Unchanged)
        );
        assert_eq!(
            s.observe_native_session(&r1, native("b"), NativeSessionLink::New),
            Ok(NativeSessionUpdate::Changed {
                previous: native("a")
            })
        );
        let run = s.run(&r1).unwrap();
        assert_eq!(run.native_sessions().len(), 2);
        assert_eq!(run.native_session(), Some(&native("b")));
        let current = run.native_ref().unwrap();
        assert_eq!(current.id, native("b"));
        assert_eq!(current.provider.as_str(), "example-agent");
    }

    #[test]
    fn rotations_are_recorded_in_order_with_fork_lineage() {
        let mut s = session();
        let r1 = RunId::new("r1").unwrap();
        s.begin_run(new_run("r1", ceiling())).unwrap();
        let steps = [
            ("resumed", NativeSessionLink::Resumed),
            ("forked", NativeSessionLink::Forked),
            ("reset", NativeSessionLink::New),
            ("resumed", NativeSessionLink::Resumed),
        ];
        for (id, link) in steps {
            s.observe_native_session(&r1, native(id), link).unwrap();
        }
        let history = s.run(&r1).unwrap().native_sessions();
        let recorded: Vec<_> = history.iter().map(|e| (e.id.as_str(), e.link)).collect();
        assert_eq!(recorded, steps);
        // The parent of a forked entry is the entry before it.
        assert_eq!(history[1].link, NativeSessionLink::Forked);
        assert_eq!(history[0].id, native("resumed"));
    }

    #[test]
    fn record_native_session_never_fails_on_a_different_id() {
        let mut s = session();
        let r1 = RunId::new("r1").unwrap();
        s.begin_run(new_run("r1", ceiling())).unwrap();
        s.record_native_session(&r1, native("a")).unwrap();
        s.record_native_session(&r1, native("b")).unwrap();
        let history = s.run(&r1).unwrap().native_sessions();
        assert_eq!(history[1].link, NativeSessionLink::Unspecified);
        assert_eq!(s.run(&r1).unwrap().native_session(), Some(&native("b")));
    }

    #[test]
    fn ended_runs_accept_native_observations_and_unknown_runs_fail() {
        let mut s = session();
        let r1 = RunId::new("r1").unwrap();
        s.begin_run(new_run("r1", ceiling())).unwrap();
        s.transition_run(
            &r1,
            RunTransition::End(RunEnd::Cancelled),
            Timestamp::from_unix_millis(2),
        )
        .unwrap();
        assert_eq!(
            s.observe_native_session(&r1, native("captured"), NativeSessionLink::Unspecified),
            Ok(NativeSessionUpdate::First)
        );
        let ghost = RunId::new("ghost").unwrap();
        assert_eq!(
            s.observe_native_session(&ghost, native("x"), NativeSessionLink::New),
            Err(SessionError::UnknownRun(ghost))
        );
    }

    fn session_in(id: &str, dir: &str) -> Session {
        Session::new(
            SessionId::new(id).unwrap(),
            std::env::temp_dir().join(dir),
            ceiling(),
            Timestamp::from_unix_millis(0),
        )
        .unwrap()
    }

    #[test]
    fn run_context_distinguishes_identical_runs_from_different_workspaces() {
        let mut a = session_in("same-session", "uuldana-workspace-a");
        let mut b = session_in("other-session", "uuldana-workspace-b");
        let r1 = RunId::new("r1").unwrap();
        for s in [&mut a, &mut b] {
            s.begin_run(new_run("r1", ceiling())).unwrap();
            s.record_native_session(&r1, native("same-native")).unwrap();
        }
        // The bare run records cannot tell the workspaces apart.
        assert_eq!(a.run(&r1), b.run(&r1));
        let ctx_a = a.run_context(&r1).unwrap();
        let ctx_b = b.run_context(&r1).unwrap();
        assert!(ctx_a.workspace().is_absolute() && ctx_b.workspace().is_absolute());
        assert_ne!(ctx_a.workspace(), ctx_b.workspace());
        assert_ne!(ctx_a.session_id(), ctx_b.session_id());
        assert_eq!(ctx_a.workspace(), a.workspace());
        assert_eq!(ctx_a.run().id(), &r1);
        assert_eq!(ctx_a.native_session().unwrap().id, native("same-native"));
    }

    #[test]
    fn run_context_for_unknown_run_is_none() {
        let s = session();
        assert!(s.run_context(&RunId::new("ghost").unwrap()).is_none());
    }

    #[test]
    fn unknown_run_is_reported() {
        let mut s = session();
        let ghost = RunId::new("ghost").unwrap();
        assert_eq!(
            s.transition_run(
                &ghost,
                RunTransition::Launch,
                Timestamp::from_unix_millis(1)
            ),
            Err(SessionError::UnknownRun(ghost))
        );
    }

    #[test]
    fn records_effective_not_requested_permissions() {
        let mut s = session();
        let requested = ceiling();
        let stricter = PermissionPolicy::most_restrictive();
        let permissions = validate_mapping(&requested, &PermissionMapping::new(stricter)).unwrap();
        let run = s
            .begin_run(NewRun {
                id: RunId::new("r1").unwrap(),
                provider: ProviderId::new("example-agent").unwrap(),
                transport: TransportKind::Acp,
                permissions,
                at: Timestamp::from_unix_millis(1),
            })
            .unwrap();
        assert_eq!(run.permissions(), &stricter);
    }
}
