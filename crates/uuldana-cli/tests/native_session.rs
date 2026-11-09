//! End to end: the real `uuldana` binary inside a real pseudo terminal runs the
//! real agents installed on this machine and relays their native terminal UI.
//!
//! The tests only open an agent's interface and leave with Ctrl+C. They send no
//! prompt, so nothing is billed. No agent is named: the agents are the ones
//! `uuldana` learns on this machine, and a machine with none says so instead of
//! passing for an agent that is not there.

mod support;

use std::{path::PathBuf, time::Duration};

use support::{Agent, Scratch, child_command_lines};
use uuldana_core::*;
use uuldana_runtime::config::automatic_ceiling;
use uuldana_transport::{Frame, Process};

/// How a run with the `uuldana` binary ends before the test looks at it.
enum Ending {
    /// Leave the way a user does: Ctrl+C twice, and expect a clean exit.
    CtrlC,
    /// The test has what it wanted; stop the process.
    Terminate,
}

/// Starts `uuldana` with a configuration that lists only `agent`, waits until
/// the agent's interface has drawn itself and gone quiet, and then
/// `on_ui` runs with the screen so far and the pid of `uuldana`.
async fn run_agent(
    agent: &Agent,
    permissions: Option<&str>,
    cli_args: &[&str],
    ending: Ending,
    on_ui: impl FnOnce(&str, u32),
) {
    let scratch = Scratch::new("uuldana-sesi");
    let config = scratch.0.join("config.json");
    let permissions =
        permissions.map_or(String::new(), |block| format!(r#""permissions":{block},"#));
    std::fs::write(
        &config,
        format!(
            r#"{{"version":1,{permissions}"auto_detect":false,"providers":[{{"id":"{}"}}]}}"#,
            agent.id
        ),
    )
    .unwrap();
    let data = scratch.0.join("data");
    support::seed_cache(&data);
    let workspace = scratch.0.join("proyek");
    std::fs::create_dir_all(&workspace).unwrap();
    // Some agents unpack an installer into the temporary folder every time
    // they start. Keep that inside the scratch folder, which is removed.
    let temporary = scratch.0.join("tmp");
    std::fs::create_dir_all(&temporary).unwrap();

    let permissions = validate_mapping(
        &PermissionPolicy::default(),
        &PermissionMapping::new(PermissionPolicy::default()),
    )
    .unwrap();
    let request = LaunchRequest::new(workspace.clone(), TransportKind::NativePty, permissions);
    let plan = LaunchPlan::new(
        PathBuf::from(env!("CARGO_BIN_EXE_uuldana")),
        workspace,
        TransportSpec::NativePty {
            initial_size: TerminalSize::new(40, 120),
        },
    )
    .args(cli_args.iter().copied())
    .env_set("UULDANA_CONFIG", &config)
    .env_set("UULDANA_DATA", &data)
    .env_set("TEMP", &temporary)
    .env_set("TMP", &temporary)
    .env_set("TMPDIR", &temporary)
    // The scratch folder lies inside this repository's target folder. An agent
    // that asks whether to trust a folder in a git repository would stop at
    // that question, so the scratch folder is hidden from git's search upward.
    .env_set("GIT_CEILING_DIRECTORIES", scratch.0.parent().unwrap());
    let mut process = Process::spawn(&plan, &request).await.unwrap();

    let header = format!("now: {}", agent.id);
    let mut screen = Vec::new();
    let mut answered_cursor = 0;
    let mut header_at: Option<usize> = None;
    let mut quiet_since = tokio::time::Instant::now();
    let seen = tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            // Poll, so that quiet can be noticed while nothing arrives.
            if let Ok(frame) =
                tokio::time::timeout(Duration::from_millis(300), process.next_frame()).await
            {
                match frame.unwrap() {
                    Some(Frame::Output(bytes)) => {
                        screen.extend(bytes);
                        quiet_since = tokio::time::Instant::now();
                    }
                    Some(Frame::Exited(_)) | None => return false,
                    Some(_) => {}
                }
            }
            // Act like the terminal emulator: answer every cursor position
            // query. Each pseudo terminal in the chain asks once.
            let asked = screen.windows(4).filter(|w| *w == b"\x1b[6n").count();
            while answered_cursor < asked {
                process.write_input(b"\x1b[1;1R").await.unwrap();
                answered_cursor += 1;
            }
            if header_at.is_none() && String::from_utf8_lossy(&screen).contains(&header) {
                header_at = Some(screen.len());
            }
            // The agent's own screen is up when it has drawn something beyond
            // the header and then stopped drawing.
            if let Some(at) = header_at
                && screen.len() > at + 600
                && quiet_since.elapsed() >= Duration::from_millis(2500)
            {
                return true;
            }
        }
    })
    .await;
    let text = String::from_utf8_lossy(&screen).into_owned();
    assert_eq!(seen, Ok(true), "{} UI never appeared:\n{text}", agent.id);
    on_ui(&text, process.process_id().expect("uuldana pid"));
    if matches!(ending, Ending::Terminate) {
        let _ = process.force_terminate();
        return;
    }
    // An agent that asks whether to trust the folder is waiting for an answer,
    // and Ctrl+C there is the agent cancelling, which it ends with its own
    // failure code. That says nothing about Uuldana, and answering would mean
    // writing the agent's own settings, so this run is not a fair one.
    if text.to_lowercase().contains("trust this folder") {
        eprintln!(
            "SKIPPED {}: it asks to trust the folder; run it once in a folder you trust",
            agent.id
        );
        let _ = process.force_terminate();
        return;
    }

    // Leave the way a user does: Ctrl+C twice. Right after a fresh build the
    // first start of the binary can be slow enough that the pair arrives before
    // the agent is listening, and a user would simply press it again, so a
    // second pair is allowed before the test gives up.
    let mut after = Vec::new();
    let mut exit = Ok(None);
    for round in 1..=2 {
        process.write_input(b"\x03").await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        process.write_input(b"\x03").await.unwrap();
        exit = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                match process.next_frame().await.unwrap() {
                    Some(Frame::Exited(info)) => return Some(info),
                    Some(Frame::Output(bytes)) => after.extend(bytes),
                    Some(_) => {}
                    None => return None,
                }
            }
        })
        .await;
        if exit.is_ok() {
            break;
        }
        eprintln!("{}: still running after Ctrl+C round {round}", agent.id);
    }
    if !matches!(exit, Ok(Some(_))) {
        let _ = process.force_terminate();
    }
    let after = String::from_utf8_lossy(&after);
    assert!(
        matches!(exit, Ok(Some(info)) if info.success),
        "{}: uuldana did not exit cleanly after Ctrl+C: {exit:?}\nafter Ctrl+C:\n{after}\nbefore:\n{text}",
        agent.id
    );

    // The session was journaled: one session, one run of this agent.
    let sessions = uuldana_runtime::journal::list(&data);
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    assert_eq!(sessions[0].runs, 1);
    assert_eq!(
        sessions[0].last_provider.as_ref().unwrap().as_str(),
        agent.id
    );
}

