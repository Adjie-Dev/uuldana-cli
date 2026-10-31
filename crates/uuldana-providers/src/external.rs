//! Adapters described by a manifest file, so a provider can be added without
//! writing or compiling Rust.
//!
//! A manifest says how to find the provider, which flags select which
//! permission behavior, and how to pass a prompt or resume a session. It is
//! data about an executable that Uuldana launches in its own terminal UI. It
//! never runs code of its own, and the same rules as for adapters learned
//! from a program's help apply: only what the manifest states is claimed, and a policy that no
//! listed mode can honor is refused.
//!
//! ```json
//! {
//!   "version": 1,
//!   "id": "my-agent",
//!   "name": "My Agent",
//!   "executables": ["my-agent"],
//!   "version_args": ["--version"],
//!   "version_contains": "my-agent",
//!   "modes": [
//!     { "approval": "never_ask", "filesystem": "unrestricted", "network": "allowed",
//!       "args": ["--yes"] },
//!     { "approval": "ask_every_action", "filesystem": "unrestricted", "network": "allowed",
//!       "args": [] }
//!   ],
//!   "prompt_args": ["--prompt", "{prompt}"],
//!   "resume_args": ["--resume", "{session}"],
//!   "session_id_args": ["--session-id", "{session}"],
//!   "interrupts": 2,
//!   "grace_seconds": 30,
//!   "limit_phrases": ["quota exhausted"]
//! }
//! ```
//!
//! `modes` is ordered from the most to the least permissive. `prompt_args`,
//! `resume_args`, `continue_args` and `session_id_args` are optional; leaving
//! one out means the provider does not support that feature. `{prompt}` and
//! `{session}` are the only placeholders. `leading_args` come first on every
//! launch, for a provider that needs a subcommand before anything else.
//! `continue_args` continue the latest conversation of the folder, for a
//! provider that can do that without a session id.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::Value;
use uuldana_provider_sdk::{
    AdapterError, ApprovalMode, Capability, CapabilitySet, ErrorClass, EventDecoder, Evidence,
    FilesystemAccess, HostProbe, Installation, LaunchPlan, LaunchRequest, NativeSessionBinding,
    NativeSessionId, NetworkAccess, PermissionMapping, PermissionPolicy, ProviderAdapter,
    ProviderDescriptor, ProviderId, ResumeRequest, SandboxPolicy, ShutdownPlan, ShutdownStep,
    Support, TransportKind,
};

use crate::{
    common::{checked_prompt, detect, has_version_number, new_uuid, pty_plan, require_native},
    limits::PhraseLimitDecoder,
};

/// How long a program gets to leave after it was asked to, when a manifest
/// does not say.
const DEFAULT_GRACE_SECONDS: u64 = 5;

struct ModeSpec {
    effective: PermissionPolicy,
    args: Vec<String>,
    note: Option<String>,
}

/// A provider described by a manifest.
pub struct ExternalAdapter {
    descriptor: ProviderDescriptor,
    executables: Vec<String>,
    version_args: Vec<String>,
    version_contains: Option<String>,
    modes: Vec<ModeSpec>,
    leading_args: Vec<String>,
    prompt_args: Option<Vec<String>>,
    resume_args: Option<Vec<String>>,
    continue_args: Option<Vec<String>>,
    session_id_args: Option<Vec<String>>,
    interrupts: usize,
    grace: Duration,
    limit_phrases: Vec<String>,
}

fn strings(value: &Value, key: &str) -> Result<Option<Vec<String>>, String> {
    let Some(raw) = value.get(key) else {
        return Ok(None);
    };
    let list = raw
        .as_array()
        .ok_or_else(|| format!("\"{key}\" must be a list of text"))?;
    list.iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("\"{key}\" must be a list of text"))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

