use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};
use uuldana_core::*;
use uuldana_transport::{Frame, Process};

const OUTPUT_LIMIT: usize = 1024 * 1024;
const PROBE_CLEANUP: Duration = Duration::from_secs(3);

/// A host whose environment lookups see extra variables first. Adapter
/// callbacks about a run use it so they look where that run's account keeps
/// its files, without changing the environment of Uuldana itself.
pub struct EnvOverlay {
    inner: std::sync::Arc<dyn HostProbe>,
    env: Vec<(OsString, OsString)>,
}
impl EnvOverlay {
    /// Wraps `inner`, answering `env` entries before it.
    pub fn new(inner: std::sync::Arc<dyn HostProbe>, env: Vec<(OsString, OsString)>) -> Self {
        Self { inner, env }
    }
}
impl HostProbe for EnvOverlay {
    fn find_executable(&self, name: &str) -> Option<PathBuf> {
        self.inner.find_executable(name)
    }
    fn run(
        &self,
        program: &Path,
        args: &[&str],
        timeout: Duration,
    ) -> Result<ProbeOutput, ProbeError> {
        self.inner.run(program, args, timeout)
    }
    fn env_var(&self, name: &str) -> Option<OsString> {
        self.env
            .iter()
            .find(|(key, _)| key.as_os_str() == std::ffi::OsStr::new(name))
            .map(|(_, value)| value.clone())
            .or_else(|| self.inner.env_var(name))
    }
    fn platform(&self) -> Platform {
        self.inner.platform()
    }
}

/// Read-only host access for executable discovery and bounded version probes.
#[derive(Debug, Default)]
pub struct LocalHost;
impl LocalHost {
    /// Creates a host probe using the inherited PATH and PATHEXT.
    pub fn new() -> Self {
        Self
    }
}
impl HostProbe for LocalHost {
    fn find_executable(&self, name: &str) -> Option<PathBuf> {
        let paths = std::env::var_os("PATH").unwrap_or_default();
        let ext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        resolve(
            name,
            &std::env::split_paths(&paths).collect::<Vec<_>>(),
            &ext,
            cfg!(windows),
        )
    }
    fn run(
        &self,
        program: &Path,
        args: &[&str],
        timeout: Duration,
    ) -> Result<ProbeOutput, ProbeError> {
        if !program.is_absolute() {
            return Err(ProbeError::Denied(
                "probe requires an absolute executable".into(),
            ));
        }
        let deadline = Instant::now() + timeout;
        let program = program.to_path_buf();
        let args = args.iter().map(OsString::from).collect::<Vec<_>>();
        // HostProbe is synchronous and can be invoked from an existing Tokio
        // runtime. A dedicated thread/runtime avoids nested block_on calls and
        // shares the same contained process lifecycle as provider runs.
        thread::Builder::new()
            .name("uuldana-probe".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .build()
                    .map_err(probe_io)?;
                let result = runtime.block_on(run_probe(program, args, deadline));
                runtime.shutdown_timeout(PROBE_CLEANUP);
                result
            })
            .map_err(probe_io)?
            .join()
            .map_err(|_| ProbeError::Io("probe worker panicked".into()))?
    }

    fn env_var(&self, name: &str) -> Option<OsString> {
        std::env::var_os(name)
    }
    fn platform(&self) -> Platform {
        Platform::current()
    }
}
async fn run_probe(
    program: PathBuf,
    args: Vec<OsString>,
    deadline: Instant,
) -> Result<ProbeOutput, ProbeError> {
    let cwd = std::env::current_dir().map_err(probe_io)?;
    let policy = PermissionPolicy::default();
    let permissions = validate_mapping(&policy, &PermissionMapping::new(policy))
        .map_err(|e| ProbeError::Denied(e.to_string()))?;
    let request = LaunchRequest::new(cwd.clone(), TransportKind::CustomCli, permissions);
    let mut plan = LaunchPlan::new(
        program,
        cwd,
        TransportSpec::CustomCli {
            stdout: StreamFraming::Raw,
            stdin: StdinMode::Closed,
        },
    )
    .args(args);
    // The program is only asked what it is. It does not need the user's
    // credentials, so it gets the plain system environment and nothing else.
    for (key, _) in std::env::vars_os() {
        // Windows keeps per-drive folders in names such as `=C:`, which are
        // not variables a program reads and cannot be named in a plan.
        if !probe_keeps(&key) && !key.to_string_lossy().contains('=') {
            plan = plan.env_remove(key);
        }
    }
    let deadline = tokio::time::Instant::from_std(deadline);
    let mut process = tokio::time::timeout_at(deadline, Process::spawn(&plan, &request))
        .await
        .map_err(|_| ProbeError::TimedOut)?
        .map_err(probe_transport)?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let error = loop {
        match tokio::time::timeout_at(deadline, process.next_frame()).await {
            Ok(Ok(Some(Frame::Output(bytes)))) => {
                if bytes.len() > OUTPUT_LIMIT - stdout.len() {
                    break ProbeError::Io("probe stdout exceeds 1 MiB".into());
                }
                stdout.extend(bytes);
            }
            Ok(Ok(Some(Frame::Stderr(bytes)))) => {
                if bytes.len() > OUTPUT_LIMIT - stderr.len() {
                    break ProbeError::Io("probe stderr exceeds 1 MiB".into());
                }
                stderr.extend(bytes);
            }
            Ok(Ok(Some(Frame::Exited(exit)))) => {
                return Ok(ProbeOutput {
                    exit,
                    stdout,
                    stderr,
                });
            }
            Ok(Ok(Some(Frame::Diagnostic(message)))) => {
                break ProbeError::Io(format!("probe transport diagnostic: {message}"));
            }
            Ok(Ok(Some(Frame::Message(_)))) => {
                break ProbeError::Io("raw probe unexpectedly returned a structured record".into());
            }
            Ok(Ok(None)) => break ProbeError::Io("probe ended without a child status".into()),
            Ok(Err(error)) => break probe_transport(error),
            Err(_) => break ProbeError::TimedOut,
        }
    };
    Err(clean_up_probe(&mut process, error).await)
}

