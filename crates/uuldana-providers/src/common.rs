//! Helpers shared by the provider adapters.

use std::{
    collections::hash_map::RandomState,
    ffi::OsString,
    hash::{BuildHasher, Hasher},
    path::{Path, PathBuf},
    time::Duration,
};

use uuldana_provider_sdk::{
    AdapterError, ErrorClass, HostProbe, Installation, LaunchPlan, LaunchRequest, TransportKind,
    TransportSpec,
};

const VERSION_TIMEOUT: Duration = Duration::from_secs(10);

/// Locates the provider executable, resolves npm style shims through
/// `resolve`, and checks the version output with `recognize`.
pub(crate) fn detect(
    probe: &dyn HostProbe,
    names: &[&str],
    version_args: &[&str],
    recognize: fn(&str) -> bool,
    resolve: fn(&dyn HostProbe, PathBuf) -> Result<PathBuf, AdapterError>,
) -> Result<Installation, AdapterError> {
    let found = names
        .iter()
        .find_map(|name| probe.find_executable(name))
        .ok_or_else(|| {
            AdapterError::new(
                ErrorClass::NotInstalled,
                format!("{} not found on PATH", names[0]),
            )
        })?;
    let executable = resolve(probe, found)?;
    let output = probe.run(&executable, version_args, VERSION_TIMEOUT)?;
    let text = String::from_utf8_lossy(&output.stdout);
    let first = text.lines().next().unwrap_or("").trim().to_owned();
    if !output.exit.success || !recognize(&first) {
        return Err(AdapterError::new(
            ErrorClass::IncompatibleVersion,
            format!("unexpected version output from {}", executable.display()),
        ));
    }
    Ok(Installation::new(executable).with_version(first))
}

/// Accepts a direct native executable and refuses Windows batch shims, which
/// the PTY transport cannot host safely.
pub(crate) fn require_native(_: &dyn HostProbe, path: PathBuf) -> Result<PathBuf, AdapterError> {
    if is_script_shim(&path) {
        return Err(AdapterError::new(
            ErrorClass::Unsupported,
            format!(
                "{} is a script shim; a native executable is required",
                path.display()
            ),
        ));
    }
    Ok(path)
}

pub(crate) fn is_script_shim(path: &Path) -> bool {
    path.extension().is_some_and(|ext| {
        ["cmd", "bat", "ps1"]
            .iter()
            .any(|shim| ext.eq_ignore_ascii_case(shim))
    })
}

/// A PTY launch of the provider's own interactive terminal UI.
pub(crate) fn pty_plan(
    installation: &Installation,
    request: &LaunchRequest,
    args: Vec<OsString>,
) -> Result<LaunchPlan, AdapterError> {
    if request.transport != TransportKind::NativePty {
        return Err(AdapterError::new(
            ErrorClass::Unsupported,
            "this provider is launched through its native terminal UI",
        ));
    }
    Ok(LaunchPlan::new(
        installation.executable.clone(),
        request.workspace.clone(),
        TransportSpec::NativePty {
            initial_size: request.terminal_size.unwrap_or_default(),
        },
    )
    .args(args))
}

/// A prompt becomes a command line argument, so one that looks like a flag
/// could change how the provider runs.
pub(crate) fn checked_prompt(request: &LaunchRequest) -> Result<Option<&str>, AdapterError> {
    match request.initial_prompt.as_deref() {
        Some(prompt) if prompt.starts_with('-') => Err(AdapterError::new(
            ErrorClass::Configuration,
            "an initial prompt may not start with '-'",
        )),
        other => Ok(other),
    }
}

/// A random version 4 UUID for providers that accept a caller chosen session
/// id. Not cryptographic; it only needs to be unique.
pub(crate) fn new_uuid() -> String {
    let mut bytes = [0u8; 16];
    for half in bytes.chunks_mut(8) {
        half.copy_from_slice(&RandomState::new().build_hasher().finish().to_le_bytes());
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// First numeric `major.minor` token in a version line.
pub(crate) fn has_version_number(line: &str) -> bool {
    line.split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .any(|token| {
            let mut parts = token.split('.');
            matches!(
                (parts.next(), parts.next()),
                (Some(a), Some(b)) if !a.is_empty() && !b.is_empty()
                    && a.bytes().all(|c| c.is_ascii_digit())
                    && b.bytes().all(|c| c.is_ascii_digit())
            )
        })
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::{ffi::OsString, sync::Mutex};
    use uuldana_provider_sdk::{Platform, ProbeError, ProbeOutput};

    /// Host double with a fixed executable table and scripted probe output.
    pub struct FakeHost {
        pub executables: Vec<(String, PathBuf)>,
        pub version_output: Mutex<Option<Vec<u8>>>,
        pub platform: Platform,
    }

    impl FakeHost {
        pub fn new(executables: &[(&str, &str)], version: &str) -> Self {
            Self {
                executables: executables
                    .iter()
                    .map(|(n, p)| ((*n).to_owned(), PathBuf::from(p)))
                    .collect(),
                version_output: Mutex::new(Some(version.as_bytes().to_vec())),
                platform: Platform::current(),
            }
        }
    }

    impl HostProbe for FakeHost {
        fn find_executable(&self, name: &str) -> Option<PathBuf> {
            self.executables
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, p)| p.clone())
        }
        fn run(&self, _: &Path, _: &[&str], _: Duration) -> Result<ProbeOutput, ProbeError> {
            let stdout = self
                .version_output
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_default();
            Ok(ProbeOutput {
                exit: uuldana_provider_sdk::ExitInfo::from_code(0),
                stdout,
                stderr: Vec::new(),
            })
        }
        fn env_var(&self, _: &str) -> Option<OsString> {
            None
        }
        fn platform(&self) -> Platform {
            self.platform
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_has_v4_shape_and_differs() {
        let (a, b) = (new_uuid(), new_uuid());
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(a.as_bytes()[14], b'4');
        assert!(matches!(a.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));
    }

    #[test]
    fn version_number_detection() {
        assert!(has_version_number("2.1.310 (Some Tool)"));
        assert!(has_version_number("some-cli 0.164.0"));
        assert!(has_version_number("v1.4"));
        assert!(!has_version_number("command not found"));
        assert!(!has_version_number(""));
    }

    #[test]
    fn script_shims_are_recognized() {
        for shim in ["a.cmd", "a.BAT", "a.ps1"] {
            assert!(is_script_shim(Path::new(shim)));
        }
        assert!(!is_script_shim(Path::new("tool.exe")));
        assert!(!is_script_shim(Path::new("tool")));
    }
}
