use std::path::PathBuf;

pub fn fixture() -> PathBuf {
    std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("examples")
        .join(format!("uuldana-mock{}", std::env::consts::EXE_SUFFIX))
}

#[allow(dead_code)]
pub fn acp_fixture() -> PathBuf {
    fixture().with_file_name(format!(
        "uuldana-acp-fixture{}",
        std::env::consts::EXE_SUFFIX
    ))
}

/// Failure diagnostics: history, exit/terminal counts and transport notices of
/// one runtime. Only call it from a panic or assert message, so a passing run
/// never formats anything.
#[allow(dead_code)]
pub fn describe_run(runtime: &uuldana_runtime::Supervisor) -> String {
    use std::fmt::Write;
    use uuldana_core::{AgentEvent, RunState};
    let events = runtime.events();
    let exits = events
        .iter()
        .filter(|e| matches!(e.event, AgentEvent::ProcessExited(_)))
        .count();
    let ended = events
        .iter()
        .filter(|e| {
            matches!(
                e.event,
                AgentEvent::RunStateChanged {
                    to: RunState::Ended(_),
                    ..
                }
            )
        })
        .count();
    let mut out = format!(
        "runtime events={} process_exited={exits} ended_transitions={ended} stop_forced={}\n",
        events.len(),
        runtime.last_stop_forced()
    );
    for run in runtime.session().runs() {
        let _ = writeln!(out, "run state={:?}", run.state());
    }
    for envelope in events {
        let note = match &envelope.event {
            AgentEvent::Diagnostic { message } if message.contains("post-exit output wait") => {
                " <- transport drain bound reached; later output was discarded"
            }
            AgentEvent::Diagnostic { .. } => " <- transport diagnostic",
            _ => "",
        };
        let _ = writeln!(out, "  #{:<3} {:?}{note}", envelope.seq, envelope.event);
    }
    out
}

/// Drains every frame like the inline loops it replaces; on failure it panics
/// with the history instead of a bare `Elapsed` or unwrap.
#[allow(dead_code)]
pub async fn drain_to_end(runtime: &mut uuldana_runtime::Supervisor, limit: std::time::Duration) {
    let drained = tokio::time::timeout(limit, async {
        loop {
            match runtime.next_frame().await {
                Ok(Some(_)) => {}
                Ok(None) => break Ok(()),
                Err(error) => break Err(error),
            }
        }
    })
    .await;
    match drained {
        Ok(Ok(())) => {}
        Ok(Err(error)) => panic!("next_frame failed: {error:?}\n{}", describe_run(runtime)),
        Err(_) => panic!("drain exceeded {limit:?}\n{}", describe_run(runtime)),
    }
}

/// Diagnostics only: preserves every LocalHost argument, result and deadline.
#[derive(Default)]
#[allow(dead_code)]
pub struct DiagnosticHost(uuldana_runtime::LocalHost);
impl uuldana_core::HostProbe for DiagnosticHost {
    fn find_executable(&self, name: &str) -> Option<PathBuf> {
        self.0.find_executable(name)
    }
    fn run(
        &self,
        program: &std::path::Path,
        args: &[&str],
        timeout: std::time::Duration,
    ) -> Result<uuldana_core::ProbeOutput, uuldana_core::ProbeError> {
        let started = std::time::Instant::now();
        let result = self.0.run(program, args, timeout);
        eprintln!(
            "host-probe pid={} program={program:?} args={args:?} deadline={timeout:?} elapsed={:?} result={:?}",
            std::process::id(),
            started.elapsed(),
            result
                .as_ref()
                .map(|output| (output.exit, output.stdout.len(), output.stderr.len()))
        );
        result
    }
    fn env_var(&self, name: &str) -> Option<std::ffi::OsString> {
        self.0.env_var(name)
    }
    fn platform(&self) -> uuldana_core::Platform {
        self.0.platform()
    }
}