/// Variables a probed program still sees: where programs and the user's folders
/// are, the locale and the terminal kind. Every other variable, which is where
/// API keys and tokens live, is removed for the probe.
const PROBE_ENV: &[&str] = &[
    "PATH",
    "PATHEXT",
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    "WINDIR",
    "COMSPEC",
    "TEMP",
    "TMP",
    "TMPDIR",
    "HOME",
    "HOMEDRIVE",
    "HOMEPATH",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "PROGRAMW6432",
    "COMMONPROGRAMFILES",
    "COMMONPROGRAMFILES(X86)",
    "COMMONPROGRAMW6432",
    "ALLUSERSPROFILE",
    "PUBLIC",
    "USERNAME",
    "USERDOMAIN",
    "COMPUTERNAME",
    "OS",
    "PROCESSOR_ARCHITECTURE",
    "PROCESSOR_ARCHITEW6432",
    "NUMBER_OF_PROCESSORS",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "LANGUAGE",
    "TZ",
    "TERM",
    "COLORTERM",
    "NO_COLOR",
];

fn probe_keeps(name: &std::ffi::OsStr) -> bool {
    let upper = name.to_string_lossy().to_ascii_uppercase();
    PROBE_ENV.contains(&upper.as_str()) || upper.starts_with("LC_") || upper.starts_with("XDG_")
}

async fn clean_up_probe(process: &mut Process, original: ProbeError) -> ProbeError {
    let mut diagnostics = Vec::new();
    if let Err(error) = process.force_terminate() {
        diagnostics.push(format!("tree termination failed: {error}"));
    }
    let cleanup = tokio::time::timeout(PROBE_CLEANUP, async {
        loop {
            match process.next_frame().await {
                Ok(Some(Frame::Exited(_))) => return,
                Ok(Some(Frame::Diagnostic(message))) => diagnostics.push(message),
                Ok(Some(_)) => {}
                Ok(None) if process.observed_exit().is_some() => return,
                Ok(None) => {
                    diagnostics.push("cleanup ended without a child status".into());
                    return;
                }
                Err(error) => {
                    diagnostics.push(format!("cleanup transport failed: {error}"));
                    return;
                }
            }
        }
    })
    .await;
    if cleanup.is_err() {
        diagnostics.push("probe tree reap or drain exceeded its 3 second cleanup bound".into());
    }
    if diagnostics.is_empty() {
        original
    } else {
        ProbeError::Io(format!("{original}; cleanup: {}", diagnostics.join("; ")))
    }
}

