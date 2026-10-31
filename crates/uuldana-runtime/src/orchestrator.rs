//! Runs one supervised session across the configured providers.
//!
//! The orchestrator starts the highest priority entry that is available,
//! relays the user's terminal to it and watches for a usage limit. When an
//! entry runs out of quota it rests that entry, carries the visible context to
//! the next entry in the user's priority order and continues there, with no
//! Ctrl+C, no repeated prompt and no new sign-in from the user. Every run
//! stays inside the session's permission ceiling, which failover never raises.
//!
//! What it does not do: it never edits provider configuration, never reads
//! credentials, and never claims hidden reasoning, memory or approvals moved
//! with the work.

use std::{
    collections::{HashMap, HashSet},
    fmt,
    io::Write,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use tokio::sync::mpsc;
use uuldana_core::*;

use crate::{
    Error, StartOptions, Supervisor,
    agent_menu::{AgentMenu, Candidate, Step, parse_keys},
    config::{self, Config, ProviderEntry},
    cooldown::Cooldowns,
    handoff,
    host::EnvOverlay,
    relay::{CursorPosition, RelayEnd, RelayOptions, relay_with},
};

/// Provider-specific knowledge the orchestrator cannot have itself: how to read
/// a provider's own session store. Implemented next to the adapters.
pub trait NativeContext: Send + Sync {
    /// The visible conversation of a native session, if its store is readable.
    fn conversation(
        &self,
        provider: &ProviderId,
        probe: &dyn HostProbe,
        workspace: &std::path::Path,
        native: &NativeSessionId,
    ) -> Option<Vec<TranscriptEntry>>;

    /// The native session a provider created in `workspace` since `since`, for
    /// providers that choose their own id.
    fn discover(
        &self,
        provider: &ProviderId,
        probe: &dyn HostProbe,
        workspace: &std::path::Path,
        since: Timestamp,
    ) -> Option<NativeSessionId>;
}

/// A [`NativeContext`] for providers with no readable store.
pub struct NoNativeContext;

impl NativeContext for NoNativeContext {
    fn conversation(
        &self,
        _: &ProviderId,
        _: &dyn HostProbe,
        _: &std::path::Path,
        _: &NativeSessionId,
    ) -> Option<Vec<TranscriptEntry>> {
        None
    }
    fn discover(
        &self,
        _: &ProviderId,
        _: &dyn HostProbe,
        _: &std::path::Path,
        _: Timestamp,
    ) -> Option<NativeSessionId> {
        None
    }
}

/// The user's terminal as the orchestrator sees it.
pub struct Io {
    /// Keyboard bytes. Taken for each run and given back, so one stream spans
    /// every provider of the session.
    pub input: Option<mpsc::UnboundedReceiver<Vec<u8>>>,
    /// Where the provider's output goes.
    pub output: Box<dyn Write + Send>,
    /// Current terminal size.
    pub terminal_size: Box<dyn FnMut() -> Option<TerminalSize> + Send>,
    /// Cursor position source, on hosts whose pseudo terminal asks for it.
    pub cursor: Option<Box<dyn FnMut() -> (u16, u16) + Send>>,
    /// Receives messages for the user, for example "continuing on another agent".
    pub notice: Box<dyn FnMut(&str) + Send>,
    /// Told the label of each agent as it starts, and whether it was given a
    /// first prompt, for a screen that shows which one is working.
    pub agent: Option<AgentStarted>,
    /// Puts text in the new agent's input box without sending it, when the user
    /// moved the work themselves. Without it the text is the agent's first
    /// prompt, which it starts working on at once.
    pub prefill: Option<Notify>,
    /// Reads the plain text of the agent's screen. Asked for when an agent
    /// stops and its work moves on, so an agent whose own history cannot be
    /// read still hands over what was said.
    pub screen_text: Option<ScreenText>,
}

/// A callback that receives a line of text.
pub type Notify = Box<dyn FnMut(&str) + Send>;

/// A callback told that an agent started: its label, and whether it was given
/// a first prompt.
pub type AgentStarted = Box<dyn FnMut(&str, bool) + Send>;

/// A callback that returns text.
pub type ScreenText = Box<dyn FnMut() -> String + Send>;

/// How to begin.
pub enum Mode {
    /// A new piece of work, with an optional first prompt.
    Fresh {
        /// First prompt for the provider.
        prompt: Option<String>,
    },
    /// Continue the session that was restored into the supervisor.
    Resume,
}

/// One run of a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    /// Configured entry that ran.
    pub label: String,
    /// Run identifier.
    pub run: RunId,
    /// How the run ended.
    pub end: Option<RunEnd>,
}

