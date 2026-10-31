//! Durable, append-only session journal.
//!
//! One JSON Lines file per session holds a header, one record per run and one
//! record per normalized event. Replaying the file rebuilds the [`Session`]
//! (runs, lifecycle states, native session history) and the event log, so
//! Uuldana can resume work after a restart or a crash.
//!
//! Damage is tolerated, never fatal: a truncated last line (a crash while
//! writing) and unreadable lines are skipped and counted, and a run that was
//! still active when the file ended is closed as failed during replay.
//!
//! Retention is a count limit, applied by [`prune`]: the oldest journals beyond
//! the limit are deleted. Nothing else is ever deleted.

use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

use serde_json::{Value, json};
use uuldana_core::*;

use crate::codec::{self, decode_event, encode_event};

/// Journal schema version, written in the header.
pub const SCHEMA_VERSION: u64 = 1;

/// Journals kept by [`prune`] when the caller has no other preference.
pub const DEFAULT_KEEP: usize = 200;

/// Creates `path` and its missing parents readable by the owner only on Unix.
/// On Windows a new folder inherits the access of the user's profile.
pub fn create_private_dir_all(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

/// Makes a file about to be created readable by its owner only on Unix.
/// Journals hold conversation text, so they are not for other users of the machine.
pub fn restrict_to_owner(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = options;
}

/// Writes `bytes` to `path`, replacing it, with the access of [`restrict_to_owner`].
pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    restrict_to_owner(&mut options);
    options.open(path)?.write_all(bytes)
}

/// Where journals live: `UULDANA_DATA` when set, otherwise
/// `%LOCALAPPDATA%\uuldana` on Windows and `$XDG_DATA_HOME/uuldana` (or
/// `~/.local/share/uuldana`) elsewhere. Journals are under `sessions/`.
pub fn data_dir() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("UULDANA_DATA") {
        return Some(PathBuf::from(path));
    }
    let base = if cfg!(windows) {
        PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
    } else if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        PathBuf::from(xdg)
    } else {
        PathBuf::from(std::env::var_os("HOME")?)
            .join(".local")
            .join("share")
    };
    Some(base.join("uuldana"))
}

/// Directory holding the journals under a data directory.
pub fn sessions_dir(data: &Path) -> PathBuf {
    data.join("sessions")
}

/// Journal file of a session.
pub fn path_for(data: &Path, session: &SessionId) -> PathBuf {
    sessions_dir(data).join(format!("{}.jsonl", session.as_str()))
}

fn policy_json(policy: &PermissionPolicy) -> Value {
    json!({
        "approval": match policy.approval {
            ApprovalMode::AskEveryAction => "ask_every_action",
            ApprovalMode::AskForRisky => "ask_for_risky",
            ApprovalMode::NeverAsk => "never_ask",
        },
        "filesystem": match policy.sandbox.filesystem {
            FilesystemAccess::ReadOnly => "read_only",
            FilesystemAccess::WorkspaceWrite => "workspace_write",
            FilesystemAccess::Unrestricted => "unrestricted",
        },
        "network": match policy.sandbox.network {
            NetworkAccess::Denied => "denied",
            NetworkAccess::Allowed => "allowed",
        },
    })
}

fn parse_policy(value: &Value) -> Option<PermissionPolicy> {
    Some(PermissionPolicy {
        approval: match value.get("approval")?.as_str()? {
            "ask_every_action" => ApprovalMode::AskEveryAction,
            "ask_for_risky" => ApprovalMode::AskForRisky,
            "never_ask" => ApprovalMode::NeverAsk,
            _ => return None,
        },
        sandbox: SandboxPolicy {
            filesystem: match value.get("filesystem")?.as_str()? {
                "read_only" => FilesystemAccess::ReadOnly,
                "workspace_write" => FilesystemAccess::WorkspaceWrite,
                "unrestricted" => FilesystemAccess::Unrestricted,
                _ => return None,
            },
            network: match value.get("network")?.as_str()? {
                "denied" => NetworkAccess::Denied,
                "allowed" => NetworkAccess::Allowed,
                _ => return None,
            },
        },
    })
}

fn transport_name(kind: TransportKind) -> &'static str {
    kind.as_str()
}

