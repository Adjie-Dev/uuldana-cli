//! Finding programs worth asking about, without a list of names.
//!
//! Every program on the search path outside the operating system's own folders
//! is a candidate, as long as it is a console program that can be started with
//! `--help` without opening a window. Nothing is run here: the choice is made
//! from the files alone.

use std::{
    collections::HashSet,
    fs,
    io::Read,
    path::{Path, PathBuf},
};

/// A program that may turn out to be an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Candidate {
    /// The file name without its extension, lowercase.
    pub id: String,
    /// The program itself, behind any script shim.
    pub path: PathBuf,
}

const MAX_SHIM_DEPTH: usize = 8;

/// Whether a PE image is a console program (not a window program).
pub(crate) fn pe_is_console(head: &[u8]) -> bool {
    if head.len() < 0x40 || &head[..2] != b"MZ" {
        return false;
    }
    let at = u32::from_le_bytes([head[0x3c], head[0x3d], head[0x3e], head[0x3f]]) as usize;
    // "PE\0\0", the 20 byte file header, then the optional header whose
    // subsystem field is 68 bytes in for both the 32 and the 64 bit layout.
    let subsystem = at + 4 + 20 + 68;
    if head.len() < subsystem + 2 || &head[at..at + 4] != b"PE\0\0" {
        return false;
    }
    u16::from_le_bytes([head[subsystem], head[subsystem + 1]]) == 3
}

fn read_head(path: &Path, limit: usize) -> Option<Vec<u8>> {
    let mut file = fs::File::open(path).ok()?;
    let mut head = vec![0; limit];
    let mut filled = 0;
    while filled < limit {
        match file.read(&mut head[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => return None,
        }
    }
    head.truncate(filled);
    Some(head)
}

#[cfg(windows)]
fn is_alias(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    // A reparse point is how the Store's app execution aliases are made; they
    // open an installer when run.
    metadata.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
fn is_alias(_: &fs::Metadata) -> bool {
    false
}

fn id_of(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_string_lossy().to_lowercase();
    let id: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let first = id.chars().next()?;
    (id.len() <= 64 && (first.is_ascii_lowercase() || first.is_ascii_digit())).then_some(id)
}

fn is_system_dir(dir: &Path, windows: bool, system_root: Option<&Path>) -> bool {
    if windows {
        if let Some(root) = system_root
            && dir.starts_with(root)
        {
            return true;
        }
        let text = dir.to_string_lossy().to_lowercase();
        text.contains("\\windowsapps") || text.contains("/windowsapps")
    } else {
        [
            "/bin",
            "/sbin",
            "/usr/bin",
            "/usr/sbin",
            "/usr/libexec",
            "/usr/lib",
        ]
        .iter()
        .any(|system| dir == Path::new(system))
    }
}

/// The console program an npm script shim starts: the shim names a package
/// under `node_modules`, and the program is a file of the same name inside it.
pub(crate) fn exe_behind_shim(shim: &Path) -> Option<PathBuf> {
    let text = String::from_utf8_lossy(&read_head(shim, 64 * 1024)?).into_owned();
    let stem = shim.file_stem()?.to_string_lossy().to_lowercase();
    let at = text.find("node_modules")?;
    let tail = &text[at + "node_modules".len()..];
    let mut segments = tail
        .trim_start_matches(['\\', '/'])
        .split(['\\', '/', '"', '\'', ' ', '\r', '\n'])
        .filter(|segment| !segment.is_empty());
    let first = segments.next()?;
    let mut package = shim.parent()?.join("node_modules").join(first);
    if first.starts_with('@') {
        package = package.join(segments.next()?);
    }
    find_console_exe(&package, &stem, 0)
}

fn find_console_exe(dir: &Path, stem: &str, depth: usize) -> Option<PathBuf> {
    if depth > MAX_SHIM_DEPTH {
        return None;
    }
    let mut folders = Vec::new();
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            folders.push(path);
        } else if kind.is_file()
            && path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
            && path
                .file_stem()
                .is_some_and(|name| name.to_string_lossy().to_lowercase() == stem)
            && read_head(&path, 1024).is_some_and(|head| pe_is_console(&head))
        {
            return Some(path);
        }
    }
    folders.sort();
    folders
        .iter()
        .find_map(|folder| find_console_exe(folder, stem, depth + 1))
}

