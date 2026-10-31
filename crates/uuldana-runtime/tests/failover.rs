//! Orchestrated failover across configured providers, with real child processes
//! in real pseudo terminals. The fixture processes print what a provider's
//! screen would print; the limit wording is recognized by the runtime's
//! general reader, and the conversation store is a test double.
mod common;
use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc;
use uuldana_core::*;
use uuldana_runtime::{
    LocalHost, Supervisor,
    config::Config,
    cooldown::{self, Cooldowns},
    journal::{self, Journal},
    orchestrator::{Io, Mode, NativeContext, Orchestrator, OrchestratorError, Report},
    universal_limit::UniversalLimitDecoder,
};

/// A fixture provider: a local process in a pseudo terminal.
struct Fixture {
    descriptor: ProviderDescriptor,
    mode: &'static str,
    resumable: bool,
    preassigned: Option<&'static str>,
    /// The adapter has no limit decoder of its own, as an agent Uuldana has
    /// no special knowledge of.
    plain: bool,
}

impl Fixture {
    fn new(id: &str, mode: &'static str) -> Self {
        Self {
            descriptor: ProviderDescriptor::new(
                ProviderId::new(id).unwrap(),
                id,
                vec![TransportKind::NativePty],
            ),
            mode,
            resumable: false,
            preassigned: None,
            plain: false,
        }
    }
    fn plain(mut self) -> Self {
        self.plain = true;
        self
    }
    fn resumable(mut self) -> Self {
        self.resumable = true;
        self
    }
    /// The provider takes its session id at launch.
    fn preassigned(mut self, id: &'static str) -> Self {
        self.preassigned = Some(id);
        self
    }
}

impl ProviderAdapter for Fixture {
    fn descriptor(&self) -> &ProviderDescriptor {
        &self.descriptor
    }
    fn detect(&self, probe: &dyn HostProbe) -> Result<Installation, AdapterError> {
        probe.run(&common::fixture(), &["--version"], Duration::from_secs(5))?;
        Ok(Installation::new(common::fixture()))
    }
    fn capabilities(&self, _: &Installation) -> CapabilitySet {
        let support = if self.resumable {
            Support::Supported
        } else {
            Support::Unsupported
        };
        CapabilitySet::new()
            .with(
                Capability::NativeTerminalUi,
                Support::Supported,
                Evidence::Declared,
            )
            .with(
                Capability::InitialPrompt,
                Support::Supported,
                Evidence::Declared,
            )
            .with(Capability::ResumeById, support, Evidence::Declared)
    }
    fn map_permissions(&self, _: &Installation, requested: &PermissionPolicy) -> PermissionMapping {
        PermissionMapping::new(*requested)
    }
    fn launch_plan(
        &self,
        installation: &Installation,
        request: &LaunchRequest,
    ) -> Result<LaunchPlan, AdapterError> {
        let mut plan = LaunchPlan::new(
            installation.executable.clone(),
            request.workspace.clone(),
            TransportSpec::NativePty {
                initial_size: request.terminal_size.unwrap_or_default(),
            },
        )
        .arg(self.mode);
        if let Some(prompt) = &request.initial_prompt {
            plan = plan.arg(prompt);
        }
        if let Some(id) = self.preassigned {
            plan.native_session =
                NativeSessionBinding::Preassigned(NativeSessionId::new(id).unwrap());
        }
        Ok(plan)
    }
    fn resume_plan(
        &self,
        installation: &Installation,
        request: &ResumeRequest,
    ) -> Result<LaunchPlan, AdapterError> {
        let plan = self.launch_plan(installation, &request.launch)?;
        Ok(plan.arg("--resume").arg(request.native.as_str()))
    }
    fn event_decoder(&self, transport: TransportKind) -> Option<Box<dyn EventDecoder>> {
        if self.plain {
            return None;
        }
        // A decoder the adapter supplies itself; a plain fixture leaves it to
        // the supervisor's fallback.
        let _ = transport;
        Some(Box::new(UniversalLimitDecoder::new()))
    }
    fn shutdown_plan(&self, _: TransportKind) -> ShutdownPlan {
        ShutdownPlan {
            steps: vec![ShutdownStep::WriteInput(b"quit\r".to_vec())],
            grace: Duration::from_secs(3),
        }
    }
}

/// Conversation store double: remembers one conversation per provider id.
struct Store {
    conversations: HashMap<String, (NativeSessionId, Vec<TranscriptEntry>)>,
}

impl NativeContext for Store {
    fn conversation(
        &self,
        provider: &ProviderId,
        _: &dyn HostProbe,
        _: &Path,
        native: &NativeSessionId,
    ) -> Option<Vec<TranscriptEntry>> {
        let (id, transcript) = self.conversations.get(provider.as_str())?;
        (id == native).then(|| transcript.clone())
    }
    fn discover(
        &self,
        provider: &ProviderId,
        _: &dyn HostProbe,
        _: &Path,
        _: Timestamp,
    ) -> Option<NativeSessionId> {
        self.conversations
            .get(provider.as_str())
            .map(|(id, _)| id.clone())
    }
}