fn probe_transport(error: uuldana_transport::Error) -> ProbeError {
    match error {
        uuldana_transport::Error::Io(error) => probe_io(error),
        error => ProbeError::Io(error.to_string()),
    }
}

fn probe_io(e: std::io::Error) -> ProbeError {
    match e.kind() {
        std::io::ErrorKind::NotFound => ProbeError::NotFound,
        std::io::ErrorKind::PermissionDenied => ProbeError::Denied(e.to_string()),
        _ => ProbeError::Io(e.to_string()),
    }
}
fn resolve(name: &str, paths: &[PathBuf], extensions: &str, windows: bool) -> Option<PathBuf> {
    let name = Path::new(name);
    let bases = if name.is_absolute() {
        vec![name.to_path_buf()]
    } else if name.components().count() == 1 {
        paths
            .iter()
            .filter(|p| p.is_absolute())
            .map(|p| p.join(name))
            .collect()
    } else {
        return None;
    };
    for base in bases {
        let mut candidates = Vec::new();
        if windows && base.extension().is_none() {
            candidates.extend(
                extensions
                    .split(';')
                    .filter(|e| e.starts_with('.'))
                    .map(|e| {
                        let mut p = base.clone().into_os_string();
                        p.push(e);
                        PathBuf::from(p)
                    }),
            );
        } else {
            candidates.push(base);
        }
        for path in candidates {
            if executable(&path, windows) {
                return Some(path);
            }
        }
    }
    None
}
fn executable(path: &Path, windows: bool) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    if !windows {
        use std::os::unix::fs::PermissionsExt;
        return path
            .metadata()
            .is_ok_and(|m| m.permissions().mode() & 0o111 != 0);
    }
    let _ = windows;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_probe_keeps_the_system_environment_and_drops_everything_else() {
        for kept in [
            "PATH",
            "Path",
            "SystemRoot",
            "LC_ALL",
            "XDG_DATA_HOME",
            "TEMP",
        ] {
            assert!(probe_keeps(std::ffi::OsStr::new(kept)), "{kept}");
        }
        for dropped in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "GITHUB_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "NPM_TOKEN",
            "MY_PASSWORD",
        ] {
            assert!(!probe_keeps(std::ffi::OsStr::new(dropped)), "{dropped}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_probed_program_does_not_see_the_users_other_variables() {
        let cmd = PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32")
            .join("cmd.exe");
        let output = LocalHost::new()
            .run(&cmd, &["/d", "/c", "set"], Duration::from_secs(10))
            .unwrap();
        let seen = String::from_utf8_lossy(&output.stdout).to_ascii_uppercase();
        assert!(
            seen.contains("\nPATH=") || seen.starts_with("PATH="),
            "{seen}"
        );
        for (name, _) in std::env::vars_os() {
            // `cmd` defines its own PROMPT whatever it inherits.
            if !probe_keeps(&name) && name != "PROMPT" {
                let needle = format!("\n{}=", name.to_string_lossy().to_ascii_uppercase());
                assert!(!seen.contains(&needle), "{name:?} reached the probe");
            }
        }
    }

    #[test]
    fn pathext_finds_cmd_shims_in_absolute_search_directories() {
        let root = std::env::temp_dir().join(format!("uuldana-pathext-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let shim = root.join("local-fixture.CMD");
        std::fs::write(&shim, "@exit /b 0\r\n").unwrap();
        let posix_shim = root.join("local-fixture");
        std::fs::write(&posix_shim, "#!/bin/sh\nexit 0\n").unwrap();
        assert_eq!(
            resolve(
                "local-fixture",
                std::slice::from_ref(&root),
                ".EXE;.CMD",
                true
            ),
            Some(shim.clone())
        );
        assert_eq!(
            resolve("missing", std::slice::from_ref(&root), ".CMD", true),
            None
        );
        std::fs::remove_file(shim).unwrap();
        std::fs::remove_file(posix_shim).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
}
