//! Interactive supervised sessions and provider listing.

use std::{
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
    time::{Duration, SystemTime},
};

use uuldana_core::{
    ErrorClass, HostProbe, NativeSessionId, PermissionPolicy, ProviderId, ProviderRegistry,
    Session, SessionId, TerminalSize, Timestamp, TranscriptEntry,
};
use uuldana_runtime::{
    LocalHost, Supervisor,
    config::{self, Config, ProviderEntry},
    cooldown,
    journal::{self, Journal},
    orchestrator::{Io, Mode, NativeContext, Orchestrator, OrchestratorError},
};

use crate::{frame, terminal};

/// The agents found on this machine, learned from what they print for
/// `--help`, plus the provider manifests in the adapters directory. A manifest
/// that cannot be used is reported, never silently dropped, and a manifest
/// wins over what was learned for the same id. `announce` is told when a first
/// look at the machine is about to take a while. With `only`, no program that
/// is not named there is run at all.
fn registry(only: Option<&[String]>, announce: &dyn Fn(usize)) -> (ProviderRegistry, Vec<String>) {
    let mut registry = ProviderRegistry::new();
    let mut warnings = Vec::new();
    if let Some(dir) = config::adapters_dir() {
        for loaded in uuldana_providers::load_external_adapters(&dir) {
            match loaded {
                Ok(adapter) => {
                    if let Err(error) = registry.register(Arc::new(adapter)) {
                        warnings.push(format!("adapter not loaded: {error}"));
                    }
                }
                Err(message) => warnings.push(format!("adapter not loaded: {message}")),
            }
        }
    }
    let cache = journal::data_dir().map(|dir| dir.join("programs.json"));
    let progress = Progress::default();
    let host = LocalHost::new();
    let show = |asked, total| progress.show(asked, total);
    let learned = match only {
        Some(only) => uuldana_providers::learn_listed_with_progress(
            &host,
            cache.as_deref(),
            only,
            announce,
            &show,
        ),
        None => uuldana_providers::learn_installed_with_progress(
            &host,
            cache.as_deref(),
            announce,
            &show,
        ),
    };
    for adapter in learned {
        // A manifest of the same id has already been registered and is kept.
        let _ = registry.register(adapter);
    }
    progress.finish();
    (registry, warnings)
}

/// What a first look at the machine says while it takes its time.
fn say_looking(count: usize) {
    eprintln!(
        "uuldana: looking for coding agents among {count} programs (once; can take a minute)"
    );
}

/// How far a first look at the machine has got, on one line that is rewritten
/// as programs answer. Only a terminal is written to, and the count only goes
/// up, though the programs are asked from several threads.
#[derive(Default)]
struct Progress {
    shown: std::sync::Mutex<usize>,
}

impl Progress {
    fn show(&self, asked: usize, total: usize) {
        use std::io::IsTerminal;
        if !std::io::stderr().is_terminal() {
            return;
        }
        let Ok(mut shown) = self.shown.lock() else {
            return;
        };
        if asked > *shown {
            *shown = asked;
            eprint!("\r  {asked} of {total} asked");
        }
    }

    /// Ends the line that was being rewritten, if there was one.
    fn finish(&self) {
        if self.shown.lock().is_ok_and(|shown| *shown > 0) {
            eprintln!();
        }
    }
}