fn alpha_store() -> Arc<Store> {
    let entry = |role, text: &str| TranscriptEntry {
        role,
        text: text.into(),
        fidelity: Fidelity::Inferred,
    };
    Arc::new(Store {
        conversations: HashMap::from([(
            "test.alpha".to_owned(),
            (
                NativeSessionId::new("alpha-native-1").unwrap(),
                vec![
                    entry(Role::User, "perbaiki tes login"),
                    entry(Role::Agent, "Sudah saya cek, masalahnya di zona waktu."),
                ],
            ),
        )]),
    })
}

/// What the "terminal" showed, with control sequences removed.
#[derive(Clone, Default)]
struct Screen(Arc<Mutex<Vec<u8>>>);
impl Write for Screen {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Screen {
    fn text(&self) -> String {
        let bytes = self.0.lock().unwrap().clone();
        let raw = String::from_utf8_lossy(&bytes).into_owned();
        let mut out = String::new();
        let mut chars = raw.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\x1b' if chars.peek() == Some(&'[') => {
                    chars.next();
                    let mut last = ' ';
                    for next in chars.by_ref() {
                        if ('@'..='~').contains(&next) {
                            last = next;
                            break;
                        }
                    }
                    if matches!(last, 'A'..='H' | 'f') {
                        out.push(' ');
                    }
                }
                '\x1b' => {
                    chars.next();
                }
                c if c.is_control() => out.push(' '),
                c => out.push(c),
            }
        }
        out.split_whitespace().collect::<Vec<_>>().join(" ")
    }
}

struct Scratch(PathBuf);
impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("uuldana-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Harness {
    orchestrator: Orchestrator,
    io: Io,
    screen: Screen,
    notices: Arc<Mutex<Vec<String>>>,
    clock: Arc<AtomicU64>,
    _keys: mpsc::UnboundedSender<Vec<u8>>,
}

const START_MS: u64 = 1_800_000_000_000;

fn harness(
    config: &str,
    adapters: Vec<Fixture>,
    data: &Path,
    supervisor_session: Option<(Session, Vec<EventEnvelope>)>,
    labels: HashMap<RunId, String>,
) -> Harness {
    harness_with_store(
        config,
        adapters,
        data,
        supervisor_session,
        labels,
        alpha_store(),
    )
}

fn harness_with_store(
    config: &str,
    adapters: Vec<Fixture>,
    data: &Path,
    supervisor_session: Option<(Session, Vec<EventEnvelope>)>,
    labels: HashMap<RunId, String>,
    store: Arc<Store>,
) -> Harness {
    let config = Config::parse(config).unwrap();
    let mut registry = ProviderRegistry::new();
    for adapter in adapters {
        registry.register(Arc::new(adapter)).unwrap();
    }
    let probe: Arc<dyn HostProbe> = Arc::new(LocalHost::new());
    let (session, events, journal) = match supervisor_session {
        Some((session, events)) => {
            let path = journal::path_for(data, session.id());
            (session, events, Journal::open_append(&path).unwrap())
        }
        None => {
            let session = Session::new(
                SessionId::new("s-failover").unwrap(),
                std::env::current_dir().unwrap(),
                config.ceiling,
                Timestamp::from_unix_millis(START_MS),
            )
            .unwrap();
            let journal =
                Journal::create(&journal::path_for(data, session.id()), &session).unwrap();
            (session, Vec::new(), journal)
        }
    };
    let mut supervisor = Supervisor::with_history(registry, probe.clone(), session, events);
    supervisor.attach_journal(journal);
    let clock = Arc::new(AtomicU64::new(START_MS));
    let ticking = clock.clone();
    let orchestrator = Orchestrator::new(supervisor, config, probe, store, labels)
        .with_cooldown_file(cooldown::path_in(data))
        .with_clock(Arc::new(move || ticking.load(Ordering::SeqCst)))
        .with_settle(Duration::from_millis(300));
    let screen = Screen::default();
    let notices = Arc::new(Mutex::new(Vec::new()));
    let (keys, input) = mpsc::unbounded_channel();
    let sink = notices.clone();
    let io = Io {
        input: Some(input),
        output: Box::new(screen.clone()),
        terminal_size: Box::new(|| Some(TerminalSize::new(50, 500))),
        cursor: cfg!(windows).then(|| Box::new(|| (1u16, 1u16)) as Box<_>),
        notice: Box::new(move |text| sink.lock().unwrap().push(text.to_owned())),
        agent: None,
        prefill: None,
        screen_text: Some({
            let screen = screen.clone();
            Box::new(move || screen.text())
        }),
    };
    Harness {
        orchestrator,
        io,
        screen,
        notices,
        clock,
        _keys: keys,
    }
}

