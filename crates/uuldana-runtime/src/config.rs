//! User configuration: which providers and accounts Uuldana may use, in which
//! order, and the permission ceiling of every session.
//!
//! The file is optional. Without it, or without a `providers` list, Uuldana
//! uses the agents it finds installed, under the automatic permission ceiling
//! (see [`automatic_ceiling`]). Priority is the order of `providers`,
//! chosen by the user; agents that are installed but not listed follow after
//! them unless `auto_detect` is `false`. Nothing here is hardcoded. An entry may carry extra environment variables for its child
//! process, which is how one provider is used with several accounts (for
//! example a different configuration directory per account). Uuldana never
//! reads or copies the credentials behind such a variable.
//!
//! ```json
//! {
//!   "version": 1,
//!   "permissions": {
//!     "approval": "ask_for_risky",
//!     "filesystem": "workspace_write",
//!     "network": "allowed"
//!   },
//!   "providers": [
//!     { "id": "agent-a", "label": "account-work", "cooldown_minutes": 300 },
//!     { "id": "agent-b" }
//!   ],
//!   "cooldown_minutes": 300,
//!   "failover": true
//! }
//! ```

use std::{
    ffi::OsString,
    fmt,
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::Value;
use uuldana_core::{
    ApprovalMode, FilesystemAccess, NetworkAccess, PermissionPolicy, ProviderId, SandboxPolicy,
};

/// One provider account Uuldana may run, in priority order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderEntry {
    /// Registered adapter id.
    pub id: ProviderId,
    /// Unique name of this entry, used in history and messages.
    pub label: String,
    /// Extra environment for the child process (account selection).
    pub env: Vec<(OsString, OsString)>,
    /// How long this entry rests after its usage limit, when the provider does
    /// not say when the limit resets.
    pub cooldown: Duration,
    /// Found installed on this machine, not written in the configuration file.
    pub auto: bool,
}

/// Parsed configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Ceiling for every session. Failover never exceeds it.
    pub ceiling: PermissionPolicy,
    /// Providers and accounts, first entry has the highest priority.
    pub providers: Vec<ProviderEntry>,
    /// Whether to continue on the next entry when one runs out of quota.
    pub failover: bool,
    /// Whether installed agents that `providers` does not list are used after
    /// the listed ones.
    pub auto_detect: bool,
}

/// Default rest after a usage limit: the five hour window most plans use.
pub const DEFAULT_COOLDOWN_MINUTES: u64 = 300;

/// The ceiling used when the configuration has no `permissions` at all, or
/// there is no configuration file: the agents ask before risky actions, and
/// Uuldana adds no filesystem or network restriction of its own. It is what
/// running an agent by hand does, so Uuldana works without setup. No agent can
/// enforce "no network" with a flag, so the strictest policy would start none of
/// them. A user who wants a tighter ceiling writes one.
pub const fn automatic_ceiling() -> PermissionPolicy {
    PermissionPolicy {
        approval: ApprovalMode::AskForRisky,
        sandbox: SandboxPolicy {
            filesystem: FilesystemAccess::Unrestricted,
            network: NetworkAccess::Allowed,
        },
    }
}

/// Why a configuration could not be used.
#[derive(Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// The file could not be read.
    Read(String),
    /// The text is not valid JSON.
    Json(String),
    /// A field is missing, of the wrong type or has an unknown value.
    Invalid(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(m) => write!(f, "cannot read configuration: {m}"),
            Self::Json(m) => write!(f, "configuration is not valid JSON: {m}"),
            Self::Invalid(m) => write!(f, "invalid configuration: {m}"),
        }
    }
}

impl std::error::Error for ConfigError {}

fn invalid<T>(message: impl Into<String>) -> Result<T, ConfigError> {
    Err(ConfigError::Invalid(message.into()))
}

