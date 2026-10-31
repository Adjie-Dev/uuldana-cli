//! A failover stops the agent it leaves. This checks that every agent installed
//! on this machine shuts down gracefully on the stop Uuldana asks for, so its
//! own session state is saved and the run ends `Cancelled`, not `Terminated`.
//!
//! Only the agent's interface is opened; no prompt is sent. A machine with no
//! agent learns none, and the test says so.

mod support;

use std::{
    io::Write,
    sync::{Arc, Mutex},
    time::Duration,
};

use support::{Agent, Scratch};
use tokio::sync::mpsc;
use uuldana_core::*;
use uuldana_runtime::{
    LocalHost, StartOptions, Supervisor,
    config::automatic_ceiling,
    relay::{RelayEnd, relay},
};

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
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
    fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }
}

async fn stops_gracefully(agent: &Agent) {
    let id = agent.id.as_str();
    let policy = automatic_ceiling();
    let mut registry = ProviderRegistry::new();
    registry.register(agent.adapter.clone()).unwrap();
    let probe = Arc::new(LocalHost::new());
    let scratch = Scratch::new(&format!("uuldana-henti-{id}"));
    let session = Session::new(
        SessionId::new("s-stop").unwrap(),
        scratch.0.clone(),
        policy,
        Timestamp::from_unix_millis(1),
    )
    .unwrap();
    let mut supervisor = Supervisor::new(registry, probe, session);
    let mut options = StartOptions::new(ProviderId::new(id).unwrap(), RunId::new("run-1").unwrap());
    options.permissions = policy;
    options.transport = Some(TransportKind::NativePty);
    options.terminal_size = Some(TerminalSize::new(40, 120));
    supervisor.start(options).await.unwrap();

    let screen = Screen::default();
    let (_keys, input) = mpsc::unbounded_channel();
    let mut position = || (1u16, 1u16);
    let cursor: Option<uuldana_runtime::relay::CursorPosition<'_>> =
        cfg!(windows).then_some(&mut position);
    let watched = screen.clone();
    let (result, _) = tokio::time::timeout(
        Duration::from_secs(120),
        relay(
            &mut supervisor,
            input,
            screen.clone(),
            || Some(TerminalSize::new(40, 120)),
            cursor,
            // Stop once the interface has drawn itself and gone quiet, like a
            // failover would.
            move |_, idle| watched.len() > 300 && idle >= Duration::from_millis(1500),
        ),
    )
    .await
    .unwrap_or_else(|_| panic!("{id}: never reached a stop point\n{}", screen.text()));
    assert!(matches!(result, Ok(RelayEnd::Stopped)), "{id}: {result:?}");
    assert!(
        !supervisor.last_stop_forced(),
        "{id} had to be force-killed"
    );
    assert_eq!(
        supervisor.session().runs()[0].state(),
        RunState::Ended(RunEnd::Cancelled),
        "{id}"
    );
}

#[tokio::test]
async fn every_agent_on_this_machine_stops_gracefully() {
    let agents = support::agents();
    if agents.is_empty() {
        eprintln!("SKIPPED: no agent was found on this machine");
        return;
    }
    for agent in agents {
        if support::launch_args(agent, &automatic_ceiling()).is_none() {
            eprintln!(
                "SKIPPED {}: it cannot run under the default ceiling",
                agent.id
            );
            continue;
        }
        eprintln!("stopping {}", agent.id);
        stops_gracefully(agent).await;
    }
}