const TWO_ENTRIES: &str = r#"{"version":1,"cooldown_minutes":300,
    "permissions":{"approval":"ask_for_risky","filesystem":"workspace_write","network":"allowed"},
    "providers":[{"id":"test.alpha"},{"id":"test.beta"}]}"#;

fn within(seconds: u64) -> Duration {
    Duration::from_secs(seconds)
}

#[tokio::test]
async fn usage_limit_moves_work_to_the_next_entry_with_context_and_the_same_ceiling() {
    let data = Scratch::new("failover");
    let mut h = harness(
        TWO_ENTRIES,
        vec![
            Fixture::new("test.alpha", "pty-limit"),
            Fixture::new("test.beta", "pty-continue"),
        ],
        &data.0,
        None,
        HashMap::new(),
    );
    let report = tokio::time::timeout(
        within(60),
        h.orchestrator.run(
            &mut h.io,
            Mode::Fresh {
                prompt: Some("perbaiki tes login".into()),
            },
        ),
    )
    .await
    .unwrap_or_else(|_| panic!("failover timed out\n{}", h.screen.text()))
    .unwrap_or_else(|e| panic!("{e}\n{}", h.screen.text()));

    let labels: Vec<_> = report.runs.iter().map(|r| r.label.as_str()).collect();
    assert_eq!(labels, ["test.alpha", "test.beta"]);
    assert_eq!(report.switches, 1);
    assert_eq!(report.exit.map(|e| e.success), Some(true));

    let screen = h.screen.text();
    assert!(screen.contains("CONTINUED"), "{screen}");
    assert!(screen.contains("ARG|User: perbaiki tes login"), "{screen}");
    assert!(
        screen.contains("ARG|Agent: Sudah saya cek, masalahnya di zona waktu."),
        "{screen}"
    );
    assert!(
        screen.contains("it could no longer be used on this account"),
        "{screen}"
    );
    assert!(screen.contains("Not carried over:"), "{screen}");

    // Priority order, rest period and ceiling.
    let session = h.orchestrator.supervisor().session();
    assert_eq!(session.runs().len(), 2);
    for run in session.runs() {
        assert!(run.permissions().is_within(session.permission_ceiling()));
    }
    assert_eq!(
        h.orchestrator
            .cooldowns()
            .returns_at("test.alpha", START_MS),
        Some(START_MS + 300 * 60_000)
    );
    assert!(!h.orchestrator.cooldowns().is_resting("test.beta", START_MS));
    let notices = h.notices.lock().unwrap().join("\n");
    assert!(
        notices.contains("test.alpha reached its usage limit"),
        "{notices}"
    );
    assert!(
        notices.contains("continuing on test.beta from test.alpha"),
        "{notices}"
    );

    // The journal holds both runs and the limit, and survives a reload.
    let reloaded = journal::load(&journal::path_for(&data.0, session.id())).unwrap();
    assert_eq!(reloaded.session.runs().len(), 2);
    assert_eq!(reloaded.labels[&RunId::new("run-1").unwrap()], "test.alpha");
    assert!(reloaded.events.iter().any(|e| matches!(
        &e.event,
        AgentEvent::LimitReached(s) if s.kind == LimitKind::UsageQuota
    )));
    assert_eq!(
        Cooldowns::load(&cooldown::path_in(&data.0)).returns_at("test.alpha", START_MS),
        Some(START_MS + 300 * 60_000)
    );
}

/// Runs an agent with no decoder of its own that prints `mode` and then waits,
/// with a second entry to move to.
async fn plain_agent_run(mode: &'static str) -> (Report, Harness) {
    let data = Scratch::new(&format!("plain-{mode}"));
    // An agent whose own history nobody can read.
    let nothing = Arc::new(Store {
        conversations: HashMap::new(),
    });
    let mut h = harness_with_store(
        TWO_ENTRIES,
        vec![
            Fixture::new("test.alpha", mode).plain(),
            Fixture::new("test.beta", "pty-continue"),
        ],
        &data.0,
        None,
        HashMap::new(),
        nothing,
    );
    // If nothing moves the work, the agent waits for the user, who quits.
    let keys = h._keys.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(4)).await;
        let _ = keys.send(b"quit\r".to_vec());
    });
    let report = tokio::time::timeout(
        within(60),
        h.orchestrator.run(&mut h.io, Mode::Fresh { prompt: None }),
    )
    .await
    .unwrap_or_else(|_| panic!("timed out\n{}", h.screen.text()))
    .unwrap();
    (report, h)
}