fn parse_transport(name: &str) -> Option<TransportKind> {
    Some(match name {
        "native_pty" => TransportKind::NativePty,
        "acp" => TransportKind::Acp,
        "custom_cli" => TransportKind::CustomCli,
        _ => return None,
    })
}

/// Append-only writer for one session's journal.
pub struct Journal {
    file: File,
    path: PathBuf,
}

impl Journal {
    /// Creates the journal of a new session and writes its header. Fails if
    /// the file already exists, so a session is never overwritten.
    pub fn create(path: &Path, session: &Session) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            create_private_dir_all(parent)?;
        }
        let mut options = OpenOptions::new();
        options.create_new(true).append(true);
        restrict_to_owner(&mut options);
        let file = options.open(path)?;
        let mut journal = Self {
            file,
            path: path.to_owned(),
        };
        journal.write(&json!({
            "v": SCHEMA_VERSION,
            "type": "session",
            "id": session.id().as_str(),
            "workspace": session.workspace().to_string_lossy(),
            "ceiling": policy_json(session.permission_ceiling()),
            "created_ms": session.created_at().unix_millis(),
        }))?;
        Ok(journal)
    }

    /// Opens an existing journal to continue it. If the last line was cut
    /// short by a crash, a newline is added first so new records start clean.
    pub fn open_append(path: &Path) -> io::Result<Self> {
        let needs_newline = {
            let bytes = fs::read(path)?;
            bytes.last().is_some_and(|b| *b != b'\n')
        };
        let mut file = OpenOptions::new().append(true).open(path)?;
        if needs_newline {
            file.write_all(b"\n")?;
        }
        Ok(Self {
            file,
            path: path.to_owned(),
        })
    }

    /// File this journal writes to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn write(&mut self, record: &Value) -> io::Result<()> {
        let mut line = record.to_string();
        line.push('\n');
        self.file.write_all(line.as_bytes())
    }

    /// Records the start of a run.
    pub fn append_run(&mut self, run: &RunRecord, label: Option<&str>) -> io::Result<()> {
        self.write(&json!({
            "type": "run",
            "run": run.id().as_str(),
            "provider": run.provider().as_str(),
            "label": label,
            "transport": transport_name(run.transport()),
            "effective": policy_json(run.permissions()),
            "started_ms": run.started_at().unix_millis(),
        }))
    }

    /// Records one event.
    pub fn append_event(&mut self, envelope: &EventEnvelope) -> io::Result<()> {
        self.write(&json!({
            "type": "event",
            "seq": envelope.seq,
            "at_ms": envelope.at.unix_millis(),
            "run": envelope.run.as_str(),
            "provider": envelope.provider.as_str(),
            "fidelity": codec::fidelity(envelope.fidelity),
            "data": encode_event(&envelope.event),
        }))
    }

    /// Forces the data to disk. Call it when a run ends.
    pub fn sync(&mut self) -> io::Result<()> {
        self.file.sync_data()
    }
}

/// A session rebuilt from its journal.
#[derive(Debug)]
pub struct Loaded {
    /// The session with every run, lifecycle state and native session.
    pub session: Session,
    /// The event log, in order.
    pub events: Vec<EventEnvelope>,
    /// Configuration label of each run, when it was recorded.
    pub labels: HashMap<RunId, String>,
    /// Lines that could not be read and were skipped.
    pub skipped_lines: usize,
    /// Whether a run was still active at the end and was closed during replay.
    pub recovered_active_run: bool,
    /// Time of the last record.
    pub last_activity_ms: u64,
}

/// Why a journal could not be loaded.
#[derive(Debug)]
pub enum LoadError {
    /// The file could not be read.
    Io(io::Error),
    /// The first record is not a valid session header.
    BadHeader(String),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "cannot read journal: {e}"),
            Self::BadHeader(m) => write!(f, "invalid journal header: {m}"),
        }
    }
}

impl std::error::Error for LoadError {}