fn parse_mode(value: &Value, index: usize) -> Result<ModeSpec, String> {
    let text = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("modes[{index}].{key} must be text"))
    };
    let approval = match text("approval")? {
        "ask_every_action" => ApprovalMode::AskEveryAction,
        "ask_for_risky" => ApprovalMode::AskForRisky,
        "never_ask" => ApprovalMode::NeverAsk,
        other => return Err(format!("modes[{index}].approval: unknown value {other:?}")),
    };
    let filesystem = match text("filesystem")? {
        "read_only" => FilesystemAccess::ReadOnly,
        "workspace_write" => FilesystemAccess::WorkspaceWrite,
        "unrestricted" => FilesystemAccess::Unrestricted,
        other => {
            return Err(format!(
                "modes[{index}].filesystem: unknown value {other:?}"
            ));
        }
    };
    let network = match text("network")? {
        "denied" => NetworkAccess::Denied,
        "allowed" => NetworkAccess::Allowed,
        other => return Err(format!("modes[{index}].network: unknown value {other:?}")),
    };
    Ok(ModeSpec {
        effective: PermissionPolicy {
            approval,
            sandbox: SandboxPolicy {
                filesystem,
                network,
            },
        },
        args: strings(value, "args")?.unwrap_or_default(),
        note: value.get("note").and_then(Value::as_str).map(str::to_owned),
    })
}

impl ExternalAdapter {
    /// Parses a manifest.
    pub fn from_manifest(text: &str) -> Result<Self, String> {
        let root: Value = serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}"))?;
        Self::from_value(&root, true)
    }

    /// Builds an adapter from a manifest value. `ordered` makes the listing
    /// order a rule (most to least permissive, each mode inside the one before
    /// it); a profile learned from a program's own help lists modes that
    /// cannot be put in one line, and the best one is picked by what fits.
    pub(crate) fn from_value(root: &Value, ordered: bool) -> Result<Self, String> {
        let root = root.clone();
        if root.get("version").and_then(Value::as_u64) != Some(1) {
            return Err("\"version\" must be 1".into());
        }
        let id = root
            .get("id")
            .and_then(Value::as_str)
            .ok_or("\"id\" must be text")?;
        let id = ProviderId::new(id).map_err(|e| format!("\"id\": {e}"))?;
        let name = root
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(id.as_str())
            .to_owned();
        let executables = strings(&root, "executables")?.unwrap_or_default();
        if executables.is_empty() {
            return Err("\"executables\" must name at least one executable".into());
        }
        let modes = root
            .get("modes")
            .and_then(Value::as_array)
            .ok_or("\"modes\" must be a list")?
            .iter()
            .enumerate()
            .map(|(i, m)| parse_mode(m, i))
            .collect::<Result<Vec<_>, _>>()?;
        if modes.is_empty() {
            return Err("\"modes\" must list at least one mode".into());
        }
        for pair in modes.windows(2) {
            if ordered && !pair[1].effective.is_within(&pair[0].effective) {
                return Err("\"modes\" must be ordered from most to least permissive".into());
            }
        }
        let prompt_args = strings(&root, "prompt_args")?;
        if let Some(args) = &prompt_args
            && !args.iter().any(|a| a.contains("{prompt}"))
        {
            return Err("\"prompt_args\" must contain {prompt}".into());
        }
        let resume_args = strings(&root, "resume_args")?;
        if let Some(args) = &resume_args
            && !args.iter().any(|a| a.contains("{session}"))
        {
            return Err("\"resume_args\" must contain {session}".into());
        }
        let leading_args = strings(&root, "leading_args")?.unwrap_or_default();
        let continue_args = strings(&root, "continue_args")?;
        let session_id_args = strings(&root, "session_id_args")?;
        if let Some(args) = &session_id_args
            && !args.iter().any(|a| a.contains("{session}"))
        {
            return Err("\"session_id_args\" must contain {session}".into());
        }
        let interrupts = match root.get("interrupts") {
            None => 1,
            Some(v) => match v.as_u64() {
                Some(n @ 1..=3) => n as usize,
                _ => return Err("\"interrupts\" must be 1, 2 or 3".into()),
            },
        };
        let grace = match root.get("grace_seconds") {
            None => Duration::from_secs(DEFAULT_GRACE_SECONDS),
            Some(v) => match v.as_u64() {
                Some(seconds @ 1..=300) => Duration::from_secs(seconds),
                _ => return Err("\"grace_seconds\" must be between 1 and 300".into()),
            },
        };
        Ok(Self {
            descriptor: ProviderDescriptor::new(id, name, vec![TransportKind::NativePty]),
            executables,
            version_args: strings(&root, "version_args")?
                .unwrap_or_else(|| vec!["--version".into()]),
            version_contains: root
                .get("version_contains")
                .and_then(Value::as_str)
                .map(str::to_owned),
            modes,
            leading_args,
            prompt_args,
            resume_args,
            continue_args,
            session_id_args,
            interrupts,
            grace,
            limit_phrases: strings(&root, "limit_phrases")?.unwrap_or_default(),
        })
    }

    fn mode(&self, requested: &PermissionPolicy) -> &ModeSpec {
        self.modes
            .iter()
            .find(|m| m.effective.is_within(requested))
            .unwrap_or_else(|| self.modes.last().expect("at least one mode"))
    }

    fn substitute(template: &[String], key: &str, value: &str) -> Vec<OsString> {
        template
            .iter()
            .map(|item| OsString::from(item.replace(key, value)))
            .collect()
    }

    fn args(
        &self,
        request: &LaunchRequest,
        leading: Vec<OsString>,
    ) -> Result<Vec<OsString>, AdapterError> {
        let mut args: Vec<OsString> = self.leading_args.iter().map(OsString::from).collect();
        args.extend(leading);
        args.extend(
            self.mode(request.permissions.effective())
                .args
                .iter()
                .map(OsString::from),
        );
        if let Some(prompt) = checked_prompt(request)? {
            let Some(template) = &self.prompt_args else {
                return Err(AdapterError::unsupported(Capability::InitialPrompt));
            };
            args.extend(Self::substitute(template, "{prompt}", prompt));
        }
        Ok(args)
    }
}

