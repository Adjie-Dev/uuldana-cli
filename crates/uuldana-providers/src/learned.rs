//! Agents found and learned on this machine.
//!
//! The search path is read for console programs, each is asked for `--help`
//! once, and the ones whose help describes a coding agent become adapters
//! (see [`crate::learn`]). The answer is kept in a cache file keyed by the
//! program's path, size and modification time, so a program is only asked
//! again after it changes.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, UNIX_EPOCH},
};

use serde_json::{Value as Json, json};
use uuldana_provider_sdk::{
    AdapterError, CapabilitySet, EventDecoder, HostProbe, Installation, LaunchPlan, LaunchRequest,
    PermissionMapping, PermissionPolicy, Platform, ProviderAdapter, ProviderDescriptor,
    ResumeRequest, ShutdownPlan, TransportKind,
};

use crate::{
    discover::{self, Candidate},
    external::ExternalAdapter,
    help, learn,
};

/// How long a program gets to print its help.
const HELP_TIMEOUT: Duration = Duration::from_secs(5);
/// Programs asked at the same time.
const PARALLEL: usize = 16;
/// Subcommands read when the top level does not say enough.
const SUBCOMMANDS: usize = 6;
/// Bumped whenever what is learned from a help text, or how it is written
/// into a manifest, changes: the cache holds the manifests, so an old one
/// would otherwise outlive the change.
const CACHE_VERSION: u64 = 4;

/// An agent learned from its own help.
pub struct LearnedAdapter {
    inner: ExternalAdapter,
    executable: PathBuf,
    version: Option<String>,
    notes: Vec<String>,
}

impl LearnedAdapter {
    fn from_record(record: &Record) -> Option<Self> {
        Some(Self {
            inner: ExternalAdapter::from_value(&record.manifest, false).ok()?,
            executable: PathBuf::from(&record.path),
            version: record.version.clone(),
            notes: record.notes.clone(),
        })
    }
}

impl ProviderAdapter for LearnedAdapter {
    fn descriptor(&self) -> &ProviderDescriptor {
        self.inner.descriptor()
    }

    fn detect(&self, _: &dyn HostProbe) -> Result<Installation, AdapterError> {
        if !self.executable.is_file() {
            return Err(AdapterError::new(
                uuldana_provider_sdk::ErrorClass::NotInstalled,
                format!("{} is gone", self.executable.display()),
            ));
        }
        let mut installation = Installation::new(self.executable.clone());
        installation.version = Some(
            self.version
                .clone()
                .unwrap_or_else(|| "unknown version".to_owned()),
        );
        installation.notes = self.notes.clone();
        Ok(installation)
    }

    fn capabilities(&self, installation: &Installation) -> CapabilitySet {
        self.inner.capabilities(installation)
    }

    fn map_permissions(
        &self,
        installation: &Installation,
        requested: &PermissionPolicy,
    ) -> PermissionMapping {
        self.inner.map_permissions(installation, requested)
    }

    fn launch_plan(
        &self,
        installation: &Installation,
        request: &LaunchRequest,
    ) -> Result<LaunchPlan, AdapterError> {
        self.inner.launch_plan(installation, request)
    }

    fn resume_plan(
        &self,
        installation: &Installation,
        request: &ResumeRequest,
    ) -> Result<LaunchPlan, AdapterError> {
        self.inner.resume_plan(installation, request)
    }

    fn event_decoder(&self, transport: TransportKind) -> Option<Box<dyn EventDecoder>> {
        self.inner.event_decoder(transport)
    }

    fn shutdown_plan(&self, transport: TransportKind) -> ShutdownPlan {
        self.inner.shutdown_plan(transport)
    }
}

/// What is kept about one program.
#[derive(Debug, Clone)]
struct Record {
    path: String,
    version: Option<String>,
    manifest: Json,
    notes: Vec<String>,
}

#[derive(Debug, Clone)]
struct Entry {
    size: u64,
    modified: u64,
    /// `None`: asked, and it is not an agent. A program that does not print
    /// its help in time counts as one, until the program changes.
    agent: Option<Record>,
}

fn stamp(path: &Path) -> Option<(u64, u64)> {
    let metadata = fs::metadata(path).ok()?;
    let modified = metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis() as u64;
    Some((metadata.len(), modified))
}

type Shims = BTreeMap<String, (u64, u64, Option<PathBuf>)>;

fn shim_path(cache: &Path) -> PathBuf {
    cache.with_extension("shims.json")
}

