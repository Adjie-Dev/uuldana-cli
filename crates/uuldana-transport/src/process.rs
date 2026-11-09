use crate::{Error, environment};
use portable_pty::{CommandBuilder, MasterPty, PtySize};
use std::{
    io::{Read, Write},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};
use uuldana_core::*;

const RECORD_LIMIT: usize = 1024 * 1024;

/// Owned data at the asynchronous transport boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// Raw terminal or unframed standard output.
    Output(Vec<u8>),
    /// Separate stderr; a PTY never emits this variant.
    Stderr(Vec<u8>),
    /// Complete UTF-8 record, without its line ending.
    Message(String),
    /// Transport cleanup or bounded-drain information for the supervisor.
    Diagnostic(String),
    /// Reaped child status. Emitted exactly once and last.
    Exited(ExitInfo),
}
impl Frame {
    /// Borrows data for the frozen decoder interface; diagnostics stay local.
    pub fn as_borrowed(&self) -> Option<TransportFrame<'_>> {
        Some(match self {
            Self::Diagnostic(_) => return None,
            Self::Output(b) => TransportFrame::Output(b),
            Self::Stderr(b) => TransportFrame::Stderr(b),
            Self::Message(s) => TransportFrame::Message(s),
            Self::Exited(e) => TransportFrame::Exited(*e),
        })
    }
}
enum Child {
    Pty(Box<dyn portable_pty::Child + Send + Sync>),
    Pipe(std::process::Child),
}
impl Child {
    fn pid(&self) -> Option<u32> {
        match self {
            Self::Pty(c) => c.process_id(),
            Self::Pipe(c) => Some(c.id()),
        }
    }
    fn wait(&mut self) -> std::io::Result<ExitInfo> {
        match self {
            Self::Pty(c) => c.wait().map(|s| ExitInfo {
                code: if s.signal().is_some() {
                    None
                } else {
                    Some(s.exit_code() as i32)
                },
                success: s.success(),
            }),
            Self::Pipe(c) => c.wait().map(|s| ExitInfo {
                code: s.code(),
                success: s.success(),
            }),
        }
    }
}

// Group IDs are usable only while our unreaped group leader pins its PID.
// The wait worker locks this same state, kills remaining group members BEFORE
// reaping, and retires the group ID before releasing the lock.
struct Tree {
    #[cfg(unix)]
    group: Option<rustix::process::Pid>,
    #[cfg(windows)]
    job: Option<Arc<Mutex<subc_jobobject::JobObject>>>,
    #[cfg(windows)]
    termination_started: Option<std::time::Instant>,
    #[cfg(windows)]
    completion: Option<subc_jobobject::CompletionObserver>,
    #[cfg(windows)]
    completion_error: Option<String>,
    #[cfg(all(windows, test))]
    termination_delay: Duration,
    #[cfg(all(windows, test))]
    membership_retired_early: bool,
    #[cfg(windows)]
    direct: Option<portable_pty::win::ProcessStatus>,
}
impl Tree {
    fn force_terminate(&mut self) -> std::io::Result<bool> {
        // Classify at the native-status check while holding the cleanup lock.
        // A wait notification may still be in flight after native exit. Killing
        // remaining descendants in that case is cleanup, not a forced stop.
        #[cfg(windows)]
        let running = self
            .direct
            .as_ref()
            .map(|status| status.has_exited().map(|exited| !exited))
            .transpose()?
            .unwrap_or(false);
        #[cfg(unix)]
        let running = if let Some(group) = self.group {
            use rustix::process::{WaitId, WaitIdOptions, waitid};
            loop {
                match waitid(
                    WaitId::Pid(group),
                    WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
                ) {
                    Ok(status) => break status.is_none(),
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(error) => return Err(error.into()),
                }
            }
        } else {
            false
        };
        self.terminate().map(|terminated| terminated && running)
    }

    fn terminate(&mut self) -> std::io::Result<bool> {
        #[cfg(unix)]
        {
            if let Some(group) = self.group {
                match rustix::process::kill_process_group(group, rustix::process::Signal::KILL) {
                    Ok(()) => {}
                    Err(rustix::io::Errno::SRCH) => {}
                    // macOS refuses to signal a group whose leader has exited
                    // but is not reaped yet, although it still signals the
                    // other members. Linux accepts it. Nothing is left to do.
                    Err(rustix::io::Errno::PERM) if leader_has_exited(group) => {}
                    Err(e) => return Err(e.into()),
                }
                return Ok(true);
            }
        }
        #[cfg(windows)]
        if let Some(job) = &self.job {
            if self.termination_started.is_some() {
                return Ok(false);
            }
            match subc_jobobject::CompletionObserver::capture(
                &job.lock().unwrap_or_else(|p| p.into_inner()),
            ) {
                Ok(observer) => self.completion = Some(observer),
                Err(error) => {
                    self.completion_error = Some(format!("Job observer capture failed: {error}"))
                }
            }
            #[cfg(test)]
            if !self.termination_delay.is_zero() {
                let delay = self.termination_delay;
                let job = job.clone();
                self.termination_started = Some(std::time::Instant::now());
                thread::spawn(move || {
                    thread::sleep(delay);
                    job.lock().unwrap().terminate().unwrap();
                });
                return Ok(true);
            }
            // Keep ownership for completion observation: termination is asynchronous.
            job.lock().unwrap_or_else(|p| p.into_inner()).terminate()?;
            self.termination_started = Some(std::time::Instant::now());
            return Ok(true);
        }
        Ok(false)
    }
}

/// Whether the group leader has exited, without reaping it.
#[cfg(unix)]
fn leader_has_exited(leader: rustix::process::Pid) -> bool {
    use rustix::process::{WaitId, WaitIdOptions, waitid};
    loop {
        match waitid(
            WaitId::Pid(leader),
            WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
        ) {
            Ok(status) => return status.is_some(),
            Err(rustix::io::Errno::INTR) => continue,
            Err(_) => return false,
        }
    }
}