/// Rebuilds a session from a journal file.
pub fn load(path: &Path) -> Result<Loaded, LoadError> {
    let reader = BufReader::new(File::open(path).map_err(LoadError::Io)?);
    let mut lines = reader.split(b'\n');
    let header_bytes = lines
        .next()
        .ok_or_else(|| LoadError::BadHeader("file is empty".into()))?
        .map_err(LoadError::Io)?;
    let header: Value =
        serde_json::from_slice(&header_bytes).map_err(|e| LoadError::BadHeader(e.to_string()))?;
    let bad = |what: &str| LoadError::BadHeader(what.to_owned());
    if header.get("type").and_then(Value::as_str) != Some("session") {
        return Err(bad("first record is not a session header"));
    }
    match header.get("v").and_then(Value::as_u64) {
        Some(SCHEMA_VERSION) => {}
        Some(other) => return Err(LoadError::BadHeader(format!("unsupported schema {other}"))),
        None => return Err(bad("missing schema version")),
    }
    let id = header
        .get("id")
        .and_then(Value::as_str)
        .and_then(|s| SessionId::new(s).ok())
        .ok_or_else(|| bad("bad session id"))?;
    let workspace = header
        .get("workspace")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .ok_or_else(|| bad("bad workspace"))?;
    let ceiling = header
        .get("ceiling")
        .and_then(parse_policy)
        .ok_or_else(|| bad("bad ceiling"))?;
    let created = header
        .get("created_ms")
        .and_then(Value::as_u64)
        .map(Timestamp::from_unix_millis)
        .ok_or_else(|| bad("bad creation time"))?;
    let mut session = Session::new(id, workspace, ceiling, created)
        .map_err(|e| LoadError::BadHeader(e.to_string()))?;

    let mut events = Vec::new();
    let mut labels = HashMap::new();
    let mut skipped = 0;
    let mut last = created.unix_millis();
    for line in lines {
        let Ok(line) = line else {
            skipped += 1;
            continue;
        };
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let Ok(record) = serde_json::from_slice::<Value>(&line) else {
            skipped += 1;
            continue;
        };
        let applied = match record.get("type").and_then(Value::as_str) {
            Some("run") => apply_run(&mut session, &mut labels, &record, &mut last),
            Some("event") => apply_event(&mut session, &mut events, &record, &mut last),
            _ => None,
        };
        if applied.is_none() {
            skipped += 1;
        }
    }

    let mut recovered = false;
    if let Some(run) = session.active_run().map(|r| r.id().clone()) {
        let _ = session.transition_run(
            &run,
            RunTransition::End(RunEnd::Failed(ErrorClass::ProcessFailed)),
            Timestamp::from_unix_millis(last),
        );
        recovered = true;
    }
    Ok(Loaded {
        session,
        events,
        labels,
        skipped_lines: skipped,
        recovered_active_run: recovered,
        last_activity_ms: last,
    })
}

fn apply_run(
    session: &mut Session,
    labels: &mut HashMap<RunId, String>,
    record: &Value,
    last: &mut u64,
) -> Option<()> {
    let id = RunId::new(record.get("run")?.as_str()?).ok()?;
    let provider = ProviderId::new(record.get("provider")?.as_str()?).ok()?;
    let effective = parse_policy(record.get("effective")?)?;
    let started = record.get("started_ms")?.as_u64()?;
    // A run that never ended before the next one began was cut short.
    if let Some(open) = session.active_run().map(|r| r.id().clone()) {
        session
            .transition_run(
                &open,
                RunTransition::End(RunEnd::Failed(ErrorClass::ProcessFailed)),
                Timestamp::from_unix_millis(started),
            )
            .ok()?;
    }
    // The effective policy was valid when written, so replay records it as the
    // request too; the session ceiling check still applies.
    let permissions = validate_mapping(&effective, &PermissionMapping::new(effective)).ok()?;
    session
        .begin_run(NewRun {
            id: id.clone(),
            provider,
            transport: parse_transport(record.get("transport")?.as_str()?)?,
            permissions,
            at: Timestamp::from_unix_millis(started),
        })
        .ok()?;
    if let Some(label) = record.get("label").and_then(Value::as_str) {
        labels.insert(id, label.to_owned());
    }
    *last = (*last).max(started);
    Some(())
}