/// `uuldana providers`: every agent found on this machine, and the providers
/// described by manifests.
pub fn list_providers() -> String {
    let (registry, warnings) = registry(None, &say_looking);
    for warning in warnings {
        eprintln!("uuldana: {warning}");
    }
    let reports = registry.discover(&LocalHost::new());
    let width = reports
        .iter()
        .map(|r| r.provider.as_str().len())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for report in reports {
        let id = report.provider.as_str();
        let line = match &report.outcome {
            Ok(found) => {
                let mut line = format!(
                    "{id:<width$}  installed      {}  {}",
                    found
                        .installation
                        .version
                        .as_deref()
                        .unwrap_or("unknown version"),
                    found.installation.executable.display(),
                );
                // What was learned about it, and what had to be assumed.
                for note in &found.installation.notes {
                    line.push_str("\n    ");
                    line.push_str(note);
                }
                line
            }
            Err(error) if error.class() == ErrorClass::NotInstalled => {
                format!("{id:<width$}  not installed")
            }
            Err(error) => format!("{id:<width$}  unavailable    {error}"),
        };
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Installed agents that `config.json` does not list yet, for `/agent` to
/// offer. Looking asks every adapter for its version, so it runs on demand.
fn detected_agents(configured: Vec<uuldana_core::ProviderId>) -> Vec<ProviderEntry> {
    let (registry, _) = registry(None, &|_| {});
    registry
        .discover(&LocalHost::new())
        .into_iter()
        .filter(|report| report.outcome.is_ok() && !configured.contains(&report.provider))
        .map(|report| ProviderEntry {
            label: report.provider.as_str().to_owned(),
            id: report.provider,
            env: Vec::new(),
            cooldown: Duration::from_secs(config::DEFAULT_COOLDOWN_MINUTES * 60),
            auto: false,
        })
        .collect()
}

/// Agents installed on this machine that the configuration does not name, so
/// the user never has to list what Uuldana can see for itself. They are tried
/// after the ones the user ranked.
fn detect_unlisted(registry: &ProviderRegistry, listed: &[ProviderEntry]) -> Vec<ProviderEntry> {
    let mut found: Vec<ProviderEntry> = registry
        .discover(&LocalHost::new())
        .into_iter()
        .filter(|report| {
            report.outcome.is_ok() && !listed.iter().any(|entry| entry.id == report.provider)
        })
        .map(|report| ProviderEntry {
            label: report.provider.as_str().to_owned(),
            id: report.provider,
            env: Vec::new(),
            cooldown: Duration::from_secs(config::DEFAULT_COOLDOWN_MINUTES * 60),
            auto: true,
        })
        .collect();
    order_unranked(&mut found);
    found
}

/// No agent is preferred over another: the ones nobody ranked follow in the
/// order of their ids, and the user ranks them with `/agent` or `providers`.
fn order_unranked(found: &mut [ProviderEntry]) {
    found.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
}

/// Agents keep their conversations in files of their own, in layouts nobody
/// documents. None is read: a handoff carries what the screen showed, and an
/// agent is resumed through its own continue flag.
struct NoStore;

impl NativeContext for NoStore {
    fn conversation(
        &self,
        _: &ProviderId,
        _: &dyn HostProbe,
        _: &Path,
        _: &NativeSessionId,
    ) -> Option<Vec<TranscriptEntry>> {
        None
    }
    fn discover(
        &self,
        _: &ProviderId,
        _: &dyn HostProbe,
        _: &Path,
        _: Timestamp,
    ) -> Option<NativeSessionId> {
        None
    }
}

/// The configuration file when there is one. Without it Uuldana still starts,
/// with whatever agents it finds installed.
fn load_config() -> Result<Config, String> {
    let Some(path) = config::default_path() else {
        return Ok(Config::automatic());
    };
    if !path.exists() {
        return Ok(Config::automatic());
    }
    Config::load(&path).map_err(|e| format!("{e}\n  file: {}", path.display()))
}

/// `uuldana`: starts the highest priority available provider in this
/// directory and relays the terminal to its native UI.
pub fn start(bypass_permissions: bool) -> ExitCode {
    launch(false, bypass_permissions)
}

/// `uuldana resume`: continues the most recent session of this directory.
pub fn resume(bypass_permissions: bool) -> ExitCode {
    launch(true, bypass_permissions)
}

/// Prints the notices collected so far, then the error that ends the run.
fn fail(notices: &[String], message: &str, code: u8) -> ExitCode {
    for notice in notices {
        eprintln!("{notice}");
    }
    eprintln!("uuldana: {message}");
    ExitCode::from(code)
}

fn launch(resume: bool, bypass_permissions: bool) -> ExitCode {
    // Notices are held back and shown in the header once the screen is up.
    let mut notices: Vec<String> = Vec::new();
    let mut config = match load_config() {
        Ok(config) => config,
        Err(message) => return fail(&notices, &message, 2),
    };
    if bypass_permissions {
        config.ceiling = PermissionPolicy::unrestricted();
    }
    if !(io::stdin().is_terminal() && io::stdout().is_terminal()) {
        return fail(
            &notices,
            "a supervised session needs an interactive terminal",
            2,
        );
    }
    // A configuration that turns detection off names its agents; nothing else
    // on the search path is run to find out what it is.
    let listed: Vec<String> = config
        .providers
        .iter()
        .map(|entry| entry.id.as_str().to_owned())
        .collect();
    let only = (!config.auto_detect).then_some(listed.as_slice());
    let (registry, adapter_warnings) = registry(only, &say_looking);
    if config.auto_detect {
        let found = detect_unlisted(&registry, &config.providers);
        config.providers.extend(found);
    }
    // An entry that matches no agent found here (it was uninstalled, or the
    // name changed) is left out; it is not a reason to stop.
    let missing: Vec<String> = config
        .providers
        .iter()
        .filter(|entry| registry.get(&entry.id).is_none())
        .map(|entry| entry.label.clone())
        .collect();
    if !missing.is_empty() {
        config
            .providers
            .retain(|entry| registry.get(&entry.id).is_some());
        notices.push(format!(
            "uuldana: {} in the configuration matches no agent found here and is skipped; `uuldana providers` lists what was found",
            missing.join(", ")
        ));
    }
    if config.providers.is_empty() {
        return fail(
            &notices,
            "no AI coding agent was found on this machine. Install one and run uuldana again; \
             `uuldana providers` lists what was found",
            2,
        );
    }
    notices.extend(
        adapter_warnings
            .into_iter()
            .map(|warning| format!("uuldana: {warning}")),
    );
    let workspace = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(error) => {
            return fail(
                &notices,
                &format!("cannot read the working directory: {error}"),
                1,
            );
        }
    };
    let data = journal::data_dir();
    let prepared = match prepare(
        &config,
        &workspace,
        data.as_deref(),
        resume,
        bypass_permissions,
    ) {
        Ok(prepared) => prepared,
        Err(message) => return fail(&notices, &message, 1),
    };
    notices.extend(
        prepared
            .warnings
            .iter()
            .map(|warning| format!("uuldana: {warning}")),
    );
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("uuldana: cannot start the async runtime: {error}");
            return ExitCode::from(1);
        }
    };
    runtime.block_on(run(
        config,
        registry,
        prepared,
        data,
        resume,
        notices,
        bypass_permissions,
    ))
}