fn load_shims(path: &Path) -> Shims {
    let Ok(text) = fs::read_to_string(path) else {
        return Shims::new();
    };
    let Ok(Json::Object(map)) = serde_json::from_str::<Json>(&text) else {
        return Shims::new();
    };
    map.into_iter()
        .filter_map(|(key, value)| {
            let size = value.get("size")?.as_u64()?;
            let modified = value.get("modified")?.as_u64()?;
            let target = value
                .get("target")
                .and_then(Json::as_str)
                .map(PathBuf::from);
            Some((key, (size, modified, target)))
        })
        .collect()
}

fn save_shims(path: &Path, shims: &Shims) {
    let map: serde_json::Map<String, Json> = shims
        .iter()
        .map(|(key, (size, modified, target))| {
            (
                key.clone(),
                json!({ "size": size, "modified": modified, "target": target }),
            )
        })
        .collect();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let temporary = path.with_extension("tmp");
    if fs::write(&temporary, Json::Object(map).to_string()).is_ok()
        && fs::rename(&temporary, path).is_err()
    {
        let _ = fs::remove_file(&temporary);
    }
}

fn load_cache(path: &Path) -> BTreeMap<String, Entry> {
    let mut entries = BTreeMap::new();
    let Ok(text) = fs::read_to_string(path) else {
        return entries;
    };
    let Ok(root) = serde_json::from_str::<Json>(&text) else {
        return entries;
    };
    if root.get("version").and_then(Json::as_u64) != Some(CACHE_VERSION) {
        return entries;
    }
    let Some(map) = root.get("programs").and_then(Json::as_object) else {
        return entries;
    };
    for (key, value) in map {
        let (Some(size), Some(modified)) = (
            value.get("size").and_then(Json::as_u64),
            value.get("modified").and_then(Json::as_u64),
        ) else {
            continue;
        };
        let agent = value.get("agent").filter(|a| !a.is_null()).and_then(|a| {
            Some(Record {
                path: key.clone(),
                version: a.get("version").and_then(Json::as_str).map(str::to_owned),
                manifest: a.get("manifest")?.clone(),
                notes: a
                    .get("notes")?
                    .as_array()?
                    .iter()
                    .filter_map(|n| n.as_str().map(str::to_owned))
                    .collect(),
            })
        });
        entries.insert(
            key.clone(),
            Entry {
                size,
                modified,
                agent,
            },
        );
    }
    entries
}

fn save_cache(path: &Path, entries: &BTreeMap<String, Entry>) {
    let programs: serde_json::Map<String, Json> = entries
        .iter()
        .map(|(key, entry)| {
            let agent = entry.agent.as_ref().map(|record| {
                json!({
                    "version": record.version,
                    "manifest": record.manifest,
                    "notes": record.notes,
                })
            });
            (
                key.clone(),
                json!({ "size": entry.size, "modified": entry.modified, "agent": agent }),
            )
        })
        .collect();
    let text = json!({ "version": CACHE_VERSION, "programs": programs }).to_string();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let temporary = path.with_extension("json.tmp");
    if fs::write(&temporary, text).is_ok() && fs::rename(&temporary, path).is_err() {
        let _ = fs::remove_file(&temporary);
    }
}