#[tokio::test]
async fn an_agent_nobody_wrote_a_decoder_for_still_moves_on_when_its_allowance_is_used_up() {
    let (report, h) = plain_agent_run("pty-limit-generic").await;
    let labels: Vec<_> = report.runs.iter().map(|r| r.label.as_str()).collect();
    assert_eq!(labels, ["test.alpha", "test.beta"], "{}", h.screen.text());
    let screen = h.screen.text();
    assert!(screen.contains("CONTINUED"), "{screen}");
    // With no history to read, the next agent is given what the screen showed.
    assert!(screen.contains("own history could not be read"), "{screen}");
    assert!(
        screen
            .matches("working Daily limit reached. Try again tomorrow")
            .count()
            >= 2,
        "{screen}"
    );
}

/// Claude Code's own wording, weekly or per session or per model, moves the
/// work like any other.
#[tokio::test]
async fn claude_codes_weekly_session_and_model_limits_each_move_the_work() {
    for mode in ["pty-limit-weekly", "pty-limit-session", "pty-limit-opus"] {
        let (report, h) = plain_agent_run(mode).await;
        let labels: Vec<_> = report.runs.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(
            labels,
            ["test.alpha", "test.beta"],
            "{mode}\n{}",
            h.screen.text()
        );
        assert!(h.screen.text().contains("CONTINUED"), "{mode}");
    }
}

/// The warning Claude Code prints before the limit is not a stop.
#[tokio::test]
async fn a_usage_warning_before_the_limit_does_not_move_the_work() {
    let (report, h) = plain_agent_run("pty-warning-85").await;
    assert_eq!(report.runs.len(), 1, "{}", h.screen.text());
    assert!(!h.screen.text().contains("CONTINUED"));
}

/// The agent that takes over is shown what the first one worked on, and not
/// the text that stopped it: shown back, that would look like its own limit,
/// and the work would move again with nowhere left to go.
#[tokio::test]
async fn the_agent_that_takes_over_is_not_shown_the_limit_text_and_stays() {
    let data = Scratch::new("takeover-stays");
    let nothing = Arc::new(Store {
        conversations: HashMap::new(),
    });
    let mut h = harness_with_store(
        TWO_ENTRIES,
        vec![
            Fixture::new("test.alpha", "pty-limit-weekly").plain(),
            Fixture::new("test.beta", "pty-continue-wait").plain(),
        ],
        &data.0,
        None,
        HashMap::new(),
        nothing,
    );
    let keys = h._keys.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(9)).await;
        let _ = keys.send(b"quit\r".to_vec());
    });
    let report = tokio::time::timeout(
        within(60),
        h.orchestrator.run(&mut h.io, Mode::Fresh { prompt: None }),
    )
    .await
    .unwrap_or_else(|_| panic!("timed out\n{}", h.screen.text()))
    .unwrap();
    let labels: Vec<_> = report.runs.iter().map(|r| r.label.as_str()).collect();
    assert_eq!(labels, ["test.alpha", "test.beta"], "{}", h.screen.text());
    assert_eq!(report.switches, 1, "{}", h.screen.text());
    let screen = h.screen.text();
    assert!(screen.contains("CONTINUED"), "{screen}");
    // The screen holds both agents' output: the message is there once, from
    // the agent that printed it, and not again in what the second was given.
    assert_eq!(
        screen.matches("hit your weekly limit").count(),
        1,
        "{screen}"
    );
    assert!(screen.contains("[usage limit message removed]"), "{screen}");
}

#[tokio::test]
async fn an_agent_nobody_spoke_to_hands_over_nothing_and_the_next_runs_nothing() {
    let data = Scratch::new("nothing-said");
    // The first agent's history is readable and empty, and its screen holds
    // only its welcome text, which the user never answered.
    let mut h = harness_with_store(
        TWO_ENTRIES,
        vec![
            Fixture::new("test.alpha", "pty-limit"),
            Fixture::new("test.beta", "pty-continue"),
        ],
        &data.0,
        None,
        HashMap::new(),
        Arc::new(Store {
            conversations: HashMap::from([(
                "test.alpha".to_owned(),
                (NativeSessionId::new("alpha-native-1").unwrap(), Vec::new()),
            )]),
        }),
    );
    h.io.screen_text = Some(Box::new(String::new));
    let report = tokio::time::timeout(
        within(60),
        h.orchestrator.run(&mut h.io, Mode::Fresh { prompt: None }),
    )
    .await
    .unwrap_or_else(|_| panic!("timed out\n{}", h.screen.text()))
    .unwrap();
    let labels: Vec<_> = report.runs.iter().map(|r| r.label.as_str()).collect();
    assert_eq!(labels, ["test.alpha", "test.beta"], "{}", h.screen.text());
    let screen = h.screen.text();
    assert!(screen.contains("CONTINUED"), "{screen}");
    // No prompt at all: nothing for the next agent to spend a turn on.
    assert!(!screen.contains("ARG|"), "{screen}");
    assert!(!screen.contains("You are continuing work"), "{screen}");
}