impl Config {
    /// Parses configuration text.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let root: Value =
            serde_json::from_str(text).map_err(|e| ConfigError::Json(e.to_string()))?;
        let Some(root) = root.as_object() else {
            return invalid("the top level must be an object");
        };
        match root.get("version").and_then(Value::as_u64) {
            Some(1) => {}
            Some(other) => return invalid(format!("unsupported version {other}")),
            None => return invalid("\"version\" must be 1"),
        }
        let ceiling = match root.get("permissions") {
            None => automatic_ceiling(),
            Some(value) => parse_permissions(value)?,
        };
        let default_cooldown = match root.get("cooldown_minutes") {
            None => DEFAULT_COOLDOWN_MINUTES,
            Some(value) => match value.as_u64() {
                Some(minutes) if minutes > 0 => minutes,
                _ => return invalid("\"cooldown_minutes\" must be a positive whole number"),
            },
        };
        let failover = match root.get("failover") {
            None => true,
            Some(Value::Bool(enabled)) => *enabled,
            Some(_) => return invalid("\"failover\" must be true or false"),
        };
        let auto_detect = match root.get("auto_detect") {
            None => true,
            Some(Value::Bool(enabled)) => *enabled,
            Some(_) => return invalid("\"auto_detect\" must be true or false"),
        };
        let empty = Vec::new();
        let list = match root.get("providers") {
            None if auto_detect => &empty,
            None => return invalid("\"providers\" must be a list"),
            Some(value) => match value.as_array() {
                Some(list) => list,
                None => return invalid("\"providers\" must be a list"),
            },
        };
        if list.is_empty() && !auto_detect {
            return invalid(
                "\"providers\" must name at least one provider when \"auto_detect\" is false",
            );
        }
        let mut providers: Vec<ProviderEntry> = Vec::new();
        for (index, item) in list.iter().enumerate() {
            let entry = parse_entry(item, index, default_cooldown)?;
            if providers.iter().any(|p| p.label == entry.label) {
                return invalid(format!(
                    "providers[{index}]: label {:?} is used twice; give each account its own \"label\"",
                    entry.label
                ));
            }
            providers.push(entry);
        }
        Ok(Self {
            ceiling,
            providers,
            failover,
            auto_detect,
        })
    }

    /// What is used when there is no configuration file: the automatic
    /// ceiling, failover on, and whatever agents are installed.
    pub fn automatic() -> Self {
        Self {
            ceiling: automatic_ceiling(),
            providers: Vec::new(),
            failover: true,
            auto_detect: true,
        }
    }

    /// Reads and parses a configuration file.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::Read(format!("{}: {e}", path.display())))?;
        Self::parse(&text)
    }
}

/// Why the order could not be written back to the configuration file.
#[derive(Debug)]
pub struct SaveError(String);

impl SaveError {
    /// A save failure with this reason.
    pub fn new(reason: impl Into<String>) -> Self {
        Self(reason.into())
    }
}

impl fmt::Display for SaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SaveError {}

/// The label an entry of the file has: its own `label`, else its `id`.
fn file_label(item: &Value) -> Option<&str> {
    let object = item.as_object()?;
    object
        .get("label")
        .and_then(Value::as_str)
        .or_else(|| object.get("id").and_then(Value::as_str))
}

/// Puts the entries of the file's `providers` list into `order`, first label
/// first, and adds `added` (provider ids with no entry yet) where `order`
/// names them. Entries `order` does not name keep their relative order after
/// the named ones. Every other field of the file, and every field of every
/// entry, is kept as it is.
///
/// Returns the new text, or an error when the file has no usable `providers`
/// list. The text is not written; see [`save_order`].
pub fn reorder_text(text: &str, order: &[String], added: &[String]) -> Result<String, SaveError> {
    let mut root: Value = serde_json::from_str(text)
        .map_err(|e| SaveError(format!("the file is not valid JSON: {e}")))?;
    let Some(list) = root.get_mut("providers").and_then(Value::as_array_mut) else {
        return Err(SaveError("the file has no \"providers\" list".into()));
    };
    for id in added {
        if !list
            .iter()
            .any(|item| file_label(item) == Some(id.as_str()))
        {
            list.push(serde_json::json!({ "id": id }));
        }
    }
    let mut ordered: Vec<Value> = Vec::with_capacity(list.len());
    for label in order {
        if let Some(at) = list
            .iter()
            .position(|item| file_label(item) == Some(label.as_str()))
        {
            ordered.push(list.remove(at));
        }
    }
    ordered.append(list);
    *list = ordered;
    let mut out = serde_json::to_string_pretty(&root)
        .map_err(|e| SaveError(format!("cannot write the file: {e}")))?;
    out.push('\n');
    Ok(out)
}

