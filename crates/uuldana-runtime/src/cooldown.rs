//! Cooldowns: which provider accounts are resting after a usage limit.
//!
//! A cooldown is recorded per configured entry label, with the time (Unix
//! milliseconds) at which the entry may be used again. The table is saved next
//! to the journals so a later `uuldana` does not walk straight back into an
//! account that is still out of quota.
//!
//! The clock is always passed in, never read here, so behavior is testable and
//! deterministic.

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

use serde_json::{Value, json};

/// File name of the cooldown table under the data directory.
pub const FILE_NAME: &str = "cooldowns.json";

/// Where the table lives for a data directory.
pub fn path_in(data: &Path) -> PathBuf {
    data.join(FILE_NAME)
}

/// Resting entries and when they return.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Cooldowns {
    until: BTreeMap<String, u64>,
}

impl Cooldowns {
    /// Reads the table. A missing or unreadable file means nothing is resting:
    /// a damaged cooldown file must never lock the user out.
    pub fn load(path: &Path) -> Self {
        let Ok(text) = fs::read_to_string(path) else {
            return Self::default();
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            return Self::default();
        };
        let until = value
            .get("until")
            .and_then(Value::as_object)
            .map(|map| {
                map.iter()
                    .filter_map(|(label, ms)| Some((label.clone(), ms.as_u64()?)))
                    .collect()
            })
            .unwrap_or_default();
        Self { until }
    }

    /// Writes the table through a temporary file so a crash cannot leave half
    /// of it behind.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension("json.tmp");
        fs::write(&temporary, json!({"v": 1, "until": self.until}).to_string())?;
        fs::rename(temporary, path)
    }

    /// Marks `label` as resting until `until_ms`. A later time replaces an
    /// earlier one; an earlier time never shortens a rest.
    pub fn rest_until(&mut self, label: &str, until_ms: u64) {
        let entry = self.until.entry(label.to_owned()).or_insert(0);
        *entry = (*entry).max(until_ms);
    }

    /// Ends the rest of `label` at once, for example because the user chose to
    /// use it again.
    pub fn clear(&mut self, label: &str) {
        self.until.remove(label);
    }

    /// When `label` returns, if it is resting at `now_ms`.
    pub fn returns_at(&self, label: &str, now_ms: u64) -> Option<u64> {
        self.until
            .get(label)
            .copied()
            .filter(|until| *until > now_ms)
    }

    /// Whether `label` is resting at `now_ms`.
    pub fn is_resting(&self, label: &str, now_ms: u64) -> bool {
        self.returns_at(label, now_ms).is_some()
    }

    /// Forgets rests that have ended.
    pub fn forget_ended(&mut self, now_ms: u64) {
        self.until.retain(|_, until| *until > now_ms);
    }

    /// The earliest time any resting entry returns.
    pub fn next_return(&self, now_ms: u64) -> Option<u64> {
        self.until.values().copied().filter(|u| *u > now_ms).min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rest_ends_at_the_given_time_and_never_shortens() {
        let mut table = Cooldowns::default();
        table.rest_until("account-work", 10_000);
        assert!(table.is_resting("account-work", 9_999));
        assert!(!table.is_resting("account-work", 10_000));
        assert_eq!(table.returns_at("account-work", 5_000), Some(10_000));
        table.rest_until("account-work", 4_000);
        assert_eq!(table.returns_at("account-work", 5_000), Some(10_000));
        table.rest_until("account-work", 20_000);
        assert_eq!(table.returns_at("account-work", 5_000), Some(20_000));
        assert!(!table.is_resting("agent-b", 0));
    }

    #[test]
    fn next_return_picks_the_earliest_resting_entry() {
        let mut table = Cooldowns::default();
        table.rest_until("a", 9_000);
        table.rest_until("b", 7_000);
        table.rest_until("c", 1_000);
        assert_eq!(table.next_return(2_000), Some(7_000));
        table.forget_ended(8_000);
        assert_eq!(table.next_return(8_000), Some(9_000));
    }

    #[test]
    fn saved_table_round_trips_and_a_damaged_file_means_nothing_rests() {
        let dir = std::env::temp_dir().join(format!("uuldana-cooldown-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = path_in(&dir);
        let mut table = Cooldowns::default();
        table.rest_until("account-work", 123_456);
        table.save(&path).unwrap();
        assert_eq!(Cooldowns::load(&path), table);

        fs::write(&path, "{ not json").unwrap();
        assert_eq!(Cooldowns::load(&path), Cooldowns::default());
        assert_eq!(
            Cooldowns::load(&dir.join("absent.json")),
            Cooldowns::default()
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