/// What happened during [`Orchestrator::run`].
#[derive(Debug, Default)]
pub struct Report {
    /// Runs in order.
    pub runs: Vec<RunReport>,
    /// Exit status of the last provider, when observed.
    pub exit: Option<ExitInfo>,
    /// How many times work moved to another entry.
    pub switches: usize,
}

/// Why a session could not run or continue.
#[derive(Debug)]
pub enum OrchestratorError {
    /// No configured entry can run right now.
    NoProvider {
        /// One line per entry saying why it was not used.
        reasons: Vec<String>,
        /// When the first resting entry returns, in Unix milliseconds.
        next_return_ms: Option<u64>,
    },
    /// The session has no run to resume.
    NothingToResume,
    /// A runtime failure that ended the session.
    Runtime(Error),
}

impl fmt::Display for OrchestratorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoProvider { reasons, .. } => {
                f.write_str("no configured provider can run now")?;
                for reason in reasons {
                    write!(f, "\n  {reason}")?;
                }
                Ok(())
            }
            Self::NothingToResume => f.write_str("this session has no run to resume"),
            Self::Runtime(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for OrchestratorError {}

impl From<Error> for OrchestratorError {
    fn from(error: Error) -> Self {
        Self::Runtime(error)
    }
}

/// The stricter of two policies in every dimension.
pub fn stricter(a: &PermissionPolicy, b: &PermissionPolicy) -> PermissionPolicy {
    PermissionPolicy {
        approval: if a.approval.is_at_most(b.approval) {
            a.approval
        } else {
            b.approval
        },
        sandbox: SandboxPolicy {
            filesystem: if a.sandbox.filesystem.is_at_most(b.sandbox.filesystem) {
                a.sandbox.filesystem
            } else {
                b.sandbox.filesystem
            },
            network: if a.sandbox.network.is_at_most(b.sandbox.network) {
                a.sandbox.network
            } else {
                b.sandbox.network
            },
        },
    }
}

/// Drives a session across the configured providers.
pub struct Orchestrator {
    supervisor: Supervisor,
    config: Config,
    probe: Arc<dyn HostProbe>,
    context: Arc<dyn NativeContext>,
    cooldowns: Cooldowns,
    cooldown_path: Option<PathBuf>,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    labels: HashMap<RunId, String>,
    policy: PermissionPolicy,
    settle: Duration,
    /// How long an agent may keep working after text that looked like a usage
    /// limit before that text is taken for something else (a quotation, a
    /// file it printed, something the user typed).
    keeps_working_after: Duration,
    agent_command: Option<AgentCommand>,
    /// The running agent has the terminal's alternate screen on.
    alt_screen: AtomicBool,
    /// What the screen of the agent that just stopped showed, for the handoff.
    last_screen: Option<String>,
}

/// What the `/agent` command needs besides the configuration.
struct AgentCommand {
    /// The file the chosen order is saved to, when there is one.
    config_path: Option<PathBuf>,
    /// Finds agents on this machine that the configuration does not list. The
    /// user may add one by choosing it. Looking costs a moment, so it runs
    /// only when the menu opens.
    extras: Arc<dyn Fn() -> Vec<ProviderEntry> + Send + Sync>,
}

/// Output this recent counts as the agent still working.
const ACTIVE_WITHIN: Duration = Duration::from_millis(400);

/// What came of the `/agent` menu.
enum AgentChoice {
    /// Keep working in the running agent. `printed` is set when a notice was
    /// written to the terminal on the way.
    Stay { printed: bool },
    /// Move the work to the agent that is now first.
    Switch,
}

fn system_clock() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// What the next start should do.
struct Next {
    prompt: Option<String>,
    /// `prompt` is put in the input box for the user to send, not run.
    prefill: bool,
    resume: Option<(String, NativeSessionId)>,
    /// Entry that continues the most recent conversation of the workspace,
    /// for an agent whose session id was never learned.
    resume_latest: Option<String>,
    /// Run whose context a handoff came from, for the notice.
    from: Option<String>,
}

impl Orchestrator {
    /// Creates an orchestrator. `labels` names the entry of each earlier run of
    /// a restored session (see the journal).
    pub fn new(
        supervisor: Supervisor,
        config: Config,
        probe: Arc<dyn HostProbe>,
        context: Arc<dyn NativeContext>,
        labels: HashMap<RunId, String>,
    ) -> Self {
        // Failover can never exceed the session's ceiling, and a configuration
        // that became stricter since the session began applies at once.
        let policy = stricter(&config.ceiling, supervisor.session().permission_ceiling());
        Self {
            supervisor,
            config,
            probe,
            context,
            cooldowns: Cooldowns::default(),
            cooldown_path: None,
            clock: Arc::new(system_clock),
            labels,
            policy,
            settle: Duration::from_millis(1500),
            keeps_working_after: Duration::from_secs(20),
            agent_command: None,
            alt_screen: AtomicBool::new(false),
            last_screen: None,
        }
    }

    /// Turns on the `/agent` command: typing it as a whole line opens a menu
    /// for choosing which agent works first and which take over after it. The
    /// order is saved to `config_path` when given. `extras` finds agents the
    /// configuration does not list yet; choosing one adds it.
    pub fn with_agent_command(
        mut self,
        config_path: Option<PathBuf>,
        extras: impl Fn() -> Vec<ProviderEntry> + Send + Sync + 'static,
    ) -> Self {
        self.agent_command = Some(AgentCommand {
            config_path,
            extras: Arc::new(extras),
        });
        self
    }

    /// Loads and persists cooldowns at `path`.
    pub fn with_cooldown_file(mut self, path: PathBuf) -> Self {
        self.cooldowns = Cooldowns::load(&path);
        self.cooldown_path = Some(path);
        self
    }

    /// Replaces the clock (Unix milliseconds); for tests.
    pub fn with_clock(mut self, clock: Arc<dyn Fn() -> u64 + Send + Sync>) -> Self {
        self.clock = clock;
        self
    }

    /// How long a provider must be quiet after printing a limit message before
    /// the work moves on. Shorter than the default only in tests.
    pub fn with_keeps_working_after(mut self, keeps_working_after: Duration) -> Self {
        self.keeps_working_after = keeps_working_after;
        self
    }

    /// Sets how long the output must be quiet after a usage-limit message
    /// before the agent is stopped.
    pub fn with_settle(mut self, settle: Duration) -> Self {
        self.settle = settle;
        self
    }

    /// The supervisor, for inspection after the session.
    pub fn supervisor(&self) -> &Supervisor {
        &self.supervisor
    }

    /// The cooldown table, for inspection.
    pub fn cooldowns(&self) -> &Cooldowns {
        &self.cooldowns
    }

    /// The policy every run of this session uses.
    pub fn policy(&self) -> &PermissionPolicy {
        &self.policy
    }

    fn now(&self) -> u64 {
        (self.clock)()
    }

    fn entry_probe(&self, entry: &ProviderEntry) -> EnvOverlay {
        EnvOverlay::new(self.probe.clone(), entry.env.clone())
    }

    fn label_of(&self, run: &RunId) -> Option<String> {
        self.labels.get(run).cloned()
    }

    fn entry_for_run(&self, run: &RunId) -> Option<&ProviderEntry> {
        let record = self.supervisor.session().run(run)?;
        let label = self.label_of(run);
        self.config
            .providers
            .iter()
            .find(|e| Some(&e.label) == label.as_ref())
            .or_else(|| {
                self.config
                    .providers
                    .iter()
                    .find(|e| &e.id == record.provider())
            })
    }

    /// Learns the native session of a run whose provider chooses its own id.
    fn capture(&mut self, run: &RunId) {
        let Some(record) = self.supervisor.session().run(run) else {
            return;
        };
        if record.native_session().is_some() {
            return;
        }
        let (provider, since) = (record.provider().clone(), record.started_at());
        let Some(entry) = self.entry_for_run(run).cloned() else {
            return;
        };
        let workspace = self.supervisor.session().workspace().to_path_buf();
        let probe = self.entry_probe(&entry);
        if let Some(native) = self.context.discover(&provider, &probe, &workspace, since) {
            let _ = self.supervisor.note_native_session(run, native);
        }
    }

    /// Builds the handoff prompt for work that moves on from `run`. `None` when
    /// there is nothing to carry: nobody spoke to the agent, so a prompt would
    /// only make the next agent spend a turn on nothing.
    fn handoff_prompt(&mut self, run: &RunId, reason: &str) -> Option<String> {
        self.capture(run);
        let session = self.supervisor.session();
        let workspace = session.workspace().to_path_buf();
        let record = session.run(run).expect("known run");
        let provider = record.provider().clone();
        let native = record.native_session().cloned();
        let label = self
            .label_of(run)
            .unwrap_or_else(|| provider.as_str().to_owned());
        let mut package = HandoffPackage::new(workspace.clone());
        package.source = record.native_ref();
        let stored = match (&native, self.entry_for_run(run).cloned()) {
            (Some(native), Some(entry)) => {
                let probe = self.entry_probe(&entry);
                self.context
                    .conversation(&provider, &probe, &workspace, native)
            }
            _ => None,
        };
        let transcript = stored
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| handoff::transcript_from_events(self.supervisor.events(), run));
        package.objective = transcript
            .iter()
            .find(|entry| entry.role == Role::User)
            .map(|entry| entry.text.chars().take(500).collect());
        package.transcript = transcript;
        let screen = self
            .last_screen
            .take()
            .filter(|text| !text.trim().is_empty())
            .or_else(|| handoff::screen_from_events(self.supervisor.events(), run));
        if package.transcript.is_empty() && screen.is_none() {
            return None;
        }
        // The agent that takes over is not shown the text that stopped the one
        // before it: it would show it back, and that would look like a limit
        // of its own.
        Some(crate::universal_limit::redact_limit_text(
            &handoff::render_with_screen(&package, &label, reason, screen.as_deref()),
        ))
    }

    fn unavailable(&self, failed: &HashSet<String>, now: u64) -> OrchestratorError {
        let reasons = self
            .config
            .providers
            .iter()
            .map(|entry| {
                if let Some(until) = self.cooldowns.returns_at(&entry.label, now) {
                    format!(
                        "{}: resting after a usage limit for another {} minutes",
                        entry.label,
                        (until - now).div_ceil(60_000)
                    )
                } else if failed.contains(&entry.label) {
                    format!("{}: could not be started", entry.label)
                } else {
                    format!("{}: not available", entry.label)
                }
            })
            .collect();
        OrchestratorError::NoProvider {
            reasons,
            next_return_ms: self.cooldowns.next_return(now),
        }
    }

    fn pick(&self, next: &Next, failed: &HashSet<String>, now: u64) -> Option<usize> {
        let usable = |entry: &ProviderEntry| {
            !failed.contains(&entry.label) && !self.cooldowns.is_resting(&entry.label, now)
        };
        // Native resume only works on the entry whose account owns the session.
        let wanted = next
            .resume
            .as_ref()
            .map(|(label, _)| label)
            .or(next.resume_latest.as_ref());
        if let Some(label) = wanted
            && let Some(index) = self
                .config
                .providers
                .iter()
                .position(|e| &e.label == label && usable(e))
        {
            return Some(index);
        }
        self.config.providers.iter().position(usable)
    }

    /// Runs the session until the user's work ends, the providers run out, or
    /// an error stops it.
    pub async fn run(&mut self, io: &mut Io, mode: Mode) -> Result<Report, OrchestratorError> {
        let mut report = Report::default();
        let mut failed: HashSet<String> = HashSet::new();
        let mut next = match mode {
            Mode::Fresh { prompt } => Next {
                prompt,
                prefill: false,
                resume: None,
                resume_latest: None,
                from: None,
            },
            Mode::Resume => self.plan_resume(io)?,
        };
        let failover = self.config.failover && self.config.providers.len() > 1;
        loop {
            let now = self.now();
            self.cooldowns.forget_ended(now);
            let Some(index) = self.pick(&next, &failed, now) else {
                return Err(self.unavailable(&failed, now));
            };
            let entry = self.config.providers[index].clone();
            let run = RunId::new(format!(
                "run-{}",
                self.supervisor.session().runs().len() + 1
            ))
            .expect("valid run id");
            let mut options = StartOptions::new(entry.id.clone(), run.clone());
            options.permissions = self.policy;
            options.transport = Some(TransportKind::NativePty);
            options.terminal_size = (io.terminal_size)();
            options.env = entry.env.clone();
            options.label = Some(entry.label.clone());
            let resuming = matches!(&next.resume, Some((label, _)) if label == &entry.label);
            if resuming {
                options.native_resume = next.resume.as_ref().map(|(_, native)| native.clone());
            }
            let resuming_latest = next.resume_latest.as_deref() == Some(entry.label.as_str());
            options.continue_latest = resuming_latest;
            let resuming = resuming || resuming_latest;
            // Moved by the user and able to show the text in the input box:
            // the agent is not told to start working.
            let paste = !resuming && next.prefill && io.prefill.is_some();
            if !resuming && !paste {
                options.initial_prompt = next.prompt.clone();
            }
            let first_prompt = !resuming && !paste && next.prompt.is_some();
            if let Err(error) = self.supervisor.start(options).await {
                (io.notice)(&format!("cannot start {}: {error}", entry.label));
                failed.insert(entry.label.clone());
                // A resume that cannot happen becomes a handoff.
                if resuming && let Some(previous) = self.supervisor.session().runs().last() {
                    let previous = previous.id().clone();
                    next = Next {
                        prompt: self.handoff_prompt(&previous, "it could not be resumed"),
                        prefill: false,
                        resume: None,
                        resume_latest: None,
                        from: Some(entry.label.clone()),
                    };
                }
                continue;
            }
            self.labels.insert(run.clone(), entry.label.clone());
            if let Some(agent) = io.agent.as_mut() {
                agent(&entry.label, first_prompt);
            }
            if paste
                && let (Some(text), Some(prefill)) = (next.prompt.as_deref(), io.prefill.as_mut())
            {
                prefill(text);
            }
            if let Some(from) = &next.from {
                (io.notice)(&format!("continuing on {} from {from}", entry.label));
            }
            let outcome = self.relay_run(io, &run, failover).await?;
            self.last_screen = io.screen_text.as_mut().map(|read| read());
            if let Some(text) = self.last_screen.clone().filter(|t| !t.trim().is_empty()) {
                self.supervisor.note_screen(&run, text);
            }
            self.capture(&run);
            let end = match self.supervisor.session().run(&run).map(RunRecord::state) {
                Some(RunState::Ended(end)) => Some(end),
                _ => None,
            };
            report.runs.push(RunReport {
                label: entry.label.clone(),
                run: run.clone(),
                end,
            });
            if let RelayEnd::ProviderExited(Some(exit)) = outcome.end {
                report.exit = Some(exit);
            }
            if outcome.switched {
                // The user's own choice: no rest for the agent that is left,
                // and it works even with failover switched off.
                next = Next {
                    prompt: self.handoff_prompt(&run, "the user chose to switch agents"),
                    prefill: true,
                    resume: None,
                    resume_latest: None,
                    from: Some(entry.label.clone()),
                };
                report.switches += 1;
                continue;
            }
            let failing_exit =
                matches!(outcome.end, RelayEnd::ProviderExited(Some(e)) if !e.success);
            let should_switch = failover
                && outcome.limit.is_some()
                && (matches!(outcome.end, RelayEnd::Stopped) || failing_exit);
            if !should_switch {
                return Ok(report);
            }
            let signal = outcome.limit.expect("checked");
            let now = self.now();
            let until = signal
                .resets_at
                .map(Timestamp::unix_millis)
                .filter(|at| *at > now)
                .unwrap_or(now + entry.cooldown.as_millis() as u64);
            self.cooldowns.rest_until(&entry.label, until);
            if let Some(path) = &self.cooldown_path
                && let Err(error) = self.cooldowns.save(path)
            {
                (io.notice)(&format!("cannot save cooldowns: {error}"));
            }
            (io.notice)(&format!(
                "{} reached its usage limit; resting it and moving on",
                entry.label
            ));
            next = Next {
                prompt: self.handoff_prompt(&run, "it could no longer be used on this account"),
                prefill: false,
                resume: None,
                resume_latest: None,
                from: Some(entry.label.clone()),
            };
            report.switches += 1;
        }
    }

    fn plan_resume(&mut self, io: &mut Io) -> Result<Next, OrchestratorError> {
        let Some(last) = self.supervisor.session().runs().last() else {
            return Err(OrchestratorError::NothingToResume);
        };
        let last = last.id().clone();
        self.capture(&last);
        let record = self.supervisor.session().run(&last).expect("known run");
        let entry_label = self
            .entry_for_run(&last)
            .map(|e| e.label.clone())
            .or_else(|| self.label_of(&last));
        let native = record.native_session().cloned();
        match (entry_label, native) {
            (Some(label), Some(native)) => {
                (io.notice)(&format!("resuming {label}"));
                Ok(Next {
                    prompt: None,
                    prefill: false,
                    resume: Some((label, native)),
                    resume_latest: None,
                    from: None,
                })
            }
            (Some(label), None) => {
                // No session id was learned for this agent. It is asked to
                // continue the latest conversation of this folder instead; if
                // it cannot, the start fails and a handoff takes over.
                (io.notice)(&format!(
                    "resuming {label}: its latest conversation in this folder"
                ));
                Ok(Next {
                    prompt: None,
                    prefill: false,
                    resume: None,
                    resume_latest: Some(label),
                    from: None,
                })
            }
            (None, _) => {
                (io.notice)(
                    "the previous provider session cannot be resumed; carrying its context over",
                );
                Ok(Next {
                    prompt: self.handoff_prompt(&last, "the session was interrupted"),
                    prefill: false,
                    resume: None,
                    resume_latest: None,
                    from: None,
                })
            }
        }
    }

    async fn relay_run(
        &mut self,
        io: &mut Io,
        run: &RunId,
        failover: bool,
    ) -> Result<Outcome, OrchestratorError> {
        let start = self.supervisor.events().len();
        let settle = self.settle;
        let keeps_working = self.keeps_working_after;
        let mut seen_at: Option<Instant> = None;
        // Signals before this event are disregarded: the agent went on working
        // after them, so they were not a stop.
        let mut floor = start;
        let mut worked_after = Duration::ZERO;
        let mut last_call = Instant::now();
        // Signals that were set aside, to be written to the journal.
        let mut dropped: Vec<String> = Vec::new();
        self.alt_screen.store(false, Ordering::Relaxed);
        loop {
            let input = io
                .input
                .take()
                .unwrap_or_else(|| mpsc::unbounded_channel().1);
            let cursor: Option<CursorPosition<'_>> = io.cursor.as_deref_mut().map(|c| c as _);
            let options = RelayOptions {
                agent_command: self.agent_command.is_some(),
                alt_screen: Some(&self.alt_screen),
            };
            let (result, input) = relay_with(
                options,
                &mut self.supervisor,
                input,
                &mut io.output,
                &mut io.terminal_size,
                cursor,
                |supervisor, idle| {
                    let now = Instant::now();
                    let since = now.duration_since(last_call).min(Duration::from_secs(1));
                    last_call = now;
                    let found = limit_in(supervisor, floor);
                    if !failover || found.is_none() {
                        seen_at = None;
                        worked_after = Duration::ZERO;
                        return false;
                    }
                    let first = *seen_at.get_or_insert(now);
                    if idle < ACTIVE_WITHIN {
                        worked_after += since;
                    }
                    if worked_after >= keeps_working {
                        if let Some(signal) = found {
                            dropped.push(format!(
                                "usage limit text ignored: the agent kept working for {} s after it: {}",
                                keeps_working.as_secs(),
                                signal.message.unwrap_or_default()
                            ));
                            (io.notice)("text like a usage limit was ignored: the agent went on working");
                        }
                        floor = supervisor.events().len();
                        seen_at = None;
                        worked_after = Duration::ZERO;
                        return false;
                    }
                    idle >= settle && first.elapsed() >= settle
                },
            )
            .await;
            io.input = input;
            for message in dropped.drain(..) {
                self.supervisor.note(message);
            }
            let mut end = result?;
            let mut switched = false;
            if end == RelayEnd::AgentMenu {
                match self.choose_agent(io, run).await {
                    AgentChoice::Stay { printed } => {
                        // The terminal brings back the main screen as it was,
                        // and an agent that draws there needs nothing more. A
                        // repaint clears it, banner included, so it is asked
                        // only when the screen cannot be trusted: the agent
                        // draws on the alternate screen, which the menu used,
                        // or a notice was written over it.
                        if printed || self.alt_screen.load(Ordering::Relaxed) {
                            self.redraw(io).await;
                        }
                        continue;
                    }
                    AgentChoice::Switch => {
                        let _ = self.supervisor.stop().await;
                        end = RelayEnd::Stopped;
                        switched = true;
                    }
                }
            }
            return Ok(Outcome {
                end,
                limit: limit_in(&self.supervisor, floor)
                    .filter(|_| self.supervisor.session().run(run).is_some()),
                switched,
            });
        }
    }

    /// Shows the `/agent` menu over the running agent and applies the answer.
    /// The agent keeps running underneath; its output waits until the menu
    /// closes.
    async fn choose_agent(&mut self, io: &mut Io, run: &RunId) -> AgentChoice {
        let Some(setup) = &self.agent_command else {
            return AgentChoice::Stay { printed: false };
        };
        let (find_extras, config_path) = (setup.extras.clone(), setup.config_path.clone());
        let extras = tokio::task::spawn_blocking(move || find_extras())
            .await
            .unwrap_or_default();
        let now = self.now();
        let active = self.label_of(run);
        let mut candidates: Vec<Candidate> = self
            .config
            .providers
            .iter()
            .map(|entry| Candidate {
                entry: entry.clone(),
                active: Some(&entry.label) == active.as_ref(),
                resting_minutes: self
                    .cooldowns
                    .returns_at(&entry.label, now)
                    .map(|until| (until - now).div_ceil(60_000)),
                new: false,
            })
            .collect();
        candidates.extend(
            extras
                .iter()
                .filter(|extra| !self.config.providers.iter().any(|p| p.label == extra.label))
                .map(|entry| Candidate {
                    entry: entry.clone(),
                    active: false,
                    resting_minutes: None,
                    new: true,
                }),
        );
        if candidates.len() < 2 {
            (io.notice)("only one agent is set up; add another to config.json to switch");
            return AgentChoice::Stay { printed: true };
        }
        let Some(chosen) = self.run_menu(io, candidates.clone()).await else {
            return AgentChoice::Stay { printed: false };
        };
        let picked: Vec<&Candidate> = chosen.iter().map(|&index| &candidates[index]).collect();
        let Some(first) = picked.first().map(|c| c.entry.clone()) else {
            return AgentChoice::Stay { printed: false };
        };
        let mut printed = false;
        // The picked agents lead; the others keep their order behind them.
        let mut providers: Vec<ProviderEntry> = picked.iter().map(|c| c.entry.clone()).collect();
        for entry in &self.config.providers {
            if !providers.iter().any(|p| p.label == entry.label) {
                providers.push(entry.clone());
            }
        }
        let added: Vec<String> = picked
            .iter()
            .filter(|c| c.new || c.entry.auto)
            .map(|c| c.entry.id.as_str().to_owned())
            .collect();
        let changed = !added.is_empty()
            || providers.iter().map(|p| &p.label).ne(self
                .config
                .providers
                .iter()
                .map(|p| &p.label));
        let order: Vec<String> = providers.iter().map(|p| p.label.clone()).collect();
        self.config.providers = providers;
        if changed {
            printed = true;
            let saved = match &config_path {
                Some(path) => config::save_order(path, &order, &added)
                    .map(|()| format!("saved to {}", path.display())),
                None => Err(config::SaveError::new("there is no configuration file")),
            };
            (io.notice)(&format!(
                "agent order: {} ({})",
                order.join(" > "),
                saved.unwrap_or_else(|error| format!("not saved, this session only: {error}"))
            ));
        }
        if self.cooldowns.is_resting(&first.label, now) {
            // Choosing a resting agent first is the user saying its quota is back.
            self.cooldowns.clear(&first.label);
            if let Some(path) = &self.cooldown_path
                && let Err(error) = self.cooldowns.save(path)
            {
                printed = true;
                (io.notice)(&format!("cannot save cooldowns: {error}"));
            }
        }
        if Some(&first.label) == active.as_ref() {
            AgentChoice::Stay { printed }
        } else {
            AgentChoice::Switch
        }
    }

    /// Draws the menu on the terminal's alternate screen and reads keys until
    /// the user finishes. `None` when the user left without choosing.
    async fn run_menu(&mut self, io: &mut Io, candidates: Vec<Candidate>) -> Option<Vec<usize>> {
        let mut input = io.input.take()?;
        let mut menu = AgentMenu::new(candidates);
        let draw = |io: &mut Io, menu: &AgentMenu| {
            let width = (io.terminal_size)().map_or(80, |size| usize::from(size.cols));
            let _ = write!(io.output, "\x1b[0m\x1b[H\x1b[2J{}", menu.render(width));
            let _ = io.output.flush();
        };
        let _ = write!(io.output, "\x1b[?1049h\x1b[?25l");
        draw(io, &menu);
        let chosen = 'keys: loop {
            let Some(bytes) = input.recv().await else {
                break None;
            };
            for key in parse_keys(&bytes) {
                match menu.press(key) {
                    Step::Pending => {}
                    Step::Cancelled => break 'keys None,
                    Step::Chosen(chosen) => break 'keys Some(chosen),
                }
            }
            draw(io, &menu);
        };
        let _ = write!(io.output, "\x1b[?25h\x1b[?1049l");
        let _ = io.output.flush();
        io.input = Some(input);
        chosen
    }

    /// Makes the agent repaint after the menu: a program that draws its own
    /// screen repaints when the terminal changes size.
    async fn redraw(&mut self, io: &mut Io) {
        let Some(size) = (io.terminal_size)() else {
            return;
        };
        let nudged = TerminalSize::new(size.rows, size.cols.saturating_sub(1).max(1));
        if nudged != size && self.supervisor.resize(nudged).is_ok() {
            tokio::time::sleep(Duration::from_millis(80)).await;
            let _ = self.supervisor.resize(size);
        }
    }
}