/// Candidates on the given search path, first occurrence of a name winning.
/// `behind_shim` finds the program a script shim starts; it is a parameter so
/// the caller can remember answers, because looking takes a walk through
/// folders.
pub(crate) fn candidates(
    dirs: &[PathBuf],
    windows: bool,
    system_root: Option<&Path>,
    behind_shim: &mut dyn FnMut(&Path) -> Option<PathBuf>,
) -> Vec<Candidate> {
    let mut seen_ids = HashSet::new();
    let mut seen_paths = HashSet::new();
    let mut out = Vec::new();
    for dir in dirs {
        if is_system_dir(dir, windows, system_root) {
            continue;
        }
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        files.sort();
        for path in files {
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if !metadata.is_file() || metadata.len() == 0 || is_alias(&metadata) {
                continue;
            }
            let extension = path
                .extension()
                .map(|ext| ext.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            let program = if windows {
                match extension.as_str() {
                    "exe" => read_head(&path, 4096)
                        .filter(|head| pe_is_console(head))
                        .map(|_| path.clone()),
                    "cmd" => behind_shim(&path),
                    _ => None,
                }
            } else {
                unix_program(&path, &metadata)
            };
            let (Some(program), Some(id)) = (program, id_of(&path)) else {
                continue;
            };
            if seen_ids.contains(&id) || !seen_paths.insert(program.clone()) {
                continue;
            }
            seen_ids.insert(id.clone());
            out.push(Candidate { id, path: program });
        }
    }
    // A versioned copy of a program next to it (`tool` and `tool-1.2.3`) is
    // the same program, not another one.
    let bases: Vec<(String, Option<PathBuf>)> = out
        .iter()
        .map(|c| (c.id.clone(), c.path.parent().map(Path::to_path_buf)))
        .collect();
    out.retain(|candidate| {
        let Some(base) = without_version(&candidate.id) else {
            return true;
        };
        let here = candidate.path.parent().map(Path::to_path_buf);
        !bases.iter().any(|(id, dir)| id == base && *dir == here)
    });
    out
}

/// `tool-1.2.3` is `tool`: the part before a trailing version.
fn without_version(id: &str) -> Option<&str> {
    let (base, version) = id.rsplit_once('-')?;
    let digits = version.strip_prefix('v').unwrap_or(version);
    (!base.is_empty()
        && digits.chars().next().is_some_and(|c| c.is_ascii_digit())
        && digits.chars().all(|c| c.is_ascii_digit() || c == '.'))
    .then_some(base)
}

#[cfg(unix)]
fn unix_program(path: &Path, metadata: &fs::Metadata) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o111 == 0 {
        return None;
    }
    let head = read_head(path, 4)?;
    (head.starts_with(b"\x7fELF") || head.starts_with(b"#!")).then(|| path.to_path_buf())
}