// Covers partial initialization, a cancelled spawn, and a dropped receiver.
struct ChildGuard {
    child: Child,
    tree: Arc<Mutex<Tree>>,
    reaped: bool,
    #[cfg(windows)]
    output_eof: Option<Arc<Mutex<DrainClock>>>,
}
impl ChildGuard {
    fn wait(&mut self) -> (std::io::Result<ExitInfo>, Vec<String>) {
        self.wait_with_reaped(|| {})
    }
    fn wait_with_reaped(
        &mut self,
        reaped: impl FnOnce(),
    ) -> (std::io::Result<ExitInfo>, Vec<String>) {
        let mut diagnostics = Vec::new();
        #[cfg(unix)]
        {
            use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};
            let pid = Pid::from_raw(self.child.pid().expect("spawned child PID") as i32)
                .expect("positive PID");
            // Observe exit without reaping: the zombie pins the leader PID
            // until group cleanup and retirement have both completed.
            loop {
                match waitid(
                    WaitId::Pid(pid),
                    WaitIdOptions::EXITED | WaitIdOptions::NOWAIT,
                ) {
                    Ok(_) => break,
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(e) => return (Err(e.into()), diagnostics),
                }
            }
            let mut tree = self.tree.lock().unwrap_or_else(|p| p.into_inner());
            if let Err(e) = tree.terminate() {
                diagnostics.push(format!("process group cleanup failed: {e}"));
            }
            tree.group = None;
        }
        let result = self.child.wait();
        self.reaped = result.is_ok();
        #[cfg(not(windows))]
        if self.reaped {
            reaped();
        }
        #[cfg(windows)]
        {
            let mut tree = self.tree.lock().unwrap_or_else(|p| p.into_inner());
            if let Err(e) = tree.terminate() {
                diagnostics.push(format!("job cleanup failed: {e}"));
            }
            let started = tree
                .termination_started
                .unwrap_or_else(std::time::Instant::now);
            #[cfg(test)]
            let membership_retired_early = tree.membership_retired_early;
            let job = tree.job.take();
            let mut completion = tree.completion.take();
            let completion_error = tree.completion_error.take();
            // Never hold the tree lock while polling: cancellation and Drop must
            // remain responsive. The direct child has already been reaped.
            drop(tree);
            if self.reaped {
                reaped();
            }
            if let Some(job) = job {
                let result = wait_for_job_completion(
                    || {
                        let job = job.lock().unwrap_or_else(|p| p.into_inner());
                        if let Some(observer) = &mut completion {
                            #[allow(unused_mut)]
                            let mut evidence = observer.evidence(&job)?;
                            #[cfg(test)]
                            if membership_retired_early {
                                evidence.active_processes = 0;
                            }
                            evidence.state()
                        } else {
                            Err(std::io::Error::other(
                                completion_error
                                    .as_deref()
                                    .unwrap_or("native completion observer unavailable"),
                            ))
                        }
                    },
                    || {
                        self.output_eof.as_ref().is_some_and(|clock| {
                            let clock = clock.lock().unwrap_or_else(|p| p.into_inner());
                            clock.readers > 0 && clock.eof_readers == clock.readers
                        })
                    },
                    started + JOB_TEARDOWN,
                );
                if let Err(e) = result {
                    diagnostics.push(format!("job teardown completion failed: {e}"));
                }
                // KILL_ON_JOB_CLOSE remains the fallback on observation failure.
                drop(job);
            }
        }
        (result, diagnostics)
    }
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if !self.reaped {
            let result = self
                .tree
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .terminate();
            if let Err(e) = result {
                eprintln!("uuldana emergency process tree cleanup failed: {e}");
            }
            let (result, diagnostics) = self.wait();
            for diagnostic in diagnostics {
                eprintln!("uuldana emergency cleanup: {diagnostic}");
            }
            if let Err(e) = result {
                eprintln!("uuldana emergency reap failed: {e}");
            }
        }
    }
}

struct WaitResult {
    completed_at: std::time::Instant,
    exit: std::io::Result<ExitInfo>,
    diagnostics: Vec<String>,
}
const POST_EXIT_DRAIN: Duration = Duration::from_millis(250);
// One-time grace for a reader that is still alive when POST_EXIT_DRAIN is spent.
// It covers output already in the pipe or in the reader's hands but not yet
// forwarded. Granted once and never replenished; JOB_TEARDOWN is unchanged.
const FINAL_DRAIN: Duration = Duration::from_millis(500);
// Reserve the unchanged reader budget inside the existing three-second cleanup
// allowance. HostProbe's whole-probe deadline still bounds normal execution.
#[cfg(windows)]
const JOB_TEARDOWN: Duration = Duration::from_secs(3).saturating_sub(POST_EXIT_DRAIN);

#[cfg(windows)]
fn wait_for_job_completion(
    mut observe: impl FnMut() -> std::io::Result<subc_jobobject::CompletionState>,
    mut output_eof: impl FnMut() -> bool,
    deadline: std::time::Instant,
) -> std::io::Result<()> {
    loop {
        let state = observe()?;
        if state == subc_jobobject::CompletionState::NativeComplete {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "native Job completion unproven at cleanup bound: state={state:?}, output_eof={}",
                    output_eof()
                ),
            ));
        }
        thread::sleep(remaining.min(Duration::from_millis(5)));
    }
}
// A cumulative reader-progress budget. Full-channel sends pause the clock;
// an idle OS read does not. Receiving data never replenishes the budget.
#[derive(Default)]
struct DrainClock {
    updated: Option<std::time::Instant>,
    spent: Duration,
    blocked: usize,
    readers: usize,
    eof_readers: usize,
    extended: bool,
}
impl DrainClock {
    /// Adds the one-time final window; false if it was already granted.
    fn grant_extension(&mut self) -> bool {
        !std::mem::replace(&mut self.extended, true)
    }
    fn budget(&self) -> Duration {
        if self.extended {
            POST_EXIT_DRAIN + FINAL_DRAIN
        } else {
            POST_EXIT_DRAIN
        }
    }
    fn update(&mut self) {
        let now = std::time::Instant::now();
        if let Some(previous) = self.updated.replace(now)
            && self.blocked == 0
        {
            self.spent += now.duration_since(previous);
        }
    }
    fn deadline(&mut self) -> tokio::time::Instant {
        self.update();
        let remaining = self.budget().saturating_sub(self.spent);
        tokio::time::Instant::now()
            + if self.blocked > 0 && !remaining.is_zero() {
                POST_EXIT_DRAIN
            } else {
                remaining
            }
    }
}

fn send_frame(
    tx: &mpsc::Sender<Result<Frame, Error>>,
    clock: &Mutex<DrainClock>,
    frame: Result<Frame, Error>,
) -> bool {
    // Mark the send before it can block, closing the race with expiry. The
    // mutex is never held during a bounded-channel wait.
    {
        let mut clock = clock.lock().unwrap_or_else(|p| p.into_inner());
        if clock.updated.is_some() {
            clock.update();
        }
        clock.blocked += 1;
    }
    let result = tx.blocking_send(frame).is_ok();
    {
        let mut clock = clock.lock().unwrap_or_else(|p| p.into_inner());
        if clock.updated.is_some() {
            clock.update();
        }
        clock.blocked -= 1;
    }
    result
}
struct Input {
    bytes: Vec<u8>,
    reply: oneshot::Sender<Result<(), Error>>,
}

/// Independent input half: write concurrently while the owner drains output.
/// Closing the process input disables all clones; no clone keeps stdin open.
/// Already-started writes may drain before EOF; closure does not cancel them.
#[derive(Clone)]
pub struct InputHandle(Arc<Mutex<Option<mpsc::Sender<Input>>>>);
impl InputHandle {
    /// Writes unchanged bytes and waits for the physical write to finish.
    /// Drive `Process::next_frame` concurrently to avoid child backpressure.
    pub async fn write(&self, bytes: &[u8]) -> Result<(), Error> {
        let tx = self
            .0
            .lock()
            .map_err(|_| Error::WorkerStopped)?
            .clone()
            .ok_or(Error::Unsupported("child input is closed"))?;
        let (reply, rx) = oneshot::channel();
        tx.send(Input {
            bytes: bytes.to_vec(),
            reply,
        })
        .await
        .map_err(|_| Error::WorkerStopped)?;
        rx.await.map_err(|_| Error::WorkerStopped)?
    }
    fn close(&self) {
        if let Ok(mut input) = self.0.lock() {
            input.take();
        }
    }
}

/// Direct child with bounded output buffers and concurrently drained streams.
/// Dropping a live process terminates its tree; the wait worker reaps it.
/// Use the runtime's graceful stop path when lifecycle events are required.
pub struct Process {
    child: Option<ChildGuard>,
    tree: Option<Arc<Mutex<Tree>>>,
    pid: Option<u32>,
    waited: Option<oneshot::Receiver<WaitResult>>,
    native_reaped: Option<oneshot::Receiver<()>>,
    master: Option<Box<dyn MasterPty + Send>>,
    input: Option<InputHandle>,
    frames: mpsc::Receiver<Result<Frame, Error>>,
    exit: Option<ExitInfo>,
    output_closed: bool,
    exit_sent: bool,
    drain_deadline: Option<tokio::time::Instant>,
    drain_clock: Option<Arc<Mutex<DrainClock>>>,
    output_unverified: bool,
    diagnostics: std::collections::VecDeque<String>,
    /// A wait-worker failure waiting behind the diagnostics queued before it.
    pending_failure: Option<Error>,
}
/// Reads why a suspended PTY child may keep its creator's CTRL+C ignore state.
#[cfg(windows)]
fn suspended_ctrl_c_diagnostic(child: &portable_pty::win::SuspendedChild) -> Option<String> {
    child.ctrl_c_release_diagnostic()
}