struct Outcome {
    end: RelayEnd,
    limit: Option<LimitSignal>,
    /// The user moved the work to another agent with `/agent`.
    switched: bool,
}

/// The usage-limit signal observed in events since `start`, if any. Only
/// provider evidence counts; Uuldana's own synthetic events do not.
fn limit_in(supervisor: &Supervisor, start: usize) -> Option<LimitSignal> {
    supervisor.events()[start..]
        .iter()
        .rev()
        .find_map(|envelope| match &envelope.event {
            AgentEvent::LimitReached(signal)
                if signal.kind == LimitKind::UsageQuota
                    && envelope.fidelity != Fidelity::Synthetic =>
            {
                Some(signal.clone())
            }
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stricter_takes_the_lower_value_in_every_dimension() {
        let a = PermissionPolicy {
            approval: ApprovalMode::NeverAsk,
            sandbox: SandboxPolicy {
                filesystem: FilesystemAccess::ReadOnly,
                network: NetworkAccess::Allowed,
            },
        };
        let b = PermissionPolicy {
            approval: ApprovalMode::AskForRisky,
            sandbox: SandboxPolicy {
                filesystem: FilesystemAccess::Unrestricted,
                network: NetworkAccess::Denied,
            },
        };
        let mixed = stricter(&a, &b);
        assert_eq!(mixed.approval, ApprovalMode::AskForRisky);
        assert_eq!(mixed.sandbox.filesystem, FilesystemAccess::ReadOnly);
        assert_eq!(mixed.sandbox.network, NetworkAccess::Denied);
        assert!(mixed.is_within(&a) && mixed.is_within(&b));
        assert_eq!(stricter(&a, &a), a);
    }
}
