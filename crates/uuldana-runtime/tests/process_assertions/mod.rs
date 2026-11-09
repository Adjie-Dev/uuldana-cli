//! Test assertions for actual process termination, independent of heartbeats.
use std::time::{Duration, Instant};

pub fn assert_process_exited(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while process_running(pid) {
        assert!(
            Instant::now() < deadline,
            "process {pid} remained alive after cleanup"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(windows)]
pub fn process_running(pid: u32) -> bool {
    subc_jobobject::process_exists(pid)
}

#[cfg(unix)]
pub fn process_running(pid: u32) -> bool {
    let pid_value = rustix::process::Pid::from_raw(pid as i32).expect("positive process ID");
    match rustix::process::test_kill_process(pid_value) {
        Err(rustix::io::Errno::SRCH) => false,
        // macOS answers EPERM for a process that has exited and is not reaped
        // yet; it still exists until its parent reaps it.
        Err(rustix::io::Errno::PERM) => true,
        Err(error) => panic!("cannot observe process {pid}: {error}"),
        Ok(()) => {
            // A Linux zombie has exited but awaits its new parent's reap; our
            // test process cannot reap a reparented descendant itself.
            #[cfg(target_os = "linux")]
            if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                return !stat
                    .rsplit_once(") ")
                    .is_some_and(|(_, fields)| fields.starts_with("Z "));
            }
            true
        }
    }
}