#[tokio::test]
async fn a_pause_for_a_rate_limit_does_not_move_the_work() {
    let (report, h) = plain_agent_run("pty-rate-pause").await;
    assert_eq!(report.runs.len(), 1, "{}", h.screen.text());
    assert!(!h.screen.text().contains("CONTINUED"));
}

/// Runs `pty-limit-then-work` as the first entry: it prints a line that reads
/// like a usage limit and keeps working for three seconds after it.
async fn limit_like_text_then_work(keeps_working_after: Duration) -> (Report, Harness) {
    let data = Scratch::new(&format!(
        "limit-then-work-{}",
        keeps_working_after.as_secs()
    ));
    let mut h = harness(
        TWO_ENTRIES,
        vec![
            Fixture::new("test.alpha", "pty-limit-then-work"),
            Fixture::new("test.beta", "pty-continue"),
        ],
        &data.0,
        None,
        HashMap::new(),
    );
    h.orchestrator = h.orchestrator.with_keeps_working_after(keeps_working_after);
    // If nothing stops it, the provider waits for the user, who quits.
    let keys = h._keys.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(8)).await;
        let _ = keys.send(b"quit\r".to_vec());
    });
    let report = tokio::time::timeout(
        within(60),
        h.orchestrator.run(&mut h.io, Mode::Fresh { prompt: None }),
    )
    .await
    .unwrap_or_else(|_| panic!("timed out\n{}", h.screen.text()))
    .unwrap();
    (report, h)
}

#[tokio::test]
async fn an_agent_that_keeps_working_after_limit_like_text_is_not_stopped() {
    let (report, h) = limit_like_text_then_work(Duration::from_secs(1)).await;
    assert_eq!(report.runs.len(), 1, "{}", h.screen.text());
    assert_eq!(report.switches, 0);
    assert!(!h.screen.text().contains("CONTINUED"));
    assert!(
        !h.orchestrator
            .cooldowns()
            .is_resting("test.alpha", START_MS)
    );
    // And it was said on the screen at the time.
    let notices = h.notices.lock().unwrap().join("\n");
    assert!(
        notices.contains("text like a usage limit was ignored"),
        "{notices}"
    );
    // The text that was set aside left a note, so it can be looked at later.
    let noted = h
        .orchestrator
        .supervisor()
        .events()
        .iter()
        .any(|e| matches!(&e.event, AgentEvent::Diagnostic { message } if message.contains("usage limit text ignored")));
    assert!(noted);
}

/// The same run with the guard switched off moves on at the first quiet
/// moment, which shows the guard is what keeps the agent running above.
#[tokio::test]
async fn without_the_guard_the_same_run_is_stopped_and_moved() {
    let (report, h) = limit_like_text_then_work(Duration::from_secs(3600)).await;
    let labels: Vec<_> = report.runs.iter().map(|r| r.label.as_str()).collect();
    assert_eq!(labels, ["test.alpha", "test.beta"], "{}", h.screen.text());
    assert!(h.screen.text().contains("CONTINUED"));
}

#[tokio::test]
async fn a_resting_entry_is_skipped_at_start_and_priority_is_the_users() {
    let data = Scratch::new("resting");
    let mut table = Cooldowns::default();
    table.rest_until("test.alpha", START_MS + 60_000);
    table.save(&cooldown::path_in(&data.0)).unwrap();
    let mut h = harness(
        TWO_ENTRIES,
        vec![
            Fixture::new("test.alpha", "pty-limit"),
            Fixture::new("test.beta", "pty-continue"),
        ],
        &data.0,
        None,
        HashMap::new(),
    );
    let report = tokio::time::timeout(
        within(60),
        h.orchestrator.run(&mut h.io, Mode::Fresh { prompt: None }),
    )
    .await
    .unwrap_or_else(|_| panic!("timed out\n{}", h.screen.text()))
    .unwrap();
    let labels: Vec<_> = report.runs.iter().map(|r| r.label.as_str()).collect();
    assert_eq!(labels, ["test.beta"]);
    assert_eq!(report.switches, 0);

    // The same providers in the opposite order run in the opposite order.
    let data = Scratch::new("reversed");
    let reversed = r#"{"version":1,"permissions":{"approval":"ask_for_risky","filesystem":"workspace_write","network":"allowed"},
        "providers":[{"id":"test.beta"},{"id":"test.alpha"}]}"#;
    let mut h = harness(
        reversed,
        vec![
            Fixture::new("test.alpha", "pty-limit"),
            Fixture::new("test.beta", "pty-continue"),
        ],
        &data.0,
        None,
        HashMap::new(),
    );
    let report = tokio::time::timeout(
        within(60),
        h.orchestrator.run(&mut h.io, Mode::Fresh { prompt: None }),
    )
    .await
    .unwrap_or_else(|_| panic!("timed out\n{}", h.screen.text()))
    .unwrap();
    assert_eq!(report.runs[0].label, "test.beta");
}

