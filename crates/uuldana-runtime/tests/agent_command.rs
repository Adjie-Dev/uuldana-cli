//! The `/agent` command inside a supervised session, with real child processes
//! in real pseudo terminals. The fixture processes stand in for providers: one
//! stays up and repeats what it is typed, the other prints the prompt it was
//! started with.
mod common;
use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use uuldana_core::*;
use uuldana_runtime::{
    LocalHost, Supervisor,
    config::{Config, ProviderEntry},
    cooldown::{self, Cooldowns},
    journal::{self, Journal},
    orchestrator::{Io, Mode, NoNativeContext, Orchestrator, Report},
};

struct Fixture {
    descriptor: ProviderDescriptor,
    mode: &'static str,
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
        }
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
        Ok(plan)
    }
    fn resume_plan(
        &self,
        installation: &Installation,
        request: &ResumeRequest,
    ) -> Result<LaunchPlan, AdapterError> {
        self.launch_plan(installation, &request.launch)
    }
    fn event_decoder(&self, _: TransportKind) -> Option<Box<dyn EventDecoder>> {
        None
    }
    fn shutdown_plan(&self, _: TransportKind) -> ShutdownPlan {
        ShutdownPlan {
            steps: vec![ShutdownStep::WriteInput(b"quit\r".to_vec())],
            grace: Duration::from_secs(3),
        }
    }
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

    /// Whether the raw bytes contain `needle`, control sequences included.
    fn raw_contains(&self, needle: &str) -> bool {
        let bytes = self.0.lock().unwrap();
        String::from_utf8_lossy(&bytes).contains(needle)
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

const START_MS: u64 = 1_800_000_000_000;
const PERMISSIONS: &str = r#""permissions":{"approval":"ask_for_risky","filesystem":"workspace_write","network":"allowed"}"#;

fn config_text(providers: &[&str]) -> String {
    let list: Vec<String> = providers
        .iter()
        .map(|id| format!(r#"{{"id":"{id}"}}"#))
        .collect();
    format!(
        r#"{{"version":1,{PERMISSIONS},"providers":[{}]}}"#,
        list.join(",")
    )
}

struct Harness {
    orchestrator: Orchestrator,
    io: Io,
    screen: Screen,
    notices: Arc<Mutex<Vec<String>>>,
    keys: mpsc::UnboundedSender<Vec<u8>>,
    config_path: PathBuf,
}

/// `with_command` turns `/agent` on; `extras` are the detected agents offered
/// besides the configured ones.
fn harness(
    data: &Path,
    config: &str,
    adapters: Vec<Fixture>,
    with_command: bool,
    extras: Vec<ProviderEntry>,
) -> Harness {
    let config_path = data.join("config.json");
    std::fs::write(&config_path, config).unwrap();
    let parsed = Config::parse(config).unwrap();
    let mut registry = ProviderRegistry::new();
    for adapter in adapters {
        registry.register(Arc::new(adapter)).unwrap();
    }
    let probe: Arc<dyn HostProbe> = Arc::new(LocalHost::new());
    let session = Session::new(
        SessionId::new("s-agent-command").unwrap(),
        std::env::current_dir().unwrap(),
        parsed.ceiling,
        Timestamp::from_unix_millis(START_MS),
    )
    .unwrap();
    let journal = Journal::create(&journal::path_for(data, session.id()), &session).unwrap();
    let mut supervisor = Supervisor::with_history(registry, probe.clone(), session, Vec::new());
    supervisor.attach_journal(journal);
    let mut orchestrator = Orchestrator::new(
        supervisor,
        parsed,
        probe,
        Arc::new(NoNativeContext),
        HashMap::new(),
    )
    .with_cooldown_file(cooldown::path_in(data))
    .with_clock(Arc::new(|| START_MS));
    if with_command {
        orchestrator =
            orchestrator.with_agent_command(Some(config_path.clone()), move || extras.clone());
    }
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
        // Something was said to the agent, so there is a conversation to carry.
        screen_text: Some(Box::new(|| "User: nomor kasus 7731".to_owned())),
    };
    Harness {
        orchestrator,
        io,
        screen,
        notices,
        keys,
        config_path,
    }
}

fn extra(id: &str) -> ProviderEntry {
    ProviderEntry {
        id: ProviderId::new(id).unwrap(),
        label: id.to_owned(),
        env: Vec::new(),
        cooldown: Duration::from_secs(300 * 60),
        auto: false,
    }
}

async fn wait_for(screen: &Screen, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        if screen.text().contains(needle) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "never saw {needle:?}\n{}",
            screen.text()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Types `text` one key per read, then Enter.
async fn type_line(keys: &mpsc::UnboundedSender<Vec<u8>>, text: &str) {
    for byte in text.bytes() {
        keys.send(vec![byte]).unwrap();
        tokio::time::sleep(Duration::from_millis(4)).await;
    }
    keys.send(b"\r".to_vec()).unwrap();
}

/// Runs the session while `drive` plays the user. Fails with the screen when
/// the session does not finish.
async fn run_session<F: std::future::Future<Output = ()>>(h: &mut Harness, drive: F) -> Report {
    let screen = h.screen.clone();
    let (report, ()) = tokio::time::timeout(Duration::from_secs(90), async {
        tokio::join!(
            h.orchestrator.run(&mut h.io, Mode::Fresh { prompt: None }),
            drive
        )
    })
    .await
    .unwrap_or_else(|_| panic!("session timed out\n{}", screen.text()));
    report.unwrap_or_else(|e| panic!("{e}\n{}", screen.text()))
}

fn saved_labels(path: &Path) -> Vec<String> {
    Config::load(path)
        .unwrap()
        .providers
        .into_iter()
        .map(|p| p.label)
        .collect()
}

const TWO: [&str; 2] = ["test.alpha", "test.beta"];

fn two_fixtures() -> Vec<Fixture> {
    vec![
        Fixture::new("test.alpha", "pty-echo"),
        Fixture::new("test.beta", "pty-continue"),
    ]
}

#[tokio::test]
async fn picking_another_agent_first_moves_the_work_and_saves_the_order() {
    let data = Scratch::new("agent-switch");
    let mut h = harness(
        &data.0,
        &config_text(&TWO),
        two_fixtures(),
        true,
        Vec::new(),
    );
    let (keys, screen) = (h.keys.clone(), h.screen.clone());
    let report = run_session(&mut h, async move {
        wait_for(&screen, "READY").await;
        type_line(&keys, "/agent").await;
        wait_for(&screen, "Agent order (first agent to use)").await;
        // First test.beta, then test.alpha behind it.
        keys.send(b"2".to_vec()).unwrap();
        wait_for(&screen, "Agent order (second agent to use)").await;
        keys.send(b"\r".to_vec()).unwrap();
    })
    .await;

    let labels: Vec<_> = report.runs.iter().map(|r| r.label.as_str()).collect();
    assert_eq!(labels, ["test.alpha", "test.beta"]);
    assert_eq!(report.switches, 1);
    assert!(report.runs[0].end.is_some());
    let screen = h.screen.text();
    assert!(screen.contains("CONTINUED"), "{screen}");
    assert!(
        screen.contains("the user chose to switch agents"),
        "{screen}"
    );
    // The typed command never reached the first agent: it left on "quit"
    // instead of having to be forced, which it could not if "/agent" were
    // still sitting in its input.
    assert!(!screen.contains("ECHO|/agent"), "{screen}");
    assert!(!h.orchestrator.supervisor().last_stop_forced(), "{screen}");
    // The menu used the alternate screen and gave it back.
    assert!(h.screen.raw_contains("\x1b[?1049h"));
    assert!(h.screen.raw_contains("\x1b[?1049l"));
    assert_eq!(saved_labels(&h.config_path), ["test.beta", "test.alpha"]);
    let notices = h.notices.lock().unwrap().join("\n");
    assert!(
        notices.contains("agent order: test.beta > test.alpha"),
        "{notices}"
    );
    assert!(
        notices.contains("continuing on test.beta from test.alpha"),
        "{notices}"
    );
    let session = h.orchestrator.supervisor().session();
    for run in session.runs() {
        assert!(run.permissions().is_within(session.permission_ceiling()));
    }
}

/// Picks the second agent first, then "no more": the work moves by hand.
async fn move_to_beta(h: &mut Harness) -> Report {
    let (keys, screen) = (h.keys.clone(), h.screen.clone());
    run_session(h, async move {
        wait_for(&screen, "READY").await;
        type_line(&keys, "/agent").await;
        wait_for(&screen, "Agent order (first agent to use)").await;
        keys.send(b"2".to_vec()).unwrap();
        wait_for(&screen, "Agent order (second agent to use)").await;
        keys.send(b"\r".to_vec()).unwrap();
    })
    .await
}

#[tokio::test]
async fn moving_the_work_by_hand_puts_the_conversation_in_the_input_without_sending_it() {
    let data = Scratch::new("agent-prefill");
    let mut h = harness(
        &data.0,
        &config_text(&TWO),
        two_fixtures(),
        true,
        Vec::new(),
    );
    let pasted = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = pasted.clone();
    h.io.prefill = Some(Box::new(move |text| {
        sink.lock().unwrap().push(text.to_owned())
    }));
    let report = move_to_beta(&mut h).await;
    assert_eq!(report.switches, 1);
    let screen = h.screen.text();
    // The new agent was started with nothing to run, so nothing was spent.
    assert!(screen.contains("CONTINUED"), "{screen}");
    assert!(!screen.contains("ARG|"), "{screen}");
    let pasted = pasted.lock().unwrap();
    assert_eq!(pasted.len(), 1, "{pasted:?}");
    assert!(pasted[0].contains("the user chose to switch agents"));
    assert!(pasted[0].contains("nomor kasus 7731"));
}

#[tokio::test]
async fn when_nobody_spoke_to_the_agent_nothing_is_carried_and_nothing_runs() {
    let data = Scratch::new("agent-empty");
    let mut h = harness(
        &data.0,
        &config_text(&TWO),
        two_fixtures(),
        true,
        Vec::new(),
    );
    h.io.screen_text = Some(Box::new(String::new));
    let pasted = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = pasted.clone();
    h.io.prefill = Some(Box::new(move |text| {
        sink.lock().unwrap().push(text.to_owned())
    }));
    let report = move_to_beta(&mut h).await;
    assert_eq!(report.switches, 1);
    let screen = h.screen.text();
    assert!(screen.contains("CONTINUED"), "{screen}");
    assert!(!screen.contains("ARG|"), "{screen}");
    assert!(pasted.lock().unwrap().is_empty());
}

#[tokio::test]
async fn without_a_way_to_paste_the_conversation_is_the_first_prompt_as_before() {
    let data = Scratch::new("agent-noprefill");
    let mut h = harness(
        &data.0,
        &config_text(&TWO),
        two_fixtures(),
        true,
        Vec::new(),
    );
    let report = move_to_beta(&mut h).await;
    assert_eq!(report.switches, 1);
    let screen = h.screen.text();
    assert!(
        screen.contains("the user chose to switch agents"),
        "{screen}"
    );
    assert!(screen.contains("nomor kasus 7731"), "{screen}");
}

#[tokio::test]
async fn escape_leaves_the_menu_and_the_agent_keeps_working() {
    let data = Scratch::new("agent-cancel");
    let mut h = harness(
        &data.0,
        &config_text(&TWO),
        two_fixtures(),
        true,
        Vec::new(),
    );
    let before = std::fs::read_to_string(&h.config_path).unwrap();
    let (keys, screen) = (h.keys.clone(), h.screen.clone());
    let report = run_session(&mut h, async move {
        wait_for(&screen, "READY").await;
        type_line(&keys, "/agent").await;
        wait_for(&screen, "Agent order (first agent to use)").await;
        keys.send(b"\x1b".to_vec()).unwrap();
        // Typing reaches the agent again once the menu is gone.
        tokio::time::sleep(Duration::from_millis(400)).await;
        type_line(&keys, "halo").await;
        wait_for(&screen, "ECHO|halo").await;
        type_line(&keys, "quit").await;
    })
    .await;

    let labels: Vec<_> = report.runs.iter().map(|r| r.label.as_str()).collect();
    assert_eq!(labels, ["test.alpha"]);
    assert_eq!(report.switches, 0);
    assert_eq!(report.exit.map(|e| e.success), Some(true));
    assert_eq!(std::fs::read_to_string(&h.config_path).unwrap(), before);
    assert!(!h.screen.text().contains("ECHO|/agent"));
}

#[tokio::test]
async fn keeping_the_running_agent_first_changes_only_what_follows() {
    let data = Scratch::new("agent-stay");
    let config = config_text(&["test.alpha", "test.beta", "test.gamma"]);
    let mut fixtures = two_fixtures();
    fixtures.push(Fixture::new("test.gamma", "pty-continue"));
    let mut h = harness(&data.0, &config, fixtures, true, Vec::new());
    let (keys, screen) = (h.keys.clone(), h.screen.clone());
    let report = run_session(&mut h, async move {
        wait_for(&screen, "READY").await;
        type_line(&keys, "/agent").await;
        wait_for(&screen, "Agent order (first agent to use)").await;
        // alpha stays first; gamma is second, beta is left where it was.
        keys.send(b"1".to_vec()).unwrap();
        wait_for(&screen, "Agent order (second agent to use)").await;
        keys.send(b"2".to_vec()).unwrap();
        wait_for(&screen, "Agent order (third agent to use)").await;
        keys.send(b"\r".to_vec()).unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        type_line(&keys, "halo").await;
        wait_for(&screen, "ECHO|halo").await;
        type_line(&keys, "quit").await;
    })
    .await;

    // No switch, and the agent still got its input afterwards.
    let labels: Vec<_> = report.runs.iter().map(|r| r.label.as_str()).collect();
    assert_eq!(labels, ["test.alpha"]);
    assert_eq!(report.switches, 0);
    assert_eq!(
        saved_labels(&h.config_path),
        ["test.alpha", "test.gamma", "test.beta"]
    );
    // The agent was nudged to repaint: a size change and the restored size.
    assert!(h.screen.text().contains("ECHO|halo"));
}

#[tokio::test]
async fn an_installed_agent_outside_the_config_can_be_added_by_choosing_it() {
    let data = Scratch::new("agent-extra");
    let mut h = harness(
        &data.0,
        &config_text(&["test.alpha"]),
        two_fixtures(),
        true,
        vec![extra("test.beta")],
    );
    let (keys, screen) = (h.keys.clone(), h.screen.clone());
    let report = run_session(&mut h, async move {
        wait_for(&screen, "READY").await;
        type_line(&keys, "/agent").await;
        wait_for(&screen, "not in config.json yet").await;
        keys.send(b"2".to_vec()).unwrap();
        wait_for(&screen, "Agent order (second agent to use)").await;
        // "No more": the rest keeps its order.
        keys.send(b"2".to_vec()).unwrap();
    })
    .await;

    let labels: Vec<_> = report.runs.iter().map(|r| r.label.as_str()).collect();
    assert_eq!(labels, ["test.alpha", "test.beta"]);
    assert_eq!(saved_labels(&h.config_path), ["test.beta", "test.alpha"]);
}

#[tokio::test]
async fn choosing_a_resting_agent_first_ends_its_rest() {
    let data = Scratch::new("agent-resting");
    let mut table = Cooldowns::default();
    table.rest_until("test.beta", START_MS + 90 * 60_000);
    table.save(&cooldown::path_in(&data.0)).unwrap();
    let mut h = harness(
        &data.0,
        &config_text(&TWO),
        two_fixtures(),
        true,
        Vec::new(),
    );
    let (keys, screen) = (h.keys.clone(), h.screen.clone());
    let report = run_session(&mut h, async move {
        wait_for(&screen, "READY").await;
        type_line(&keys, "/agent").await;
        wait_for(&screen, "about 90 min left").await;
        keys.send(b"2".to_vec()).unwrap();
        wait_for(&screen, "Agent order (second agent to use)").await;
        keys.send(b"\r".to_vec()).unwrap();
    })
    .await;

    let labels: Vec<_> = report.runs.iter().map(|r| r.label.as_str()).collect();
    assert_eq!(labels, ["test.alpha", "test.beta"]);
    assert!(!h.orchestrator.cooldowns().is_resting("test.beta", START_MS));
    assert!(!Cooldowns::load(&cooldown::path_in(&data.0)).is_resting("test.beta", START_MS));
}

#[tokio::test]
async fn with_a_single_agent_the_command_only_says_so() {
    let data = Scratch::new("agent-single");
    let mut h = harness(
        &data.0,
        &config_text(&["test.alpha"]),
        two_fixtures(),
        true,
        Vec::new(),
    );
    let (keys, screen) = (h.keys.clone(), h.screen.clone());
    let report = run_session(&mut h, async move {
        wait_for(&screen, "READY").await;
        type_line(&keys, "/agent").await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        type_line(&keys, "halo").await;
        wait_for(&screen, "ECHO|halo").await;
        type_line(&keys, "quit").await;
    })
    .await;
    assert_eq!(report.runs.len(), 1);
    let notices = h.notices.lock().unwrap().join("\n");
    assert!(notices.contains("only one agent is set up"), "{notices}");
    assert!(!h.screen.raw_contains("\x1b[?1049h"));
}

#[tokio::test]
async fn without_the_command_enabled_the_line_goes_to_the_agent_as_typed() {
    // The control for the tests above: the same keys, nothing intercepting.
    let data = Scratch::new("agent-off");
    let mut h = harness(
        &data.0,
        &config_text(&TWO),
        two_fixtures(),
        false,
        Vec::new(),
    );
    let (keys, screen) = (h.keys.clone(), h.screen.clone());
    let report = run_session(&mut h, async move {
        wait_for(&screen, "READY").await;
        type_line(&keys, "/agent").await;
        wait_for(&screen, "ECHO|/agent").await;
        type_line(&keys, "quit").await;
    })
    .await;
    assert_eq!(report.runs.len(), 1);
    assert!(!h.screen.text().contains("Agent order"));
}