/// Rewrites the configuration file at `path` with `order` (see
/// [`reorder_text`]). The file is replaced through a temporary file next to it,
/// so an interrupted write cannot leave half a configuration behind.
pub fn save_order(path: &Path, order: &[String], added: &[String]) -> Result<(), SaveError> {
    // Without a file yet, the order starts one. Only what is named is written;
    // the rest keeps being found by itself.
    let text = if path.exists() {
        std::fs::read_to_string(path)
            .map_err(|e| SaveError(format!("cannot read {}: {e}", path.display())))?
    } else {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| SaveError(format!("cannot create {}: {e}", parent.display())))?;
        }
        "{\"version\": 1, \"providers\": []}".to_owned()
    };
    let updated = reorder_text(&text, order, added)?;
    let temporary = path.with_extension("json.tmp");
    // A configuration can carry environment values for an account, so the
    // replacement is for its owner alone on Unix.
    crate::journal::write_private(&temporary, updated.as_bytes())
        .map_err(|e| SaveError(format!("cannot write {}: {e}", temporary.display())))?;
    std::fs::rename(&temporary, path).map_err(|e| {
        let _ = std::fs::remove_file(&temporary);
        SaveError(format!("cannot replace {}: {e}", path.display()))
    })
}

fn parse_entry(
    item: &Value,
    index: usize,
    default_cooldown: u64,
) -> Result<ProviderEntry, ConfigError> {
    let Some(object) = item.as_object() else {
        return invalid(format!("providers[{index}] must be an object"));
    };
    let Some(id) = object.get("id").and_then(Value::as_str) else {
        return invalid(format!("providers[{index}].id must be text"));
    };
    let id = ProviderId::new(id)
        .map_err(|e| ConfigError::Invalid(format!("providers[{index}].id: {e}")))?;
    let label = match object.get("label") {
        None => id.as_str().to_owned(),
        Some(Value::String(label)) if !label.trim().is_empty() => label.clone(),
        Some(_) => return invalid(format!("providers[{index}].label must be non-empty text")),
    };
    let mut env = Vec::new();
    if let Some(value) = object.get("env") {
        let Some(map) = value.as_object() else {
            return invalid(format!("providers[{index}].env must be an object"));
        };
        for (key, value) in map {
            let Some(value) = value.as_str() else {
                return invalid(format!("providers[{index}].env.{key} must be text"));
            };
            if key.is_empty() || key.contains(['=', '\0']) || value.contains('\0') {
                return invalid(format!(
                    "providers[{index}].env.{key} is not a valid variable"
                ));
            }
            env.push((OsString::from(key), OsString::from(value)));
        }
    }
    let minutes = match object.get("cooldown_minutes") {
        None => default_cooldown,
        Some(value) => match value.as_u64() {
            Some(minutes) if minutes > 0 => minutes,
            _ => {
                return invalid(format!(
                    "providers[{index}].cooldown_minutes must be a positive whole number"
                ));
            }
        },
    };
    Ok(ProviderEntry {
        id,
        label,
        env,
        cooldown: Duration::from_secs(minutes * 60),
        auto: false,
    })
}