impl Process {
    /// Validates then spawns on a blocking worker, preserving argv and cwd.
    pub async fn spawn(plan: &LaunchPlan, request: &LaunchRequest) -> Result<Self, Error> {
        plan.validate(request).map_err(Error::InvalidPlan)?;
        let plan = plan.clone();
        tokio::task::spawn_blocking(move || Self::spawn_blocking(&plan))
            .await
            .map_err(|_| Error::WorkerStopped)?
    }
    fn spawn_blocking(plan: &LaunchPlan) -> Result<Self, Error> {
        Self::spawn_blocking_with(
            plan,
            #[cfg(all(windows, test))]
            false,
        )
    }
    /// The launch itself. Under test, `failing_release` makes the child's
    /// CTRL+C release fail through the real release code (the OS refuses it), so
    /// the diagnostic travels the unchanged production path
    /// [`suspended_ctrl_c_diagnostic`] and queue below. Production never sets it.
    fn spawn_blocking_with(
        plan: &LaunchPlan,
        #[cfg(all(windows, test))] failing_release: bool,
    ) -> Result<Self, Error> {
        let (tx, frames) = mpsc::channel(64);
        let vars = environment::overlay(&plan.env);
        let mut process = Self {
            child: None,
            tree: None,
            pid: None,
            waited: None,
            native_reaped: None,
            master: None,
            input: None,
            frames,
            exit: None,
            output_closed: false,
            exit_sent: false,
            drain_deadline: None,
            drain_clock: Some(Arc::new(Mutex::new(DrainClock::default()))),
            output_unverified: false,
            diagnostics: std::collections::VecDeque::new(),
            pending_failure: None,
        };
        let writer: Option<Box<dyn Write + Send>>;
        match plan.transport {
            TransportSpec::NativePty { initial_size } => {
                validate_size(initial_size)?;
                if cfg!(windows)
                    && plan.program.extension().is_some_and(|s| {
                        s.eq_ignore_ascii_case("cmd") || s.eq_ignore_ascii_case("bat")
                    })
                {
                    return Err(Error::Unsupported(
                        "Windows PTY batch shims; launch the native executable instead",
                    ));
                }
                #[cfg(windows)]
                let job = subc_jobobject::JobObject::new()?;
                let pair = portable_pty::native_pty_system()
                    .openpty(pty_size(initial_size))
                    .map_err(backend)?;
                let reader = pair.master.try_clone_reader().map_err(backend)?;
                writer = Some(pair.master.take_writer().map_err(backend)?);
                let mut cmd = CommandBuilder::new(&plan.program);
                cmd.args(&plan.args);
                cmd.cwd(&plan.cwd);
                cmd.env_clear();
                for (key, value) in vars {
                    cmd.env(key, value);
                }
                #[cfg(not(windows))]
                let child = pair.slave.spawn_command(cmd).map_err(backend);
                #[cfg(windows)]
                let mut ctrl_c_message = None;
                #[cfg(all(windows, test))]
                let suspended = if failing_release {
                    pair.slave.spawn_command_suspended_with_failing_release(cmd)
                } else {
                    pair.slave.spawn_command_suspended(cmd)
                };
                #[cfg(all(windows, not(test)))]
                let suspended = pair.slave.spawn_command_suspended(cmd);
                #[cfg(windows)]
                let child = suspended
                    .and_then(|child| {
                        // Read before the resume, which consumes the guard. A
                        // failed release never stops the launch.
                        ctrl_c_message = suspended_ctrl_c_diagnostic(&child);
                        child.resume_in_job(&job)
                    })
                    .map_err(backend);
                // Install master ownership before propagating spawn errors:
                // even failed ConPTY setup must close off the blocking caller,
                // alongside reader/input teardown rather than synchronously.
                drop(pair.slave);
                process.master = Some(pair.master);
                let child = child?;
                process.attach_child(
                    Child::Pty(child),
                    #[cfg(windows)]
                    job,
                )?;
                // Delivered as a Frame::Diagnostic ahead of the exit frame.
                #[cfg(windows)]
                {
                    if let Some(message) = ctrl_c_message {
                        process.diagnostics.push_back(message);
                    }
                }
                start_reader(
                    reader,
                    tx.clone(),
                    StreamFraming::Raw,
                    false,
                    true,
                    process.drain_clock.as_ref().unwrap().clone(),
                )?;
            }
            TransportSpec::CustomCli { .. } | TransportSpec::Acp { .. } => {
                // ACP runs over the child's standard streams as newline-delimited
                // JSON-RPC, so it shares the pipe launch with framed custom CLIs.
                let (stdout, stdin) = match plan.transport {
                    TransportSpec::CustomCli { stdout, stdin } => (stdout, stdin),
                    _ => (StreamFraming::Lines, StdinMode::Piped),
                };
                let mut cmd = Command::new(&plan.program);
                cmd.args(&plan.args)
                    .current_dir(&plan.cwd)
                    .env_clear()
                    .envs(vars)
                    .stdin(if stdin == StdinMode::Piped {
                        Stdio::piped()
                    } else {
                        Stdio::null()
                    })
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                #[cfg(unix)]
                {
                    use std::os::unix::process::CommandExt;
                    cmd.process_group(0);
                }
                #[cfg(windows)]
                let subc_jobobject::ContainedChild { mut child, job } =
                    subc_jobobject::spawn_contained(&mut cmd)?;
                #[cfg(not(windows))]
                let mut child = cmd.spawn()?;
                writer = child
                    .stdin
                    .take()
                    .map(|s| Box::new(s) as Box<dyn Write + Send>);
                let out = child.stdout.take().expect("piped stdout");
                let err = child.stderr.take().expect("piped stderr");
                process.attach_child(
                    Child::Pipe(child),
                    #[cfg(windows)]
                    job,
                )?;
                start_reader(
                    Box::new(out),
                    tx.clone(),
                    stdout,
                    false,
                    false,
                    process.drain_clock.as_ref().unwrap().clone(),
                )?;
                start_reader(
                    Box::new(err),
                    tx.clone(),
                    StreamFraming::Raw,
                    true,
                    false,
                    process.drain_clock.as_ref().unwrap().clone(),
                )?;
            }
            _ => {
                return Err(Error::Unsupported("unknown transport family"));
            }
        }
        if let Some(mut writer) = writer {
            let (input_tx, mut rx) = mpsc::channel::<Input>(8);
            thread::Builder::new()
                .name("uuldana-input".into())
                .spawn(move || {
                    while let Some(input) = rx.blocking_recv() {
                        let result = writer
                            .write_all(&input.bytes)
                            .and_then(|()| writer.flush())
                            .map_err(Error::Io);
                        let failed = result.is_err();
                        let _ = input.reply.send(result);
                        if failed {
                            break;
                        }
                    }
                })?;
            process.input = Some(InputHandle(Arc::new(Mutex::new(Some(input_tx)))));
        }
        process.start_wait_worker()?;
        Ok(process)
    }
    fn start_wait_worker(&mut self) -> Result<(), Error> {
        let drain_clock = self.drain_clock.as_ref().unwrap().clone();
        let mut child = self.child.take().expect("attached child");
        let (tx, rx) = oneshot::channel();
        let (native_tx, native_rx) = oneshot::channel();
        self.native_reaped = Some(native_rx);
        thread::Builder::new()
            .name("uuldana-wait".into())
            .spawn(move || {
                let (exit, diagnostics) = child.wait_with_reaped(|| {
                    let _ = native_tx.send(());
                });
                drain_clock
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .updated = Some(std::time::Instant::now());
                if let Err(result) = tx.send(WaitResult {
                    completed_at: std::time::Instant::now(),
                    exit,
                    diagnostics,
                }) {
                    for diagnostic in result.diagnostics {
                        eprintln!("uuldana detached cleanup: {diagnostic}");
                    }
                    if let Err(e) = result.exit {
                        eprintln!("uuldana detached reap failed: {e}");
                    }
                }
            })?;
        self.waited = Some(rx);
        Ok(())
    }
    fn attach_child(
        &mut self,
        child: Child,
        #[cfg(windows)] job: subc_jobobject::JobObject,
    ) -> Result<(), Error> {
        self.pid = child.pid();
        let tree = Arc::new(Mutex::new(Tree {
            #[cfg(unix)]
            group: self
                .pid
                .and_then(|pid| rustix::process::Pid::from_raw(pid as i32)),
            #[cfg(windows)]
            job: Some(Arc::new(Mutex::new(job))),
            #[cfg(windows)]
            termination_started: None,
            #[cfg(windows)]
            completion: None,
            #[cfg(windows)]
            completion_error: None,
            #[cfg(all(windows, test))]
            termination_delay: Duration::ZERO,
            #[cfg(all(windows, test))]
            membership_retired_early: false,
            #[cfg(windows)]
            direct: None,
        }));
        self.tree = Some(tree.clone());
        self.child = Some(ChildGuard {
            child,
            tree,
            reaped: false,
            #[cfg(windows)]
            output_eof: self.drain_clock.clone(),
        });
        #[cfg(windows)]
        {
            let direct = match &self.child.as_ref().expect("attached child").child {
                Child::Pty(child) => child.process_status().map_err(backend)?,
                Child::Pipe(child) => {
                    portable_pty::win::ProcessStatus::from_handle(child).map_err(backend)?
                }
            };
            self.tree
                .as_ref()
                .expect("attached tree")
                .lock()
                .map_err(|_| Error::WorkerStopped)?
                .direct = Some(direct);
        }
        Ok(())
    }
    /// Direct child PID for diagnostics; never use it for unguarded signaling.
    pub fn process_id(&self) -> Option<u32> {
        self.pid
    }
    /// Most recently received direct-child status, independent of stream EOF.
    pub fn observed_exit(&self) -> Option<ExitInfo> {
        self.exit
    }
    /// False once the post-exit bound passed while an output reader was still
    /// alive: output produced before exit may not have been delivered, so the
    /// absence of a terminal record proves nothing. Meaningful after `Exited`.
    pub fn output_verified(&self) -> bool {
        !self.output_unverified
    }
    /// Receives frames in arrival order, then exactly one final exit.
    /// After reaping, allow 250 ms of reader progress, excluding channel
    /// backpressure. Queued frames survive expiry; progress never resets the budget.
    pub async fn next_frame(&mut self) -> Result<Option<Frame>, Error> {
        if self.pending_failure.is_some() {
            return self.report_wait_failure();
        }
        if self.exit_sent {
            return Ok(None);
        }
        loop {
            if let Some(native) = &mut self.native_reaped {
                match native.try_recv() {
                    Ok(()) => self.close_reaped_io(),
                    Err(oneshot::error::TryRecvError::Closed) => {
                        self.native_reaped.take();
                    }
                    Err(oneshot::error::TryRecvError::Empty) => {}
                }
            }
            // Check exit even under sustained backpressure, before returning
            // buffered output. The deadline is anchored to native wait completion.
            if let Some(rx) = &mut self.waited {
                match rx.try_recv() {
                    Ok(result) => {
                        self.waited.take();
                        if let Err(error) = self.accept_wait(result) {
                            self.retire_after_wait_failure();
                            self.pending_failure = Some(error);
                            return self.report_wait_failure();
                        }
                    }
                    Err(oneshot::error::TryRecvError::Empty) => {}
                    Err(oneshot::error::TryRecvError::Closed) => {
                        self.retire_after_wait_failure();
                        self.pending_failure = Some(Error::WorkerStopped);
                        return self.report_wait_failure();
                    }
                }
            }
            if self.drain_deadline.is_some()
                && let Some(clock) = &self.drain_clock
            {
                self.drain_deadline =
                    Some(clock.lock().unwrap_or_else(|p| p.into_inner()).deadline());
            }
            // Closing admission freezes a finite backlog (at most channel
            // capacity). Unlike marking output_closed, close() retains queued
            // frames and prevents a continuous producer extending cleanup.
            if self
                .drain_deadline
                .is_some_and(|deadline| deadline <= tokio::time::Instant::now())
            {
                // A live reader gets one final window, granted once and never
                // replenished. Open admission keeps its pipe output deliverable.
                let extended = !self.frames.is_closed()
                    && self.drain_clock.as_ref().is_some_and(|clock| {
                        clock
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .grant_extension()
                    });
                if extended && let Some(clock) = &self.drain_clock {
                    self.drain_deadline =
                        Some(clock.lock().unwrap_or_else(|p| p.into_inner()).deadline());
                }
                // A window with no time left must not fall through to the timer,
                // whose granularity would grant a nondeterministic sliver.
                if !extended
                    || self
                        .drain_deadline
                        .is_some_and(|deadline| deadline <= tokio::time::Instant::now())
                {
                    self.stop_output_wait();
                }
            }
            if !self.output_closed {
                match self.frames.try_recv() {
                    Ok(frame) => return frame.map(Some),
                    Err(mpsc::error::TryRecvError::Disconnected) => self.output_closed = true,
                    Err(mpsc::error::TryRecvError::Empty) => {}
                }
            }
            if let Some(message) = self.diagnostics.pop_front() {
                return Ok(Some(Frame::Diagnostic(message)));
            }
            if self.output_closed
                && let Some(exit) = self.exit
            {
                self.exit_sent = true;
                return Ok(Some(Frame::Exited(exit)));
            }
            let deadline = self.drain_deadline;
            tokio::select! {
                biased;
                _ = async { self.native_reaped.as_mut().expect("native reap receiver").await }, if self.native_reaped.is_some() => {
                    self.close_reaped_io();
                },
                result = async { self.waited.as_mut().expect("wait receiver").await }, if self.waited.is_some() => {
                    self.waited.take();
                    if let Err(error) = result.map_err(|_| Error::WorkerStopped).and_then(|result| self.accept_wait(result)) {
                        self.retire_after_wait_failure();
                        self.pending_failure = Some(error);
                        return self.report_wait_failure();
                    }
                },
                () = async {
                    if let Some(deadline) = deadline { tokio::time::sleep_until(deadline).await; }
                    else { std::future::pending::<()>().await; }
                } => {
                    // Recheck the reader clock: backpressure may have paused it.
                },
                frame = self.frames.recv(), if !self.output_closed => match frame {
                    Some(result) => return result.map(Some),
                    None => self.output_closed = true,
                },
            }
        }
    }
    fn stop_output_wait(&mut self) {
        self.drain_deadline = None;
        if !self.frames.is_closed() {
            self.frames.close();
            self.output_unverified = true;
            self.diagnostics.push_back("post-exit output wait reached its 250 ms bound; buffered frames retained, further output discarded; delivery of earlier output is unverified".into());
        }
    }
    /// Diagnostics queued before the worker failed come first, then the failure
    /// exactly once, then the end of the stream. No exit is ever invented.
    fn report_wait_failure(&mut self) -> Result<Option<Frame>, Error> {
        if let Some(message) = self.diagnostics.pop_front() {
            return Ok(Some(Frame::Diagnostic(message)));
        }
        self.exit_sent = true;
        self.pending_failure.take().map_or(Ok(None), Err)
    }
    fn wait_failed(&mut self, error: Error) -> Error {
        self.retire_after_wait_failure();
        self.exit_sent = true;
        error
    }
    fn retire_after_wait_failure(&mut self) {
        // No authoritative exit can arrive after the wait worker fails. Retire
        // the receive side without fabricating status or waiting for stream EOF.
        self.waited.take();
        self.native_reaped.take();
        self.frames.close();
        self.close_input();
        if let Some(tree) = &self.tree
            && let Err(cleanup) = tree.lock().unwrap_or_else(|p| p.into_inner()).terminate()
        {
            eprintln!("uuldana wait failure tree cleanup: {cleanup}");
        }
        if let Some(master) = self.master.take() {
            thread::spawn(move || drop(master));
        }
    }
    fn close_reaped_io(&mut self) {
        self.native_reaped.take();
        self.close_input();
        if let Some(master) = self.master.take() {
            thread::spawn(move || drop(master));
        }
    }
    fn accept_wait(&mut self, result: WaitResult) -> Result<(), Error> {
        self.diagnostics.extend(result.diagnostics);
        self.exit = Some(result.exit?);
        self.drain_deadline = Some(if let Some(clock) = &self.drain_clock {
            clock.lock().unwrap_or_else(|p| p.into_inner()).deadline()
        } else {
            tokio::time::Instant::from_std(result.completed_at + POST_EXIT_DRAIN)
        });
        self.close_input();
        if let Some(master) = self.master.take() {
            // ConPTY close must run concurrently with output draining, including
            // its initial cursor handshake; never block an async executor here.
            thread::spawn(move || drop(master));
        }
        Ok(())
    }
    /// Independent writer for simultaneous input/output forwarding.
    pub fn input_handle(&self) -> Option<InputHandle> {
        self.input.clone()
    }
    /// Writes bytes unchanged on a dedicated input worker. For large writes,
    /// use `input_handle` and drain frames concurrently.
    pub async fn write_input(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let tx = self
            .input
            .as_ref()
            .ok_or(Error::Unsupported("child input is closed"))?;
        tx.write(bytes).await
    }
    /// Disables new input; already-started writes may finish before EOF.
    /// PTY EOF semantics remain platform-specific.
    pub fn close_input(&mut self) {
        if let Some(input) = self.input.take() {
            input.close();
        }
    }
    /// Propagates terminal dimensions, never to a pipe.
    pub fn resize(&mut self, size: TerminalSize) -> Result<(), Error> {
        validate_size(size)?;
        self.master
            .as_ref()
            .ok_or(Error::Unsupported("resize requires a live PTY"))?
            .resize(pty_size(size))
            .map_err(backend)
    }
    /// Executes one graceful step; pipe interrupts are unsupported.
    pub async fn shutdown_step(&mut self, step: &ShutdownStep) -> Result<(), Error> {
        match step {
            ShutdownStep::WriteInput(b) => self.write_input(b).await,
            ShutdownStep::CloseInput => {
                self.close_input();
                Ok(())
            }
            ShutdownStep::Interrupt if self.master.is_some() => self.write_input(&[3]).await,
            _ => Err(Error::Unsupported(
                "interrupt requires a PTY; pipe signal support is pending",
            )),
        }
    }
    /// Terminates the owned job or unreaped process group, including descendants.
    /// Keep receiving frames to obtain direct-child status and cleanup diagnostics.
    pub fn force_terminate(&mut self) -> Result<bool, Error> {
        if let Some(rx) = &mut self.waited {
            match rx.try_recv() {
                Ok(result) => {
                    self.waited.take();
                    if let Err(error) = self.accept_wait(result) {
                        return Err(self.wait_failed(error));
                    }
                }
                Err(oneshot::error::TryRecvError::Empty) => {}
                Err(oneshot::error::TryRecvError::Closed) => {
                    return Err(self.wait_failed(Error::WorkerStopped));
                }
            }
        }
        if self.exit.is_some() {
            return Ok(false);
        }
        self.tree.as_ref().map_or(Ok(false), |tree| {
            tree.lock()
                .map_err(|_| Error::WorkerStopped)?
                .force_terminate()
                .map_err(Error::Io)
        })
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        self.frames.close();
        self.close_input();
        if let Some(tree) = self.tree.take()
            && let Err(e) = tree.lock().unwrap_or_else(|p| p.into_inner()).terminate()
        {
            eprintln!("uuldana dropped process tree cleanup failed: {e}");
        }
        let child = self.child.take();
        let master = self.master.take();
        if child.is_some() || master.is_some() {
            thread::spawn(move || {
                drop(child);
                drop(master);
            });
        }
    }
}
fn validate_size(size: TerminalSize) -> Result<(), Error> {
    if !size.is_valid()
        || (cfg!(windows) && (size.rows > i16::MAX as u16 || size.cols > i16::MAX as u16))
    {
        Err(Error::InvalidSize(size))
    } else {
        Ok(())
    }
}
fn pty_size(size: TerminalSize) -> PtySize {
    PtySize {
        rows: size.rows,
        cols: size.cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}
fn backend(e: impl std::fmt::Display) -> Error {
    Error::Backend(e.to_string())
}
fn start_reader(
    mut reader: Box<dyn Read + Send>,
    tx: mpsc::Sender<Result<Frame, Error>>,
    framing: StreamFraming,
    stderr: bool,
    pty: bool,
    clock: Arc<Mutex<DrainClock>>,
) -> Result<(), Error> {
    clock.lock().unwrap_or_else(|p| p.into_inner()).readers += 1;
    thread::Builder::new()
        .name("uuldana-output".into())
        .spawn(move || {
            let mut chunk = [0; 8192];
            let mut record = Vec::new();
            let mut discard = false;
            loop {
                let count = match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::BrokenPipe
                            || (cfg!(unix) && pty && e.raw_os_error() == Some(5)) =>
                    {
                        break;
                    }
                    Err(e) => {
                        let _ = send_frame(&tx, &clock, Err(Error::Io(e)));
                        return;
                    }
                };
                if framing == StreamFraming::Raw {
                    let frame = if stderr {
                        Frame::Stderr(chunk[..count].to_vec())
                    } else {
                        Frame::Output(chunk[..count].to_vec())
                    };
                    if !send_frame(&tx, &clock, Ok(frame)) {
                        return;
                    }
                } else {
                    for &byte in &chunk[..count] {
                        if discard {
                            if byte == b'\n' {
                                discard = false;
                            }
                            continue;
                        }
                        if byte == b'\n' {
                            if record.last() == Some(&b'\r') {
                                record.pop();
                            }
                            let result = String::from_utf8(std::mem::take(&mut record))
                                .map(Frame::Message)
                                .map_err(|_| Error::InvalidEncoding);
                            if !send_frame(&tx, &clock, result) {
                                return;
                            }
                        } else if record.len() == RECORD_LIMIT {
                            if !send_frame(&tx, &clock, Err(Error::RecordTooLarge)) {
                                return;
                            }
                            record.clear();
                            discard = true;
                        } else {
                            record.push(byte);
                        }
                    }
                }
            }
            if !discard && !record.is_empty() {
                let result = String::from_utf8(record)
                    .map(Frame::Message)
                    .map_err(|_| Error::InvalidEncoding);
                if !send_frame(&tx, &clock, result) {
                    return;
                }
            }
            clock.lock().unwrap_or_else(|p| p.into_inner()).eof_readers += 1;
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[tokio::test]
    async fn delayed_job_teardown_does_not_spend_reader_budget() {
        delayed_job_teardown(false).await;
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn zero_membership_during_delayed_job_teardown_does_not_spend_reader_budget() {
        delayed_job_teardown(true).await;
    }

    #[cfg(windows)]
    async fn delayed_job_teardown(retired_early: bool) {
        let fixture = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("examples/uuldana-test-child.exe");
        let heartbeat = std::env::temp_dir().join(format!(
            "uuldana-delayed-job-{}-{retired_early}",
            std::process::id()
        ));
        let mut command = Command::new(fixture);
        command
            .arg("descendant-exit")
            .arg(&heartbeat)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let subc_jobobject::ContainedChild { mut child, job } =
            subc_jobobject::spawn_contained(&mut command).unwrap();
        let stdout = child.stdout.take().unwrap();
        let (output, mut process) = controlled_process();
        let clock = Arc::new(Mutex::new(DrainClock::default()));
        process.drain_clock = Some(clock.clone());
        process.attach_child(Child::Pipe(child), job).unwrap();
        process
            .tree
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .termination_delay = Duration::from_millis(600);
        start_reader(
            Box::new(stdout),
            output,
            StreamFraming::Lines,
            false,
            false,
            clock.clone(),
        )
        .unwrap();
        process
            .tree
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .membership_retired_early = retired_early;
        process.start_wait_worker().unwrap();
        let frames = tokio::time::timeout(Duration::from_secs(3), async {
            let mut frames = Vec::new();
            while let Some(frame) = process.next_frame().await.unwrap() {
                frames.push(frame);
            }
            frames
        })
        .await
        .expect("delayed cleanup must remain bounded");
        assert!(
            frames
                .iter()
                .any(|f| matches!(f, Frame::Message(s) if s.starts_with("DESCENDANT=")))
        );
        assert!(
            !frames.iter().any(|f| matches!(f, Frame::Diagnostic(_))),
            "false I/O diagnostic: {frames:?}"
        );
        assert_eq!(frames.last(), Some(&Frame::Exited(ExitInfo::from_code(0))));
        assert_eq!(
            frames
                .iter()
                .filter(|f| matches!(f, Frame::Exited(_)))
                .count(),
            1
        );
        assert_eq!(process.next_frame().await.unwrap(), None);
        let descendant = frames
            .iter()
            .find_map(|frame| match frame {
                Frame::Message(line) => line
                    .strip_prefix("DESCENDANT=")
                    .and_then(|pid| pid.parse().ok()),
                _ => None,
            })
            .unwrap();
        assert!(subc_jobobject::wait_for_process_exit(
            descendant,
            Duration::from_secs(1)
        ));
        let _ = std::fs::remove_file(heartbeat);
    }

    #[cfg(windows)]
    #[test]
    fn job_completion_observation_is_bounded_and_preserves_query_errors() {
        let started = std::time::Instant::now();
        let error = wait_for_job_completion(
            || Ok(subc_jobobject::CompletionState::Pending),
            || true,
            started + Duration::from_millis(30),
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
        let error = wait_for_job_completion(
            || Err(std::io::Error::other("injected Job query failure")),
            || true,
            started + Duration::from_secs(1),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "injected Job query failure");
        assert!(
            wait_for_job_completion(
                || Ok(subc_jobobject::CompletionState::NativeComplete),
                || false,
                started
            )
            .is_ok()
        );
    }

    #[cfg(windows)]
    #[test]
    fn eof_never_overrides_missing_native_coverage_or_pending_handles() {
        use subc_jobobject::CompletionState::*;
        let expired = std::time::Instant::now();
        assert!(wait_for_job_completion(|| Ok(CoverageIncomplete), || true, expired).is_err());
        assert!(wait_for_job_completion(|| Ok(CoverageIncomplete), || false, expired).is_err());
        assert!(wait_for_job_completion(|| Ok(Pending), || true, expired).is_err());
    }

    fn controlled_process() -> (mpsc::Sender<Result<Frame, Error>>, Process) {
        let (tx, frames) = mpsc::channel(64);
        (
            tx,
            Process {
                child: None,
                tree: None,
                pid: None,
                waited: None,
                native_reaped: None,
                master: None,
                input: None,
                frames,
                exit: None,
                output_closed: false,
                exit_sent: false,
                drain_deadline: None,
                drain_clock: None,
                output_unverified: false,
                diagnostics: std::collections::VecDeque::new(),
                pending_failure: None,
            },
        )
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn force_after_native_exit_before_wait_notification_is_not_a_forced_stop() {
        let mut command = Command::new(
            std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
                .join("System32")
                .join("cmd.exe"),
        );
        command.args(["/d", "/c", "exit 0"]);
        let subc_jobobject::ContainedChild { mut child, job } =
            subc_jobobject::spawn_contained(&mut command).unwrap();
        assert!(child.wait().unwrap().success());
        let (output, mut process) = controlled_process();
        process.attach_child(Child::Pipe(child), job).unwrap();
        let (notify, rx) = oneshot::channel();
        process.waited = Some(rx);
        // Native exit has happened, but the worker has not published it. The
        // still-owned Job requires cleanup, not force-cancellation attribution.
        assert!(!process.force_terminate().unwrap());
        notify
            .send(WaitResult {
                completed_at: std::time::Instant::now(),
                exit: Ok(ExitInfo::from_code(0)),
                diagnostics: vec![],
            })
            .ok()
            .unwrap();
        drop(output);
        assert_eq!(
            process.next_frame().await.unwrap(),
            Some(Frame::Exited(ExitInfo::from_code(0)))
        );
        assert_eq!(process.next_frame().await.unwrap(), None);
    }

    #[tokio::test]
    async fn expired_drain_preserves_buffered_done_limit_and_order_before_one_exit() {
        for records in [
            vec!["DONE"],
            vec!["LIMIT"],
            vec!["first", "DONE", "LIMIT", "tail"],
        ] {
            let (tx, mut process) = controlled_process();
            for record in &records {
                tx.try_send(Ok(Frame::Message((*record).into()))).unwrap();
            }
            process
                .accept_wait(WaitResult {
                    completed_at: std::time::Instant::now() - Duration::from_secs(1),
                    exit: Ok(ExitInfo::from_code(0)),
                    diagnostics: vec![],
                })
                .unwrap();
            let mut messages = Vec::new();
            let mut exits = Vec::new();
            tokio::time::timeout(Duration::from_secs(1), async {
                while let Some(frame) = process.next_frame().await.unwrap() {
                    match frame {
                        Frame::Message(message) => {
                            assert!(exits.is_empty(), "exit must follow all output");
                            messages.push(message);
                        }
                        Frame::Exited(exit) => exits.push(exit),
                        _ => {}
                    }
                }
            })
            .await
            .unwrap();
            assert_eq!(messages, records);
            assert_eq!(exits, [ExitInfo::from_code(0)]);
            assert!(tx.try_send(Ok(Frame::Message("late".into()))).is_err());
        }
    }

    #[tokio::test]
    async fn failed_wait_worker_is_terminal_instead_of_pending_on_open_output() {
        for disconnected in [false, true] {
            let (_output_owner, mut process) = controlled_process();
            let (tx, rx) = oneshot::channel();
            process.waited = Some(rx);
            if !disconnected {
                tx.send(WaitResult {
                    completed_at: std::time::Instant::now(),
                    exit: Err(std::io::Error::other("injected native wait failure")),
                    diagnostics: vec![],
                })
                .ok()
                .unwrap();
            } else {
                drop(tx);
            }
            assert!(process.next_frame().await.is_err());
            assert!(process.observed_exit().is_none(), "never invent status");
            assert_eq!(
                tokio::time::timeout(Duration::from_millis(100), process.next_frame())
                    .await
                    .expect("failed wait must retire transport")
                    .unwrap(),
                None
            );
        }
    }

    /// Whichever way the wait worker fails (it reports an error, or it vanishes),
    /// diagnostics queued before the failure still reach the consumer first. The
    /// failure is reported once, no exit is invented, and nothing follows it.
    #[tokio::test]
    async fn a_wait_worker_failure_is_reported_once_after_the_queued_diagnostics() {
        for disconnected in [false, true] {
            let (_output_owner, mut process) = controlled_process();
            process.diagnostics.push_back("release diagnostic".into());
            let (tx, rx) = oneshot::channel();
            process.waited = Some(rx);
            let reported = if disconnected {
                drop(tx);
                vec![]
            } else {
                tx.send(WaitResult {
                    completed_at: std::time::Instant::now(),
                    exit: Err(std::io::Error::other("injected native wait failure")),
                    diagnostics: vec!["worker cleanup note".into()],
                })
                .ok()
                .unwrap();
                vec![Frame::Diagnostic("worker cleanup note".into())]
            };
            let mut frames = Vec::new();
            let error = loop {
                match process.next_frame().await {
                    Ok(Some(frame)) => frames.push(frame),
                    Ok(None) => panic!("the failure was never reported: {frames:?}"),
                    Err(error) => break error,
                }
            };
            assert!(
                matches!(error, Error::Io(_) | Error::WorkerStopped),
                "{error:?}"
            );
            let mut expected = vec![Frame::Diagnostic("release diagnostic".into())];
            expected.extend(reported);
            assert_eq!(frames, expected);
            assert!(process.observed_exit().is_none(), "never invent status");
            assert!(process.diagnostics.is_empty());
            assert_eq!(
                tokio::time::timeout(Duration::from_millis(100), process.next_frame())
                    .await
                    .expect("a failed wait retires the transport")
                    .unwrap(),
                None,
                "the failure is reported once and nothing follows it"
            );
        }
    }

    /// The same guarantee when the failure arrives while a read is already
    /// waiting, so the wait receiver is the one that wakes it.
    #[tokio::test]
    async fn a_wait_failure_during_a_pending_read_still_follows_the_queued_diagnostics() {
        for disconnected in [false, true] {
            let (_output_owner, mut process) = controlled_process();
            process.diagnostics.push_back("release diagnostic".into());
            let (tx, rx) = oneshot::channel();
            process.waited = Some(rx);
            assert_eq!(
                process.next_frame().await.unwrap(),
                Some(Frame::Diagnostic("release diagnostic".into()))
            );
            let late = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                if !disconnected {
                    tx.send(WaitResult {
                        completed_at: std::time::Instant::now(),
                        exit: Err(std::io::Error::other("injected native wait failure")),
                        diagnostics: vec!["worker cleanup note".into()],
                    })
                    .ok()
                    .unwrap();
                }
            });
            let first = tokio::time::timeout(Duration::from_secs(2), process.next_frame())
                .await
                .expect("the failure must wake the pending read")
                .map(|frame| frame.map(|f| matches!(f, Frame::Diagnostic(_))));
            late.await.unwrap();
            if disconnected {
                assert!(matches!(first, Err(Error::WorkerStopped)), "{first:?}");
            } else {
                assert!(matches!(first, Ok(Some(true))), "{first:?}");
                assert!(matches!(process.next_frame().await, Err(Error::Io(_))));
            }
            assert!(process.observed_exit().is_none());
            assert_eq!(process.next_frame().await.unwrap(), None);
        }
    }

    /// Every diagnostic queued before the worker failed is delivered, in order,
    /// ahead of the terminal error, and no exit frame is invented.
    #[tokio::test]
    async fn queued_diagnostics_reach_the_consumer_before_a_wait_worker_failure() {
        let (_output_owner, mut process) = controlled_process();
        process.diagnostics.push_back("release diagnostic".into());
        let (tx, rx) = oneshot::channel();
        process.waited = Some(rx);
        tx.send(WaitResult {
            completed_at: std::time::Instant::now(),
            exit: Err(std::io::Error::other("injected native wait failure")),
            diagnostics: vec!["worker cleanup note".into()],
        })
        .ok()
        .unwrap();
        let mut frames = Vec::new();
        let error = loop {
            match process.next_frame().await {
                Ok(Some(frame)) => frames.push(frame),
                Ok(None) => panic!("the failure was never reported: {frames:?}"),
                Err(error) => break error,
            }
        };
        assert!(matches!(error, Error::Io(_) | Error::WorkerStopped));
        assert_eq!(
            frames,
            [
                Frame::Diagnostic("release diagnostic".into()),
                Frame::Diagnostic("worker cleanup note".into())
            ]
        );
        assert!(!frames.iter().any(|f| matches!(f, Frame::Exited(_))));
    }

    #[tokio::test]
    async fn expired_drain_freezes_full_backlog_and_rejects_blocked_new_output() {
        let (tx, mut process) = controlled_process();
        for index in 0..64 {
            tx.try_send(Ok(Frame::Message(index.to_string()))).unwrap();
        }
        let late = tokio::spawn(async move { tx.send(Ok(Frame::Message("late".into()))).await });
        process
            .accept_wait(WaitResult {
                completed_at: std::time::Instant::now() - Duration::from_secs(1),
                exit: Ok(ExitInfo::from_code(0)),
                diagnostics: vec![],
            })
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            for index in 0..64 {
                assert_eq!(
                    process.next_frame().await.unwrap(),
                    Some(Frame::Message(index.to_string()))
                );
            }
            assert!(matches!(
                process.next_frame().await.unwrap(),
                Some(Frame::Diagnostic(_))
            ));
            assert_eq!(
                process.next_frame().await.unwrap(),
                Some(Frame::Exited(ExitInfo::from_code(0)))
            );
            assert_eq!(process.next_frame().await.unwrap(), None);
            assert!(late.await.unwrap().is_err());
        })
        .await
        .expect("full backlog must drain without admitting a new producer");
    }

    #[cfg(windows)]
    #[test]
    fn conpty_size_uses_signed_coord_bounds_on_both_dimensions() {
        assert!(validate_size(TerminalSize::new(32767, 32767)).is_ok());
        for size in [TerminalSize::new(32768, 24), TerminalSize::new(24, 32768)] {
            assert!(matches!(validate_size(size), Err(Error::InvalidSize(value)) if value == size));
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_pty_size_accepts_the_full_unsigned_dimension_range() {
        assert!(validate_size(TerminalSize::new(u16::MAX, u16::MAX)).is_ok());
    }

    #[tokio::test]
    async fn reaped_child_with_open_stream_emits_bounded_drain_diagnostic_before_exit() {
        let (_stream_owner, frames) = mpsc::channel(1);
        let mut process = Process {
            child: None,
            tree: None,
            pid: None,
            waited: None,
            native_reaped: None,
            master: None,
            input: None,
            frames,
            exit: Some(ExitInfo::from_code(0)),
            output_closed: false,
            exit_sent: false,
            drain_deadline: Some(tokio::time::Instant::now() + POST_EXIT_DRAIN),
            drain_clock: None,
            output_unverified: false,
            diagnostics: std::collections::VecDeque::new(),
            pending_failure: None,
        };
        let diagnostic = tokio::time::timeout(Duration::from_secs(1), process.next_frame())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(diagnostic, Some(Frame::Diagnostic(s)) if s.contains("250 ms") && s.contains("discarded"))
        );
        assert_eq!(
            process.next_frame().await.unwrap(),
            Some(Frame::Exited(ExitInfo::from_code(0)))
        );
        assert_eq!(process.next_frame().await.unwrap(), None);
    }

    #[cfg(windows)]
    fn cmd_plan(args: &[&str]) -> LaunchPlan {
        LaunchPlan::new(
            std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
                .join("System32")
                .join("cmd.exe"),
            std::env::current_dir().unwrap(),
            uuldana_core::TransportSpec::NativePty {
                initial_size: uuldana_core::TerminalSize::DEFAULT,
            },
        )
        .args(args)
    }

    /// Answers ConPTY's startup cursor request while collecting every frame
    /// that is not raw output, up to the end of the stream.
    #[cfg(windows)]
    async fn non_output_frames(process: &mut Process) -> Vec<Frame> {
        let mut frames = Vec::new();
        let mut seen = Vec::<u8>::new();
        let mut answered = false;
        tokio::time::timeout(Duration::from_secs(15), async {
            while let Some(frame) = process.next_frame().await.unwrap() {
                match frame {
                    Frame::Output(bytes) => {
                        seen.extend(bytes);
                        if !answered && seen.windows(4).any(|b| b == b"[6n") {
                            process.write_input(b"[1;1R").await.unwrap();
                            answered = true;
                        }
                    }
                    other => frames.push(other),
                }
            }
        })
        .await
        .expect("the PTY stream must end");
        frames
    }

    /// The release diagnostic must reach the frame consumer, not only a log
    /// macro. Nothing between the failure and the frame is substituted: the
    /// child's release really fails (the OS refuses the memory access), the real
    /// `suspended_ctrl_c_diagnostic` reads it, and the real queue delivers it as
    /// one Frame::Diagnostic before the single exit frame, with the child's own
    /// status passed through unchanged. Unrelated diagnostics (the bounded output
    /// wait can fire on a loaded host) are tolerated, so the test names only
    /// what it owns.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_failed_ctrl_c_release_reaches_the_frame_consumer_before_the_single_exit() {
        let plan = cmd_plan(&["/d", "/c", "exit 3"]);
        let mut process = Process::spawn_blocking_with(&plan, true).unwrap();
        let frames = non_output_frames(&mut process).await;
        let release_failure = |f: &Frame| matches!(f, Frame::Diagnostic(m) if m.contains("inherited CTRL+C ignore attribute"));
        assert_eq!(
            frames.iter().filter(|f| release_failure(f)).count(),
            1,
            "{frames:?}"
        );
        let Some(Frame::Diagnostic(text)) = frames.iter().find(|f| release_failure(f)) else {
            unreachable!()
        };
        assert!(text.contains("native block: "), "{text}");
        assert!(text.contains("os error 5"), "{text}");
        assert!(text.contains("no block was released"), "{text}");
        assert!(
            text.contains("interactive CTRL+C may not reach the child"),
            "{text}"
        );
        assert!(release_failure(&frames[0]), "{frames:?}");
        assert_eq!(
            frames
                .iter()
                .filter(|f| matches!(f, Frame::Exited(_)))
                .count(),
            1,
            "{frames:?}"
        );
        assert_eq!(
            frames.last(),
            Some(&Frame::Exited(ExitInfo::from_code(3))),
            "{frames:?}"
        );
        assert_eq!(process.next_frame().await.unwrap(), None);
        assert_eq!(process.observed_exit(), Some(ExitInfo::from_code(3)));
    }

    /// The failure is fail-open and the child cleanup stays bounded: a child
    /// that kept its creator's attribute is still stopped by force, with one
    /// honest exit, the release diagnostic once and ahead of it, and no process
    /// left behind.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_child_whose_ctrl_c_release_failed_is_still_stopped_in_bound() {
        let ping = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32")
            .join("ping.exe");
        let plan = LaunchPlan::new(
            ping,
            std::env::current_dir().unwrap(),
            uuldana_core::TransportSpec::NativePty {
                initial_size: uuldana_core::TerminalSize::DEFAULT,
            },
        )
        .args(["-n", "30", "127.0.0.1"]);
        let mut process = Process::spawn_blocking_with(&plan, true).unwrap();
        let pid = process.process_id().unwrap();
        let release_failure = |f: &Frame| matches!(f, Frame::Diagnostic(m) if m.contains("inherited CTRL+C ignore attribute"));
        let mut seen = Vec::<u8>::new();
        let mut answered = false;
        let mut delivered = 0;
        tokio::time::timeout(Duration::from_secs(10), async {
            while !String::from_utf8_lossy(&seen).contains("Pinging") {
                match process.next_frame().await.unwrap() {
                    Some(Frame::Output(bytes)) => seen.extend(bytes),
                    Some(other) if release_failure(&other) => delivered += 1,
                    Some(Frame::Diagnostic(_)) => {}
                    other => panic!("unexpected frame before the first reply: {other:?}"),
                }
                if !answered && seen.windows(4).any(|b| b == b"[6n") {
                    process.write_input(b"[1;1R").await.unwrap();
                    answered = true;
                }
            }
        })
        .await
        .expect("the child must start");
        let started = std::time::Instant::now();
        assert!(process.force_terminate().unwrap());
        let frames = non_output_frames(&mut process).await;
        assert!(started.elapsed() < Duration::from_secs(10), "{frames:?}");
        // Delivered once in all, whether before or after the force stop.
        assert_eq!(
            delivered + frames.iter().filter(|f| release_failure(f)).count(),
            1,
            "{frames:?}"
        );
        assert_eq!(
            frames
                .iter()
                .filter(|f| matches!(f, Frame::Exited(_)))
                .count(),
            1,
            "{frames:?}"
        );
        assert!(matches!(frames.last(), Some(Frame::Exited(exit)) if !exit.success));
        assert!(subc_jobobject::wait_for_process_exit(
            pid,
            Duration::from_secs(5)
        ));
    }

    /// The control for the test above, through the real vendor release: a
    /// successful release emits no CTRL+C diagnostic.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_successful_ctrl_c_release_emits_no_diagnostic() {
        let plan = cmd_plan(&["/d", "/c", "exit 3"]);
        let mut process = Process::spawn_blocking(&plan).unwrap();
        let frames = non_output_frames(&mut process).await;
        assert!(
            !frames
                .iter()
                .any(|f| matches!(f, Frame::Diagnostic(m) if m.contains("CTRL+C"))),
            "{frames:?}"
        );
        assert_eq!(
            frames.last(),
            Some(&Frame::Exited(ExitInfo::from_code(3))),
            "{frames:?}"
        );
    }

    /// A child that survives CTRL+C (an interactive shell cancels its line
    /// instead of exiting) must not hold the stop open: the Interrupt step does
    /// not end it, force termination does, within a bound, with one honest exit.
    #[cfg(windows)]
    #[tokio::test]
    async fn stopping_a_child_that_survives_ctrl_c_is_bounded_and_honest() {
        let plan = cmd_plan(&["/d"]);
        let mut process = Process::spawn_blocking(&plan).unwrap();
        let mut seen = Vec::<u8>::new();
        let prompt = tokio::time::timeout(Duration::from_secs(10), async {
            let mut answered = false;
            loop {
                match process.next_frame().await.unwrap() {
                    Some(Frame::Output(bytes)) => seen.extend(bytes),
                    other => panic!("unexpected frame before the banner: {other:?}"),
                }
                if !answered && seen.windows(4).any(|b| b == b"[6n") {
                    process.write_input(b"[1;1R").await.unwrap();
                    answered = true;
                }
                // The banner comes before the prompt, whose text is user-configurable.
                if answered && String::from_utf8_lossy(&seen).contains("Microsoft Windows") {
                    return;
                }
            }
        })
        .await;
        assert!(
            prompt.is_ok(),
            "no banner from the shell: {:?}",
            String::from_utf8_lossy(&seen)
        );
        // Let the shell settle at its prompt: no output for a moment.
        while let Ok(frame) =
            tokio::time::timeout(Duration::from_millis(400), process.next_frame()).await
        {
            assert!(
                matches!(frame.unwrap(), Some(Frame::Output(_))),
                "shell ended early"
            );
        }
        process
            .shutdown_step(&ShutdownStep::Interrupt)
            .await
            .unwrap();
        // The shell took CTRL+C and is still running: nothing ended it.
        let ended = tokio::time::timeout(Duration::from_millis(1500), async {
            loop {
                match process.next_frame().await.unwrap() {
                    Some(Frame::Output(_)) => {}
                    other => return other,
                }
            }
        })
        .await;
        assert!(ended.is_err(), "CTRL+C must not end the shell: {ended:?}");
        // Stopping it is bounded and reports one honest exit.
        let started = std::time::Instant::now();
        assert!(process.force_terminate().unwrap());
        let frames = non_output_frames(&mut process).await;
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(matches!(frames.last(), Some(Frame::Exited(exit)) if !exit.success));
        assert_eq!(
            frames
                .iter()
                .filter(|f| matches!(f, Frame::Exited(_)))
                .count(),
            1
        );
        assert!(!process.force_terminate().unwrap());
    }

    include!("process_drain_regression.rs");
}