/// A session ready to run: new, or restored from its journal.
struct Prepared {
    session: Session,
    events: Vec<uuldana_core::EventEnvelope>,
    labels: std::collections::HashMap<uuldana_core::RunId, String>,
    journal: Option<Journal>,
    warnings: Vec<String>,
}

fn prepare(
    config: &Config,
    workspace: &Path,
    data: Option<&Path>,
    resume: bool,
    bypass_permissions: bool,
) -> Result<Prepared, String> {
    let mut warnings = Vec::new();
    if resume {
        let data = data.ok_or("no data directory; nothing can be resumed")?;
        let summary = journal::list(data)
            .into_iter()
            .find(|s| journal::same_workspace(&s.workspace, workspace))
            .ok_or_else(|| {
                format!(
                    "no earlier session in {}; start one with `uuldana`",
                    workspace.display()
                )
            })?;
        let loaded = journal::load(&summary.path).map_err(|e| e.to_string())?;
        // A session's ceiling is fixed when it is created and never raised, so
        // a saved session that was stricter cannot be bypassed.
        if bypass_permissions && *loaded.session.permission_ceiling() != config.ceiling {
            return Err(
                "this session was created with a stricter permission ceiling, which cannot be \
                 raised; start a new one with `uuldana --bypass-permissions`"
                    .into(),
            );
        }
        if loaded.recovered_active_run {
            warnings.push("the previous run did not end cleanly; it is marked as failed".into());
        }
        if loaded.skipped_lines > 0 {
            warnings.push(format!(
                "{} damaged line(s) in the session journal were skipped",
                loaded.skipped_lines
            ));
        }
        let journal = Journal::open_append(&summary.path)
            .map_err(|e| format!("cannot continue the session journal: {e}"))?;
        return Ok(Prepared {
            session: loaded.session,
            events: loaded.events,
            labels: loaded.labels,
            journal: Some(journal),
            warnings,
        });
    }
    let now = Timestamp::from_system_time(SystemTime::now());
    let id = SessionId::new(format!("s-{}-{}", now.unix_millis(), std::process::id()))
        .expect("valid session id");
    let session = Session::new(id, workspace.to_path_buf(), config.ceiling, now)
        .map_err(|e| e.to_string())?;
    let journal = match data {
        Some(data) => {
            journal::prune(data, journal::DEFAULT_KEEP);
            match Journal::create(&journal::path_for(data, session.id()), &session) {
                Ok(journal) => Some(journal),
                Err(error) => {
                    warnings.push(format!("this session will not be saved: {error}"));
                    None
                }
            }
        }
        None => {
            warnings.push("no data directory; this session will not be saved".into());
            None
        }
    };
    Ok(Prepared {
        session,
        events: Vec::new(),
        labels: Default::default(),
        journal,
        warnings,
    })
}