fn apply_event(
    session: &mut Session,
    events: &mut Vec<EventEnvelope>,
    record: &Value,
    last: &mut u64,
) -> Option<()> {
    let run = RunId::new(record.get("run")?.as_str()?).ok()?;
    let provider = ProviderId::new(record.get("provider")?.as_str()?).ok()?;
    let at = record.get("at_ms")?.as_u64()?;
    let fidelity = codec::parse_fidelity(record.get("fidelity")?.as_str()?)?;
    let event = decode_event(record.get("data")?)?;
    let stamp = Timestamp::from_unix_millis(at);
    match &event {
        AgentEvent::RunStateChanged { to, .. } => {
            let transition = match to {
                RunState::Launching => RunTransition::Launch,
                RunState::Running => RunTransition::Started,
                RunState::Stopping => RunTransition::StopRequested,
                RunState::Ended(end) => RunTransition::End(*end),
                RunState::Pending => return None,
            };
            session.transition_run(&run, transition, stamp).ok()?;
        }
        AgentEvent::NativeSessionIdentified { native } => {
            session
                .observe_native_session(&run, native.clone(), NativeSessionLink::Unspecified)
                .ok()?;
        }
        AgentEvent::NativeSessionChanged { native, link } => {
            session
                .observe_native_session(&run, native.clone(), *link)
                .ok()?;
        }
        _ => {}
    }
    events.push(EventEnvelope::new(
        events.len() as u64,
        stamp,
        session.id().clone(),
        run,
        provider,
        DecodedEvent { fidelity, event },
    ));
    *last = (*last).max(at);
    Some(())
}

/// One line of `uuldana resume` listings.
#[derive(Debug, Clone)]
pub struct Summary {
    /// Journal file.
    pub path: PathBuf,
    /// Session id.
    pub id: SessionId,
    /// Workspace of the session.
    pub workspace: PathBuf,
    /// Number of runs.
    pub runs: usize,
    /// Provider of the most recent run.
    pub last_provider: Option<ProviderId>,
    /// Time of the last record.
    pub last_activity_ms: u64,
}

/// Summaries of every readable journal under `data`, newest first. Unreadable
/// files are skipped.
pub fn list(data: &Path) -> Vec<Summary> {
    let mut found: Vec<Summary> = journal_files(data)
        .into_iter()
        .filter_map(|path| {
            let loaded = load(&path).ok()?;
            Some(Summary {
                id: loaded.session.id().clone(),
                workspace: loaded.session.workspace().to_owned(),
                runs: loaded.session.runs().len(),
                last_provider: loaded.session.runs().last().map(|r| r.provider().clone()),
                last_activity_ms: loaded.last_activity_ms,
                path,
            })
        })
        .collect();
    found.sort_by_key(|summary| std::cmp::Reverse(summary.last_activity_ms));
    found
}

fn journal_files(data: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(sessions_dir(data)) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .collect()
}

/// Whether two workspace paths name the same directory. Windows paths compare
/// case-insensitively and ignore the `\\?\` prefix.
pub fn same_workspace(a: &Path, b: &Path) -> bool {
    fn key(path: &Path) -> String {
        let text = path.to_string_lossy();
        let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
        let text = text.trim_end_matches(['\\', '/']);
        if cfg!(windows) {
            text.replace('/', "\\").to_lowercase()
        } else {
            text.to_owned()
        }
    }
    key(a) == key(b)
}