fn some_agents() -> Option<&'static [Agent]> {
    let agents = support::agents();
    if agents.is_empty() {
        eprintln!("SKIPPED: no agent was found on this machine");
        return None;
    }
    Some(agents)
}

/// With no configuration of permissions at all, every agent that can run
/// under the default ceiling opens and exits on Ctrl+C.
#[tokio::test]
async fn every_agent_opens_without_any_setup_and_exits_on_ctrl_c() {
    let Some(agents) = some_agents() else { return };
    for agent in agents {
        if support::launch_args(agent, &automatic_ceiling()).is_none() {
            eprintln!(
                "SKIPPED {}: it cannot run under the default ceiling",
                agent.id
            );
            continue;
        }
        eprintln!("opening {}", agent.id);
        run_agent(agent, None, &[], Ending::CtrlC, |screen, _| {
            assert!(
                screen.contains("EARLY ACCESS"),
                "{}: the header does not say early access",
                agent.id
            );
        })
        .await;
    }
}

/// `--bypass-permissions` overrides a strict configuration: the real process
/// must be started with the agent's own flags for no restriction.
#[tokio::test]
async fn bypass_permissions_flags_reach_every_agent() {
    let Some(agents) = some_agents() else { return };
    let strict = r#"{"approval":"ask_every_action","filesystem":"read_only","network":"allowed"}"#;
    let mut checked = 0;
    for agent in agents {
        let Some(expected) = support::launch_args(agent, &PermissionPolicy::unrestricted()) else {
            eprintln!("SKIPPED {}: no mode without restrictions", agent.id);
            continue;
        };
        checked += 1;
        run_agent(
            agent,
            Some(strict),
            &["--bypass-permissions"],
            Ending::Terminate,
            |screen, pid| {
                // The agent can be spawned a moment after the first screen
                // settles, so give the process list time to show it.
                let mut lines = String::new();
                for _ in 0..40 {
                    lines = child_command_lines(pid).join("\n");
                    if !lines.trim().is_empty() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
                for arg in &expected {
                    assert!(
                        lines.contains(arg.as_str()),
                        "{} was not started with {arg}: {lines}\nscreen:\n{screen}",
                        agent.id
                    );
                }
            },
        )
        .await;
    }
    assert!(checked > 0 || agents.is_empty());
}

/// Control: the same strict configuration without the flag stays strict.
#[tokio::test]
async fn without_the_flag_every_agent_stays_inside_the_configured_ceiling() {
    let Some(agents) = some_agents() else { return };
    let strict = r#"{"approval":"ask_every_action","filesystem":"read_only","network":"allowed"}"#;
    let ceiling = PermissionPolicy {
        approval: ApprovalMode::AskEveryAction,
        sandbox: SandboxPolicy {
            filesystem: FilesystemAccess::ReadOnly,
            network: NetworkAccess::Allowed,
        },
    };
    let args_for = support::launch_args;
    for agent in agents {
        let Some(expected) = args_for(agent, &ceiling) else {
            eprintln!("SKIPPED {}: no mode inside that ceiling", agent.id);
            continue;
        };
        let free = args_for(agent, &PermissionPolicy::unrestricted()).unwrap_or_default();
        run_agent(agent, Some(strict), &[], Ending::Terminate, |_, pid| {
            let lines = child_command_lines(pid).join("\n");
            for arg in &expected {
                assert!(
                    lines.contains(arg.as_str()),
                    "{} did not get {arg}: {lines}",
                    agent.id
                );
            }
            for arg in free.iter().filter(|arg| !expected.contains(arg)) {
                assert!(
                    !lines.contains(arg.as_str()),
                    "{} was given {arg} without the flag: {lines}",
                    agent.id
                );
            }
        })
        .await;
    }
}

/// A saved session keeps the ceiling it was created with. `resume
/// --bypass-permissions` must refuse instead of quietly staying strict.
#[tokio::test]
async fn resume_with_bypass_is_refused_for_a_stricter_saved_session() {
    let Some(agents) = some_agents() else { return };
    let scratch = Scratch::new("uuldana-sesi");
    let workspace = scratch.0.join("proyek");
    let data = scratch.0.join("data");
    std::fs::create_dir_all(&workspace).unwrap();
    support::seed_cache(&data);
    let config = scratch.0.join("config.json");
    std::fs::write(
        &config,
        format!(
            r#"{{"version":1,"permissions":{{"approval":"ask_every_action","filesystem":"read_only","network":"allowed"}},"providers":[{{"id":"{}"}}]}}"#,
            agents[0].id
        ),
    )
    .unwrap();

    // A session that was created with the strict ceiling of that configuration.
    let strict = PermissionPolicy::most_restrictive();
    let now = Timestamp::from_system_time(std::time::SystemTime::now());
    let session = Session::new(
        SessionId::new("s-strict-1").unwrap(),
        workspace.clone(),
        strict,
        now,
    )
    .unwrap();
    uuldana_runtime::journal::Journal::create(
        &uuldana_runtime::journal::path_for(&data, session.id()),
        &session,
    )
    .unwrap();

    let permissions = validate_mapping(
        &PermissionPolicy::default(),
        &PermissionMapping::new(PermissionPolicy::default()),
    )
    .unwrap();
    let request = LaunchRequest::new(workspace.clone(), TransportKind::NativePty, permissions);
    let plan = LaunchPlan::new(
        PathBuf::from(env!("CARGO_BIN_EXE_uuldana")),
        workspace,
        TransportSpec::NativePty {
            initial_size: TerminalSize::new(40, 200),
        },
    )
    .args(["resume", "--bypass-permissions"])
    .env_set("UULDANA_CONFIG", &config)
    .env_set("UULDANA_DATA", &data);
    let mut process = Process::spawn(&plan, &request).await.unwrap();

    let mut screen = Vec::new();
    let mut answered_cursor = 0;
    let exit = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            match process.next_frame().await.unwrap() {
                Some(Frame::Output(bytes)) => screen.extend(bytes),
                Some(Frame::Exited(info)) => return Some(info),
                Some(_) => {}
                None => return None,
            }
            let asked = screen.windows(4).filter(|w| *w == b"\x1b[6n").count();
            while answered_cursor < asked {
                process.write_input(b"\x1b[1;1R").await.unwrap();
                answered_cursor += 1;
            }
        }
    })
    .await;
    let text = String::from_utf8_lossy(&screen).into_owned();
    let exit = exit.unwrap_or_else(|_| panic!("uuldana did not exit:\n{text}"));
    let exit = exit.expect("exit info");
    assert!(!exit.success, "{text}");
    assert!(text.contains("stricter permission ceiling"), "{text}");
}