/// Rows one notch of the mouse wheel moves.
const WHEEL_ROWS: isize = 3;

/// What a read from the keyboard or the mouse becomes: `None` when Uuldana keeps
/// it, otherwise the bytes for the agent.
///
/// Shift+PageUp and Shift+PageDown look through the history on uuldana's
/// screen. The mouse wheel does what a terminal does with it: an agent that
/// asked for mouse events (some agents do) gets the wheel as a mouse event in the
/// encoding it asked for, and scrolls its own screen; an agent on the alternate
/// screen that did not ask gets arrow keys; otherwise the wheel looks through
/// the history on uuldana's screen. Any other key goes to the agent and brings
/// the live screen back. `in_paste` follows a paste from one read to the next.
fn route(screen: &frame::Frame, in_paste: &mut bool, bytes: Vec<u8>) -> Option<Vec<u8>> {
    if let Some(wheel) = terminal::parse_wheel(&bytes) {
        if let Some(sgr) = screen.mouse_tracking() {
            let row = wheel.row.saturating_sub(screen.body_top()).max(1);
            return Some(terminal::mouse_report(sgr, wheel.up, wheel.column, row));
        }
        if screen.on_alternate_screen() {
            let arrow: &[u8] = if wheel.up { b"\x1b[A" } else { b"\x1b[B" };
            return Some(arrow.repeat(WHEEL_ROWS as usize));
        }
        screen.scroll(if wheel.up { WHEEL_ROWS } else { -WHEEL_ROWS });
        return None;
    }
    // Shift+Enter and Ctrl+Enter: a line break in the way the agent's console
    // takes it.
    if terminal::is_line_break(&bytes) {
        screen.to_live();
        return Some(if screen.takes_win32_keys() {
            terminal::WIN32_LINE_BREAK.to_vec()
        } else {
            terminal::LINE_FEED.to_vec()
        });
    }
    // An agent that did not ask for bracketed pastes gets the text as typed.
    let bytes = if screen.takes_paste() {
        bytes
    } else {
        let typed = terminal::unbracket(&bytes, in_paste);
        if typed.is_empty() {
            return None;
        }
        typed
    };
    match bytes.as_slice() {
        b"\x1b[5;2~" => screen.scroll(screen.page()),
        b"\x1b[6;2~" => screen.scroll(-screen.page()),
        _ => {
            screen.to_live();
            screen.note_sent(&bytes);
            return Some(bytes);
        }
    }
    None
}

/// Keyboard bytes for the agent, with the scrolling kept back (see [`route`]).
/// The sender is the way in for text Uuldana itself puts in the agent's input.
fn keys(
    screen: frame::Frame,
) -> (
    tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
) {
    let mut raw = terminal::spawn_input();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let typed = tx.clone();
    tokio::spawn(async move {
        let mut in_paste = false;
        while let Some(bytes) = raw.recv().await {
            if let Some(bytes) = route(&screen, &mut in_paste, bytes)
                && tx.send(bytes).is_err()
            {
                break;
            }
        }
    });
    (rx, typed)
}