impl ProviderAdapter for ExternalAdapter {
    fn descriptor(&self) -> &ProviderDescriptor {
        &self.descriptor
    }

    fn detect(&self, probe: &dyn HostProbe) -> Result<Installation, AdapterError> {
        let names: Vec<&str> = self.executables.iter().map(String::as_str).collect();
        let version_args: Vec<&str> = self.version_args.iter().map(String::as_str).collect();
        // The manifest's recognition text, or else any version number.
        let needle = self.version_contains.clone();
        let found = match &needle {
            Some(_) => detect(
                probe,
                &names,
                &version_args,
                |_| true,
                |probe, path: PathBuf| require_native(probe, path),
            ),
            None => detect(
                probe,
                &names,
                &version_args,
                has_version_number,
                |probe, path: PathBuf| require_native(probe, path),
            ),
        }?;
        if let Some(needle) = needle
            && !found
                .version
                .as_deref()
                .unwrap_or("")
                .to_lowercase()
                .contains(&needle.to_lowercase())
        {
            return Err(AdapterError::new(
                ErrorClass::IncompatibleVersion,
                format!(
                    "unexpected version output from {}",
                    found.executable.display()
                ),
            ));
        }
        Ok(found)
    }

    fn capabilities(&self, _: &Installation) -> CapabilitySet {
        let flag = |present: bool| {
            if present {
                Support::Supported
            } else {
                Support::Unsupported
            }
        };
        CapabilitySet::new()
            .with(
                Capability::NativeTerminalUi,
                Support::Supported,
                Evidence::Declared,
            )
            .with(
                Capability::InitialPrompt,
                flag(self.prompt_args.is_some()),
                Evidence::Declared,
            )
            .with(
                Capability::ResumeById,
                flag(self.resume_args.is_some()),
                Evidence::Declared,
            )
            .with(
                Capability::ResumeLatest,
                flag(self.continue_args.is_some()),
                Evidence::Declared,
            )
            .with(
                Capability::PreassignSessionId,
                flag(self.session_id_args.is_some()),
                Evidence::Declared,
            )
    }

    fn map_permissions(&self, _: &Installation, requested: &PermissionPolicy) -> PermissionMapping {
        let mode = self.mode(requested);
        PermissionMapping::new(mode.effective).with_note(
            mode.note
                .clone()
                .unwrap_or_else(|| "permission behavior comes from the provider manifest".into()),
        )
    }

