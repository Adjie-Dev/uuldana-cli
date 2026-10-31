//! Test-only Windows process discovery for the transport tests.
//!
//! Replaces a PowerShell/CIM query and a `tasklist` call with documented Win32
//! calls (Toolhelp32 snapshot, GetProcessTimes, QueryFullProcessImageNameW).
//! No dependencies: the few kernel32 entry points are declared by hand.
//!
//! Commands:
//!   tree <ppid>                     direct children of <ppid>, one TSV row each:
//!                                   pid, creation FILETIME, image name, image path, machine
//!   alive <pid> <image> [creation]  exit 0 alive, 1 not running, 2 usage, 3 enumeration failed
//!
//! Failure to enumerate is never reported as an empty or alive result: it is an
//! error exit with a message on stderr.
#![cfg(windows)]

use std::ffi::c_void;
use std::process::ExitCode;

type Handle = *mut c_void;

const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;
const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x0000_1000;
const SYNCHRONIZE: u32 = 0x0010_0000;
const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_INVALID_PARAMETER: u32 = 87;
const ERROR_NO_MORE_FILES: u32 = 18;
const MAX_PATH: usize = 260;
const EXIT_OK: u8 = 0;
const EXIT_NO: u8 = 1;
const EXIT_USAGE: u8 = 2;
const EXIT_ENUM: u8 = 3;

#[repr(C)]
struct ProcessEntry32W {
    size: u32,
    usage: u32,
    process_id: u32,
    default_heap_id: usize,
    module_id: u32,
    threads: u32,
    parent_process_id: u32,
    pri_class_base: i32,
    flags: u32,
    exe_file: [u16; MAX_PATH],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FileTime {
    low: u32,
    high: u32,
}

impl FileTime {
    fn as_u64(self) -> u64 {
        (u64::from(self.high) << 32) | u64::from(self.low)
    }
}

#[link(name = "kernel32")]
extern "system" {
    fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> Handle;
    fn Process32FirstW(snapshot: Handle, entry: *mut ProcessEntry32W) -> i32;
    fn Process32NextW(snapshot: Handle, entry: *mut ProcessEntry32W) -> i32;
    fn OpenProcess(access: u32, inherit: i32, process_id: u32) -> Handle;
    fn GetProcessTimes(
        process: Handle,
        creation: *mut FileTime,
        exit: *mut FileTime,
        kernel: *mut FileTime,
        user: *mut FileTime,
    ) -> i32;
    fn QueryFullProcessImageNameW(
        process: Handle,
        flags: u32,
        name: *mut u16,
        size: *mut u32,
    ) -> i32;
    fn IsWow64Process(process: Handle, wow64: *mut i32) -> i32;
    fn CloseHandle(object: Handle) -> i32;
    fn GetLastError() -> u32;
}

/// One row of a process snapshot.
#[derive(Clone)]
struct Entry {
    pid: u32,
    ppid: u32,
    name: String,
}

/// An owned process handle, closed on drop.
struct Process(Handle);

impl Drop for Process {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful OpenProcess and is closed once.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

enum Open {
    Ok(Process),
    /// The process may exist, but this caller cannot query it.
    Denied,
    /// The id no longer names a process.
    Gone,
}

/// A direct child kept for output. `handle` is `None` when the child could not be opened.
struct Child {
    entry: Entry,
    created: Option<u64>,
    path: Option<String>,
    machine: &'static str,
    _handle: Option<Process>,
}

/// Fault injection exists only in a build made with `--cfg procinfo_fault`.
fn fault(kind: &str) -> bool {
    #[cfg(procinfo_fault)]
    {
        std::env::var("PROCINFO_FAULT").is_ok_and(|v| v == kind)
    }
    #[cfg(not(procinfo_fault))]
    {
        let _ = kind;
        false
    }
}

fn snapshot() -> Result<Vec<Entry>, String> {
    if fault("snapshot") {
        return Err("injected snapshot failure".into());
    }
    // SAFETY: plain Win32 calls; the snapshot handle is closed on every path.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return Err(format!(
                "CreateToolhelp32Snapshot failed: {}",
                GetLastError()
            ));
        }
        let mut entry: ProcessEntry32W = std::mem::zeroed();
        entry.size = std::mem::size_of::<ProcessEntry32W>() as u32;
        let mut rows = Vec::new();
        let mut more = Process32FirstW(snap, &mut entry);
        if more == 0 {
            let code = GetLastError();
            CloseHandle(snap);
            return Err(format!("Process32FirstW failed: {code}"));
        }
        while more != 0 {
            let len = entry
                .exe_file
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(MAX_PATH);
            rows.push(Entry {
                pid: entry.process_id,
                ppid: entry.parent_process_id,
                name: String::from_utf16_lossy(&entry.exe_file[..len]),
            });
            more = Process32NextW(snap, &mut entry);
        }
        // A complete walk ends with ERROR_NO_MORE_FILES; anything else is a truncated listing.
        let code = GetLastError();
        CloseHandle(snap);
        if code != ERROR_NO_MORE_FILES {
            return Err(format!("Process32NextW stopped early: {code}"));
        }
        Ok(rows)
    }
}

fn open(pid: u32) -> Open {
    if fault("open") {
        return Open::Denied;
    }
    // SAFETY: OpenProcess has no preconditions; a null result is handled.
    unsafe {
        let mut handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid);
        if handle.is_null() && GetLastError() == ERROR_ACCESS_DENIED {
            handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        }
        if !handle.is_null() {
            return Open::Ok(Process(handle));
        }
        match GetLastError() {
            ERROR_INVALID_PARAMETER => Open::Gone,
            // Access denied, or any other failure, leaves the process unverifiable, not gone.
            _ => Open::Denied,
        }
    }
}

