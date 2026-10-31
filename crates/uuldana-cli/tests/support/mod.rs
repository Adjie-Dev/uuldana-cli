//! Shared by the tests that run the real agents installed on this machine.
//!
//! No agent is named here. The agents are the ones `uuldana` itself learns on
//! this machine, and a test that finds none says so instead of passing for an
//! agent that is not there.

#![allow(dead_code)]

use std::{
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::Duration,
};

use uuldana_core::{
    Installation, LaunchRequest, PermissionPolicy, ProviderAdapter, TransportKind, validate_mapping,
};
use uuldana_runtime::LocalHost;

/// An agent learned on this machine.
pub struct Agent {
    pub id: String,
    pub adapter: Arc<dyn ProviderAdapter>,
}

/// Every agent learned on this machine, asked for once per test binary.
pub fn agents() -> &'static [Agent] {
    static AGENTS: OnceLock<Vec<Agent>> = OnceLock::new();
    AGENTS.get_or_init(|| {
        let cache = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("programs.json");
        uuldana_providers::learn_installed(&LocalHost::new(), Some(&cache), &|asked| {
            eprintln!("asking {asked} programs for their help");
        })
        .into_iter()
        .map(|adapter| Agent {
            id: adapter.descriptor().id.as_str().to_owned(),
            adapter,
        })
        .collect()
    })
}

/// Copies what was learned about this machine into `data`, so a `uuldana`
/// started with that data directory does not look at every program again. Call
/// it after [`agents`], which fills the shared cache.
pub fn seed_cache(data: &std::path::Path) {
    let shared = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(data).unwrap();
    for name in ["programs.json", "programs.shims.json"] {
        let from = shared.join(name);
        if from.exists() {
            std::fs::copy(&from, data.join(name)).unwrap();
        }
    }
}

/// The arguments `agent` is launched with under `policy`, or `None` when its
/// permission modes cannot stay inside `policy`.
pub fn launch_args(agent: &Agent, policy: &PermissionPolicy) -> Option<Vec<String>> {
    let host = LocalHost::new();
    let installation: Installation = agent.adapter.detect(&host).ok()?;
    let mapping = agent.adapter.map_permissions(&installation, policy);
    let validated = validate_mapping(policy, &mapping).ok()?;
    let request = LaunchRequest::new(
        std::env::current_dir().ok()?,
        TransportKind::NativePty,
        validated,
    );
    let plan = agent.adapter.launch_plan(&installation, &request).ok()?;
    Some(
        plan.args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            // A session id chosen at launch differs on every start.
            .filter(|arg| !looks_like_a_session_id(arg))
            .collect(),
    )
}

fn looks_like_a_session_id(text: &str) -> bool {
    text.len() == 36
        && text.chars().enumerate().all(|(at, c)| {
            matches!(at, 8 | 13 | 18 | 23) == (c == '-') && (c == '-' || c.is_ascii_hexdigit())
        })
}

/// Tests of one binary run in parallel, so each needs its own directory.
pub fn unique() -> usize {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}

/// A scratch directory inside cargo's area for integration tests.
pub struct Scratch(pub PathBuf);

impl Scratch {
    pub fn new(name: &str) -> Self {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "{name}-{}-{}",
            std::process::id(),
            unique()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // An agent that was just stopped can still hold its working directory
        // for a moment on Windows, so retry, and give up quietly.
        for _ in 0..25 {
            if std::fs::remove_dir_all(&self.0).is_ok() || !self.0.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

/// Command lines of the processes started by `parent`.
pub fn child_command_lines(parent: u32) -> Vec<String> {
    let query = format!(
        "Get-CimInstance Win32_Process | Where-Object {{ $_.ParentProcessId -eq {parent} }} | ForEach-Object {{ $_.CommandLine }}"
    );
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &query])
        .output()
        .expect("powershell runs");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}