fn parse_permissions(value: &Value) -> Result<PermissionPolicy, ConfigError> {
    let Some(object) = value.as_object() else {
        return invalid("\"permissions\" must be an object");
    };
    let strict = PermissionPolicy::default();
    let pick = |key: &str| object.get(key).map(|v| v.as_str());
    let approval = match pick("approval") {
        None => strict.approval,
        Some(Some("ask_every_action")) => ApprovalMode::AskEveryAction,
        Some(Some("ask_for_risky")) => ApprovalMode::AskForRisky,
        Some(Some("never_ask")) => ApprovalMode::NeverAsk,
        Some(_) => {
            return invalid(
                "permissions.approval must be ask_every_action, ask_for_risky or never_ask",
            );
        }
    };
    let filesystem = match pick("filesystem") {
        None => strict.sandbox.filesystem,
        Some(Some("read_only")) => FilesystemAccess::ReadOnly,
        Some(Some("workspace_write")) => FilesystemAccess::WorkspaceWrite,
        Some(Some("unrestricted")) => FilesystemAccess::Unrestricted,
        Some(_) => {
            return invalid(
                "permissions.filesystem must be read_only, workspace_write or unrestricted",
            );
        }
    };
    let network = match pick("network") {
        None => strict.sandbox.network,
        Some(Some("denied")) => NetworkAccess::Denied,
        Some(Some("allowed")) => NetworkAccess::Allowed,
        Some(_) => return invalid("permissions.network must be denied or allowed"),
    };
    Ok(PermissionPolicy {
        approval,
        sandbox: SandboxPolicy {
            filesystem,
            network,
        },
    })
}

/// Where the configuration lives: `UULDANA_CONFIG` when set, otherwise
/// `%APPDATA%\uuldana\config.json` on Windows and
/// `$XDG_CONFIG_HOME/uuldana/config.json` (or `~/.config/...`) elsewhere.
pub fn default_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("UULDANA_CONFIG") {
        return Some(PathBuf::from(path));
    }
    let base = if cfg!(windows) {
        PathBuf::from(std::env::var_os("APPDATA")?)
    } else if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(xdg)
    } else {
        PathBuf::from(std::env::var_os("HOME")?).join(".config")
    };
    Some(base.join("uuldana").join("config.json"))
}