    fn launch_plan(
        &self,
        installation: &Installation,
        request: &LaunchRequest,
    ) -> Result<LaunchPlan, AdapterError> {
        let mut leading = Vec::new();
        let mut binding = NativeSessionBinding::Unavailable;
        if request.continue_latest {
            let Some(template) = &self.continue_args else {
                return Err(AdapterError::unsupported(Capability::ResumeLatest));
            };
            let args = self.args(request, template.iter().map(OsString::from).collect())?;
            return pty_plan(installation, request, args);
        }
        if let Some(template) = &self.session_id_args {
            let id = new_uuid();
            leading = Self::substitute(template, "{session}", &id);
            binding = NativeSessionBinding::Preassigned(
                NativeSessionId::new(id).expect("uuid is a valid id"),
            );
        }
        let mut plan = pty_plan(installation, request, self.args(request, leading)?)?;
        plan.native_session = binding;
        Ok(plan)
    }

    fn resume_plan(
        &self,
        installation: &Installation,
        request: &ResumeRequest,
    ) -> Result<LaunchPlan, AdapterError> {
        let Some(template) = &self.resume_args else {
            return Err(AdapterError::unsupported(Capability::ResumeById));
        };
        let leading = Self::substitute(template, "{session}", request.native.as_str());
        pty_plan(
            installation,
            &request.launch,
            self.args(&request.launch, leading)?,
        )
    }

    fn event_decoder(&self, transport: TransportKind) -> Option<Box<dyn EventDecoder>> {
        (transport == TransportKind::NativePty && !self.limit_phrases.is_empty()).then(|| {
            Box::new(PhraseLimitDecoder::new(&self.limit_phrases)) as Box<dyn EventDecoder>
        })
    }

    fn shutdown_plan(&self, _: TransportKind) -> ShutdownPlan {
        ShutdownPlan {
            steps: vec![ShutdownStep::Interrupt; self.interrupts],
            grace: self.grace,
        }
    }
}