/// Puts `text` in the agent's input box as a paste that is not sent, so the
/// user decides whether to go on, and nothing is spent until they do. It waits
/// until the agent has drawn its screen and asks for pastes. An agent that
/// never does gets nothing typed into it: the text is saved to a file instead
/// and its place is shown in the header.
async fn paste_when_ready(
    screen: frame::Frame,
    typed: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    text: String,
    data: Option<PathBuf>,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(40);
    while tokio::time::Instant::now() < deadline {
        if screen.ready_for_paste(Duration::from_millis(1500)) {
            let mut bytes = b"\x1b[200~".to_vec();
            bytes.extend_from_slice(text.as_bytes());
            bytes.extend_from_slice(b"\x1b[201~");
            let _ = typed.send(bytes);
            screen.set_status(
                "earlier conversation pasted, not sent: Enter goes on, clearing it starts fresh. \
                 Not shown? The agent is not at its prompt",
            );
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let saved = data.map(|dir| dir.join("handoff.md")).and_then(|path| {
        uuldana_runtime::journal::create_private_dir_all(path.parent()?).ok()?;
        uuldana_runtime::journal::write_private(&path, text.as_bytes()).ok()?;
        Some(path)
    });
    screen.set_status(&match saved {
        Some(path) => format!(
            "this agent takes no paste; the earlier conversation is in {}",
            path.display()
        ),
        None => "this agent takes no paste, so the earlier conversation was not carried".into(),
    });
}

async fn run(
    config: Config,
    registry: ProviderRegistry,
    prepared: Prepared,
    data: Option<PathBuf>,
    resume: bool,
    notices: Vec<String>,
    bypass_permissions: bool,
) -> ExitCode {
    let probe: Arc<dyn HostProbe> = Arc::new(LocalHost::new());
    let mut supervisor =
        Supervisor::with_history(registry, probe.clone(), prepared.session, prepared.events);
    if let Some(journal) = prepared.journal {
        supervisor.attach_journal(journal);
    }
    let configured: Vec<_> = config.providers.iter().map(|e| e.id.clone()).collect();
    let mut orchestrator = Orchestrator::new(
        supervisor,
        config,
        probe,
        Arc::new(NoStore),
        prepared.labels,
    )
    .with_agent_command(config::default_path(), move || {
        detected_agents(configured.clone())
    });
    if let Some(data) = &data {
        orchestrator = orchestrator.with_cooldown_file(cooldown::path_in(data));
    }
    let raw = match terminal::RawMode::enable() {
        Ok(raw) => raw,
        Err(error) => {
            eprintln!("uuldana: cannot take over the terminal: {error}");
            return ExitCode::from(1);
        }
    };
    // The agent runs on a pseudo terminal the size of the area under uuldana's
    // header and draws into that area through the frame. ConPTY asks its host
    // for the cursor position at startup; the relay answers it from the frame
    // (see `uuldana_runtime::relay`). Unix PTYs never ask.
    let no_color = std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty());
    let dumb_terminal = std::env::var_os("TERM").is_some_and(|value| value == "dumb");
    let screen = frame::Frame::start(
        Box::new(io::stdout()),
        terminal::size().unwrap_or(TerminalSize::DEFAULT),
        !no_color && !dumb_terminal,
        bypass_permissions.then(|| "PERMISSIONS BYPASSED".to_owned()),
    );
    let restore = frame::Restore(screen.clone());
    if !notices.is_empty() {
        screen.set_status(&notices.join("  "));
    }
    let (input, typed) = keys(screen.clone());
    let mut io = Io {
        input: Some(input),
        output: Box::new(frame::FrameWriter(screen.clone())),
        terminal_size: {
            let screen = screen.clone();
            Box::new(move || terminal::size().map(|real| screen.sync(real)))
        },
        cursor: cfg!(windows).then(|| {
            let screen = screen.clone();
            Box::new(move || screen.cursor()) as Box<_>
        }),
        notice: {
            let screen = screen.clone();
            Box::new(move |text| screen.set_status(text))
        },
        agent: Some({
            let screen = screen.clone();
            Box::new(move |label, first_prompt| screen.set_agent(label, first_prompt))
        }),
        prefill: Some({
            let screen = screen.clone();
            let data = data.clone();
            Box::new(move |text| {
                tokio::spawn(paste_when_ready(
                    screen.clone(),
                    typed.clone(),
                    text.to_owned(),
                    data.clone(),
                ));
            })
        }),
        // Only what was said to the agent counts: its welcome screen is not a
        // conversation, and carrying it would make the next agent spend a turn
        // on nothing.
        screen_text: Some({
            let screen = screen.clone();
            Box::new(move || {
                if screen.spoke() {
                    screen.transcript()
                } else {
                    String::new()
                }
            })
        }),
    };
    let mode = if resume {
        Mode::Resume
    } else {
        Mode::Fresh { prompt: None }
    };
    let result = orchestrator.run(&mut io, mode).await;
    drop(io);
    drop(restore);
    drop(raw);
    let _ = io::stdout().flush();
    if let Some(error) = orchestrator.supervisor().journal_error() {
        eprintln!("uuldana: the session journal stopped working: {error}");
    }
    match result {
        Ok(report) => match report.exit {
            Some(exit) if exit.success => ExitCode::SUCCESS,
            Some(exit) => ExitCode::from(exit.code.map_or(1, |c| u8::try_from(c).unwrap_or(1))),
            None => ExitCode::SUCCESS,
        },
        Err(OrchestratorError::NoProvider {
            reasons,
            next_return_ms,
        }) => {
            eprintln!("uuldana: no configured provider can run right now");
            for reason in reasons {
                eprintln!("  {reason}");
            }
            if let Some(at) = next_return_ms {
                let now = Timestamp::from_system_time(SystemTime::now()).unix_millis();
                eprintln!(
                    "  the first one is available again in about {} minutes",
                    at.saturating_sub(now).div_ceil(60_000)
                );
            }
            ExitCode::from(1)
        }
        Err(error) => {
            eprintln!("uuldana: {error}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str) -> ProviderEntry {
        ProviderEntry {
            id: ProviderId::new(id).unwrap(),
            label: id.to_owned(),
            env: Vec::new(),
            cooldown: Duration::from_secs(60),
            auto: true,
        }
    }

    #[test]
    fn agents_nobody_ranked_follow_in_the_order_of_their_ids() {
        let mut found = vec![
            entry("delta"),
            entry("bravo"),
            entry("alpha"),
            entry("charlie"),
        ];
        order_unranked(&mut found);
        let ids: Vec<&str> = found.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["alpha", "bravo", "charlie", "delta"]);
    }

    fn screen() -> frame::Frame {
        frame::Frame::start(
            Box::new(io::sink()),
            TerminalSize::new(40, 100),
            false,
            None,
        )
    }

    fn wheel(up: bool) -> Vec<u8> {
        let kind = if up {
            crossterm::event::MouseEventKind::ScrollUp
        } else {
            crossterm::event::MouseEventKind::ScrollDown
        };
        // The pointer is on the 21st row, which is the 12th of the agent's
        // area under a header of nine rows.
        terminal::wheel_bytes(kind, 4, 20).unwrap()
    }

    fn agent_writes(screen: &frame::Frame, text: &str) {
        let mut writer = frame::FrameWriter(screen.clone());
        writer.write_all(text.as_bytes()).unwrap();
        writer.flush().unwrap();
    }

    fn many_lines(screen: &frame::Frame) {
        let lines: String = (1..=80).map(|n| format!("line {n}\r\n")).collect();
        agent_writes(screen, &lines);
    }

    #[test]
    fn a_paste_reaches_an_agent_the_way_it_asked_for() {
        let whole = terminal::paste_bytes("one\ntwo");
        let paste = [whole.as_slice(), b"\x1b[200~one\n".as_slice()];
        // It asked for bracketed pastes: nothing is touched.
        let asks = screen();
        agent_writes(&asks, "\x1b[?2004h");
        let mut inside = false;
        for bytes in paste {
            assert_eq!(
                route(&asks, &mut inside, bytes.to_vec()),
                Some(bytes.to_vec())
            );
        }
        // It did not: the markers go and the line feeds become returns, also
        // when the paste comes in pieces.
        let plain = screen();
        let mut inside = false;
        assert_eq!(
            route(&plain, &mut inside, paste[0].to_vec()),
            Some(b"one\rtwo".to_vec())
        );
        assert_eq!(
            route(&plain, &mut inside, b"\x1b[200~".to_vec()),
            None,
            "a marker alone is nothing to send"
        );
        assert_eq!(
            route(&plain, &mut inside, b"a\nb".to_vec()),
            Some(b"a\rb".to_vec())
        );
        assert_eq!(route(&plain, &mut inside, b"\x1b[201~".to_vec()), None);
        assert_eq!(
            route(&plain, &mut inside, b"\n".to_vec()),
            Some(b"\n".to_vec())
        );
    }

    #[test]
    fn the_wheel_scrolls_the_history_and_is_not_passed_to_the_agent() {
        let screen = screen();
        many_lines(&screen);
        assert_eq!(route(&screen, &mut false, wheel(true)), None);
        assert!(screen.transcript().contains("line 80"));
        // Scrolled back: a key goes to the agent and the live screen returns.
        assert_eq!(
            route(&screen, &mut false, b"a".to_vec()),
            Some(b"a".to_vec())
        );
        assert!(!screen.to_live());
        // The wheel the other way, at the live screen, changes nothing.
        assert_eq!(route(&screen, &mut false, wheel(false)), None);
        assert!(!screen.to_live());
    }

    #[test]
    fn the_wheel_moves_history_one_notch_at_a_time_and_a_key_ends_it() {
        let screen = screen();
        many_lines(&screen);
        assert_eq!(route(&screen, &mut false, wheel(true)), None);
        assert_eq!(route(&screen, &mut false, wheel(true)), None);
        assert_eq!(route(&screen, &mut false, wheel(false)), None);
        // Still somewhere back in the history.
        assert!(screen.to_live());
    }

    #[test]
    fn an_agent_on_the_alternate_screen_gets_the_wheel_as_arrow_keys() {
        let screen = screen();
        agent_writes(&screen, "\x1b[?1049hmenu");
        assert!(screen.on_alternate_screen());
        assert_eq!(
            route(&screen, &mut false, wheel(true)),
            Some(b"\x1b[A\x1b[A\x1b[A".to_vec())
        );
        assert_eq!(
            route(&screen, &mut false, wheel(false)),
            Some(b"\x1b[B\x1b[B\x1b[B".to_vec())
        );
    }

    #[test]
    fn an_agent_that_asked_for_the_mouse_is_sent_the_wheel_as_a_mouse_event() {
        // What an agent can do: the alternate screen and then every mouse event in
        // the SGR encoding.
        let screen = screen();
        agent_writes(&screen, "\x1b[?1049h\x1b[?1003h\x1b[?1006hhello");
        assert_eq!(screen.mouse_tracking(), Some(true));
        assert_eq!(
            route(&screen, &mut false, wheel(true)),
            Some(b"\x1b[<64;5;12M".to_vec())
        );
        assert_eq!(
            route(&screen, &mut false, wheel(false)),
            Some(b"\x1b[<65;5;12M".to_vec())
        );
    }

    #[test]
    fn the_older_mouse_encoding_is_used_when_that_is_what_the_agent_asked_for() {
        let screen = screen();
        agent_writes(&screen, "\x1b[?1000hhello");
        assert_eq!(screen.mouse_tracking(), Some(false));
        let sent = route(&screen, &mut false, wheel(true)).unwrap();
        assert_eq!(&sent[..3], b"\x1b[M");
        assert_eq!(sent[3..], [96, 37, 44]);
    }

    #[test]
    fn an_agent_on_the_main_screen_that_asked_for_the_mouse_is_sent_it_too() {
        let screen = screen();
        agent_writes(&screen, "\x1b[?1002h\x1b[?1006hhello");
        assert!(!screen.on_alternate_screen());
        let sent = route(&screen, &mut false, wheel(true)).unwrap();
        assert!(sent.starts_with(b"\x1b[<64;"), "{sent:?}");
        // Nothing was scrolled on uuldana's side.
        assert!(!screen.to_live());
    }

    #[test]
    fn a_new_agent_does_not_inherit_the_mouse_the_last_one_asked_for() {
        let screen = screen();
        screen.set_agent("agent-b", false);
        agent_writes(&screen, "\x1b[?1049h\x1b[?1003h\x1b[?1006h");
        assert_eq!(screen.mouse_tracking(), Some(true));
        screen.set_agent("agent-a", false);
        assert_eq!(screen.mouse_tracking(), None);
        assert!(!screen.on_alternate_screen());
        many_lines(&screen);
        assert_eq!(route(&screen, &mut false, wheel(true)), None);
        assert!(screen.to_live());
    }

    /// Scrolling and the arrow keys keep working after the work moves to
    /// another agent, whatever state the one before it left the screen in:
    /// scrolled back, on the alternate screen, asking for the mouse.
    #[test]
    fn the_wheel_and_the_arrow_keys_work_after_the_work_moves_to_another_agent() {
        let arrows: [&[u8]; 4] = [b"\x1b[A", b"\x1b[B", b"\x1b[C", b"\x1b[D"];
        let left_in: [fn(&frame::Frame); 3] = [
            // Scrolled back into the history.
            |screen| {
                many_lines(screen);
                assert_eq!(route(screen, &mut false, wheel(true)), None);
            },
            // On the alternate screen.
            |screen| agent_writes(screen, "\x1b[?1049hmenu"),
            // Asking for the mouse in the SGR encoding.
            |screen| agent_writes(screen, "\x1b[?1049h\x1b[?1003h\x1b[?1006hhello"),
        ];
        for leave in left_in {
            let screen = screen();
            screen.set_agent("first", false);
            leave(&screen);
            screen.set_agent("second", false);
            // The arrow keys go to the new agent as they are.
            for arrow in arrows {
                assert_eq!(
                    route(&screen, &mut false, arrow.to_vec()),
                    Some(arrow.to_vec())
                );
            }
            // The new agent is on the live screen, and its output can be
            // scrolled back through with the wheel, and forward again.
            assert!(!screen.on_alternate_screen());
            assert_eq!(screen.mouse_tracking(), None);
            many_lines(&screen);
            assert_eq!(route(&screen, &mut false, wheel(true)), None);
            assert!(screen.transcript().contains("line 80"));
            assert_eq!(route(&screen, &mut false, wheel(false)), None);
            assert_eq!(
                route(&screen, &mut false, b"x".to_vec()),
                Some(b"x".to_vec())
            );
        }
    }

    #[test]
    fn a_line_break_key_reaches_each_agent_the_way_its_console_takes_it() {
        let marker = |modifiers| {
            terminal::encode_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                modifiers,
            ))
            .unwrap()
        };
        let shift = crossterm::event::KeyModifiers::SHIFT;
        let ctrl = crossterm::event::KeyModifiers::CONTROL;
        // A console that has not announced that it takes keys as Windows does
        // is given a line feed.
        let plain = screen();
        for modifiers in [shift, ctrl] {
            assert_eq!(
                route(&plain, &mut false, marker(modifiers)),
                Some(b"\n".to_vec())
            );
        }
        // One that has is given Ctrl+J as a key, for Shift+Enter too: Enter
        // with Shift would be a plain Enter to an agent that reads text.
        let windows = screen();
        agent_writes(&windows, "\x1b[?9001h");
        assert!(windows.takes_win32_keys());
        for modifiers in [shift, ctrl, shift | ctrl] {
            assert_eq!(
                route(&windows, &mut false, marker(modifiers)),
                Some(terminal::WIN32_LINE_BREAK.to_vec())
            );
        }
        // The announcement can arrive split, and is withdrawn when the work
        // moves to another agent, whose console announces itself again.
        let split = screen();
        agent_writes(&split, "text\x1b[?90");
        assert!(!split.takes_win32_keys());
        agent_writes(&split, "01h more");
        assert!(split.takes_win32_keys());
        split.set_agent("next", false);
        assert!(!split.takes_win32_keys());
        assert_eq!(
            route(&split, &mut false, marker(shift)),
            Some(b"\n".to_vec())
        );
        agent_writes(&split, "\x1b[?9001h");
        assert!(split.takes_win32_keys());
        // A plain Enter is still a plain Enter.
        assert_eq!(
            route(&split, &mut false, b"\r".to_vec()),
            Some(b"\r".to_vec())
        );
    }

    #[test]
    fn shift_page_keys_scroll_too_and_ordinary_keys_pass_through() {
        let screen = screen();
        many_lines(&screen);
        assert_eq!(route(&screen, &mut false, b"\x1b[5;2~".to_vec()), None);
        assert!(screen.to_live());
        assert_eq!(
            route(&screen, &mut false, b"\x1b[A".to_vec()),
            Some(b"\x1b[A".to_vec())
        );
    }
}