#[cfg(not(unix))]
fn unix_program(_: &Path, _: &fs::Metadata) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal PE image with the given subsystem.
    fn pe(subsystem: u16) -> Vec<u8> {
        let mut image = vec![0u8; 512];
        image[..2].copy_from_slice(b"MZ");
        image[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        image[0x80..0x84].copy_from_slice(b"PE\0\0");
        let at = 0x80 + 4 + 20 + 68;
        image[at..at + 2].copy_from_slice(&subsystem.to_le_bytes());
        image
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("uuldana-discover-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn only_console_images_are_taken_for_programs() {
        assert!(pe_is_console(&pe(3)));
        assert!(!pe_is_console(&pe(2)));
        assert!(!pe_is_console(b"MZ"));
        assert!(!pe_is_console(&[0u8; 600]));
        let mut broken = pe(3);
        broken[0x80] = b'X';
        assert!(!pe_is_console(&broken));
    }

    #[test]
    fn candidates_skip_window_programs_empty_files_and_system_folders() {
        let dir = scratch("path");
        let system = dir.join("system");
        fs::create_dir_all(&system).unwrap();
        let tools = dir.join("tools");
        fs::create_dir_all(&tools).unwrap();
        fs::write(tools.join("Console-Tool.exe"), pe(3)).unwrap();
        fs::write(tools.join("window.exe"), pe(2)).unwrap();
        fs::write(tools.join("empty.exe"), b"").unwrap();
        fs::write(tools.join("notes.txt"), b"MZ").unwrap();
        fs::write(system.join("inbox.exe"), pe(3)).unwrap();
        let found = candidates(
            &[system.clone(), tools.clone()],
            true,
            Some(&system),
            &mut exe_behind_shim,
        );
        let ids: Vec<&str> = found.iter().map(|c| c.id.as_str()).collect();
        let _ = fs::remove_dir_all(&dir);
        if cfg!(windows) {
            assert_eq!(ids, ["console-tool"]);
        }
    }

    #[test]
    fn the_first_program_of_a_name_on_the_path_wins() {
        let dir = scratch("first");
        let (one, two) = (dir.join("one"), dir.join("two"));
        fs::create_dir_all(&one).unwrap();
        fs::create_dir_all(&two).unwrap();
        fs::write(one.join("tool.exe"), pe(3)).unwrap();
        fs::write(two.join("tool.exe"), pe(3)).unwrap();
        let found = candidates(&[one.clone(), two], true, None, &mut exe_behind_shim);
        let _ = fs::remove_dir_all(&dir);
        if cfg!(windows) {
            assert_eq!(found.len(), 1);
            assert!(found[0].path.starts_with(&one));
        }
    }

    #[test]
    fn an_npm_shim_leads_to_the_program_inside_its_package() {
        let dir = scratch("shim");
        let shim = dir.join("agent.cmd");
        fs::write(
            &shim,
            "@ECHO off\r\nnode \"%dp0%\\node_modules\\@vendor\\agent\\bin\\agent.js\" %*\r\n",
        )
        .unwrap();
        let deep = dir
            .join("node_modules")
            .join("@vendor")
            .join("agent")
            .join("node_modules")
            .join("@vendor")
            .join("agent-win32-x64")
            .join("vendor")
            .join("bin");
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("agent.exe"), pe(3)).unwrap();
        fs::write(deep.join("helper.exe"), pe(3)).unwrap();
        let found = exe_behind_shim(&shim);
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(
            found
                .as_deref()
                .map(|p| p.file_name().unwrap().to_string_lossy().into_owned()),
            Some("agent.exe".to_owned())
        );
    }

    #[test]
    fn a_shim_without_a_program_behind_it_leads_nowhere() {
        let dir = scratch("noshim");
        let shim = dir.join("script.cmd");
        fs::write(
            &shim,
            "@echo off\r\nnode \"%dp0%\\node_modules\\pure-js\\cli.js\" %*\r\n",
        )
        .unwrap();
        fs::create_dir_all(dir.join("node_modules").join("pure-js")).unwrap();
        fs::write(dir.join("node_modules").join("pure-js").join("cli.js"), "x").unwrap();
        let found = exe_behind_shim(&shim);
        let _ = fs::remove_dir_all(&dir);
        assert!(found.is_none());
    }

    #[test]
    fn a_versioned_copy_next_to_a_program_is_not_another_program() {
        assert_eq!(without_version("tool-1.2.3"), Some("tool"));
        assert_eq!(without_version("tool-v2"), Some("tool"));
        assert_eq!(without_version("tool-cli"), None);
        assert_eq!(without_version("tool"), None);
        let dir = scratch("versions");
        fs::write(dir.join("tool.exe"), pe(3)).unwrap();
        fs::write(dir.join("tool-1.2.3.exe"), pe(3)).unwrap();
        fs::write(dir.join("other-9.exe"), pe(3)).unwrap();
        let found = candidates(std::slice::from_ref(&dir), true, None, &mut exe_behind_shim);
        let _ = fs::remove_dir_all(&dir);
        if cfg!(windows) {
            let ids: Vec<&str> = found.iter().map(|c| c.id.as_str()).collect();
            assert_eq!(ids, ["other-9", "tool"]);
        }
    }

    #[test]
    fn ids_follow_the_provider_id_rules() {
        assert_eq!(
            id_of(Path::new("C:/x/Some Tool.exe")).as_deref(),
            Some("some-tool")
        );
        assert_eq!(
            id_of(Path::new("tool-cli.EXE")).as_deref(),
            Some("tool-cli")
        );
        assert!(id_of(Path::new("-bad.exe")).is_none());
    }
}