/// The text a program printed for `args`, standard output and error together.
fn ask(probe: &dyn HostProbe, program: &Path, args: &[&str]) -> Option<String> {
    let output = probe.run(program, args, HELP_TIMEOUT).ok()?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.stderr.is_empty() {
        text.push('\n');
        text.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    (!text.trim().is_empty()).then_some(text)
}

/// A version line, when the program prints one.
fn version_of(probe: &dyn HostProbe, program: &Path) -> Option<String> {
    let text = ask(probe, program, &["--version"])?;
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    (line.len() <= 120 && line.chars().any(|c| c.is_ascii_digit())).then(|| line.to_owned())
}

/// Asks one program what it is. `None` when it does not print its help in
/// time, or is not a coding agent.
fn learn_program(probe: &dyn HostProbe, candidate: &Candidate) -> Option<Record> {
    let top = help::parse(&ask(probe, &candidate.path, &["--help"])?);
    if top.options.is_empty() && top.commands.is_empty() {
        return None;
    }
    let mut subs = Vec::new();
    if !learn::top_is_enough(&top) {
        for name in learn::subcommands_to_read(&top, SUBCOMMANDS) {
            if let Some(text) = ask(probe, &candidate.path, &[name.as_str(), "--help"]) {
                subs.push((name, help::parse(&text)));
            }
        }
    }
    let profile = learn::learn(&top, &subs)?;
    let version = version_of(probe, &candidate.path);
    let path = candidate.path.to_string_lossy().into_owned();
    let mut notes = profile.reasons.clone();
    notes.extend(profile.assumptions.iter().map(|a| format!("assumed: {a}")));
    Some(Record {
        manifest: profile.manifest(&candidate.id, &path),
        path,
        version,
        notes,
    })
}

/// The agents installed on this machine, learned from what they print. Every
/// program is asked at most once while it stays unchanged; the answers are
/// kept in `cache` when given.
///
/// `announce` is told how many programs are about to be asked when that is
/// more than a few, so a first look at a big search path can say what takes
/// the time.
pub fn learn_installed(
    probe: &dyn HostProbe,
    cache: Option<&Path>,
    announce: &dyn Fn(usize),
) -> Vec<Arc<dyn ProviderAdapter>> {
    learn_installed_with_progress(probe, cache, announce, &|_, _| {})
}

/// [`learn_installed`], and `progress` is told `(asked so far, to ask)` each
/// time a program has answered or timed out. It is called from the threads
/// that ask, so it must be quick and must not block.
pub fn learn_installed_with_progress(
    probe: &dyn HostProbe,
    cache: Option<&Path>,
    announce: &dyn Fn(usize),
    progress: &(dyn Fn(usize, usize) + Sync),
) -> Vec<Arc<dyn ProviderAdapter>> {
    learn_among(probe, cache, None, announce, progress)
}

/// [`learn_installed_with_progress`] for the programs named in `only` and no
/// other: nothing else on the search path is ever run. This is what a
/// configuration with `"auto_detect": false` asks for.
pub fn learn_listed_with_progress(
    probe: &dyn HostProbe,
    cache: Option<&Path>,
    only: &[String],
    announce: &dyn Fn(usize),
    progress: &(dyn Fn(usize, usize) + Sync),
) -> Vec<Arc<dyn ProviderAdapter>> {
    learn_among(probe, cache, Some(only), announce, progress)
}

fn learn_among(
    probe: &dyn HostProbe,
    cache: Option<&Path>,
    only: Option<&[String]>,
    announce: &dyn Fn(usize),
    progress: &(dyn Fn(usize, usize) + Sync),
) -> Vec<Arc<dyn ProviderAdapter>> {
    let windows = probe.platform() == Platform::Windows;
    let dirs: Vec<PathBuf> = probe
        .env_var("PATH")
        .map(|paths| std::env::split_paths(&paths).collect())
        .unwrap_or_default();
    let system_root = probe
        .env_var("SystemRoot")
        .or_else(|| probe.env_var("windir"))
        .map(PathBuf::from);
    let mut known = cache.map(load_cache).unwrap_or_default();
    // With a list, a program that is not on it is not even looked at.
    let listed = |id: &str| only.is_none_or(|only| only.iter().any(|name| name == id));
    let mut shims = cache
        .map(|path| load_shims(&shim_path(path)))
        .unwrap_or_default();
    let before = shims.clone();
    let mut found = discover::candidates(
        &dirs,
        windows,
        system_root.as_deref(),
        &mut |shim: &Path| {
            let key = shim.to_string_lossy().into_owned();
            let stamp = stamp(shim)?;
            if let Some((size, modified, target)) = shims.get(&key)
                && (*size, *modified) == stamp
            {
                return target.clone();
            }
            let target = discover::exe_behind_shim(shim);
            shims.insert(key, (stamp.0, stamp.1, target.clone()));
            target
        },
    );
    if let Some(cache) = cache
        && shims != before
    {
        save_shims(&shim_path(cache), &shims);
    }
    found.retain(|candidate| listed(&candidate.id));
    let mut to_ask: Vec<(Candidate, u64, u64)> = Vec::new();
    for candidate in &found {
        let key = candidate.path.to_string_lossy().into_owned();
        let Some((size, modified)) = stamp(&candidate.path) else {
            continue;
        };
        match known.get(&key) {
            Some(entry) if entry.size == size && entry.modified == modified => {}
            _ => to_ask.push((candidate.clone(), size, modified)),
        }
    }
    if to_ask.len() > 3 {
        announce(to_ask.len());
    }
    if !to_ask.is_empty() {
        let next = AtomicUsize::new(0);
        let finished = AtomicUsize::new(0);
        let total = to_ask.len();
        let answers: Mutex<Vec<(String, Entry)>> = Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for _ in 0..PARALLEL.min(to_ask.len()) {
                scope.spawn(|| {
                    loop {
                        let at = next.fetch_add(1, Ordering::Relaxed);
                        let Some((candidate, size, modified)) = to_ask.get(at) else {
                            break;
                        };
                        let agent = learn_program(probe, candidate);
                        progress(finished.fetch_add(1, Ordering::Relaxed) + 1, total);
                        let key = candidate.path.to_string_lossy().into_owned();
                        if let Ok(mut answers) = answers.lock() {
                            answers.push((
                                key,
                                Entry {
                                    size: *size,
                                    modified: *modified,
                                    agent,
                                },
                            ));
                        }
                    }
                });
            }
        });
        for (key, entry) in answers.into_inner().unwrap_or_default() {
            known.insert(key, entry);
        }
        if let Some(cache) = cache {
            save_cache(cache, &known);
        }
    }
    found
        .iter()
        .filter_map(|candidate| {
            let key = candidate.path.to_string_lossy().into_owned();
            known.get(&key)?.agent.as_ref()
        })
        .filter_map(LearnedAdapter::from_record)
        .map(|adapter| Arc::new(adapter) as Arc<dyn ProviderAdapter>)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuldana_provider_sdk::{ProbeError, ProbeOutput};

    /// A host whose search path is one folder and which records the programs
    /// it was asked to run.
    struct Recording {
        dir: PathBuf,
        asked: Mutex<Vec<String>>,
    }
    impl HostProbe for Recording {
        fn find_executable(&self, _: &str) -> Option<PathBuf> {
            None
        }
        fn run(&self, program: &Path, _: &[&str], _: Duration) -> Result<ProbeOutput, ProbeError> {
            let name = program.file_stem().unwrap().to_string_lossy().into_owned();
            self.asked.lock().unwrap().push(name);
            Err(ProbeError::NotFound)
        }
        fn env_var(&self, name: &str) -> Option<std::ffi::OsString> {
            (name == "PATH").then(|| self.dir.clone().into_os_string())
        }
        fn platform(&self) -> Platform {
            Platform::current()
        }
    }

    #[test]
    fn a_listed_scan_never_runs_a_program_that_is_not_on_the_list() {
        let dir = std::env::temp_dir().join(format!("uuldana-listed-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let extension = std::env::consts::EXE_SUFFIX;
        let me = std::env::current_exe().unwrap();
        for name in ["wanted", "unwanted"] {
            fs::copy(&me, dir.join(format!("{name}{extension}"))).unwrap();
        }
        let host = Recording {
            dir: dir.clone(),
            asked: Mutex::new(Vec::new()),
        };
        let only = vec!["wanted".to_owned()];
        learn_listed_with_progress(&host, None, &only, &|_| {}, &|_, _| {});
        let asked = host.asked.lock().unwrap().clone();
        let _ = fs::remove_dir_all(&dir);
        assert!(!asked.is_empty(), "the listed program was not asked");
        assert!(asked.iter().all(|name| name == "wanted"), "{asked:?}");
    }

    #[test]
    fn a_cache_round_trips_and_ignores_files_it_does_not_understand() {
        let dir = std::env::temp_dir().join(format!("uuldana-learned-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("programs.json");
        let mut entries = BTreeMap::new();
        entries.insert(
            "C:/x/not-an-agent.exe".to_owned(),
            Entry {
                size: 10,
                modified: 20,
                agent: None,
            },
        );
        entries.insert(
            "C:/x/agent.exe".to_owned(),
            Entry {
                size: 11,
                modified: 21,
                agent: Some(Record {
                    path: "C:/x/agent.exe".to_owned(),
                    version: Some("agent 1.2".to_owned()),
                    manifest: json!({"version": 1, "id": "agent"}),
                    notes: vec!["resume by id".to_owned()],
                }),
            },
        );
        save_cache(&file, &entries);
        let loaded = load_cache(&file);
        assert_eq!(loaded.len(), 2);
        assert!(loaded["C:/x/not-an-agent.exe"].agent.is_none());
        let agent = loaded["C:/x/agent.exe"].agent.as_ref().unwrap();
        assert_eq!(agent.version.as_deref(), Some("agent 1.2"));
        assert_eq!(agent.notes, ["resume by id"]);
        assert!(!file.with_extension("json.tmp").exists());
        fs::write(&file, "nope").unwrap();
        assert!(load_cache(&file).is_empty());
        fs::write(&file, r#"{"version": 99, "programs": {}}"#).unwrap();
        assert!(load_cache(&file).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
