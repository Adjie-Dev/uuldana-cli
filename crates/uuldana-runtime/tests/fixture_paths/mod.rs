//! Test-only identifiers: Windows clock precision does not ensure uniqueness.
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

pub fn heartbeat_path(prefix: &str) -> PathBuf {
    heartbeat_path_at(
        prefix,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    )
}

/// Also permits the uniqueness regression to force a coarse/frozen clock.
pub fn heartbeat_path_at(prefix: &str, nanos: u128) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "{prefix}-{}-{nanos}-{sequence}",
        std::process::id(),
    ))
}