#[tokio::test]
async fn failover_stops_when_disabled_or_when_there_is_only_one_entry() {
    for config in [
        r#"{"version":1,"failover":false,"permissions":{"approval":"ask_for_risky","filesystem":"workspace_write","network":"allowed"},
            "providers":[{"id":"test.alpha"},{"id":"test.beta"}]}"#,
        r#"{"version":1,"permissions":{"approval":"ask_for_risky","filesystem":"workspace_write","network":"allowed"},
            "providers":[{"id":"test.alpha"}]}"#,
    ] {
        let data = Scratch::new("no-failover");
        let mut h = harness(
            config,
            vec![
                Fixture::new("test.alpha", "pty-limit"),
                Fixture::new("test.beta", "pty-continue"),
            ],
            &data.0,
            None,
            HashMap::new(),
        );
        // The provider waits on the limit message forever; the user quits.
        let keys = h._keys.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let _ = keys.send(b"quit\r".to_vec());
        });
        let report = tokio::time::timeout(
            within(60),
            h.orchestrator.run(&mut h.io, Mode::Fresh { prompt: None }),
        )
        .await
        .unwrap_or_else(|_| panic!("timed out\n{}", h.screen.text()))
        .unwrap();
        assert_eq!(report.runs.len(), 1, "{config}");
        assert_eq!(report.switches, 0);
        assert!(!h.screen.text().contains("CONTINUED"));
    }
}

#[tokio::test]
async fn when_every_entry_is_resting_the_error_says_when_one_returns() {
    let data = Scratch::new("all-resting");
    let mut table = Cooldowns::default();
    table.rest_until("test.alpha", START_MS + 10 * 60_000);
    table.rest_until("test.beta", START_MS + 5 * 60_000);
    table.save(&cooldown::path_in(&data.0)).unwrap();
    let mut h = harness(
        TWO_ENTRIES,
        vec![
            Fixture::new("test.alpha", "pty-limit"),
            Fixture::new("test.beta", "pty-continue"),
        ],
        &data.0,
        None,
        HashMap::new(),
    );
    let error = h
        .orchestrator
        .run(&mut h.io, Mode::Fresh { prompt: None })
        .await
        .unwrap_err();
    let OrchestratorError::NoProvider {
        reasons,
        next_return_ms,
    } = &error
    else {
        panic!("unexpected {error}");
    };
    assert_eq!(*next_return_ms, Some(START_MS + 5 * 60_000));
    assert!(
        reasons
            .iter()
            .all(|r| r.contains("resting after a usage limit"))
    );
    assert!(h.orchestrator.supervisor().session().runs().is_empty());

    // Six minutes later beta has finished resting and is used.
    h.clock.fetch_add(6 * 60_000, Ordering::SeqCst);
    let report = tokio::time::timeout(
        within(60),
        h.orchestrator.run(&mut h.io, Mode::Fresh { prompt: None }),
    )
    .await
    .unwrap_or_else(|_| panic!("timed out\n{}", h.screen.text()))
    .unwrap();
    assert_eq!(report.runs[0].label, "test.beta");
}