/// Deletes the oldest journals beyond `keep`. Returns how many were deleted.
pub fn prune(data: &Path, keep: usize) -> usize {
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = journal_files(data)
        .into_iter()
        .filter_map(|p| Some((fs::metadata(&p).ok()?.modified().ok()?, p)))
        .collect();
    if files.len() <= keep {
        return 0;
    }
    files.sort_by_key(|file| std::cmp::Reverse(file.0));
    files
        .into_iter()
        .skip(keep)
        .filter(|(_, path)| fs::remove_file(path).is_ok())
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dir(PathBuf);
    impl Dir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("uuldana-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn ceiling() -> PermissionPolicy {
        PermissionPolicy::unrestricted()
    }

    fn new_session(workspace: &Path, id: &str) -> Session {
        Session::new(
            SessionId::new(id).unwrap(),
            workspace.to_owned(),
            ceiling(),
            Timestamp::from_unix_millis(1_000),
        )
        .unwrap()
    }

    struct Clock {
        seq: u64,
        at: u64,
    }

    fn emit(
        journal: &mut Journal,
        clock: &mut Clock,
        session: &SessionId,
        run: &RunId,
        provider: &ProviderId,
        event: AgentEvent,
    ) {
        clock.at += 10;
        let envelope = EventEnvelope::new(
            clock.seq,
            Timestamp::from_unix_millis(clock.at),
            session.clone(),
            run.clone(),
            provider.clone(),
            DecodedEvent::synthetic(event),
        );
        clock.seq += 1;
        journal.append_event(&envelope).unwrap();
    }

    fn record_run(
        session: &mut Session,
        journal: &mut Journal,
        run: &str,
        provider: &str,
        native: &str,
        end: Option<RunEnd>,
        clock: &mut Clock,
    ) {
        let run_id = RunId::new(run).unwrap();
        let provider_id = ProviderId::new(provider).unwrap();
        let session_id = session.id().clone();
        let permissions = validate_mapping(&ceiling(), &PermissionMapping::new(ceiling())).unwrap();
        session
            .begin_run(NewRun {
                id: run_id.clone(),
                provider: provider_id.clone(),
                transport: TransportKind::NativePty,
                permissions,
                at: Timestamp::from_unix_millis(clock.at),
            })
            .unwrap();
        journal
            .append_run(session.run(&run_id).unwrap(), Some(provider))
            .unwrap();
        for (to, transition) in [
            (RunState::Launching, RunTransition::Launch),
            (RunState::Running, RunTransition::Started),
        ] {
            let from = session.run(&run_id).unwrap().state();
            session
                .transition_run(
                    &run_id,
                    transition,
                    Timestamp::from_unix_millis(clock.at + 10),
                )
                .unwrap();
            let event = AgentEvent::RunStateChanged { from, to };
            emit(journal, clock, &session_id, &run_id, &provider_id, event);
        }
        session
            .observe_native_session(
                &run_id,
                NativeSessionId::new(native).unwrap(),
                NativeSessionLink::New,
            )
            .unwrap();
        let event = AgentEvent::NativeSessionChanged {
            native: NativeSessionId::new(native).unwrap(),
            link: NativeSessionLink::New,
        };
        emit(journal, clock, &session_id, &run_id, &provider_id, event);
        let event = AgentEvent::MessageChunk {
            role: Role::User,
            text: format!("pesan untuk {run}"),
        };
        emit(journal, clock, &session_id, &run_id, &provider_id, event);
        if let Some(end) = end {
            let from = session.run(&run_id).unwrap().state();
            session
                .transition_run(
                    &run_id,
                    RunTransition::End(end),
                    // The same instant `emit` stamps on the matching event.
                    Timestamp::from_unix_millis(clock.at + 10),
                )
                .unwrap();
            let event = AgentEvent::RunStateChanged {
                from,
                to: RunState::Ended(end),
            };
            emit(journal, clock, &session_id, &run_id, &provider_id, event);
            journal.sync().unwrap();
        }
    }

    #[test]
    fn replay_rebuilds_the_same_session_and_event_log() {
        let dir = Dir::new("journal-replay");
        let workspace = std::env::temp_dir();
        let mut session = new_session(&workspace, "s-replay");
        let path = path_for(&dir.0, session.id());
        let mut journal = Journal::create(&path, &session).unwrap();
        let mut clock = Clock { seq: 0, at: 2_000 };
        record_run(
            &mut session,
            &mut journal,
            "run-1",
            "agent-a",
            "native-1",
            Some(RunEnd::LimitReached(LimitKind::UsageQuota)),
            &mut clock,
        );
        record_run(
            &mut session,
            &mut journal,
            "run-2",
            "agent-b",
            "native-2",
            Some(RunEnd::Completed),
            &mut clock,
        );
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.session, session);
        assert_eq!(loaded.events.len(), clock.seq as usize);
        assert_eq!(loaded.skipped_lines, 0);
        assert!(!loaded.recovered_active_run);
        assert_eq!(loaded.labels[&RunId::new("run-1").unwrap()], "agent-a");
        assert_eq!(loaded.last_activity_ms, clock.at);
    }

    #[test]
    fn crash_mid_run_is_recovered_and_a_torn_last_line_is_skipped() {
        let dir = Dir::new("journal-crash");
        let workspace = std::env::temp_dir();
        let mut session = new_session(&workspace, "s-crash");
        let path = path_for(&dir.0, session.id());
        let mut journal = Journal::create(&path, &session).unwrap();
        let mut clock = Clock { seq: 0, at: 2_000 };
        record_run(
            &mut session,
            &mut journal,
            "run-1",
            "agent-b",
            "native-1",
            None,
            &mut clock,
        );
        drop(journal);
        // Simulate a crash while writing the next record.
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(br#"{"type":"event","seq":9,"at_ms":99,"run":"run-1","prov"#)
            .unwrap();
        drop(file);

        let loaded = load(&path).unwrap();
        assert_eq!(loaded.skipped_lines, 1);
        assert!(loaded.recovered_active_run);
        let run = &loaded.session.runs()[0];
        assert_eq!(
            run.state(),
            RunState::Ended(RunEnd::Failed(ErrorClass::ProcessFailed))
        );
        assert_eq!(run.native_session().unwrap().as_str(), "native-1");

        // Continuing the journal starts on a clean line and stays readable.
        let mut journal = Journal::open_append(&path).unwrap();
        let mut resumed = loaded.session;
        record_run(
            &mut resumed,
            &mut journal,
            "run-2",
            "agent-a",
            "native-2",
            Some(RunEnd::Completed),
            &mut clock,
        );
        let again = load(&path).unwrap();
        assert_eq!(again.session.runs().len(), 2);
        assert_eq!(again.skipped_lines, 1);
    }

    #[test]
    fn damaged_and_foreign_files_are_reported_precisely() {
        let dir = Dir::new("journal-bad");
        let empty = dir.0.join("empty.jsonl");
        fs::write(&empty, "").unwrap();
        assert!(matches!(load(&empty), Err(LoadError::BadHeader(_))));
        let wrong = dir.0.join("wrong.jsonl");
        fs::write(&wrong, "{\"type\":\"event\"}\n").unwrap();
        assert!(matches!(load(&wrong), Err(LoadError::BadHeader(_))));
        let future = dir.0.join("future.jsonl");
        fs::write(&future, "{\"v\":99,\"type\":\"session\"}\n").unwrap();
        let error = load(&future).unwrap_err().to_string();
        assert!(error.contains("unsupported schema 99"), "{error}");
    }

    #[test]
    fn create_never_overwrites_an_existing_session() {
        let dir = Dir::new("journal-exists");
        let session = new_session(&std::env::temp_dir(), "s-once");
        let path = path_for(&dir.0, session.id());
        Journal::create(&path, &session).unwrap();
        assert!(Journal::create(&path, &session).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_journal_and_its_folders_are_for_their_owner_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Dir::new("journal-private");
        let session = new_session(&std::env::temp_dir(), "s-private");
        let path = path_for(&dir.0.join("data"), session.id());
        Journal::create(&path, &session).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(mode(path.parent().unwrap().parent().unwrap()), 0o700);
    }

    #[test]
    fn listing_is_newest_first_and_prune_keeps_the_newest() {
        let dir = Dir::new("journal-list");
        let workspace = std::env::temp_dir();
        for (index, id) in ["s-a", "s-b", "s-c"].into_iter().enumerate() {
            let mut session = new_session(&workspace, id);
            let path = path_for(&dir.0, session.id());
            let mut journal = Journal::create(&path, &session).unwrap();
            let mut clock = Clock {
                seq: 0,
                at: 10_000 * (index as u64 + 1),
            };
            record_run(
                &mut session,
                &mut journal,
                "run-1",
                "agent-b",
                "native",
                Some(RunEnd::Completed),
                &mut clock,
            );
            // Distinct modification times for the retention check.
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        let ids: Vec<_> = list(&dir.0).into_iter().map(|s| s.id.to_string()).collect();
        assert_eq!(ids, ["s-c", "s-b", "s-a"]);
        assert_eq!(prune(&dir.0, 2), 1);
        let ids: Vec<_> = list(&dir.0).into_iter().map(|s| s.id.to_string()).collect();
        assert_eq!(ids, ["s-c", "s-b"]);
        assert_eq!(prune(&dir.0, 2), 0);
    }

    #[test]
    fn workspace_comparison_ignores_case_and_prefix_on_windows() {
        assert!(same_workspace(Path::new("/a/b"), Path::new("/a/b/")));
        assert!(!same_workspace(Path::new("/a/b"), Path::new("/a/c")));
        if cfg!(windows) {
            assert!(same_workspace(
                Path::new(r"D:\Proyek\Satu"),
                Path::new(r"\\?\d:\proyek\satu")
            ));
        }
    }
}
