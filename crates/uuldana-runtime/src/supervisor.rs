use crate::{Error, journal::Journal};
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
    time::{Duration, SystemTime},
};
use uuldana_core::*;
use uuldana_transport::{Frame, Process};

/// Parameters for one new supervised run.
pub struct StartOptions {
    /// Registered adapter id.
    pub provider: ProviderId,
    /// Unique run id in this session.
    pub run: RunId,
    /// Requested policy; defaults to most restrictive.
    pub permissions: PermissionPolicy,
    /// Explicit transport, or the first implemented transport in the descriptor.
    pub transport: Option<TransportKind>,
    /// Optional native resume request.
    pub native_resume: Option<NativeSessionId>,
    /// Continue the most recent conversation of the workspace, for providers
    /// that can do that without knowing a session id.
    pub continue_latest: bool,
    /// Initial PTY dimensions.
    pub terminal_size: Option<TerminalSize>,
    /// Optional first prompt, interpreted only by the adapter.
    pub initial_prompt: Option<String>,
    /// Extra environment for the child, for example to select an account.
    /// Applied after the adapter's own plan; never read or logged.
    pub env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    /// Name of the configured provider entry, recorded in the journal.
    pub label: Option<String>,
}
impl StartOptions {
    /// Creates a restrictive request with no prompt or resume id.
    pub fn new(provider: ProviderId, run: RunId) -> Self {
        Self {
            provider,
            run,
            permissions: PermissionPolicy::default(),
            transport: None,
            native_resume: None,
            continue_latest: false,
            terminal_size: None,
            initial_prompt: None,
            env: Vec::new(),
            label: None,
        }
    }
}
/// Namespace and name of the event that holds the text of an agent's screen.
pub const SCREEN_NAMESPACE: &str = "uuldana";
/// See [`SCREEN_NAMESPACE`].
pub const SCREEN_EVENT: &str = "screen";

/// Time between two presses of Ctrl+C when a program needs two to leave.
const INTERRUPT_GAP: Duration = Duration::from_millis(500);