#[tokio::test]
async fn an_entry_that_cannot_honor_the_ceiling_is_skipped_not_run() {
    struct Strict(Fixture);
    impl ProviderAdapter for Strict {
        fn descriptor(&self) -> &ProviderDescriptor {
            self.0.descriptor()
        }
        fn detect(&self, probe: &dyn HostProbe) -> Result<Installation, AdapterError> {
            self.0.detect(probe)
        }
        fn capabilities(&self, i: &Installation) -> CapabilitySet {
            self.0.capabilities(i)
        }
        fn map_permissions(
            &self,
            i: &Installation,
            requested: &PermissionPolicy,
        ) -> PermissionMapping {
            // Cannot restrict the network: reports the permissive value.
            let mut effective = *requested;
            effective.sandbox.network = NetworkAccess::Allowed;
            let _ = i;
            PermissionMapping::new(effective)
        }
        fn launch_plan(
            &self,
            i: &Installation,
            r: &LaunchRequest,
        ) -> Result<LaunchPlan, AdapterError> {
            self.0.launch_plan(i, r)
        }
        fn event_decoder(&self, t: TransportKind) -> Option<Box<dyn EventDecoder>> {
            self.0.event_decoder(t)
        }
    }
    let config = r#"{"version":1,
        "permissions":{"approval":"ask_for_risky","filesystem":"workspace_write","network":"denied"},
        "providers":[{"id":"test.strict"},{"id":"test.beta"}]}"#;
    let parsed = Config::parse(config).unwrap();
    let mut registry = ProviderRegistry::new();
    registry
        .register(Arc::new(Strict(Fixture::new(
            "test.strict",
            "pty-continue",
        ))))
        .unwrap();
    registry
        .register(Arc::new(Fixture::new("test.beta", "pty-continue")))
        .unwrap();
    let probe: Arc<dyn HostProbe> = Arc::new(LocalHost::new());
    let session = Session::new(
        SessionId::new("s-ceiling").unwrap(),
        std::env::current_dir().unwrap(),
        parsed.ceiling,
        Timestamp::from_unix_millis(START_MS),
    )
    .unwrap();
    let supervisor = Supervisor::new(registry, probe.clone(), session);
    let mut orchestrator = Orchestrator::new(
        supervisor,
        parsed,
        probe,
        Arc::new(uuldana_runtime::orchestrator::NoNativeContext),
        HashMap::new(),
    );
    let screen = Screen::default();
    let notices = Arc::new(Mutex::new(Vec::new()));
    let sink = notices.clone();
    let (_keys, input) = mpsc::unbounded_channel();
    let mut io = Io {
        input: Some(input),
        output: Box::new(screen.clone()),
        terminal_size: Box::new(|| Some(TerminalSize::new(50, 500))),
        cursor: cfg!(windows).then(|| Box::new(|| (1u16, 1u16)) as Box<_>),
        notice: Box::new(move |text| sink.lock().unwrap().push(text.to_owned())),
        agent: None,
        prefill: None,
        screen_text: None,
    };
    let report = tokio::time::timeout(
        within(60),
        orchestrator.run(&mut io, Mode::Fresh { prompt: None }),
    )
    .await
    .unwrap_or_else(|_| panic!("timed out\n{}", screen.text()))
    .unwrap();
    // The strict entry came first by priority, was refused, and never ran.
    let labels: Vec<_> = report.runs.iter().map(|r| r.label.as_str()).collect();
    assert_eq!(labels, ["test.beta"]);
    let notices = notices.lock().unwrap().join("\n");
    assert!(notices.contains("cannot start test.strict"), "{notices}");
    let run = &orchestrator.supervisor().session().runs()[0];
    assert_eq!(run.permissions().sandbox.network, NetworkAccess::Denied);
}

#[tokio::test]
async fn resume_continues_the_native_session_on_the_entry_that_owns_it() {
    let data = Scratch::new("resume");
    // First session: alpha runs and the process "dies" with the run active.
    let first = {
        let mut h = harness(
            TWO_ENTRIES,
            vec![
                Fixture::new("test.alpha", "pty-continue").resumable(),
                Fixture::new("test.beta", "pty-continue"),
            ],
            &data.0,
            None,
            HashMap::new(),
        );
        let report = tokio::time::timeout(
            within(60),
            h.orchestrator.run(&mut h.io, Mode::Fresh { prompt: None }),
        )
        .await
        .unwrap_or_else(|_| panic!("timed out\n{}", h.screen.text()))
        .unwrap();
        assert_eq!(report.runs[0].label, "test.alpha");
        h.orchestrator.supervisor().session().id().clone()
    };

    // Second start: restore from the journal and resume.
    let path = journal::path_for(&data.0, &first);
    let loaded = journal::load(&path).unwrap();
    assert_eq!(loaded.session.runs().len(), 1);
    let native = loaded.session.runs()[0].native_session().cloned();
    assert_eq!(native.as_ref().map(|n| n.as_str()), Some("alpha-native-1"));
    let mut h = harness(
        TWO_ENTRIES,
        vec![
            Fixture::new("test.alpha", "pty-continue").resumable(),
            Fixture::new("test.beta", "pty-continue"),
        ],
        &data.0,
        Some((loaded.session, loaded.events)),
        loaded.labels,
    );
    let report = tokio::time::timeout(within(60), h.orchestrator.run(&mut h.io, Mode::Resume))
        .await
        .unwrap_or_else(|_| panic!("timed out\n{}", h.screen.text()))
        .unwrap_or_else(|e| panic!("{e}\n{}", h.screen.text()));
    assert_eq!(report.runs.len(), 1);
    assert_eq!(report.runs[0].label, "test.alpha");
    let screen = h.screen.text();
    assert!(screen.contains("ARG|--resume"), "{screen}");
    assert!(screen.contains("ARG|alpha-native-1"), "{screen}");
    let session = h.orchestrator.supervisor().session();
    assert_eq!(session.runs().len(), 2);
    assert_eq!(
        session.runs()[1].native_session().map(|n| n.as_str()),
        Some("alpha-native-1")
    );
}