/// Directory of provider manifests, next to the configuration file:
/// `adapters/` beside `config.json`.
pub fn adapters_dir() -> Option<PathBuf> {
    Some(default_path()?.parent()?.join("adapters"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_priority_order_accounts_and_ceiling() {
        let config = Config::parse(
            r#"{
              "version": 1,
              "permissions": {"approval": "ask_for_risky", "filesystem": "workspace_write", "network": "allowed"},
              "providers": [
                {"id": "agent-a", "label": "account-work", "env": {"ACCOUNT_DIR": "D:\\akun\\kantor"}},
                {"id": "agent-a", "label": "account-home"},
                {"id": "agent-b"}
              ]
            }"#,
        )
        .unwrap();
        let labels: Vec<_> = config.providers.iter().map(|p| p.label.as_str()).collect();
        assert_eq!(labels, ["account-work", "account-home", "agent-b"]);
        assert_eq!(config.providers[0].env.len(), 1);
        assert_eq!(config.ceiling.approval, ApprovalMode::AskForRisky);
        assert_eq!(
            config.ceiling.sandbox.filesystem,
            FilesystemAccess::WorkspaceWrite
        );
        assert_eq!(config.ceiling.sandbox.network, NetworkAccess::Allowed);
    }

    #[test]
    fn cooldown_and_failover_have_defaults_and_overrides() {
        let config = Config::parse(
            r#"{"version":1,"cooldown_minutes":120,"failover":false,
                "providers":[{"id":"agent-b"},{"id":"agent-a","cooldown_minutes":30}]}"#,
        )
        .unwrap();
        assert!(!config.failover);
        assert_eq!(config.providers[0].cooldown, Duration::from_secs(120 * 60));
        assert_eq!(config.providers[1].cooldown, Duration::from_secs(30 * 60));
        let plain = Config::parse(r#"{"version":1,"providers":[{"id":"agent-b"}]}"#).unwrap();
        assert!(plain.failover);
        assert_eq!(
            plain.providers[0].cooldown,
            Duration::from_secs(DEFAULT_COOLDOWN_MINUTES * 60)
        );
    }

    const SAVED: &str = r#"{
      "version": 1,
      "permissions": {"approval": "ask_for_risky", "filesystem": "workspace_write", "network": "allowed"},
      "cooldown_minutes": 120,
      "providers": [
        {"id": "agent-a", "label": "account-work", "env": {"ACCOUNT_DIR": "D:\\akun\\kantor"}},
        {"id": "agent-a", "label": "account-home"},
        {"id": "agent-b", "cooldown_minutes": 30}
      ]
    }"#;

    fn labels(text: &str) -> Vec<String> {
        Config::parse(text)
            .unwrap()
            .providers
            .into_iter()
            .map(|p| p.label)
            .collect()
    }

    #[test]
    fn reordering_moves_the_named_entries_first_and_keeps_the_rest() {
        let order = ["agent-b".to_owned(), "account-home".to_owned()];
        let text = reorder_text(SAVED, &order, &[]).unwrap();
        assert_eq!(labels(&text), ["agent-b", "account-home", "account-work"]);
        // Nothing else in the file changed.
        let before = Config::parse(SAVED).unwrap();
        let after = Config::parse(&text).unwrap();
        assert_eq!(before.ceiling, after.ceiling);
        let kantor = after
            .providers
            .iter()
            .find(|p| p.label == "account-work")
            .unwrap();
        assert_eq!(kantor.env.len(), 1);
        let second = &after.providers[0];
        assert_eq!(second.cooldown, Duration::from_secs(30 * 60));
        assert!(text.contains("\"cooldown_minutes\": 120"));
    }

    #[test]
    fn a_detected_agent_is_added_when_it_is_named() {
        let order = ["agent-c".to_owned(), "agent-b".to_owned()];
        let text = reorder_text(SAVED, &order, &["agent-c".to_owned()]).unwrap();
        assert_eq!(
            labels(&text),
            ["agent-c", "agent-b", "account-work", "account-home"]
        );
        // Adding the same agent twice does not duplicate it.
        let again = reorder_text(&text, &order, &["agent-c".to_owned()]).unwrap();
        assert_eq!(labels(&again), labels(&text));
    }

    #[test]
    fn unknown_labels_are_ignored_and_a_bad_file_is_refused() {
        let text = reorder_text(SAVED, &["nope".to_owned()], &[]).unwrap();
        assert_eq!(labels(&text), labels(SAVED));
        assert!(reorder_text("nope", &[], &[]).is_err());
        assert!(reorder_text(r#"{"version":1}"#, &[], &[]).is_err());
    }

    #[test]
    fn save_order_replaces_the_file_and_leaves_no_temporary() {
        let dir = std::env::temp_dir().join(format!("uuldana-order-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, SAVED).unwrap();
        save_order(&path, &["agent-b".to_owned()], &[]).unwrap();
        assert_eq!(
            labels(&std::fs::read_to_string(&path).unwrap())[0],
            "agent-b"
        );
        assert!(!path.with_extension("json.tmp").exists());
        // A file that is not valid JSON is refused, not overwritten.
        let broken = dir.join("broken.json");
        std::fs::write(&broken, "nope").unwrap();
        assert!(save_order(&broken, &["agent-b".to_owned()], &[]).is_err());
        assert_eq!(std::fs::read_to_string(&broken).unwrap(), "nope");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saving_without_a_file_starts_one_with_what_was_named() {
        let dir = std::env::temp_dir().join(format!("uuldana-first-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("config.json");
        let order = ["agent-b".to_owned(), "agent-a".to_owned()];
        save_order(&path, &order, &["agent-b".to_owned()]).unwrap();
        let config = Config::load(&path).unwrap();
        let labels: Vec<_> = config.providers.iter().map(|p| p.label.as_str()).collect();
        assert_eq!(labels, ["agent-b"]);
        assert!(config.auto_detect);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_providers_means_the_installed_agents_are_used() {
        for text in [
            r#"{"version":1}"#,
            r#"{"version":1,"providers":[]}"#,
            r#"{"version":1,"auto_detect":true}"#,
        ] {
            let config = Config::parse(text).unwrap();
            assert!(config.providers.is_empty(), "{text}");
            assert!(config.auto_detect, "{text}");
        }
        let config =
            Config::parse(r#"{"version":1,"auto_detect":false,"providers":[{"id":"agent-b"}]}"#)
                .unwrap();
        assert!(!config.auto_detect);
        assert_eq!(Config::automatic().providers.len(), 0);
        assert!(Config::automatic().failover);
    }

    #[test]
    fn no_permissions_means_the_automatic_ceiling() {
        let config = Config::parse(r#"{"version":1,"providers":[{"id":"agent-b"}]}"#).unwrap();
        assert_eq!(config.ceiling, automatic_ceiling());
        assert_eq!(Config::automatic().ceiling, automatic_ceiling());
        // The automatic ceiling is not the strictest one, and is within what an
        // unrestricted run allows.
        assert_ne!(automatic_ceiling(), PermissionPolicy::most_restrictive());
        assert!(automatic_ceiling().is_within(&PermissionPolicy::unrestricted()));
    }

    #[test]
    fn a_permissions_object_still_takes_omitted_fields_as_the_strictest() {
        let config = Config::parse(
            r#"{"version":1,"permissions":{"approval":"ask_for_risky"},"providers":[{"id":"agent-b"}]}"#,
        )
        .unwrap();
        assert_eq!(config.ceiling.approval, ApprovalMode::AskForRisky);
        assert_eq!(
            config.ceiling.sandbox,
            PermissionPolicy::most_restrictive().sandbox
        );
        let empty =
            Config::parse(r#"{"version":1,"permissions":{},"providers":[{"id":"agent-b"}]}"#)
                .unwrap();
        assert_eq!(empty.ceiling, PermissionPolicy::most_restrictive());
    }

    #[test]
    fn rejects_bad_input_with_a_precise_message() {
        for (text, needle) in [
            ("nope", "not valid JSON"),
            ("[]", "top level"),
            (r#"{"providers":[{"id":"x"}]}"#, "version"),
            (
                r#"{"version":2,"providers":[{"id":"x"}]}"#,
                "unsupported version",
            ),
            (r#"{"version":1,"auto_detect":false}"#, "providers"),
            (
                r#"{"version":1,"auto_detect":false,"providers":[]}"#,
                "at least one",
            ),
            (r#"{"version":1,"auto_detect":"no"}"#, "auto_detect"),
            (
                r#"{"version":1,"providers":[{"id":"Bad Id"}]}"#,
                "providers[0].id",
            ),
            (
                r#"{"version":1,"providers":[{"id":"agent-b"},{"id":"agent-b"}]}"#,
                "used twice",
            ),
            (
                r#"{"version":1,"permissions":{"network":"maybe"},"providers":[{"id":"agent-b"}]}"#,
                "permissions.network",
            ),
            (
                r#"{"version":1,"providers":[{"id":"agent-b","env":{"A=B":"x"}}]}"#,
                "not a valid variable",
            ),
            (
                r#"{"version":1,"cooldown_minutes":0,"providers":[{"id":"agent-b"}]}"#,
                "cooldown_minutes",
            ),
            (
                r#"{"version":1,"failover":"yes","providers":[{"id":"agent-b"}]}"#,
                "failover",
            ),
            (
                r#"{"version":1,"providers":[{"id":"agent-b","cooldown_minutes":-5}]}"#,
                "providers[0].cooldown_minutes",
            ),
        ] {
            let error = Config::parse(text).unwrap_err().to_string();
            assert!(error.contains(needle), "{text}: {error}");
        }
    }
}