/// Provider-neutral supervisor owning one session and its in-memory events.
pub struct Supervisor {
    registry: ProviderRegistry,
    probe: Arc<dyn HostProbe>,
    session: Session,
    events: Vec<EventEnvelope>,
    active: Option<Active>,
    last_stop_forced: bool,
    journal: Option<Journal>,
    journal_error: Option<String>,
}
struct Active {
    run: RunId,
    provider: ProviderId,
    spawning: Option<tokio::task::JoinHandle<Result<Process, uuldana_transport::Error>>>,
    process: Option<Process>,
    decoder: Option<Box<dyn EventDecoder>>,
    shutdown: ShutdownPlan,
    capabilities: CapabilitySet,
    outcome: RunOutcome,
    finish: Option<RunFinish>,
    /// Native session known before launch, journaled once the process runs.
    announce: Option<(NativeSessionId, NativeSessionLink)>,
}
impl Supervisor {
    /// Creates a supervisor with caller-provided registry, probe, and session.
    pub fn new(registry: ProviderRegistry, probe: Arc<dyn HostProbe>, session: Session) -> Self {
        Self {
            registry,
            probe,
            session,
            events: Vec::new(),
            active: None,
            last_stop_forced: false,
            journal: None,
            journal_error: None,
        }
    }
    /// Continues a restored session: `events` is its existing log, so new
    /// sequence numbers carry on from it.
    pub fn with_history(
        registry: ProviderRegistry,
        probe: Arc<dyn HostProbe>,
        session: Session,
        events: Vec<EventEnvelope>,
    ) -> Self {
        let mut supervisor = Self::new(registry, probe, session);
        supervisor.events = events;
        supervisor
    }
    /// Persists every run and event from now on. A write failure detaches the
    /// journal and is reported by [`Supervisor::journal_error`]; it never
    /// stops the run.
    pub fn attach_journal(&mut self, journal: Journal) {
        self.journal = Some(journal);
    }
    /// The first journal write failure, if any.
    pub fn journal_error(&self) -> Option<&str> {
        self.journal_error.as_deref()
    }
    /// Validates policies and starts one run.
    pub async fn start(&mut self, options: StartOptions) -> Result<(), Error> {
        if let Some(run) = self.session.active_run() {
            return Err(SessionError::RunAlreadyActive(run.id().clone()).into());
        }
        let adapter = self
            .registry
            .get(&options.provider)
            .cloned()
            .ok_or_else(|| Error::UnknownProvider(options.provider.clone()))?;
        let probe = self.probe.clone();
        let workspace = self.session.workspace().to_path_buf();
        let run = options.run.clone();
        let provider = options.provider.clone();
        let label = options.label.clone();
        let (request, plan, capabilities, decoder, shutdown, native) =
            tokio::task::spawn_blocking(move || {
                let installation = adapter.detect(probe.as_ref())?;
                let declared = adapter.capabilities(&installation);
                let transport = options
                    .transport
                    .or_else(|| {
                        adapter
                            .descriptor()
                            .transports
                            .iter()
                            .copied()
                            .find(|transport| {
                                matches!(
                                    transport,
                                    TransportKind::NativePty | TransportKind::CustomCli
                                )
                            })
                    })
                    .ok_or_else(|| {
                        AdapterError::new(
                            ErrorClass::Unsupported,
                            "no implemented transport declared",
                        )
                    })?;
                if !adapter.descriptor().supports_transport(transport) {
                    return Err(Error::Adapter(AdapterError::new(
                        ErrorClass::Unsupported,
                        "adapter does not declare requested transport",
                    )));
                }
                let validated = validate_mapping(
                    &options.permissions,
                    &adapter.map_permissions(&installation, &options.permissions),
                )?;
                let mut request = LaunchRequest::new(workspace, transport, validated);
                request.terminal_size = options.terminal_size;
                request.initial_prompt = options.initial_prompt;
                request.continue_latest = options.continue_latest;
                if request.initial_prompt.is_some() {
                    declared
                        .require(&Capability::InitialPrompt)
                        .map_err(|_| AdapterError::unsupported(Capability::InitialPrompt))?;
                }
                if request.continue_latest {
                    declared
                        .require(&Capability::ResumeLatest)
                        .map_err(|_| AdapterError::unsupported(Capability::ResumeLatest))?;
                }
                let plan = if let Some(native) = &options.native_resume {
                    declared
                        .require(&Capability::ResumeById)
                        .map_err(|_| AdapterError::unsupported(Capability::ResumeById))?;
                    adapter.resume_plan(
                        &installation,
                        &ResumeRequest::new(request.clone(), native.clone()),
                    )?
                } else {
                    adapter.launch_plan(&installation, &request)?
                };
                let mut plan = plan;
                for (key, value) in options.env {
                    plan = plan.env_set(key, value);
                }
                plan.validate(&request)?;
                // ACP semantics are implemented once: an adapter without its own
                // decoder gets the generic ACP mapping. A native terminal agent
                // without one gets the universal usage-limit reader, so any
                // agent can fail over.
                let decoder = adapter
                    .event_decoder(transport)
                    .or_else(|| match &plan.transport {
                        TransportSpec::Acp { versions } => {
                            Some(Box::new(crate::acp::AcpDecoder::new(*versions))
                                as Box<dyn EventDecoder>)
                        }
                        TransportSpec::NativePty { .. } => Some(Box::new(
                            crate::universal_limit::UniversalLimitDecoder::new(),
                        )
                            as Box<dyn EventDecoder>),
                        _ => None,
                    });
                let shutdown = adapter.shutdown_plan(transport);
                let native = options
                    .native_resume
                    .map(|id| (id, NativeSessionLink::Resumed))
                    .or_else(|| {
                        if let NativeSessionBinding::Preassigned(id) = &plan.native_session {
                            Some((id.clone(), NativeSessionLink::New))
                        } else {
                            None
                        }
                    });
                let capabilities =
                    CapabilitySet::negotiate(&declared, &CapabilitySet::new(), transport);
                Ok::<_, Error>((request, plan, capabilities, decoder, shutdown, native))
            })
            .await
            .map_err(|_| Error::WorkerStopped)??;
        self.session.begin_run(NewRun {
            id: run.clone(),
            provider: provider.clone(),
            transport: request.transport,
            permissions: request.permissions.clone(),
            at: now(),
        })?;
        if let (Some(journal), Some(record)) = (&mut self.journal, self.session.run(&run))
            && let Err(error) = journal.append_run(record, label.as_deref())
        {
            self.journal_error = Some(error.to_string());
            self.journal = None;
        }
        self.transition(&run, &provider, RunTransition::Launch)?;
        if let Some((native, link)) = &native {
            self.session
                .observe_native_session(&run, native.clone(), *link)?;
        }
        // Store the spawn task before awaiting it: cancelling start cannot orphan
        // a process or lose the handle. next_frame/stop can finish this launch.
        self.active = Some(Active {
            run,
            provider,
            spawning: Some(tokio::spawn(async move {
                Process::spawn(&plan, &request).await
            })),
            process: None,
            decoder,
            shutdown,
            capabilities,
            outcome: RunOutcome::new(),
            finish: None,
            announce: native,
        });
        self.complete_start().await
    }
    async fn complete_start(&mut self) -> Result<(), Error> {
        let Some(active) = &mut self.active else {
            return Ok(());
        };
        let Some(task) = &mut active.spawning else {
            return Ok(());
        };
        let result = task.await;
        active.spawning.take();
        match result {
            Ok(Ok(process)) => {
                active.process = Some(process);
                let run = active.run.clone();
                let provider = active.provider.clone();
                let announce = active.announce.take();
                self.transition(&run, &provider, RunTransition::Started)?;
                // A session id chosen before launch (or resumed) has no decoder
                // event, so the journal needs this one to restore it later.
                if let Some((native, link)) = announce {
                    self.record(
                        &run,
                        &provider,
                        DecodedEvent::synthetic(AgentEvent::NativeSessionChanged { native, link }),
                    );
                }
                Ok(())
            }
            failure => {
                self.end_active(RunFinish::Aborted(ErrorClass::ProcessFailed));
                match failure {
                    Ok(Err(e)) => Err(e.into()),
                    _ => Err(Error::WorkerStopped),
                }
            }
        }
    }
    /// Drives transport frames and normalized events.
    pub async fn next_frame(&mut self) -> Result<Option<Frame>, Error> {
        self.complete_start().await?;
        match self.receive().await {
            Ok(frame) => Ok(frame),
            Err(error) => {
                // Finish cleanup before returning a read/decoder/session failure.
                if self.active.is_some()
                    && let Err(cleanup) = self
                        .stop_with_reason(Some(RunFinish::Aborted(ErrorClass::ProcessFailed)))
                        .await
                {
                    self.diagnostic(format!("cleanup after {error}: {cleanup}"));
                }
                Err(error)
            }
        }
    }
    async fn receive(&mut self) -> Result<Option<Frame>, Error> {
        let Some(active) = &mut self.active else {
            return Ok(None);
        };
        let result = active
            .process
            .as_mut()
            .expect("completed spawn")
            .next_frame()
            .await;
        let frame = match result {
            Ok(Some(frame)) => frame,
            Err(
                error @ (uuldana_transport::Error::InvalidEncoding
                | uuldana_transport::Error::RecordTooLarge),
            ) => {
                let message = error.to_string();
                let run = active.run.clone();
                let provider = active.provider.clone();
                self.record(
                    &run,
                    &provider,
                    DecodedEvent::synthetic(AgentEvent::ProviderError {
                        class: ErrorClass::ProtocolViolation,
                        message: message.clone(),
                    }),
                );
                return Ok(Some(Frame::Diagnostic(message)));
            }
            Err(error) => return Err(self.transport_failure(error)),
            Ok(None) => {
                // Exhaustion is terminal even when the transport violates its exit
                // contract. Taking Active here prevents a cleanup busy loop.
                let observed = active.process.as_ref().and_then(Process::observed_exit);
                if !matches!(active.finish, Some(RunFinish::Aborted(_))) {
                    active.finish = Some(RunFinish::Aborted(ErrorClass::ProcessFailed));
                }
                self.diagnostic("transport exhausted without a final exit frame".into());
                if let Some(exit) = observed {
                    self.record_observed_exit(exit);
                }
                self.end_active(RunFinish::Aborted(ErrorClass::ProcessFailed));
                return Err(Error::TransportExhausted);
            }
        };
        if let Frame::Diagnostic(message) = &frame {
            self.diagnostic(message.clone());
            return Ok(Some(frame));
        }
        // Capture the authoritative exit before invoking provider code. Decoder
        // errors cannot consume it without finalizing the run.
        let exit = match &frame {
            Frame::Exited(exit) => Some(*exit),
            _ => None,
        };
        let mut decoded = Vec::new();
        if let Some(decoder) = &mut active.decoder
            && catch_unwind(AssertUnwindSafe(|| {
                if let Some(frame) = frame.as_borrowed() {
                    decoder.decode(frame, &mut decoded)
                }
            }))
            .is_err()
        {
            decoded.clear();
            decoded.push(DecodedEvent::synthetic(AgentEvent::ProviderError {
                class: ErrorClass::Internal,
                message: "provider decoder panicked".into(),
            }));
            active.decoder = None;
            active.finish = Some(RunFinish::Aborted(ErrorClass::Internal));
            if exit.is_none()
                && let Err(error) = active.process.as_mut().expect("process").force_terminate()
            {
                decoded.push(DecodedEvent::synthetic(AgentEvent::Diagnostic {
                    message: format!("decoder panic cleanup: {error}"),
                }));
            }
        }
        let run = active.run.clone();
        let provider = active.provider.clone();
        for decoded in decoded {
            // Only an observed transport exit is authoritative process status.
            if matches!(decoded.event, AgentEvent::ProcessExited(_)) {
                continue;
            }
            let native = match &decoded.event {
                AgentEvent::NativeSessionIdentified { native } => {
                    Some((native.clone(), NativeSessionLink::Unspecified))
                }
                AgentEvent::NativeSessionChanged { native, link } => Some((native.clone(), *link)),
                _ => None,
            };
            if let Some((native, link)) = native
                && let Err(error) = self.session.observe_native_session(&run, native, link)
            {
                self.diagnostic(format!("native session observation rejected: {error}"));
            }
            self.record(&run, &provider, decoded);
        }
        if let Some(exit) = exit {
            let verified = self
                .active
                .as_ref()
                .is_none_or(|active| active.process.as_ref().is_none_or(Process::output_verified));
            self.note_output_verification(verified);
            self.record_observed_exit(exit);
            self.end_active(RunFinish::Exited(exit));
        }
        Ok(Some(frame))
    }
    /// Gracefully stops the active run, then kills only after grace expires.
    pub async fn stop(&mut self) -> Result<(), Error> {
        self.stop_with_reason(None).await
    }
    async fn stop_with_reason(&mut self, reason: Option<RunFinish>) -> Result<(), Error> {
        self.complete_start().await?;
        let Some(active) = &mut self.active else {
            return Ok(());
        };
        if active.finish.is_none()
            || (reason.is_some() && !matches!(active.finish, Some(RunFinish::Aborted(_))))
        {
            active.finish = Some(reason.unwrap_or(RunFinish::Stopped));
        }
        let run = active.run.clone();
        let provider = active.provider.clone();
        let shutdown = active.shutdown.clone();
        if self.session.run(&run).expect("active run").state() != RunState::Stopping {
            self.transition(&run, &provider, RunTransition::StopRequested)?;
        }
        self.last_stop_forced = false;
        // Bound potentially blocking writes too. A dead child input must not
        // hold up the grace deadline while output is backpressured.
        for (index, step) in shutdown.steps.iter().enumerate() {
            let result = tokio::time::timeout(
                shutdown.grace,
                self.active
                    .as_mut()
                    .expect("active")
                    .process
                    .as_mut()
                    .expect("process")
                    .shutdown_step(step),
            )
            .await;
            if !matches!(result, Ok(Ok(()))) {
                self.diagnostic(format!(
                    "shutdown step {} failed: {result:?}",
                    step_name(step)
                ));
                if let Some(fallback) = step.fallback() {
                    let result = tokio::time::timeout(
                        shutdown.grace,
                        self.active
                            .as_mut()
                            .expect("active")
                            .process
                            .as_mut()
                            .expect("process")
                            .shutdown_step(&fallback),
                    )
                    .await;
                    self.diagnostic(format!("shutdown fallback {fallback:?}: {result:?}"));
                }
            }
            // Presses of Ctrl+C that arrive together can be read as one, so a
            // later press waits a moment, and is not sent at all to a program
            // that has already left.
            if matches!(step, ShutdownStep::Interrupt) && index + 1 < shutdown.steps.len() {
                let left = tokio::time::timeout(INTERRUPT_GAP, async {
                    while self.active.is_some() {
                        self.receive().await?;
                    }
                    Ok::<_, Error>(())
                })
                .await;
                if matches!(left, Ok(Ok(()))) || self.active.is_none() {
                    return Ok(());
                }
            }
        }
        let graceful = tokio::time::timeout(shutdown.grace, async {
            while self.active.is_some() {
                self.receive().await?;
            }
            Ok::<_, Error>(())
        })
        .await;
        if matches!(graceful, Ok(Ok(()))) {
            return Ok(());
        }
        if self.active.is_none() {
            return match graceful {
                Ok(result) => result,
                Err(_) => Err(Error::ShutdownTimeout),
            };
        }
        self.diagnostic(match &graceful {
            Ok(Err(error)) => format!("graceful cleanup failed: {error}"),
            _ => "shutdown grace expired; forcing process-tree cleanup".into(),
        });
        if let Some(active) = &mut self.active {
            match active.process.as_mut().expect("process").force_terminate() {
                Ok(forced) => {
                    self.last_stop_forced = forced;
                    if forced && !matches!(active.finish, Some(RunFinish::Aborted(_))) {
                        active.finish = Some(RunFinish::Killed);
                    }
                }
                Err(error) => {
                    self.diagnostic(format!("forced cleanup failed: {error}"));
                    self.end_active(RunFinish::Stopped);
                    return Err(error.into());
                }
            }
        }
        let drained = tokio::time::timeout(Duration::from_secs(3), async {
            while self.active.is_some() {
                self.receive().await?;
            }
            Ok::<_, Error>(())
        })
        .await;
        match drained {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => {
                self.diagnostic(format!("shutdown drain failed: {error}"));
                self.end_active(RunFinish::Stopped);
                Err(error)
            }
            Err(_) => Err(self.shutdown_timeout()),
        }
    }
    /// Session record.
    pub fn session(&self) -> &Session {
        &self.session
    }
    /// Ordered normalized history.
    pub fn events(&self) -> &[EventEnvelope] {
        &self.events
    }
    /// Whether the last explicit stop needed force termination.
    pub fn last_stop_forced(&self) -> bool {
        self.last_stop_forced
    }
    /// Effective capability declarations for the active transport.
    pub fn capabilities(&self) -> Option<&CapabilitySet> {
        self.active.as_ref().map(|a| &a.capabilities)
    }
    /// Forwards user input unchanged; this never resolves permissions itself.
    /// Use `input_handle` for large writes while concurrently driving frames.
    pub async fn write_input(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.complete_start().await?;
        self.active
            .as_mut()
            .and_then(|a| a.process.as_mut())
            .ok_or(Error::WorkerStopped)?
            .write_input(bytes)
            .await?;
        Ok(())
    }
    /// Records a native session id learned after the fact, for example from a
    /// provider's own session store. The event is labeled inferred.
    pub fn note_native_session(
        &mut self,
        run: &RunId,
        native: NativeSessionId,
    ) -> Result<(), Error> {
        let provider = self
            .session
            .run(run)
            .ok_or_else(|| SessionError::UnknownRun(run.clone()))?
            .provider()
            .clone();
        let update = self.session.observe_native_session(
            run,
            native.clone(),
            NativeSessionLink::Unspecified,
        )?;
        if update == NativeSessionUpdate::Unchanged {
            return Ok(());
        }
        self.record(
            run,
            &provider,
            DecodedEvent::inferred(AgentEvent::NativeSessionIdentified { native }),
        );
        Ok(())
    }
    /// Sends one newline-terminated message, for example an ACP JSON-RPC line.
    pub async fn send_line(&mut self, line: &str) -> Result<(), Error> {
        let mut bytes = Vec::with_capacity(line.len() + 1);
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');
        self.write_input(&bytes).await
    }
    /// Independent input half of the active process, after startup completes.
    pub fn input_handle(&self) -> Option<uuldana_transport::InputHandle> {
        self.active.as_ref()?.process.as_ref()?.input_handle()
    }
    /// Resizes the active PTY; pipe transports return Unsupported.
    pub fn resize(&mut self, size: TerminalSize) -> Result<(), Error> {
        self.active
            .as_mut()
            .and_then(|a| a.process.as_mut())
            .ok_or(Error::WorkerStopped)?
            .resize(size)?;
        Ok(())
    }
    fn record(&mut self, run: &RunId, provider: &ProviderId, event: DecodedEvent) {
        if let Some(active) = &mut self.active {
            active.outcome.observe(&event.event);
        }
        let envelope = EventEnvelope::new(
            self.events.len() as u64,
            now(),
            self.session.id().clone(),
            run.clone(),
            provider.clone(),
            event,
        );
        if let Some(journal) = &mut self.journal {
            let ended = matches!(
                envelope.event,
                AgentEvent::RunStateChanged {
                    to: RunState::Ended(_),
                    ..
                }
            );
            let written = journal
                .append_event(&envelope)
                .and_then(|()| if ended { journal.sync() } else { Ok(()) });
            if let Err(error) = written {
                self.journal_error = Some(error.to_string());
                self.journal = None;
            }
        }
        self.events.push(envelope);
    }
    fn transition(
        &mut self,
        run: &RunId,
        provider: &ProviderId,
        transition: RunTransition,
    ) -> Result<(), Error> {
        let from = self.session.run(run).expect("known run").state();
        let to = self.session.transition_run(run, transition, now())?;
        self.record(
            run,
            provider,
            DecodedEvent::synthetic(AgentEvent::RunStateChanged { from, to }),
        );
        Ok(())
    }
    /// Records the diagnostic marker when the transport could not verify that
    /// all output was delivered. Never a failure signal: `RunOutcome` ignores it.
    fn note_output_verification(&mut self, verified: bool) {
        if verified {
            return;
        }
        let Some(active) = &self.active else {
            return;
        };
        let run = active.run.clone();
        let provider = active.provider.clone();
        self.record(
            &run,
            &provider,
            DecodedEvent::synthetic(output_unverified_event()),
        );
    }
    fn record_observed_exit(&mut self, exit: ExitInfo) {
        if let Some(active) = &self.active {
            let run = active.run.clone();
            let provider = active.provider.clone();
            if !self
                .events
                .iter()
                .any(|e| e.run == run && matches!(e.event, AgentEvent::ProcessExited(_)))
            {
                self.record(
                    &run,
                    &provider,
                    DecodedEvent::synthetic(AgentEvent::ProcessExited(exit)),
                );
            }
        }
    }
    fn transport_failure(&mut self, error: uuldana_transport::Error) -> Error {
        if let Some(active) = &mut self.active
            && !matches!(active.finish, Some(RunFinish::Aborted(_)))
        {
            active.finish = Some(RunFinish::Aborted(ErrorClass::ProcessFailed));
        }
        error.into()
    }
    fn shutdown_timeout(&mut self) -> Error {
        self.diagnostic("process shutdown or output drain deadline expired".into());
        if let Some(exit) = self
            .active
            .as_ref()
            .and_then(|a| a.process.as_ref())
            .and_then(Process::observed_exit)
        {
            self.record_observed_exit(exit);
        }
        self.end_active(RunFinish::Stopped);
        Error::ShutdownTimeout
    }
    /// Records the text an agent's screen showed, so that a handoff and
    /// `resume` still have it after this process is gone.
    pub fn note_screen(&mut self, run: &RunId, text: String) {
        let Some(provider) = self
            .session
            .run(run)
            .map(|record| record.provider().clone())
        else {
            return;
        };
        self.record(
            run,
            &provider,
            DecodedEvent::synthetic(AgentEvent::Extension {
                namespace: SCREEN_NAMESPACE.into(),
                name: SCREEN_EVENT.into(),
                payload: text,
            }),
        );
    }
    /// Records a note in the journal for the run that is active, or the last
    /// one: what was decided and why.
    pub fn note(&mut self, message: String) {
        self.diagnostic(message);
    }
    fn diagnostic(&mut self, message: String) {
        if let Some(run) = self
            .active
            .as_ref()
            .map(|a| (&a.run, &a.provider))
            .or_else(|| self.session.runs().last().map(|r| (r.id(), r.provider())))
        {
            let (run, provider) = (run.0.clone(), run.1.clone());
            self.record(
                &run,
                &provider,
                DecodedEvent::synthetic(AgentEvent::Diagnostic { message }),
            );
        }
    }
    fn end_active(&mut self, finish: RunFinish) {
        if let Some(active) = self.active.take() {
            let reason = active.outcome.finalize(active.finish.unwrap_or(finish));
            if let Err(error) =
                self.transition(&active.run, &active.provider, RunTransition::End(reason))
            {
                self.record(
                    &active.run,
                    &active.provider,
                    DecodedEvent::synthetic(AgentEvent::Diagnostic {
                        message: format!("run finalization rejected: {error}"),
                    }),
                );
            }
        }
    }
}
/// Marker for a run whose output could not be verified after exit: the
/// transport's final bound passed while an output reader was still alive, so
/// output produced before exit may not have been delivered. It is a
/// diagnostic, not a failure signal. `RunOutcome` ignores it, so a `DONE` or a
/// limit that was received keeps deciding the outcome.
fn output_unverified_event() -> AgentEvent {
    AgentEvent::Extension {
        namespace: "uuldana.transport".into(),
        name: "output_unverified".into(),
        payload: r#"{"reason":"reader_alive_at_final_bound"}"#.into(),
    }
}
fn now() -> Timestamp {
    Timestamp::from_system_time(SystemTime::now())
}
fn step_name(step: &ShutdownStep) -> &'static str {
    match step {
        ShutdownStep::Interrupt => "Interrupt",
        ShutdownStep::CloseInput => "CloseInput",
        ShutdownStep::WriteInput(_) => "WriteInput",
        _ => "Other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        LocalHost,
        mock::{MockProvider, Scenario},
    };

    fn stopping_supervisor(finish: RunFinish) -> Supervisor {
        let id = ProviderId::new("test.failure").unwrap();
        let run = RunId::new("r").unwrap();
        let mut session = Session::new(
            SessionId::new("s").unwrap(),
            std::env::current_dir().unwrap(),
            PermissionPolicy::default(),
            now(),
        )
        .unwrap();
        let permissions = validate_mapping(
            &PermissionPolicy::default(),
            &PermissionMapping::new(PermissionPolicy::default()),
        )
        .unwrap();
        session
            .begin_run(NewRun {
                id: run.clone(),
                provider: id.clone(),
                transport: TransportKind::CustomCli,
                permissions,
                at: now(),
            })
            .unwrap();
        session
            .transition_run(&run, RunTransition::Launch, now())
            .unwrap();
        session
            .transition_run(&run, RunTransition::Started, now())
            .unwrap();
        session
            .transition_run(&run, RunTransition::StopRequested, now())
            .unwrap();
        let mut supervisor =
            Supervisor::new(ProviderRegistry::new(), Arc::new(LocalHost::new()), session);
        supervisor.active = Some(Active {
            run,
            provider: id,
            spawning: None,
            process: None,
            decoder: None,
            shutdown: ShutdownPlan::default(),
            capabilities: CapabilitySet::new(),
            outcome: RunOutcome::new(),
            finish: Some(finish),
            announce: None,
        });
        supervisor
    }

    #[test]
    fn a_repeated_native_session_is_recorded_once() {
        let mut supervisor = stopping_supervisor(RunFinish::Stopped);
        let run = RunId::new("r").unwrap();
        let native = NativeSessionId::new("native-1").unwrap();
        supervisor
            .note_native_session(&run, native.clone())
            .unwrap();
        supervisor.note_native_session(&run, native).unwrap();
        let identified = supervisor
            .events()
            .iter()
            .filter(|e| matches!(e.event, AgentEvent::NativeSessionIdentified { .. }))
            .count();
        assert_eq!(identified, 1);
    }

    fn completed_turn() -> RunOutcome {
        let mut outcome = RunOutcome::new();
        outcome.observe(&AgentEvent::TurnStarted);
        outcome.observe(&AgentEvent::TurnEnded {
            reason: StopReason::EndTurn,
        });
        outcome
    }

    fn marker_is(event: &AgentEvent) -> bool {
        matches!(
            event,
            AgentEvent::Extension { namespace, name, .. }
                if namespace == "uuldana.transport" && name == "output_unverified"
        )
    }

    #[test]
    fn output_unverified_marker_is_diagnostic_and_never_changes_the_outcome() {
        let marker = output_unverified_event();
        assert!(marker_is(&marker));
        let mut limited = RunOutcome::new();
        limited.observe(&AgentEvent::LimitReached(LimitSignal {
            kind: LimitKind::UsageQuota,
            resets_at: None,
            message: None,
        }));
        let mut failed = RunOutcome::new();
        failed.observe(&AgentEvent::ProviderError {
            class: ErrorClass::Transient,
            message: "x".into(),
        });
        for exit in [ExitInfo::from_code(0), ExitInfo::from_code(1)] {
            for outcome in [
                RunOutcome::new(),
                completed_turn(),
                limited.clone(),
                failed.clone(),
            ] {
                let mut marked = outcome.clone();
                marked.observe(&marker);
                assert_eq!(
                    marked, outcome,
                    "the marker must not enter RunOutcome state"
                );
                assert_eq!(marked.pending_signal(), outcome.pending_signal());
                for finish in [
                    RunFinish::Exited(exit),
                    RunFinish::Stopped,
                    RunFinish::Killed,
                    RunFinish::Aborted(ErrorClass::Internal),
                ] {
                    assert_eq!(marked.finalize(finish), outcome.finalize(finish));
                }
            }
        }
    }

    fn natural_exit_supervisor(events: &[AgentEvent]) -> Supervisor {
        let mut supervisor = stopping_supervisor(RunFinish::Stopped);
        let active = supervisor.active.as_mut().unwrap();
        active.finish = None;
        let (run, provider) = (active.run.clone(), active.provider.clone());
        for event in events {
            supervisor.record(&run, &provider, DecodedEvent::native(event.clone()));
        }
        supervisor
    }

    /// Mirrors the tail of `receive()` for `Frame::Exited`.
    fn finalize_natural_exit(supervisor: &mut Supervisor, exit: ExitInfo, verified: bool) {
        supervisor.note_output_verification(verified);
        supervisor.record_observed_exit(exit);
        supervisor.end_active(RunFinish::Exited(exit));
    }

    #[test]
    fn unverified_output_is_marked_before_the_single_terminal_finalization() {
        let quota = AgentEvent::LimitReached(LimitSignal {
            kind: LimitKind::UsageQuota,
            resets_at: None,
            message: None,
        });
        let turn_end = AgentEvent::TurnEnded {
            reason: StopReason::EndTurn,
        };
        let cases = [
            (
                "completed",
                vec![AgentEvent::TurnStarted, turn_end],
                0,
                RunEnd::Completed,
            ),
            (
                "limit",
                vec![AgentEvent::TurnStarted, quota],
                1,
                RunEnd::LimitReached(LimitKind::UsageQuota),
            ),
            ("no turn", vec![], 0, RunEnd::Exited { code: Some(0) }),
            (
                "lost record",
                vec![AgentEvent::TurnStarted],
                1,
                RunEnd::Exited { code: Some(1) },
            ),
        ];
        for (name, events, code, end) in cases {
            let exit = ExitInfo::from_code(code);
            for verified in [true, false] {
                let mut supervisor = natural_exit_supervisor(&events);
                finalize_natural_exit(&mut supervisor, exit, verified);
                let log: Vec<&AgentEvent> = supervisor.events().iter().map(|e| &e.event).collect();
                // The marker never alters the outcome.
                assert_eq!(
                    supervisor.session().runs()[0].state(),
                    RunState::Ended(end),
                    "{name} verified={verified}"
                );
                let markers: Vec<usize> = (0..log.len()).filter(|&i| marker_is(log[i])).collect();
                let exits: Vec<usize> = (0..log.len())
                    .filter(|&i| matches!(log[i], AgentEvent::ProcessExited(_)))
                    .collect();
                let ended: Vec<usize> = (0..log.len())
                    .filter(|&i| {
                        matches!(
                            log[i],
                            AgentEvent::RunStateChanged {
                                to: RunState::Ended(_),
                                ..
                            }
                        )
                    })
                    .collect();
                assert_eq!(exits.len(), 1, "{name} verified={verified}: {log:?}");
                assert_eq!(ended.len(), 1, "{name} verified={verified}: {log:?}");
                assert!(exits[0] < ended[0], "{name}: exit precedes finalization");
                assert!(supervisor.active.is_none());
                if verified {
                    assert!(
                        markers.is_empty(),
                        "{name}: verified output emits no marker"
                    );
                } else {
                    assert_eq!(markers.len(), 1, "{name}: {log:?}");
                    assert!(
                        markers[0] < exits[0],
                        "{name}: marker precedes ProcessExited"
                    );
                }
            }
        }
    }

    // Terminal classification matrix. The frame sequences are exactly those
    // pinned by the `case*` tests in uuldana-transport (process_drain_regression.rs):
    // delivered records, an optional bound diagnostic, one exit. The replay mirrors
    // `receive()`: decode, observe (skipping decoder-made `ProcessExited`), emit the
    // output-unverified marker when the transport could not verify output, then
    // finalize.
    const COMPLETE_RECORDS: [&str; 4] = [
        "READY native-mock-session",
        "TURN",
        "TEXT deterministic progress",
        "DONE",
    ];
    const BOUND: &str = "post-exit output wait reached its 250 ms bound; buffered frames retained, further output discarded; delivery of earlier output is unverified";

    struct Case {
        name: &'static str,
        frames: Vec<Frame>,
        exit: i32,
        verified: bool,
        /// Outcome if every produced record had been delivered.
        truth: RunEnd,
        /// Outcome under contract 0.2 from what was actually observed.
        expected: RunEnd,
    }

    fn frames(records: &[&str], diagnostic: bool, exit: i32) -> Vec<Frame> {
        records
            .iter()
            .map(|r| Frame::Message((*r).into()))
            .chain(diagnostic.then(|| Frame::Diagnostic(BOUND.into())))
            .chain([Frame::Exited(ExitInfo::from_code(exit))])
            .collect()
    }

    fn limit_records() -> Vec<&'static str> {
        let mut records = COMPLETE_RECORDS[..3].to_vec();
        records.push("LIMIT quota");
        records
    }

    fn cases() -> Vec<Case> {
        let quota = RunEnd::LimitReached(LimitKind::UsageQuota);
        let exited = |code| RunEnd::Exited { code: Some(code) };
        let no_done = &COMPLETE_RECORDS[..3];
        vec![
            Case {
                name: "1 DONE received, stdout never EOF",
                frames: frames(&COMPLETE_RECORDS, true, 0),
                exit: 0,
                verified: false,
                truth: RunEnd::Completed,
                expected: RunEnd::Completed,
            },
            Case {
                name: "2 DONE produced, discarded after bound",
                frames: frames(&[], true, 0),
                exit: 0,
                verified: false,
                truth: RunEnd::Completed,
                expected: exited(0),
            },
            Case {
                name: "3 no DONE produced, inherited handle open",
                frames: frames(no_done, true, 0),
                exit: 0,
                verified: false,
                truth: exited(0),
                expected: exited(0),
            },
            Case {
                name: "4a LIMIT received, clean EOF",
                frames: frames(&limit_records(), false, 1),
                exit: 1,
                verified: true,
                truth: quota,
                expected: quota,
            },
            Case {
                name: "4b LIMIT received, stdout never EOF",
                frames: frames(&limit_records(), true, 1),
                exit: 1,
                verified: false,
                truth: quota,
                expected: quota,
            },
            Case {
                name: "5 LIMIT produced, discarded after bound",
                frames: frames(&[], true, 1),
                exit: 1,
                verified: false,
                truth: quota,
                expected: exited(1),
            },
            Case {
                name: "B completed turn, second turn lost (same frames as 1)",
                frames: frames(&COMPLETE_RECORDS, true, 0),
                exit: 0,
                verified: false,
                truth: RunEnd::Failed(ErrorClass::Transient),
                expected: RunEnd::Completed,
            },
            Case {
                name: "6 DONE behind a forwarded prefix, discarded after bound",
                frames: frames(no_done, true, 0),
                exit: 0,
                verified: false,
                truth: RunEnd::Completed,
                expected: exited(0),
            },
        ]
    }

    struct Replayed {
        end: RunEnd,
        events: Vec<AgentEvent>,
    }

    fn replay(case: &Case) -> Replayed {
        let adapter = MockProvider::new(
            ProviderId::new("test.matrix").unwrap(),
            std::path::PathBuf::new(),
            Scenario::NormalCompletion,
        );
        let mut decoder = adapter.event_decoder(TransportKind::CustomCli).unwrap();
        let mut outcome = RunOutcome::new();
        let mut events = Vec::new();
        let mut exit = None;
        for frame in &case.frames {
            if let Frame::Diagnostic(message) = frame {
                events.push(AgentEvent::Diagnostic {
                    message: message.clone(),
                });
                continue;
            }
            if let Frame::Exited(info) = frame {
                exit = Some(*info);
            }
            let mut decoded = Vec::new();
            decoder.decode(frame.as_borrowed().unwrap(), &mut decoded);
            for decoded in decoded {
                if matches!(decoded.event, AgentEvent::ProcessExited(_)) {
                    continue;
                }
                outcome.observe(&decoded.event);
                events.push(decoded.event);
            }
        }
        let exit = exit.unwrap();
        assert_eq!(exit.code, Some(case.exit), "{}", case.name);
        if !case.verified {
            let marker = output_unverified_event();
            outcome.observe(&marker);
            events.push(marker);
        }
        events.push(AgentEvent::ProcessExited(exit));
        Replayed {
            end: outcome.finalize(RunFinish::Exited(exit)),
            events,
        }
    }

    #[test]
    fn classification_matrix() {
        println!("{:<56} {:<26} {:<7} truth", "case", "outcome", "marker");
        for case in cases() {
            let replayed = replay(&case);
            let marker_at = replayed.events.iter().position(marker_is);
            let exit_at = replayed
                .events
                .iter()
                .position(|e| matches!(e, AgentEvent::ProcessExited(_)))
                .unwrap();
            println!(
                "{:<56} {:<26} {:<7} {:?}\n    events: {:?}",
                case.name,
                format!("{:?}", replayed.end),
                marker_at.is_some(),
                case.truth,
                replayed.events
            );
            assert_eq!(replayed.end, case.expected, "{}", case.name);
            // Exactly one marker, only for unverified output, before the exit,
            // and after any transport diagnostic.
            assert_eq!(marker_at.is_some(), !case.verified, "{}", case.name);
            if let Some(at) = marker_at {
                assert!(at < exit_at, "{}", case.name);
                assert_eq!(replayed.events.iter().filter(|e| marker_is(e)).count(), 1);
                if let Some(diagnostic) = replayed
                    .events
                    .iter()
                    .position(|e| matches!(e, AgentEvent::Diagnostic { .. }))
                {
                    assert!(diagnostic < at, "{}", case.name);
                }
            }
            assert_eq!(
                replayed
                    .events
                    .iter()
                    .filter(|e| matches!(e, AgentEvent::ProcessExited(_)))
                    .count(),
                1
            );
        }
    }

    // Accepted residual risk of the D1 decision: a confirmed completion and a
    // stale completion whose later records were lost reach the supervisor as the
    // same frames, so both end `Completed` with the diagnostic marker.
    #[test]
    fn confirmed_and_stale_completion_are_indistinguishable_and_both_marked() {
        let all = cases();
        let (confirmed, stale) = (&all[0], &all[6]);
        assert_eq!(confirmed.frames, stale.frames);
        assert_ne!(confirmed.truth, stale.truth);
        let (a, b) = (replay(confirmed), replay(stale));
        assert_eq!(a.events, b.events);
        assert_eq!((a.end, b.end), (RunEnd::Completed, RunEnd::Completed));
        assert!(a.events.iter().any(marker_is));
    }

    #[test]
    fn shutdown_timeout_retains_cancellation_and_abort_precedence_without_inventing_exit() {
        for (finish, end) in [
            (RunFinish::Stopped, RunEnd::Cancelled),
            (RunFinish::Killed, RunEnd::Terminated),
            (
                RunFinish::Aborted(ErrorClass::Internal),
                RunEnd::Failed(ErrorClass::Internal),
            ),
        ] {
            let mut supervisor = stopping_supervisor(finish);
            assert!(matches!(
                supervisor.shutdown_timeout(),
                Error::ShutdownTimeout
            ));
            assert_eq!(supervisor.session().runs()[0].state(), RunState::Ended(end));
            assert!(
                supervisor
                    .events()
                    .iter()
                    .all(|e| !matches!(e.event, AgentEvent::ProcessExited(_)))
            );
            assert_eq!(
                supervisor
                    .events()
                    .iter()
                    .filter(|e| matches!(
                        e.event,
                        AgentEvent::RunStateChanged {
                            to: RunState::Ended(_),
                            ..
                        }
                    ))
                    .count(),
                1
            );
            assert!(supervisor.active.is_none());
        }
    }

    #[test]
    fn unrecoverable_transport_failure_outranks_both_cancellation_finishes() {
        for finish in [RunFinish::Stopped, RunFinish::Killed] {
            let mut supervisor = stopping_supervisor(finish);
            let error = supervisor.transport_failure(uuldana_transport::Error::Io(
                std::io::Error::other("injected reader failure"),
            ));
            assert!(matches!(error, Error::Transport(_)));
            supervisor.end_active(RunFinish::Stopped);
            assert_eq!(
                supervisor.session().runs()[0].state(),
                RunState::Ended(RunEnd::Failed(ErrorClass::ProcessFailed))
            );
            assert!(supervisor.active.is_none());
        }
    }

    #[test]
    fn external_watchdog_guards_exhausted_transport_cleanup() {
        const FLAG: &str = "UULDANA_EXHAUSTION_WATCHDOG";
        if std::env::var_os(FLAG).is_some() {
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
                for stopping in [false,true] {
                    let fixture = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join("examples").join(format!("uuldana-mock{}",std::env::consts::EXE_SUFFIX));
                    let id = ProviderId::new("test.exhaustion").unwrap();
                    let mut registry = ProviderRegistry::new();
                    registry.register(Arc::new(MockProvider::new(id.clone(),fixture,Scenario::NormalCompletion))).unwrap();
                    let session = Session::new(SessionId::new("exhaustion").unwrap(), std::env::current_dir().unwrap(),PermissionPolicy::default(),now()).unwrap();
                    let mut supervisor = Supervisor::new(registry,Arc::new(LocalHost::new()),session);
                    supervisor.start(StartOptions::new(id,RunId::new("r").unwrap())).await.unwrap();
                    // Deliberately consume the transport's final exit without
                    // letting Supervisor see it, reproducing exhausted cleanup.
                    let process = supervisor.active.as_mut().unwrap().process.as_mut().unwrap();
                    while process.next_frame().await.unwrap().is_some() {}
                    let result = if stopping { supervisor.stop().await } else { supervisor.next_frame().await.map(|_| ()) };
                    assert!(matches!(result,Err(Error::TransportExhausted)));
                    assert!(supervisor.active.is_none());
                    assert_eq!(supervisor.events().iter().filter(|e| matches!(e.event,AgentEvent::ProcessExited(exit) if exit.success)).count(),1);
                    assert_eq!(supervisor.session().runs()[0].state(),RunState::Ended(RunEnd::Failed(ErrorClass::ProcessFailed)));
                }
            });
            return;
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "supervisor::tests::external_watchdog_guards_exhausted_transport_cleanup",
                "--nocapture",
            ])
            .env(FLAG, "1")
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "watchdog child assertions failed");
                break;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("external watchdog detected exhausted-transport starvation");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