fn native_events(events: &[EventEnvelope], run: &str) -> Vec<(String, NativeSessionLink)> {
    events
        .iter()
        .filter(|e| e.run.as_str() == run)
        .filter_map(|e| match &e.event {
            AgentEvent::NativeSessionChanged { native, link } => {
                Some((native.as_str().to_owned(), *link))
            }
            AgentEvent::NativeSessionIdentified { native } => {
                Some((native.as_str().to_owned(), NativeSessionLink::Unspecified))
            }
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn preassigned_native_session_is_journaled_once_and_survives_a_restart() {
    let data = Scratch::new("preassigned");
    let adapters = || {
        vec![
            Fixture::new("test.alpha", "pty-continue")
                .resumable()
                .preassigned("pra-1"),
            Fixture::new("test.beta", "pty-continue"),
        ]
    };
    // The store knows nothing: the journal is the only source of the id.
    let empty = || {
        Arc::new(Store {
            conversations: HashMap::new(),
        })
    };

    let first = {
        let mut h = harness_with_store(
            TWO_ENTRIES,
            adapters(),
            &data.0,
            None,
            HashMap::new(),
            empty(),
        );
        tokio::time::timeout(
            within(60),
            h.orchestrator.run(&mut h.io, Mode::Fresh { prompt: None }),
        )
        .await
        .unwrap_or_else(|_| panic!("timed out\n{}", h.screen.text()))
        .unwrap();
        h.orchestrator.supervisor().session().id().clone()
    };

    // New process: only the journal survives.
    let path = journal::path_for(&data.0, &first);
    let loaded = journal::load(&path).unwrap();
    let run = &loaded.session.runs()[0];
    assert_eq!(run.native_session().map(|n| n.as_str()), Some("pra-1"));
    assert_eq!(run.native_sessions().len(), 1);
    assert_eq!(run.native_sessions()[0].link, NativeSessionLink::New);
    let first_run = run.id().as_str().to_owned();
    assert_eq!(
        native_events(&loaded.events, &first_run),
        [("pra-1".to_owned(), NativeSessionLink::New)]
    );

    let mut h = harness_with_store(
        TWO_ENTRIES,
        adapters(),
        &data.0,
        Some((loaded.session, loaded.events)),
        loaded.labels,
        empty(),
    );
    let report = tokio::time::timeout(within(60), h.orchestrator.run(&mut h.io, Mode::Resume))
        .await
        .unwrap_or_else(|_| panic!("timed out\n{}", h.screen.text()))
        .unwrap_or_else(|e| panic!("{e}\n{}", h.screen.text()));
    assert_eq!(report.runs.len(), 1);
    assert_eq!(report.runs[0].label, "test.alpha");
    let screen = h.screen.text();
    assert!(screen.contains("ARG|--resume"), "{screen}");
    assert!(screen.contains("ARG|pra-1"), "{screen}");
    // The original session was continued: no handoff prompt, no new id.
    assert!(!screen.contains("Original request"), "{screen}");
    let session = h.orchestrator.supervisor().session();
    assert_eq!(session.runs().len(), 2);
    assert_eq!(
        session.runs()[1].native_session().map(|n| n.as_str()),
        Some("pra-1")
    );
    assert_eq!(
        session.runs()[1].native_sessions()[0].link,
        NativeSessionLink::Resumed
    );
    drop(h);

    // The second run is journaled too: one native event per run, no repeats.
    let again = journal::load(&path).unwrap();
    assert_eq!(again.session.runs().len(), 2);
    for run in again.session.runs() {
        let events = native_events(&again.events, run.id().as_str());
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].0, "pra-1");
    }
}

#[tokio::test]
async fn resume_without_a_resumable_provider_carries_the_context_over() {
    let data = Scratch::new("resume-handoff");
    let first = {
        let mut h = harness(
            TWO_ENTRIES,
            vec![
                Fixture::new("test.alpha", "pty-continue"),
                Fixture::new("test.beta", "pty-continue"),
            ],
            &data.0,
            None,
            HashMap::new(),
        );
        tokio::time::timeout(
            within(60),
            h.orchestrator.run(&mut h.io, Mode::Fresh { prompt: None }),
        )
        .await
        .unwrap_or_else(|_| panic!("timed out\n{}", h.screen.text()))
        .unwrap();
        h.orchestrator.supervisor().session().id().clone()
    };
    let loaded = journal::load(&journal::path_for(&data.0, &first)).unwrap();
    let mut h = harness(
        TWO_ENTRIES,
        vec![
            // alpha cannot resume by id (the default fixture), so the work is
            // handed to the entry that is next by priority after the failed
            // resume attempt, with the visible conversation.
            Fixture::new("test.alpha", "pty-continue"),
            Fixture::new("test.beta", "pty-continue"),
        ],
        &data.0,
        Some((loaded.session, loaded.events)),
        loaded.labels,
    );
    let report = tokio::time::timeout(within(60), h.orchestrator.run(&mut h.io, Mode::Resume))
        .await
        .unwrap_or_else(|_| panic!("timed out\n{}", h.screen.text()))
        .unwrap_or_else(|e| panic!("{e}\n{}", h.screen.text()));
    assert!(!report.runs.is_empty());
    let screen = h.screen.text();
    assert!(screen.contains("ARG|User: perbaiki tes login"), "{screen}");
    let notices = h.notices.lock().unwrap().join("\n");
    assert!(notices.contains("cannot start test.alpha"), "{notices}");
}
