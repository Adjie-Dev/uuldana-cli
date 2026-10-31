use std::{collections::BTreeMap, ffi::OsString, process::Command};
use uuldana_core::EnvChange;

pub(crate) fn overlay(changes: &[EnvChange]) -> BTreeMap<OsString, OsString> {
    merge(std::env::vars_os(), changes)
}
fn merge(
    base: impl IntoIterator<Item = (OsString, OsString)>,
    changes: &[EnvChange],
) -> BTreeMap<OsString, OsString> {
    // Reuse std's platform environment-key comparison, including Windows
    // Unicode case folding. This builder is never executed.
    let mut builder = Command::new("unused-environment-builder");
    builder.env_clear().envs(base);
    for change in changes {
        match change {
            EnvChange::Set { key, value } => {
                builder.env(key, value);
            }
            EnvChange::Remove { key } => {
                builder.env_remove(key);
            }
        }
    }
    builder
        .get_envs()
        .filter_map(|(k, v)| v.map(|v| (k.to_os_string(), v.to_os_string())))
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    #[test]
    fn windows_overlay_removes_ascii_and_unicode_case_variants() {
        let base = [
            ("Path".into(), "old".into()),
            ("KEEP".into(), "same".into()),
            ("Ö_VAR".into(), "secret".into()),
        ];
        let changes = [
            EnvChange::Set {
                key: "PATH".into(),
                value: "new".into(),
            },
            EnvChange::Remove { key: "pAtH".into() },
            EnvChange::Remove {
                key: "ö_var".into(),
            },
        ];
        let result = merge(base, &changes);
        assert_eq!(result.len(), 1);
        assert_eq!(result[&OsString::from("KEEP")], "same");
    }
    #[cfg(unix)]
    #[test]
    fn unix_overlay_preserves_distinct_case() {
        let result = merge(
            [("Path".into(), "old".into())],
            &[EnvChange::Remove { key: "PATH".into() }],
        );
        assert_eq!(result[&OsString::from("Path")], "old");
    }
}
