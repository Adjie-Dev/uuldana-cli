//! Owned native lifecycle evidence for bounded transport teardown.
//! Unsafe operations are confined here and sys.rs; see UULDANA_PATCH.md.
#![allow(unsafe_code)]
use crate::JobObject;
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    mem::size_of,
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, DuplicateHandle, GetLastError, DUPLICATE_SAME_ACCESS, HANDLE,
        INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    System::{
        JobObjects::{
            IsProcessInJob, JobObjectAssociateCompletionPortInformation,
            JobObjectBasicAccountingInformation, JobObjectBasicProcessIdList,
            QueryInformationJobObject, SetInformationJobObject,
            JOBOBJECT_ASSOCIATE_COMPLETION_PORT, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
        },
        Threading::{
            GetCurrentProcess, GetProcessId, OpenProcess, WaitForSingleObject,
            PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        },
        IO::{CreateIoCompletionPort, GetQueuedCompletionStatus},
    },
};

#[derive(Debug)]
pub(crate) struct OwnedPort(HANDLE);
// SAFETY: exclusive owned process-wide port handle; no Sync implementation.
unsafe impl Send for OwnedPort {}
impl Drop for OwnedPort {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    } // SAFETY: closes its one owned handle.
}
impl OwnedPort {
    pub(crate) fn new() -> io::Result<Self> {
        // SAFETY: documented creation sentinel, no file association, no pointers.
        let handle = unsafe {
            CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, u32::MAX)
        };
        if handle.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(handle))
        }
    }
    pub(crate) fn associate(&self, job: HANDLE) -> io::Result<()> {
        let info = JOBOBJECT_ASSOCIATE_COMPLETION_PORT {
            CompletionKey: std::ptr::null_mut(),
            CompletionPort: self.0,
        };
        // SAFETY: both handles are owned/live, correctly sized/aligned SDK data;
        // opaque key is never dereferenced. Called before any Job assignment.
        if unsafe {
            SetInformationJobObject(
                job,
                JobObjectAssociateCompletionPortInformation,
                (&info as *const JOBOBJECT_ASSOCIATE_COMPLETION_PORT).cast(),
                size_of::<JOBOBJECT_ASSOCIATE_COMPLETION_PORT>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    fn birth_hints(&self) -> io::Result<Vec<u32>> {
        let mut hints = Vec::new();
        // Bound per-poll work. Any remaining messages are consumed next poll;
        // cumulative accounting prevents treating a partial queue as complete.
        for _ in 0..2048 {
            let mut message = 0;
            let mut key = 0;
            let mut value = std::ptr::null_mut();
            // SAFETY: live port and initialized writable SDK output slots.
            // Zero timeout; value is an opaque PID payload, never dereferenced.
            let ok =
                unsafe { GetQueuedCompletionStatus(self.0, &mut message, &mut key, &mut value, 0) };
            if ok == 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(WAIT_TIMEOUT as i32) {
                    break;
                }
                return Err(error);
            }
            if key != 0 {
                return Err(io::Error::other("unexpected private Job completion key"));
            }
            // JOB_OBJECT_MSG_NEW_PROCESS = 6. EXIT and ACTIVE_PROCESS_ZERO are
            // deliberately ignored: a notification is not native exit proof.
            if message == 6 {
                let pid = u32::try_from(value as usize)
                    .map_err(|_| io::Error::other("invalid Job birth PID"))?;
                if pid == 0 {
                    return Err(io::Error::other("zero Job birth PID"));
                }
                hints.push(pid);
            }
        }
        Ok(hints)
    }
}
enum Lookup {
    Pinned(OwnedProcess),
    Gone,
    Unverified,
}

#[derive(Debug)]
pub(crate) struct OwnedProcess {
    handle: HANDLE,
    pid: u32,
}
// SAFETY: exclusively owned process-wide handle, moved without closing it.
// No Sync implementation; every concurrent owner must own its own duplicate.
unsafe impl Send for OwnedProcess {}
impl Drop for OwnedProcess {
    fn drop(&mut self) {
        // SAFETY: this wrapper owns exactly one non-inherited handle.
        unsafe {
            CloseHandle(self.handle);
        }
    }
}
impl OwnedProcess {
    // Only the established ProcessHandle assignment boundary supplies raw input.
    pub(crate) fn duplicate(handle: HANDLE) -> io::Result<Self> {
        let mut copy = std::ptr::null_mut();
        // SAFETY: caller holds the source process handle live; target output is
        // an initialized HANDLE slot. The duplicate is non-inherited and owned.
        let ok = unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                handle,
                GetCurrentProcess(),
                &mut copy,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut owned = Self {
            handle: copy,
            pid: 0,
        };
        // SAFETY: valid owned process handle with the source's query rights.
        owned.pid = unsafe { GetProcessId(copy) };
        if owned.pid == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(owned)
    }
    fn open_in_job(pid: u32, job: &JobObject) -> io::Result<Lookup> {
        // SAFETY: numeric PID is used only to acquire a non-inherited owned
        // handle. It is never signaled/killed by PID and membership is verified.
        let handle = unsafe {
            OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                pid,
            )
        };
        if handle.is_null() {
            let error = io::Error::last_os_error();
            // A trusted nonzero birth PID no longer names a process object.
            // Only birth hints may count this; snapshot races are not counted.
            // Access denied is deliberately not treated as native-gone.
            if error.raw_os_error() == Some(87) {
                return Ok(Lookup::Gone);
            }
            return Err(error);
        }
        let owned = Self { handle, pid };
        let mut member = 0;
        // SAFETY: both handles are live/owned; output BOOL slot is valid.
        if unsafe { IsProcessInJob(handle, job.handle, &mut member) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if member == 0 {
            // PID reuse or removal raced acquisition. Never use this handle as
            // evidence for the queried member; native coverage remains incomplete.
            return Ok(Lookup::Unverified);
        }
        Ok(Lookup::Pinned(owned))
    }
    fn signaled(&self) -> io::Result<bool> {
        // SAFETY: handle remains owned for a zero-time native lifecycle wait.
        match unsafe { WaitForSingleObject(self.handle, 0) } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            _ => Err(io::Error::last_os_error()),
        }
    }
}