/// Loads every `*.json` manifest in `dir`, in file name order. Each file gives
/// an adapter or an error that names the file, so one bad manifest never hides
/// the others.
pub fn load_dir(dir: &Path) -> Vec<Result<ExternalAdapter, String>> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();
    files
        .into_iter()
        .map(|path| {
            let name = path
                .file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            std::fs::read_to_string(&path)
                .map_err(|e| format!("{name}: {e}"))
                .and_then(|text| {
                    ExternalAdapter::from_manifest(&text).map_err(|e| format!("{name}: {e}"))
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testing::FakeHost;
    use uuldana_provider_sdk::{
        AgentEvent, DecodedEvent, LimitKind, TransportFrame, validate_mapping,
    };

    const MANIFEST: &str = r#"{
        "version": 1, "id": "my-agent", "name": "My Agent",
        "executables": ["my-agent"], "version_contains": "my-agent",
        "modes": [
          {"approval":"never_ask","filesystem":"unrestricted","network":"allowed","args":["--yes"]},
          {"approval":"ask_every_action","filesystem":"workspace_write","network":"allowed","args":["--safe"]},
          {"approval":"ask_every_action","filesystem":"read_only","network":"denied","args":["--ro","--offline"]}
        ],
        "prompt_args": ["--prompt", "{prompt}"],
        "resume_args": ["--resume", "{session}"],
        "session_id_args": ["--session-id", "{session}"],
        "interrupts": 2,
        "limit_phrases": ["Quota Exhausted"]
    }"#;

    fn adapter() -> ExternalAdapter {
        ExternalAdapter::from_manifest(MANIFEST).unwrap()
    }

    fn request(policy: PermissionPolicy, prompt: Option<&str>) -> LaunchRequest {
        let installation = Installation::new(PathBuf::from("my-agent"));
        let validated =
            validate_mapping(&policy, &adapter().map_permissions(&installation, &policy)).unwrap();
        let mut request = LaunchRequest::new(
            std::env::current_dir().unwrap(),
            TransportKind::NativePty,
            validated,
        );
        request.initial_prompt = prompt.map(str::to_owned);
        request
    }

    fn strings(plan: &LaunchPlan) -> Vec<String> {
        plan.args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn the_strongest_listed_mode_within_the_request_is_chosen() {
        let installation = Installation::new(PathBuf::from("my-agent"));
        let plan = |policy| {
            adapter()
                .launch_plan(&installation, &request(policy, None))
                .unwrap()
        };
        let all = plan(PermissionPolicy::unrestricted());
        assert!(strings(&all).contains(&"--yes".to_owned()));
        let workspace = plan(PermissionPolicy {
            approval: ApprovalMode::AskEveryAction,
            sandbox: SandboxPolicy {
                filesystem: FilesystemAccess::WorkspaceWrite,
                network: NetworkAccess::Allowed,
            },
        });
        assert!(strings(&workspace).contains(&"--safe".to_owned()));
        let strict = plan(PermissionPolicy::default());
        assert!(strings(&strict).contains(&"--offline".to_owned()));
    }

    #[test]
    fn prompt_session_and_resume_follow_the_manifest() {
        let installation = Installation::new(PathBuf::from("my-agent"));
        let policy = PermissionPolicy::unrestricted();
        let plan = adapter()
            .launch_plan(&installation, &request(policy, Some("jelaskan modul auth")))
            .unwrap();
        let args = strings(&plan);
        let NativeSessionBinding::Preassigned(id) = &plan.native_session else {
            panic!("session must be preassigned")
        };
        assert_eq!(&args[..2], ["--session-id", id.as_str()]);
        assert_eq!(&args[args.len() - 2..], ["--prompt", "jelaskan modul auth"]);
        let native = NativeSessionId::new("abc-1").unwrap();
        let plan = adapter()
            .resume_plan(
                &installation,
                &ResumeRequest::new(request(policy, None), native),
            )
            .unwrap();
        assert_eq!(&strings(&plan)[..2], ["--resume", "abc-1"]);
    }

    #[test]
    fn features_the_manifest_leaves_out_are_unsupported_not_guessed() {
        let bare = ExternalAdapter::from_manifest(
            r#"{"version":1,"id":"bare","executables":["bare"],
                "modes":[{"approval":"ask_every_action","filesystem":"unrestricted","network":"allowed"}]}"#,
        )
        .unwrap();
        let installation = Installation::new(PathBuf::from("bare"));
        let caps = bare.capabilities(&installation);
        assert!(caps.require(&Capability::InitialPrompt).is_err());
        assert!(caps.require(&Capability::ResumeById).is_err());
        let policy = PermissionPolicy {
            approval: ApprovalMode::AskEveryAction,
            sandbox: SandboxPolicy {
                filesystem: FilesystemAccess::Unrestricted,
                network: NetworkAccess::Allowed,
            },
        };
        let validated =
            validate_mapping(&policy, &bare.map_permissions(&installation, &policy)).unwrap();
        let mut launch = LaunchRequest::new(
            std::env::current_dir().unwrap(),
            TransportKind::NativePty,
            validated,
        );
        launch.initial_prompt = Some("halo".into());
        assert!(bare.launch_plan(&installation, &launch).is_err());
        assert!(
            bare.resume_plan(
                &installation,
                &ResumeRequest::new(launch, NativeSessionId::new("x").unwrap())
            )
            .is_err()
        );
    }

    #[test]
    fn a_policy_no_listed_mode_can_honor_is_refused() {
        let only_open = ExternalAdapter::from_manifest(
            r#"{"version":1,"id":"open","executables":["open"],
                "modes":[{"approval":"never_ask","filesystem":"unrestricted","network":"allowed"}]}"#,
        )
        .unwrap();
        let installation = Installation::new(PathBuf::from("open"));
        let requested = PermissionPolicy::default();
        let mapped = only_open.map_permissions(&installation, &requested);
        assert!(validate_mapping(&requested, &mapped).is_err());
    }

    #[test]
    fn manifests_are_validated_with_precise_messages() {
        for (text, needle) in [
            ("nope", "not valid JSON"),
            (r#"{"version":2}"#, "\"version\" must be 1"),
            (r#"{"version":1,"id":"Bad Id"}"#, "\"id\""),
            (r#"{"version":1,"id":"a"}"#, "executables"),
            (r#"{"version":1,"id":"a","executables":["a"]}"#, "\"modes\""),
            (
                r#"{"version":1,"id":"a","executables":["a"],"modes":[
                    {"approval":"ask_every_action","filesystem":"read_only","network":"denied"},
                    {"approval":"never_ask","filesystem":"unrestricted","network":"allowed"}]}"#,
                "ordered from most to least permissive",
            ),
            (
                r#"{"version":1,"id":"a","executables":["a"],"prompt_args":["--p"],
                    "modes":[{"approval":"never_ask","filesystem":"unrestricted","network":"allowed"}]}"#,
                "{prompt}",
            ),
            (
                r#"{"version":1,"id":"a","executables":["a"],"interrupts":9,
                    "modes":[{"approval":"never_ask","filesystem":"unrestricted","network":"allowed"}]}"#,
                "interrupts",
            ),
            (
                r#"{"version":1,"id":"a","executables":["a"],
                    "modes":[{"approval":"sometimes","filesystem":"unrestricted","network":"allowed"}]}"#,
                "modes[0].approval",
            ),
        ] {
            let error = ExternalAdapter::from_manifest(text)
                .err()
                .unwrap_or_default();
            assert!(error.contains(needle), "{text}: {error}");
        }
    }

    #[test]
    fn detection_checks_the_declared_text() {
        let host = FakeHost::new(&[("my-agent", "C:/bin/my-agent.exe")], "My-Agent 3.1\n");
        assert!(adapter().detect(&host).is_ok());
        let host = FakeHost::new(&[("my-agent", "C:/bin/my-agent.exe")], "other 3.1\n");
        assert!(adapter().detect(&host).is_err());
        let host = FakeHost::new(&[], "");
        assert!(adapter().detect(&host).is_err());
    }

    #[test]
    fn manifest_limit_phrases_become_inferred_quota_events() {
        let mut decoder = adapter().event_decoder(TransportKind::NativePty).unwrap();
        let mut out: Vec<DecodedEvent> = Vec::new();
        decoder.decode(TransportFrame::Output(b"all good so far"), &mut out);
        assert!(out.is_empty());
        decoder.decode(
            TransportFrame::Output(b"\x1b[1mQUOTA\x1b[0m exhausted for today"),
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert!(matches!(
            &out[0].event,
            AgentEvent::LimitReached(s) if s.kind == LimitKind::UsageQuota
        ));
        assert_eq!(out[0].fidelity, uuldana_provider_sdk::Fidelity::Inferred);
    }

    #[test]
    fn shutdown_uses_the_declared_number_of_interrupts() {
        let plan = adapter().shutdown_plan(TransportKind::NativePty);
        assert_eq!(plan.steps, vec![ShutdownStep::Interrupt; 2]);
    }

    #[test]
    fn the_time_a_program_gets_to_leave_is_declared_or_defaults() {
        let plan = adapter().shutdown_plan(TransportKind::NativePty);
        assert_eq!(plan.grace, Duration::from_secs(5));
        let slow = ExternalAdapter::from_manifest(
            r#"{"version":1,"id":"slow","executables":["slow"],"grace_seconds":30,
                "modes":[{"approval":"never_ask","filesystem":"unrestricted","network":"allowed"}]}"#,
        )
        .unwrap();
        assert_eq!(
            slow.shutdown_plan(TransportKind::NativePty).grace,
            Duration::from_secs(30)
        );
        for bad in ["0", "301", "\"x\""] {
            let text = format!(
                r#"{{"version":1,"id":"a","executables":["a"],"grace_seconds":{bad},
                    "modes":[{{"approval":"never_ask","filesystem":"unrestricted","network":"allowed"}}]}}"#
            );
            let error = ExternalAdapter::from_manifest(&text)
                .err()
                .unwrap_or_default();
            assert!(error.contains("grace_seconds"), "{bad}: {error}");
        }
    }

    #[test]
    fn loading_a_directory_reports_each_file_separately() {
        let dir = std::env::temp_dir().join(format!("uuldana-adapters-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a-good.json"), MANIFEST).unwrap();
        std::fs::write(dir.join("b-bad.json"), "{").unwrap();
        std::fs::write(dir.join("notes.txt"), "ignored").unwrap();
        let loaded = load_dir(&dir);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(loaded[0].is_ok());
        assert!(loaded[1].as_ref().err().unwrap().starts_with("b-bad.json:"));
        assert!(load_dir(&dir).is_empty());
    }
}
