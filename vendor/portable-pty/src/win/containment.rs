//! Uuldana's isolated Windows containment boundary. See UULDANA_PATCH.md.
use super::consoleflags::CtrlCRelease;
use super::WinChild;
use crate::Child;
use anyhow::ensure;
use filedescriptor::OwnedHandle;
use std::io;
use std::os::windows::io::AsRawHandle;
use winapi::shared::winerror::WAIT_TIMEOUT;
use winapi::um::processthreadsapi::{ResumeThread, TerminateProcess};
use winapi::um::synchapi::WaitForSingleObject;
use winapi::um::winbase::WAIT_OBJECT_0;

/// An owned, non-inherited handle for observing direct-child exit without PID lookup.
#[derive(Debug)]
pub struct ProcessStatus(OwnedHandle);

impl ProcessStatus {
    /// Duplicate a process handle; the source remains owned by its caller.
    pub fn from_handle(handle: &impl AsRawHandle) -> anyhow::Result<Self> {
        Ok(Self(OwnedHandle::dup(handle)?))
    }

    /// Zero-time wait distinguishes exit code 259 from a still-running process.
    pub fn has_exited(&self) -> io::Result<bool> {
        // SAFETY: our owned duplicate stays live throughout the OS call. The
        // kernel treats HANDLE as an opaque value, not an address to dereference.
        match unsafe { WaitForSingleObject(self.0.as_raw_handle() as _, 0) } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            _ => Err(io::Error::last_os_error()),
        }
    }
}

/// Owns a child that has never executed and its original primary-thread handle.
/// Dropping it before a successful resume terminates the child with bounded reap.
pub struct SuspendedChild {
    child: Option<WinChild>,
    main_thread: OwnedHandle,
    ctrl_c_release: Option<CtrlCRelease>,
}

impl subc_jobobject::ProcessHandle for SuspendedChild {
    fn handle(&self) -> Option<std::os::windows::io::RawHandle> {
        self.child.as_ref().and_then(Child::as_raw_handle)
    }
}

impl SuspendedChild {
    pub(super) fn new(child: WinChild, main_thread: OwnedHandle) -> Self {
        Self {
            child: Some(child),
            main_thread,
            ctrl_c_release: None,
        }
    }

    pub(super) fn record_ctrl_c_release(&mut self, release: CtrlCRelease) {
        self.ctrl_c_release = Some(release);
    }

    /// Why the child may still carry its creator's "ignore CTRL+C" attribute:
    /// the release failed (per block, with the reason), was only partial, or was
    /// not attempted. `None` when every block was released or needed no change.
    /// Read it before `resume_in_job`, which consumes the guard.
    pub fn ctrl_c_release_diagnostic(&self) -> Option<String> {
        self.ctrl_c_release
            .as_ref()
            .and_then(CtrlCRelease::diagnostic)
    }

    /// Direct PID for diagnostics and tests; never needed for assignment/resume.
    pub fn process_id(&self) -> Option<u32> {
        self.child.as_ref().and_then(Child::process_id)
    }

    /// Observe abandonment cleanup independently of the guard's ownership.
    pub fn process_status(&self) -> anyhow::Result<ProcessStatus> {
        self.child
            .as_ref()
            .expect("suspended child")
            .process_status()
    }

    /// Assign the never-run process to the provided Job, then resume its retained
    /// primary thread exactly once. Every error leaves the guard armed for Drop.
    pub fn resume_in_job(
        mut self,
        job: &subc_jobobject::JobObject,
    ) -> anyhow::Result<Box<dyn Child + Send + Sync>> {
        job.assign(&self)?;
        // SAFETY: CreateProcessW returned this owned suspended thread. It is
        // not exposed to callers and remains live for this call. Release only
        // our creation suspension; another OS component may hold another one.
        let previous = unsafe { ResumeThread(self.main_thread.as_raw_handle() as _) };
        ensure!(
            previous != u32::MAX,
            "ResumeThread failed: {}",
            io::Error::last_os_error()
        );
        ensure!(
            previous >= 1,
            "unexpected primary-thread suspend count: {}",
            previous
        );
        Ok(Box::new(self.child.take().expect("suspended child")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{native_pty_system, CommandBuilder, PtySize};
    use std::time::{Duration, Instant};
    use winapi::um::processthreadsapi::SuspendThread;

    #[test]
    fn resume_releases_only_creation_suspend_after_job_assignment() {
        let pair = native_pty_system().openpty(PtySize::default()).unwrap();
        // Service ConPTY's startup cursor request while awaiting native exit.
        let mut reader = pair.master.try_clone_reader().unwrap();
        let mut writer = pair.master.take_writer().unwrap();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let mut bytes = Vec::new();
            let mut chunk = [0; 8192];
            while let Ok(n) = reader.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                bytes.extend_from_slice(&chunk[..n]);
                if bytes.windows(4).any(|b| b == b"\x1b[6n") {
                    let _ = writer.write_all(b"\x1b[1;1R");
                    let _ = writer.flush();
                    bytes.clear();
                }
            }
        });
        let executable = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32")
            .join("cmd.exe");
        let mut command = CommandBuilder::new(executable);
        command.args(["/d", "/c", "exit 0"]);
        let guard = pair.slave.spawn_command_suspended(command).unwrap();
        let thread = OwnedHandle::dup(&guard.main_thread).unwrap();
        let status = guard.process_status().unwrap();
        // SAFETY: an owned duplicate of the original primary-thread handle.
        // The extra suspension belongs to this test and is released below.
        let previous = unsafe { SuspendThread(thread.as_raw_handle() as _) };
        assert!((1..u32::MAX).contains(&previous));
        let job = subc_jobobject::JobObject::new().unwrap();
        let mut child = guard.resume_in_job(&job).unwrap();
        assert_eq!(job.process_count().unwrap(), 1);
        assert!(!status.has_exited().unwrap());
        // Only our extra hold remains. A loop resuming all suspensions would
        // fail this assertion and violate the other suspender's ownership.
        let previous = unsafe { ResumeThread(thread.as_raw_handle() as _) };
        assert!((1..u32::MAX).contains(&previous));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !status.has_exited().unwrap() {
            assert!(Instant::now() < deadline, "resumed child must exit");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(child.wait().unwrap().success());
        drop(pair.slave);
        let master = pair.master;
        std::thread::spawn(move || drop(master));
    }
}

impl Drop for SuspendedChild {
    fn drop(&mut self) {
        if let Some(child) = &self.child {
            let handle = child.as_raw_handle().expect("owned process handle");
            // SAFETY: the child still owns this process handle. No PID lookup,
            // borrowed pointer, or concurrent handle close is involved.
            if unsafe { TerminateProcess(handle as _, 1) } == 0 {
                log::error!(
                    "suspended child termination failed: {}",
                    io::Error::last_os_error()
                );
            }
            // SAFETY: same owned process handle, and a finite cleanup deadline.
            if unsafe { WaitForSingleObject(handle as _, 5000) } != WAIT_OBJECT_0 {
                log::error!("suspended child did not signal exit within 5 seconds");
            }
        }
    }
}