fn creation_time(process: &Process) -> Option<u64> {
    let mut created = FileTime::default();
    let (mut exit, mut kernel, mut user) = (
        FileTime::default(),
        FileTime::default(),
        FileTime::default(),
    );
    // SAFETY: all four out pointers reference live locals.
    let ok = unsafe { GetProcessTimes(process.0, &mut created, &mut exit, &mut kernel, &mut user) };
    (ok != 0).then(|| created.as_u64())
}

fn image_path(process: &Process) -> Option<String> {
    let mut buffer = vec![0u16; 32768];
    let mut size = buffer.len() as u32;
    // SAFETY: `size` is the buffer capacity in characters, as the API requires.
    let ok = unsafe { QueryFullProcessImageNameW(process.0, 0, buffer.as_mut_ptr(), &mut size) };
    (ok != 0).then(|| String::from_utf16_lossy(&buffer[..size as usize]))
}

fn machine(process: &Process) -> &'static str {
    let mut wow64 = 0i32;
    // SAFETY: the out pointer references a live local.
    let ok = unsafe { IsWow64Process(process.0, &mut wow64) };
    match (ok != 0, wow64 != 0) {
        (true, true) => "x86",
        (true, false) => "x64",
        _ => "?",
    }
}

fn tree(ppid: u32) -> Result<u8, String> {
    let first = snapshot()?;
    // A parent that cannot be read (gone or not accessible) imposes no start-time
    // bound, which matches the PowerShell query this replaces.
    let parent_created = match open(ppid) {
        Open::Ok(parent) => creation_time(&parent),
        Open::Denied | Open::Gone => None,
    };
    let mut kept: Vec<Child> = Vec::new();
    for entry in first.iter().filter(|e| e.ppid == ppid && e.pid != ppid) {
        match open(entry.pid) {
            // Exited between the snapshot and the open: not a live descendant any more.
            Open::Gone => {}
            // Present but unreadable: keep it, so a protected child is never silently dropped.
            Open::Denied => kept.push(Child {
                entry: entry.clone(),
                created: None,
                path: None,
                machine: "?",
                _handle: None,
            }),
            Open::Ok(handle) => {
                let created = creation_time(&handle);
                // Started before its claimed parent: the parent id was recycled.
                if matches!((created, parent_created), (Some(c), Some(p)) if c < p) {
                    continue;
                }
                kept.push(Child {
                    entry: entry.clone(),
                    created,
                    path: image_path(&handle),
                    machine: machine(&handle),
                    _handle: Some(handle),
                });
            }
        }
    }
    if !kept.is_empty() {
        // An open handle pins its process object, so an id cannot be recycled behind
        // it. This second snapshot catches an id recycled between the first snapshot
        // and the open: such a row now shows a different parent.
        let second = snapshot()?;
        kept.retain(|child| {
            second
                .iter()
                .filter(|e| e.pid == child.entry.pid)
                .all(|e| e.ppid == ppid)
        });
    }
    kept.sort_by_key(|child| child.entry.pid);
    let mut out = String::new();
    for child in &kept {
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\n",
            child.entry.pid,
            child.created.unwrap_or(0),
            child.entry.name,
            child.path.as_deref().unwrap_or("-"),
            child.machine
        ));
    }
    print!("{out}");
    Ok(EXIT_OK)
}

fn alive(pid: u32, image: &str, expected_created: Option<u64>) -> Result<u8, String> {
    let rows = snapshot()?;
    let Some(entry) = rows.iter().find(|e| e.pid == pid) else {
        println!("gone");
        return Ok(EXIT_NO);
    };
    if !entry.name.eq_ignore_ascii_case(image) {
        println!("other-image");
        return Ok(EXIT_NO);
    }
    let Some(expected) = expected_created else {
        println!("alive");
        return Ok(EXIT_OK);
    };
    match open(pid) {
        Open::Gone => {
            println!("gone");
            Ok(EXIT_NO)
        }
        Open::Denied => {
            println!("alive-unopenable");
            Ok(EXIT_OK)
        }
        Open::Ok(handle) => match creation_time(&handle) {
            Some(actual) if actual != expected => {
                println!("reused");
                Ok(EXIT_NO)
            }
            _ => {
                println!("alive");
                Ok(EXIT_OK)
            }
        },
    }
}

fn run() -> Result<u8, (u8, String)> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let number = |text: &str, what: &str| {
        text.parse::<u32>()
            .map_err(|_| (EXIT_USAGE, format!("invalid {what}: {text}")))
    };
    let enumeration = |e: String| (EXIT_ENUM, e);
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["tree", ppid] => tree(number(ppid, "ppid")?).map_err(enumeration),
        ["alive", pid, image] => alive(number(pid, "pid")?, image, None).map_err(enumeration),
        ["alive", pid, image, created] => {
            let created = created
                .parse::<u64>()
                .map_err(|_| (EXIT_USAGE, format!("invalid creation: {created}")))?;
            alive(number(pid, "pid")?, image, Some(created)).map_err(enumeration)
        }
        _ => Err((
            EXIT_USAGE,
            "usage: tree <ppid> | alive <pid> <image> [creation]".into(),
        )),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err((code, message)) => {
            eprintln!("uuldana-test-procinfo: {message}");
            ExitCode::from(code)
        }
    }
}