/// Completion evidence for observed native processes, independent of exit codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionState {
    /// At least one pinned member is not signaled, or Job membership remains.
    Pending,
    /// Every identity is pinned and signaled, or independently native-gone.
    NativeComplete,
    /// Accounting identifies missed identities. EOF cannot replace their native
    /// lifecycle evidence; callers must bound retries and report the gap.
    CoverageIncomplete,
}

/// Pins process identities before termination, then reconciles membership races.
/// It never treats an empty process list alone as native completion. No lossy
/// completion-port notification is used as a correctness prerequisite.
#[derive(Debug)]
pub struct CompletionEvidence {
    /// Current active membership; never sufficient alone.
    pub active_processes: u32,
    /// Cumulative assigned identities, including members already removed.
    pub total_processes: u32,
    /// Accounted identities: pinned handles plus deduplicated native-gone births.
    pub pinned_processes: usize,
    /// True only if every owned native handle is signaled.
    pub all_handles_signaled: bool,
}
impl CompletionEvidence {
    /// Classify independent membership, identity-coverage and native evidence.
    pub fn state(&self) -> io::Result<CompletionState> {
        classify(
            self.active_processes,
            self.total_processes,
            self.pinned_processes,
            self.all_handles_signaled,
        )
    }
}
#[derive(Debug)]
pub struct CompletionObserver {
    processes: BTreeMap<u32, OwnedProcess>,
    gone_pids: BTreeSet<u32>,
}
impl CompletionObserver {
    /// Capture before requesting Job termination. Explicitly assigned children
    /// have been pinned at assignment, before resume, including already-exited roots.
    pub fn capture(job: &JobObject) -> io::Result<Self> {
        let mut observer = Self {
            processes: BTreeMap::new(),
            gone_pids: BTreeSet::new(),
        };
        for process in job
            .assigned
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
        {
            let owned = OwnedProcess::duplicate(process.handle)?;
            observer.processes.insert(owned.pid, owned);
        }
        observer.reconcile(job)?;
        Ok(observer)
    }
    fn reconcile(&mut self, job: &JobObject) -> io::Result<()> {
        // Birth messages are hints only; losing them cannot produce success.
        for pid in job.completion_port.birth_hints()? {
            if self.processes.contains_key(&pid) {
                continue;
            }
            match OwnedProcess::open_in_job(pid, job)? {
                Lookup::Pinned(owned) => {
                    self.processes.insert(pid, owned);
                }
                Lookup::Gone => {
                    self.gone_pids.insert(pid);
                }
                Lookup::Unverified => {}
            }
        }
        // This independent snapshot recovers live members even if all birth
        // notifications were dropped. Snapshot disappearance is NOT counted.
        for pid in process_ids(job)? {
            if let std::collections::btree_map::Entry::Vacant(entry) = self.processes.entry(pid) {
                if let Lookup::Pinned(owned) = OwnedProcess::open_in_job(pid, job)? {
                    entry.insert(owned);
                }
            }
        }
        Ok(())
    }
    /// Zero-time observation; caller provides polling and the existing cleanup
    /// deadline. Growing/churning lists and native errors fail explicitly.
    pub fn poll(&mut self, job: &JobObject) -> io::Result<CompletionState> {
        self.evidence(job)?.state()
    }
    /// Collect native evidence without collapsing membership and lifecycle state.
    pub fn evidence(&mut self, job: &JobObject) -> io::Result<CompletionEvidence> {
        self.reconcile(job)?;
        let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        // SAFETY: owned Job and correctly aligned/sized writable SDK structure.
        if unsafe {
            QueryInformationJobObject(
                job.handle,
                JobObjectBasicAccountingInformation,
                (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let signaled = self
            .processes
            .values()
            .map(OwnedProcess::signaled)
            .collect::<io::Result<Vec<_>>>()?
            .into_iter()
            .all(|v| v);
        Ok(CompletionEvidence {
            active_processes: info.ActiveProcesses,
            total_processes: info.TotalProcesses,
            pinned_processes: self.processes.len() + self.gone_pids.len(),
            all_handles_signaled: signaled,
        })
    }
}
fn classify(
    active: u32,
    total: u32,
    captured: usize,
    signaled: bool,
) -> io::Result<CompletionState> {
    if captured > total as usize {
        return Err(io::Error::other(
            "Job accounting regressed below pinned identities",
        ));
    }
    Ok(if active != 0 || !signaled {
        CompletionState::Pending
    } else if captured == total as usize {
        CompletionState::NativeComplete
    } else {
        CompletionState::CoverageIncomplete
    })
}
fn process_ids(job: &JobObject) -> io::Result<Vec<u32>> {
    process_ids_with_query(|words| {
        // SAFETY: live Job, aligned initialized storage, exact allocation length;
        // no references into this buffer exist during the native write.
        let ok = unsafe {
            QueryInformationJobObject(
                job.handle,
                JobObjectBasicProcessIdList,
                words.as_mut_ptr().cast(),
                std::mem::size_of_val(words) as u32,
                std::ptr::null_mut(),
            )
        };
        let code = if ok == 0 {
            unsafe { GetLastError() }
        } else {
            0
        };
        if ok == 0 && code != 234 {
            return Err(io::Error::from_raw_os_error(code as i32));
        }
        Ok(ok != 0)
    })
}

fn process_ids_with_query(
    mut query: impl FnMut(&mut [usize]) -> io::Result<bool>,
) -> io::Result<Vec<u32>> {
    let header_words = 8usize.div_ceil(size_of::<usize>());
    let mut width = 64usize;
    let mut last = (0, 0, 0, 0, 0);
    for _ in 0..8 {
        // usize storage supplies native pointer alignment for the SDK's trailing
        // ULONG_PTR array. Eight header bytes precede it on both x86 and x64.
        let mut words = vec![0usize; header_words + width];
        let ok = query(&mut words)?;
        let header: Vec<u8> = words.iter().flat_map(|w| w.to_ne_bytes()).take(8).collect();
        let assigned = u32::from_ne_bytes(header[..4].try_into().unwrap()) as usize;
        let returned = u32::from_ne_bytes(header[4..8].try_into().unwrap()) as usize;
        last = (
            ok as u32,
            assigned,
            returned,
            width,
            if ok { 0 } else { 234 },
        );
        if ok && returned <= width {
            // A successful query defines only this prefix as valid. Assigned
            // and returned counts can differ during termination; accounting,
            // not this acquisition snapshot, establishes lifecycle coverage.
            return words[header_words..header_words + returned]
                .iter()
                .map(|pid| {
                    u32::try_from(*pid)
                        .map_err(|_| io::Error::other("Job returned an invalid process identifier"))
                })
                .collect();
        }
        width = (width * 2).max(assigned);
        if width > 65536 {
            break;
        }
    }
    Err(io::Error::other(format!(
        "Job process enumeration exceeded its bounded growth/retry limit: last (success, assigned, returned, capacity, error) = {last:?}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot(words: &mut [usize], assigned: u32, pids: &[usize]) {
        let bytes: Vec<_> = assigned
            .to_ne_bytes()
            .into_iter()
            .chain((pids.len() as u32).to_ne_bytes())
            .collect();
        for (index, chunk) in bytes.chunks(size_of::<usize>()).enumerate() {
            words[index] = usize::from_ne_bytes(chunk.try_into().unwrap());
        }
        let offset = 8usize.div_ceil(size_of::<usize>());
        words[offset..offset + pids.len()].copy_from_slice(pids);
    }
    #[test]
    fn successful_snapshot_accepts_valid_prefix_during_membership_churn() {
        for (assigned, pids) in [(1, vec![]), (2, vec![42])] {
            let actual = process_ids_with_query(|words| {
                snapshot(words, assigned, &pids);
                Ok(true)
            })
            .unwrap();
            assert_eq!(
                actual,
                pids.into_iter().map(|pid| pid as u32).collect::<Vec<_>>()
            );
        }
    }
    #[test]
    fn snapshot_growth_is_bounded_and_errors_are_preserved() {
        let mut calls = 0;
        let pids = process_ids_with_query(|words| {
            calls += 1;
            if calls == 1 {
                snapshot(words, 128, &[]);
                Ok(false)
            } else {
                snapshot(words, 2, &[42]);
                Ok(true)
            }
        })
        .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(pids, [42]);
        assert!(process_ids_with_query(|_| Ok(false)).is_err());
        let err = process_ids_with_query(|_| Err(io::Error::from_raw_os_error(5))).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(5));
        assert!(process_ids_with_query(|words| {
            let count = words.len() + 1;
            let header: Vec<_> = 1u32
                .to_ne_bytes()
                .into_iter()
                .chain((count as u32).to_ne_bytes())
                .collect();
            for (index, chunk) in header.chunks(size_of::<usize>()).enumerate() {
                words[index] = usize::from_ne_bytes(chunk.try_into().unwrap());
            }
            Ok(true)
        })
        .is_err());
    }
    #[test]
    fn observer_fixture_child() {
        let role = std::env::var("UULDANA_OBSERVER_FIXTURE_ROLE").unwrap_or_default();
        if role.is_empty() {
            return;
        }
        if role == "parent" {
            let _child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "completion::tests::observer_fixture_child",
                    "--test-threads=1",
                ])
                .env("UULDANA_OBSERVER_FIXTURE_ROLE", "grandchild")
                .spawn()
                .unwrap();
            loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
        assert_eq!(role, "grandchild");
        std::fs::write(
            std::env::var_os("UULDANA_OBSERVER_FIXTURE_PID").unwrap(),
            std::process::id().to_string(),
        )
        .unwrap();
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
    #[test]
    fn snapshot_fallback_recovers_birth_after_capture_with_notifications_discarded() {
        let path = std::env::temp_dir().join(format!(
            "uuldana-observer-fallback-{}.pid",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let job = JobObject::new().unwrap();
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "completion::tests::observer_fixture_child",
                "--test-threads=1",
            ])
            .env("UULDANA_OBSERVER_FIXTURE_ROLE", "parent")
            .env("UULDANA_OBSERVER_FIXTURE_PID", &path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        crate::suspend_on_create(&mut command);
        let mut child = command.spawn().unwrap();
        job.assign(&child).unwrap();
        let mut observer = CompletionObserver::capture(&job).unwrap();
        assert_eq!(observer.processes.len(), 1);
        crate::resume_main_thread(child.id()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let descendant = loop {
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(pid) = text.parse::<u32>() {
                    break pid;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "fixture descendant must start"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        // Deliberately lose all queued birth hints. The independent process-list
        // snapshot must capture the member born after the initial observer.
        let _ = job.completion_port.birth_hints().unwrap();
        assert_eq!(observer.poll(&job).unwrap(), CompletionState::Pending);
        assert!(observer.processes.contains_key(&descendant));
        job.terminate().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while observer.poll(&job).unwrap() != CompletionState::NativeComplete {
            assert!(
                std::time::Instant::now() < deadline,
                "native handles must signal within cleanup bound"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(observer
            .processes
            .get(&descendant)
            .unwrap()
            .signaled()
            .unwrap());
        child.wait().unwrap();
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn completion_dequeue_can_move_from_capture_thread_to_wait_worker() {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };
        let job = JobObject::new().unwrap();
        let executable = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32")
            .join("cmd.exe");
        let launch = |job: &JobObject| {
            let mut command = std::process::Command::new(&executable);
            command.args(["/d", "/c", "exit", "0"]);
            crate::suspend_on_create(&mut command);
            let child = command.spawn().unwrap();
            job.assign(&child).unwrap();
            crate::resume_main_thread(child.id()).unwrap();
            child
        };
        let mut first = launch(&job);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while job.completion_port.birth_hints().unwrap().is_empty() {
            assert!(std::time::Instant::now() < deadline);
        }
        let mut second = launch(&job);
        let ready = Arc::new(AtomicBool::new(false));
        let worker_ready = ready.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            while !worker_ready.load(Ordering::Acquire) {
                std::hint::spin_loop();
            }
            tx.send(job.completion_port.birth_hints().unwrap()).unwrap();
            job
        });
        ready.store(true, Ordering::Release);
        // Stay runnable: IOCP concurrency=1 must not gate the second caller.
        let hints = loop {
            if let Ok(hints) = rx.try_recv() {
                break hints;
            }
            assert!(std::time::Instant::now() < deadline);
            std::hint::spin_loop();
        };
        let _job = worker.join().unwrap();
        assert!(hints.contains(&second.id()));
        first.wait().unwrap();
        second.wait().unwrap();
    }
    #[test]
    fn zero_membership_does_not_override_unsignaled_owned_process() {
        assert_eq!(classify(0, 2, 2, false).unwrap(), CompletionState::Pending);
        assert_eq!(
            classify(0, 2, 2, true).unwrap(),
            CompletionState::NativeComplete
        );
        assert_eq!(
            classify(0, 3, 2, true).unwrap(),
            CompletionState::CoverageIncomplete
        );
        assert_eq!(classify(1, 3, 2, true).unwrap(), CompletionState::Pending);
        assert!(classify(0, 1, 2, true).is_err());
    }
    #[test]
    fn native_handles_preserve_exit_259_and_reject_foreign_job_membership() {
        let mut command = std::process::Command::new(
            std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
                .join("System32")
                .join("cmd.exe"),
        );
        command.args(["/d", "/c", "exit 259"]);
        let crate::ContainedChild { mut child, job } =
            crate::spawn_contained(&mut command).unwrap();
        let mut observer = CompletionObserver::capture(&job).unwrap();
        assert_eq!(child.wait().unwrap().code(), Some(259));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while observer.poll(&job).unwrap() != CompletionState::NativeComplete {
            assert!(
                std::time::Instant::now() < deadline,
                "native status and Job accounting must converge"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let other = JobObject::new().unwrap();
        assert!(matches!(
            OwnedProcess::open_in_job(child.id(), &other).unwrap(),
            Lookup::Unverified
        ));
    }
}
