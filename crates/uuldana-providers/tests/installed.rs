//! Learning against the real machine. A machine with no agent on it learns
//! none, and the test says so; it never passes for an agent that is not there.
use std::{
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
};

use uuldana_provider_sdk::{LaunchRequest, TransportKind, validate_mapping};
use uuldana_runtime::LocalHost;

#[test]
fn installed_agents_are_learned_and_resolve_to_a_native_executable() {
    let cache = std::env::temp_dir()
        .join(format!("uuldana-installed-test-{}", std::process::id()))
        .join("programs.json");
    let host = LocalHost::new();
    let announced = AtomicUsize::new(0);
    let answered = AtomicUsize::new(0);
    let highest = AtomicUsize::new(0);
    let adapters = uuldana_providers::learn_installed_with_progress(
        &host,
        Some(&cache),
        &|asked| {
            announced.store(asked, Ordering::Relaxed);
            eprintln!("asking {asked} programs for their help");
        },
        &|done, total| {
            assert_eq!(total, announced.load(Ordering::Relaxed));
            answered.fetch_add(1, Ordering::Relaxed);
            highest.fetch_max(done, Ordering::Relaxed);
        },
    );
    // Every program that was asked reports once, and the count ends at the total.
    // A look at three programs or fewer is not announced.
    if announced.load(Ordering::Relaxed) > 0 {
        assert_eq!(
            answered.load(Ordering::Relaxed),
            announced.load(Ordering::Relaxed)
        );
    }
    assert_eq!(
        highest.load(Ordering::Relaxed),
        answered.load(Ordering::Relaxed)
    );
    for adapter in &adapters {
        let found = adapter.detect(&host).unwrap_or_else(|error| {
            panic!("{} detection failed: {error}", adapter.descriptor().id)
        });
        assert!(found.executable.is_absolute(), "{:?}", found.executable);
        assert_native(&found.executable);
        assert!(found.version.is_some());
        // A new conversation must not start inside a subcommand that works on
        // an earlier one (it would open a picker, or copy a session).
        let policy = uuldana_runtime::config::automatic_ceiling();
        let mapping = adapter.map_permissions(&found, &policy);
        let validated = validate_mapping(&policy, &mapping)
            .unwrap_or_else(|error| panic!("{}: {error}", adapter.descriptor().id));
        let request = LaunchRequest::new(
            std::env::current_dir().unwrap(),
            TransportKind::NativePty,
            validated,
        );
        let plan = adapter.launch_plan(&found, &request).unwrap();
        let first = plan
            .args
            .first()
            .map(|arg| arg.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        assert!(
            !["resume", "continue", "fork", "restore"].contains(&first.as_str()),
            "{} would start with {first:?}",
            adapter.descriptor().id
        );
        eprintln!(
            "LEARNED {} -> {} ({})",
            adapter.descriptor().id,
            found.executable.display(),
            found.version.unwrap()
        );
        for note in &found.notes {
            eprintln!("    {note}");
        }
    }
    eprintln!("{} agent(s) learned on this machine", adapters.len());
    if let Some(dir) = cache.parent() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

fn assert_native(path: &Path) {
    let ext = path.extension().map(|e| e.to_ascii_lowercase());
    assert!(
        !matches!(
            ext.as_deref().and_then(|e| e.to_str()),
            Some("cmd" | "bat" | "ps1" | "js")
        ),
        "{path:?} is a script, not a native executable"
    );
}
