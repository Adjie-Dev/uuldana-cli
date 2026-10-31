//! Uuldana's inherited Ctrl+C disposition boundary. See UULDANA_PATCH.md.
//!
//! `SetConsoleCtrlHandler(NULL, TRUE)` and `CREATE_NEW_PROCESS_GROUP` set an
//! inheritable "ignore CTRL+C" attribute, stored as bit 0 of `ConsoleFlags` in
//! the process parameters block of the PEB. A creator that carries it (an MSYS
//! `bash.exe` does) hands it to every child, so a `0x03` written to the
//! pseudoconsole never interrupts the child. The attribute describes the
//! creator's own console, not the child's fresh pseudoconsole, so the child is
//! released from it while it is still suspended and has executed nothing. The
//! creator's own attribute and every other process are never touched.
//!
//! Only the child's own bit 0 is written, with a read-back check. The block
//! layout is not documented. The offsets are verified for x86_64 Windows only,
//! and are guarded by a length check on the block and a check that the stored
//! value only uses bits seen in practice. Neither check proves the layout is
//! right; they only make a wrong guess less likely to be written. Every outcome
//! is reported per block to the caller, never guessed around.
//!
//! A build for x86_64 can still run on Windows on ARM, where the child may be
//! native ARM64 or an emulated x64 or x86 process whose layout was never looked
//! at. Before anything is read or written, `IsWow64Process2` must say the host is
//! x86_64 and the child is either native or 32-bit x86. Anything else, or an
//! answer that cannot be obtained, leaves the child untouched and is reported.
// On targets without a verified layout only the reported fallback is used, and
// the locating and writing helpers below are intentionally unused there.
#![cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
use lazy_static::lazy_static;
use shared_library::shared_library;
use std::io::{Error as IoError, Result as IoResult};
use std::mem;
use std::path::Path;
use winapi::ctypes::c_void;
use winapi::um::handleapi::INVALID_HANDLE_VALUE;
use winapi::um::memoryapi::{ReadProcessMemory, WriteProcessMemory};
use winapi::um::winnt::HANDLE;

/// Bit 0 of `ConsoleFlags`: CTRL+C is ignored (and inherited).
const CONSOLE_IGNORE_CTRL_C: u32 = 0x1;

/// Bits of `ConsoleFlags` seen on x86_64 Windows 11 10.0.26300: 0x0 and 0x1 in
/// creators and suspended children, 0x6 and 0x7 in pseudoconsole children. A
/// stored value with any other bit set is not trusted to be this field.
const OBSERVED_CONSOLE_FLAGS: u32 = 0x7;

// IMAGE_FILE_MACHINE_* values from winnt.h, as `IsWow64Process2` reports them.
const MACHINE_UNKNOWN: u16 = 0;
const MACHINE_I386: u16 = 0x014c;
const MACHINE_AMD64: u16 = 0x8664;

const PROCESS_BASIC_INFORMATION_CLASS: u32 = 0;
const PROCESS_WOW64_INFORMATION_CLASS: u32 = 26;

// RTL_USER_PROCESS_PARAMETERS: MaximumLength, Length, Flags, DebugFlags, then
// ConsoleHandle and ConsoleFlags. Verified on x86_64 only; other targets get
// the reported fallback in `clear_inherited_ctrl_c_ignore`.
#[cfg(target_arch = "x86_64")]
const PEB_PROCESS_PARAMETERS: usize = 0x20;
#[cfg(target_arch = "x86_64")]
const PARAMS_CONSOLE_FLAGS: usize = 0x18;
// The same block as a 32-bit process sees it (PEB32).
#[cfg(target_arch = "x86_64")]
const PEB32_PROCESS_PARAMETERS: usize = 0x10;
#[cfg(target_arch = "x86_64")]
const PARAMS32_CONSOLE_FLAGS: usize = 0x14;

#[cfg(target_arch = "x86_64")]
#[repr(C)]
struct ProcessBasicInformation {
    exit_status: isize,
    peb_base_address: *mut c_void,
    affinity_mask: usize,
    base_priority: isize,
    unique_process_id: usize,
    inherited_from_unique_process_id: usize,
}

shared_library!(NtFuncs,
    pub fn NtQueryInformationProcess(
        process: HANDLE,
        class: u32,
        info: *mut c_void,
        length: u32,
        returned: *mut u32
    ) -> i32,
);

// Windows 10 1709 and newer; loaded dynamically so an older kernel32 is a
// reported refusal instead of a failure to load this crate.
shared_library!(WowFuncs,
    pub fn IsWow64Process2(
        process: HANDLE,
        process_machine: *mut u16,
        native_machine: *mut u16
    ) -> i32,
);

lazy_static! {
    static ref NTDLL: Option<NtFuncs> = NtFuncs::open(Path::new("ntdll.dll")).ok();
    static ref KERNEL32_WOW: Option<WowFuncs> = WowFuncs::open(Path::new("kernel32.dll")).ok();
}

/// What happened to one parameters block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum BlockOutcome {
    /// The attribute was set and is now clear, confirmed by read-back.
    Cleared,
    /// The attribute was not set, so nothing was written.
    AlreadyClear,
    /// The block was located, read or written wrongly; nothing is claimed.
    Failed(String),
}

/// What happened to the child's `ConsoleFlags` words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum CtrlCRelease {
    /// No block was touched, for a reason that applies to the whole child.
    NotAttempted(String),
    /// Each block was processed independently; one failing never skips the other.
    Attempted {
        native: BlockOutcome,
        /// `None` when the child is confirmed not to be a WOW64 process. A child
        /// whose WOW64 state could not be determined reports `Some(Failed)`.
        wow32: Option<BlockOutcome>,
    },
}

impl CtrlCRelease {
    /// Text for a failed or partial release, `None` when every block is either
    /// cleared or was already clear. This is what a consumer is told.
    pub(super) fn diagnostic(&self) -> Option<String> {
        const EFFECT: &str = "interactive CTRL+C may not reach the child";
        match self {
            Self::NotAttempted(reason) => Some(format!(
                "inherited CTRL+C ignore attribute was not released: {reason}; {EFFECT}"
            )),
            Self::Attempted { native, wow32 } => {
                let mut failed = Vec::new();
                let mut released = Vec::new();
                for (name, outcome) in [("native", Some(native)), ("32-bit", wow32.as_ref())] {
                    match outcome {
                        Some(BlockOutcome::Failed(why)) => {
                            failed.push(format!("{name} block: {why}"))
                        }
                        Some(_) => released.push(name),
                        None => {}
                    }
                }
                if failed.is_empty() {
                    return None;
                }
                let released = if released.is_empty() {
                    "no block was released".to_owned()
                } else {
                    format!("released: {}", released.join(", "))
                };
                Some(format!(
                    "inherited CTRL+C ignore attribute: {}; {released}; {EFFECT}",
                    failed.join("; ")
                ))
            }
        }
    }

    /// Blocks that were set and are now confirmed clear.
    #[cfg(test)]
    pub(super) fn cleared_blocks(&self) -> usize {
        match self {
            Self::NotAttempted(_) => 0,
            Self::Attempted { native, wow32 } => std::iter::once(native)
                .chain(wow32)
                .filter(|outcome| **outcome == BlockOutcome::Cleared)
                .count(),
        }
    }
}

/// `ConsoleFlags` as seen by the native PEB and, for a 32-bit child, PEB32.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ConsoleFlags {
    pub native: u32,
    pub wow32: Option<u32>,
}

/// Where the `ConsoleFlags` words of one process live in its own address space.
/// The two blocks are located independently, so each carries its own result.
struct Located {
    native: IoResult<usize>,
    /// `Ok(None)` is a confirmed non-WOW64 process.
    wow32: IoResult<Option<usize>>,
}

fn nt_query(process: HANDLE, class: u32, info: *mut c_void, length: usize) -> IoResult<()> {
    let nt = NTDLL
        .as_ref()
        .ok_or_else(|| IoError::other("ntdll.dll could not be loaded"))?;
    let mut returned = 0u32;
    // SAFETY: `info` points to a live local of `length` bytes, `returned` to a
    // live local, and `process` is a handle the caller keeps open for this call.
    let status = unsafe {
        (nt.NtQueryInformationProcess)(process, class, info, length as u32, &mut returned)
    };
    if status < 0 {
        return Err(IoError::other(format!(
            "NtQueryInformationProcess class {class} failed: NTSTATUS {status:#x}"
        )));
    }
    Ok(())
}

fn read_remote<T: Copy + Default>(process: HANDLE, address: usize) -> IoResult<T> {
    let mut value = T::default();
    let mut read = 0usize;
    // SAFETY: the destination is a live local of exactly `size_of::<T>()`
    // bytes; `address` is only a number in the other process, which the kernel
    // validates. Nothing at `address` is dereferenced in this process.
    let ok = unsafe {
        ReadProcessMemory(
            process,
            address as *const c_void,
            &mut value as *mut T as *mut c_void,
            mem::size_of::<T>(),
            &mut read,
        )
    };
    if ok == 0 {
        return Err(IoError::last_os_error());
    }
    if read != mem::size_of::<T>() {
        return Err(IoError::other("short read of process memory"));
    }
    Ok(value)
}

fn write_remote_u32(process: HANDLE, address: usize, value: u32) -> IoResult<()> {
    let mut written = 0usize;
    // SAFETY: the source is a live local u32; `address` is a number in the
    // other process that the kernel validates against its page protection.
    let ok = unsafe {
        WriteProcessMemory(
            process,
            address as *mut c_void,
            &value as *const u32 as *const c_void,
            mem::size_of::<u32>(),
            &mut written,
        )
    };
    if ok == 0 {
        return Err(IoError::last_os_error());
    }
    if written != mem::size_of::<u32>() {
        return Err(IoError::other("short write of process memory"));
    }
    Ok(())
}

fn offset(base: usize, by: usize) -> IoResult<usize> {
    base.checked_add(by)
        .ok_or_else(|| IoError::other("address arithmetic overflow in a foreign process"))
}

/// Locate and sanity-check the parameters block, return the `ConsoleFlags` address.
fn console_flags_address(process: HANDLE, params: usize, flags_offset: usize) -> IoResult<usize> {
    if params == 0 {
        return Err(IoError::other("process parameters block is not set"));
    }
    // MaximumLength and Length are the first two ULONGs of the block. A block
    // that does not at least cover ConsoleFlags means our layout is wrong.
    let maximum: u32 = read_remote(process, params)?;
    let length: u32 = read_remote(process, offset(params, 4)?)?;
    if (length as usize) < flags_offset.saturating_add(4) || maximum < length {
        return Err(IoError::other(format!(
            "unexpected process parameters block (length {length:#x}, maximum {maximum:#x})"
        )));
    }
    offset(params, flags_offset)
}

#[cfg(target_arch = "x86_64")]
fn locate_native(process: HANDLE) -> IoResult<usize> {
    // SAFETY: plain integers and one pointer; all-zero is a valid value.
    let mut basic: ProcessBasicInformation = unsafe { mem::zeroed() };
    nt_query(
        process,
        PROCESS_BASIC_INFORMATION_CLASS,
        &mut basic as *mut _ as *mut c_void,
        mem::size_of::<ProcessBasicInformation>(),
    )?;
    let peb = basic.peb_base_address as usize;
    if peb == 0 {
        return Err(IoError::other("process has no PEB"));
    }
    let params: u64 = read_remote(process, offset(peb, PEB_PROCESS_PARAMETERS)?)?;
    console_flags_address(process, params as usize, PARAMS_CONSOLE_FLAGS)
}

/// A 32-bit child on 64-bit Windows reads its own PEB32, which has its own copy
/// of the block. Nothing here depends on the native block having been located.
#[cfg(target_arch = "x86_64")]
fn locate_wow32(process: HANDLE) -> IoResult<Option<usize>> {
    let mut peb32 = 0usize;
    nt_query(
        process,
        PROCESS_WOW64_INFORMATION_CLASS,
        &mut peb32 as *mut _ as *mut c_void,
        mem::size_of::<usize>(),
    )?;
    // Zero means the child is not a WOW64 process.
    if peb32 == 0 {
        return Ok(None);
    }
    let params32: u32 = read_remote(process, offset(peb32, PEB32_PROCESS_PARAMETERS)?)?;
    console_flags_address(process, params32 as usize, PARAMS32_CONSOLE_FLAGS).map(Some)
}

#[cfg(target_arch = "x86_64")]
fn locate(process: HANDLE) -> Located {
    Located {
        native: locate_native(process),
        wow32: locate_wow32(process),
    }
}

/// One block: read, check the value is plausible, clear bit 0, read back.
/// `Ok(true)` when the bit was set and is now clear, `Ok(false)` when it was
/// already clear. Nothing is written unless the stored value looks like
/// `ConsoleFlags`.
fn clear_block(process: HANDLE, address: usize) -> IoResult<bool> {
    let current: u32 = read_remote(process, address)?;
    if current & !OBSERVED_CONSOLE_FLAGS != 0 {
        return Err(IoError::other(format!(
            "unexpected ConsoleFlags value {current:#x} (only bits {OBSERVED_CONSOLE_FLAGS:#x} were ever seen); not written"
        )));
    }
    if current & CONSOLE_IGNORE_CTRL_C == 0 {
        return Ok(false);
    }
    let cleared = current & !CONSOLE_IGNORE_CTRL_C;
    write_remote_u32(process, address, cleared)?;
    let verified: u32 = read_remote(process, address)?;
    if verified != cleared {
        return Err(IoError::other(format!(
            "ConsoleFlags read-back mismatch: wrote {cleared:#x}, read {verified:#x}"
        )));
    }
    Ok(true)
}

fn release_block(process: HANDLE, located: IoResult<usize>) -> BlockOutcome {
    match located.and_then(|address| clear_block(process, address)) {
        Ok(true) => BlockOutcome::Cleared,
        Ok(false) => BlockOutcome::AlreadyClear,
        Err(error) => BlockOutcome::Failed(error.to_string()),
    }
}

/// Process each located block on its own. A failure in one never prevents the
/// other from being processed, and is never folded into the other's result.
fn release_blocks(process: HANDLE, located: Located) -> CtrlCRelease {
    let native = release_block(process, located.native);
    let wow32 = match located.wow32 {
        Ok(None) => None,
        Ok(Some(address)) => Some(release_block(process, Ok(address))),
        Err(error) => Some(BlockOutcome::Failed(error.to_string())),
    };
    CtrlCRelease::Attempted { native, wow32 }
}

fn machine_name(machine: u16) -> &'static str {
    match machine {
        MACHINE_UNKNOWN => "native",
        MACHINE_I386 => "x86",
        MACHINE_AMD64 => "x64",
        0xaa64 => "arm64",
        0xa641 => "arm64ec",
        0x01c4 => "arm",
        _ => "unrecognized",
    }
}

/// Whether the parameters block of a child can be located with the offsets
/// verified for x86_64. `process_machine` is the child's, `native_machine` the
/// host's. Only a native or 32-bit x86 child on an x86_64 host qualifies.
fn supported_layout(process_machine: u16, native_machine: u16) -> Result<(), String> {
    if native_machine != MACHINE_AMD64 {
        return Err(format!(
            "the host is {} ({native_machine:#06x}), not x86_64, so the process parameters layout is unverified",
            machine_name(native_machine)
        ));
    }
    if process_machine != MACHINE_UNKNOWN && process_machine != MACHINE_I386 {
        return Err(format!(
            "the child is a {} ({process_machine:#06x}) process, whose process parameters layout is unverified",
            machine_name(process_machine)
        ));
    }
    Ok(())
}

/// Ask the OS for the child's and the host's architecture, then apply
/// `supported_layout`. Any failure to ask is a refusal, never a guess.
fn check_architecture(process: HANDLE) -> Result<(), String> {
    // The OS answers for the calling process when given a null handle, and the
    // pseudo-handle for the calling process is -1. Neither is a child, and
    // neither may ever lead to a write.
    if process.is_null() || process == INVALID_HANDLE_VALUE {
        return Err("there is no usable process handle for the child".to_owned());
    }
    let api = KERNEL32_WOW.as_ref().ok_or_else(|| {
        "IsWow64Process2 is not available, so the architecture of the child could not be determined"
            .to_owned()
    })?;
    let (mut process_machine, mut native_machine) = (MACHINE_UNKNOWN, MACHINE_UNKNOWN);
    // SAFETY: both pointers are live locals, and `process` is a handle the
    // caller keeps open for this call.
    let ok = unsafe { (api.IsWow64Process2)(process, &mut process_machine, &mut native_machine) };
    if ok == 0 {
        return Err(format!(
            "IsWow64Process2 failed ({}), so the architecture of the child could not be determined",
            IoError::last_os_error()
        ));
    }
    supported_layout(process_machine, native_machine)
}

/// The reported fallback for a target whose layout was never verified. Nothing
/// is read or written, and the consumer is told.
#[cfg_attr(target_arch = "x86_64", allow(dead_code))]
fn unsupported_release(arch: &str) -> CtrlCRelease {
    CtrlCRelease::NotAttempted(format!(
        "the ConsoleFlags layout is only verified for x86_64 Windows (this build targets {arch})"
    ))
}

/// Read the `ConsoleFlags` words of a process. Needs query and VM-read access.
#[cfg(all(test, target_arch = "x86_64"))]
pub(super) fn read_console_flags(process: HANDLE) -> IoResult<ConsoleFlags> {
    let located = locate(process);
    let native = located.native?;
    Ok(ConsoleFlags {
        native: read_remote(process, native)?,
        wow32: located
            .wow32?
            .map(|address| read_remote(process, address))
            .transpose()?,
    })
}

/// Release the child only when `architecture` accepts it. A refusal reads and
/// writes nothing. The check is a parameter so the refusal can be exercised with
/// a real child on a host that would otherwise always accept it.
#[cfg(target_arch = "x86_64")]
fn release_when(
    process: HANDLE,
    architecture: impl FnOnce(HANDLE) -> Result<(), String>,
) -> CtrlCRelease {
    match architecture(process) {
        Ok(()) => release_blocks(process, locate(process)),
        Err(reason) => CtrlCRelease::NotAttempted(format!("{reason}; nothing was read or written")),
    }
}

/// Clear the inherited "ignore CTRL+C" attribute of a child that has not run yet.
/// The handle needs query, VM-read, VM-write and VM-operation access, which the
/// handle returned by `CreateProcessW` has. Only bit 0 of the child's own
/// blocks is written; the calling process is never modified. The result says,
/// per block, what happened; it never claims more than was confirmed.
pub(super) fn clear_inherited_ctrl_c_ignore(process: HANDLE) -> CtrlCRelease {
    #[cfg(target_arch = "x86_64")]
    {
        release_when(process, check_architecture)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = process;
        unsupported_release(std::env::consts::ARCH)
    }
}

/// Test seam: the real release, run on a duplicate of the child's handle that
/// can only be queried. The OS then refuses to read the child's memory, which
/// is a genuine failure of the release rather than a made-up result.
#[cfg(feature = "uuldana-test-hooks")]
pub(super) fn release_through_a_query_only_handle(process: HANDLE) -> CtrlCRelease {
    use winapi::um::handleapi::{CloseHandle, DuplicateHandle};
    use winapi::um::processthreadsapi::GetCurrentProcess;
    use winapi::um::winnt::PROCESS_QUERY_LIMITED_INFORMATION;
    let mut query_only: HANDLE = std::ptr::null_mut();
    // SAFETY: duplicates a handle the caller keeps open into this process, with
    // reduced access; `query_only` is a live local that receives it.
    let ok = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            process,
            GetCurrentProcess(),
            &mut query_only,
            PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            0,
        )
    };
    if ok == 0 {
        return CtrlCRelease::NotAttempted(format!(
            "a query-only handle could not be derived: {}",
            IoError::last_os_error()
        ));
    }
    let outcome = clear_inherited_ctrl_c_ignore(query_only);
    // SAFETY: the duplicate made above, not used afterwards.
    unsafe { CloseHandle(query_only) };
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::win::psuedocon::PsuedoCon;
    use crate::{native_pty_system, CommandBuilder, PtySize};
    use filedescriptor::{FileDescriptor, Pipe};
    use std::collections::BTreeSet;
    use std::io::{Read, Write};
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use std::process::{Child as StdChild, Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::processthreadsapi::{GetCurrentProcess, OpenProcess};
    use winapi::um::synchapi::CreateEventW;
    use winapi::um::wincon::COORD;
    use winapi::um::winnt::{PROCESS_QUERY_INFORMATION, PROCESS_VM_READ};

    const CREATE_SUSPENDED: u32 = 0x4;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x200;
    const INNER: &str = "UULDANA_TEST_CREATOR_IGNORES_CTRL_C";
    const INNER_FAILED_RELEASE: &str = "UULDANA_TEST_FAILED_RELEASE_CREATOR_IGNORES_CTRL_C";

    fn system_exe(dir: &str, name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join(dir)
            .join(name)
    }

    /// Kills and reaps its child so a failed assertion never leaks a process.
    struct Reaped(StdChild);

    impl Reaped {
        fn handle(&self) -> HANDLE {
            self.0.as_raw_handle() as HANDLE
        }
    }

    impl Drop for Reaped {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// A suspended child whose ignore attribute is set by `CREATE_NEW_PROCESS_GROUP`,
    /// which makes the starting state independent of how the test runner was launched.
    fn suspended_ignoring_child(exe: std::path::PathBuf) -> Reaped {
        Reaped(
            Command::new(exe)
                .args(["/d", "/c", "exit 0"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(CREATE_SUSPENDED | CREATE_NEW_PROCESS_GROUP)
                .spawn()
                .unwrap(),
        )
    }

    fn own_flags() -> ConsoleFlags {
        // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no closing.
        read_console_flags(unsafe { GetCurrentProcess() }).unwrap()
    }

    fn native_address(process: HANDLE) -> usize {
        locate(process).native.unwrap()
    }

    #[test]
    fn clears_only_the_ignore_bit_of_a_suspended_child() {
        let child = suspended_ignoring_child(system_exe("System32", "cmd.exe"));
        let before = read_console_flags(child.handle()).unwrap();
        assert_eq!(before.native & CONSOLE_IGNORE_CTRL_C, CONSOLE_IGNORE_CTRL_C);
        // Give the other bits a value so a full overwrite would be visible.
        let at = native_address(child.handle());
        write_remote_u32(child.handle(), at, before.native | 0x6).unwrap();
        let release = clear_inherited_ctrl_c_ignore(child.handle());
        assert_eq!(release.cleared_blocks(), 1);
        assert_eq!(release.diagnostic(), None);
        let after = read_console_flags(child.handle()).unwrap();
        assert_eq!(after.native, (before.native | 0x6) & !CONSOLE_IGNORE_CTRL_C);
        assert_eq!(after.native & 0x6, 0x6);
        // The second call finds nothing to change and writes nothing.
        let again = clear_inherited_ctrl_c_ignore(child.handle());
        assert_eq!(again.cleared_blocks(), 0);
        assert_eq!(again.diagnostic(), None);
        assert_eq!(read_console_flags(child.handle()).unwrap(), after);
    }

    #[test]
    fn clears_both_parameter_blocks_of_a_32_bit_child() {
        let exe = system_exe("SysWOW64", "cmd.exe");
        if !exe.exists() {
            return;
        }
        let child = suspended_ignoring_child(exe);
        let before = read_console_flags(child.handle()).unwrap();
        assert_eq!(before.native & CONSOLE_IGNORE_CTRL_C, CONSOLE_IGNORE_CTRL_C);
        assert_eq!(
            before.wow32.expect("WOW64 child has a PEB32") & CONSOLE_IGNORE_CTRL_C,
            CONSOLE_IGNORE_CTRL_C
        );
        let release = clear_inherited_ctrl_c_ignore(child.handle());
        assert_eq!(release.cleared_blocks(), 2);
        assert_eq!(release.diagnostic(), None);
        let after = read_console_flags(child.handle()).unwrap();
        assert_eq!(after.native & CONSOLE_IGNORE_CTRL_C, 0);
        assert_eq!(after.wow32.unwrap() & CONSOLE_IGNORE_CTRL_C, 0);
    }

    #[test]
    fn a_handle_without_write_access_fails_and_changes_nothing() {
        let child = suspended_ignoring_child(system_exe("System32", "cmd.exe"));
        let before = read_console_flags(child.handle()).unwrap();
        // SAFETY: plain OpenProcess for the child we own; closed below.
        let readonly =
            unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, child.0.id()) };
        assert!(!readonly.is_null());
        let release = clear_inherited_ctrl_c_ignore(readonly);
        // SAFETY: the handle opened above, not used afterwards.
        unsafe { CloseHandle(readonly) };
        assert!(
            matches!(
                &release,
                CtrlCRelease::Attempted {
                    native: BlockOutcome::Failed(_),
                    ..
                }
            ),
            "{:?}",
            release
        );
        assert_eq!(release.cleared_blocks(), 0);
        assert!(release.diagnostic().unwrap().contains("native block"));
        assert_eq!(read_console_flags(child.handle()).unwrap(), before);
    }

    #[test]
    fn an_exited_child_is_a_reported_failure_not_a_panic() {
        let mut child = Command::new(system_exe("System32", "cmd.exe"))
            .args(["/d", "/c", "exit 0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child.wait().unwrap();
        let handle = child.as_raw_handle() as HANDLE;
        let release = clear_inherited_ctrl_c_ignore(handle);
        assert_eq!(release.cleared_blocks(), 0);
        assert!(release.diagnostic().is_some(), "{:?}", release);
    }

    #[test]
    fn a_null_handle_and_a_handle_of_the_wrong_type_are_refused_without_a_panic() {
        // SAFETY: an unnamed manual-reset event; closed below.
        let event = unsafe { CreateEventW(std::ptr::null_mut(), 1, 0, std::ptr::null()) };
        assert!(!event.is_null());
        // The calling process's own pseudo-handle must never be released either.
        for handle in [std::ptr::null_mut(), event, INVALID_HANDLE_VALUE] {
            let release = clear_inherited_ctrl_c_ignore(handle);
            assert_eq!(release.cleared_blocks(), 0, "{:?}", release);
            // The architecture of such a handle cannot be determined, so nothing
            // is attempted at all, and the refusal is reported.
            assert!(
                matches!(&release, CtrlCRelease::NotAttempted(_)),
                "{:?}",
                release
            );
            let text = release.diagnostic().unwrap();
            assert!(
                text.contains("no usable process handle")
                    || text.contains("could not be determined"),
                "{}",
                text
            );
            assert!(text.contains("nothing was read or written"), "{}", text);
        }
        // SAFETY: the event created above, not used afterwards.
        unsafe { CloseHandle(event) };
    }

    #[test]
    fn an_implausible_parameters_block_is_rejected_before_any_write() {
        // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no closing.
        let me = unsafe { GetCurrentProcess() };
        // A block too short to hold ConsoleFlags, or with Length > MaximumLength.
        let short = [0x10u32, 0x08, 0, 0, 0, 0, 0, 0];
        let inverted = [0x20u32, 0x440, 0, 0, 0, 0, 0, 0];
        let sound = [0x440u32, 0x430, 0, 0, 0, 0, 0, 0];
        for block in [&short, &inverted] {
            assert!(console_flags_address(me, block.as_ptr() as usize, 0x18).is_err());
        }
        let base = sound.as_ptr() as usize;
        assert_eq!(console_flags_address(me, base, 0x18).unwrap(), base + 0x18);
        assert!(console_flags_address(me, 0, 0x18).is_err());
        // An address that cannot be advanced is an error, not a wrapped address.
        assert!(offset(usize::MAX, 4).is_err());
    }

    #[test]
    fn a_value_that_does_not_look_like_console_flags_is_never_written() {
        let child = suspended_ignoring_child(system_exe("System32", "cmd.exe"));
        let at = native_address(child.handle());
        // Bit 0 set and not set: a stray bit outside the seen range blocks both.
        for stray in [0x101u32, 0x100, 0x8000_0001] {
            write_remote_u32(child.handle(), at, stray).unwrap();
            let release = clear_inherited_ctrl_c_ignore(child.handle());
            assert_eq!(release.cleared_blocks(), 0, "{:#x}: {:?}", stray, release);
            let text = release.diagnostic().unwrap();
            assert!(text.contains("unexpected ConsoleFlags value"), "{}", text);
            let kept: u32 = read_remote(child.handle(), at).unwrap();
            assert_eq!(kept, stray, "the foreign value must be left alone");
        }
    }

    #[test]
    fn a_failure_in_one_block_still_processes_the_other() {
        let exe = system_exe("SysWOW64", "cmd.exe");
        if !exe.exists() {
            return;
        }
        let failure = || IoError::other("injected locate failure");
        let wow32_of = |child: &Reaped| {
            locate(child.handle())
                .wow32
                .unwrap()
                .expect("WOW64 child has a PEB32")
        };

        // Native block lost, 32-bit block healthy.
        let child = suspended_ignoring_child(exe.clone());
        let before = read_console_flags(child.handle()).unwrap();
        let release = release_blocks(
            child.handle(),
            Located {
                native: Err(failure()),
                wow32: Ok(Some(wow32_of(&child))),
            },
        );
        let CtrlCRelease::Attempted { native, wow32 } = &release else {
            panic!("{:?}", release);
        };
        assert!(matches!(native, BlockOutcome::Failed(_)), "{:?}", release);
        assert_eq!(*wow32, Some(BlockOutcome::Cleared), "{:?}", release);
        let text = release.diagnostic().unwrap();
        assert!(
            text.contains("native block: injected locate failure"),
            "{}",
            text
        );
        assert!(text.contains("released: 32-bit"), "{}", text);
        let after = read_console_flags(child.handle()).unwrap();
        assert_eq!(after.native, before.native, "the lost block is untouched");
        assert_eq!(after.wow32.unwrap() & CONSOLE_IGNORE_CTRL_C, 0);

        // Native block healthy, 32-bit block lost.
        let child = suspended_ignoring_child(exe.clone());
        let before = read_console_flags(child.handle()).unwrap();
        let release = release_blocks(
            child.handle(),
            Located {
                native: Ok(native_address(child.handle())),
                wow32: Err(failure()),
            },
        );
        let CtrlCRelease::Attempted { native, wow32 } = &release else {
            panic!("{:?}", release);
        };
        assert_eq!(*native, BlockOutcome::Cleared, "{:?}", release);
        assert!(
            matches!(wow32, Some(BlockOutcome::Failed(_))),
            "{:?}",
            release
        );
        let text = release.diagnostic().unwrap();
        assert!(
            text.contains("32-bit block: injected locate failure"),
            "{}",
            text
        );
        assert!(text.contains("released: native"), "{}", text);
        let after = read_console_flags(child.handle()).unwrap();
        assert_eq!(after.native & CONSOLE_IGNORE_CTRL_C, 0);
        assert_eq!(after.wow32, before.wow32, "the lost block is untouched");

        // Native block's write rejected, 32-bit block healthy: a bad address
        // inside the first result is contained in it.
        let child = suspended_ignoring_child(exe);
        let release = release_blocks(
            child.handle(),
            Located {
                native: Ok(1),
                wow32: Ok(Some(wow32_of(&child))),
            },
        );
        let CtrlCRelease::Attempted { native, wow32 } = &release else {
            panic!("{:?}", release);
        };
        assert!(matches!(native, BlockOutcome::Failed(_)), "{:?}", release);
        assert_eq!(*wow32, Some(BlockOutcome::Cleared), "{:?}", release);
    }

    #[test]
    fn diagnostics_exist_only_for_failures() {
        let ok = |native, wow32| CtrlCRelease::Attempted { native, wow32 };
        assert_eq!(ok(BlockOutcome::Cleared, None).diagnostic(), None);
        assert_eq!(ok(BlockOutcome::AlreadyClear, None).diagnostic(), None);
        assert_eq!(
            ok(BlockOutcome::Cleared, Some(BlockOutcome::AlreadyClear)).diagnostic(),
            None
        );
        let failed = ok(BlockOutcome::Failed("no access".into()), None);
        let text = failed.diagnostic().unwrap();
        assert!(text.contains("native block: no access"), "{}", text);
        assert!(text.contains("no block was released"), "{}", text);
        assert!(
            text.contains("interactive CTRL+C may not reach the child"),
            "{}",
            text
        );
    }

    #[test]
    fn an_unverified_architecture_is_reported_and_touches_nothing() {
        for arch in ["aarch64", "x86", "arm64ec"] {
            let release = unsupported_release(arch);
            assert!(matches!(release, CtrlCRelease::NotAttempted(_)));
            assert_eq!(release.cleared_blocks(), 0);
            let text = release.diagnostic().unwrap();
            assert!(text.contains(arch), "{}", text);
            assert!(text.contains("only verified for x86_64"), "{}", text);
        }
    }

    const ARM64: u16 = 0xaa64;
    const ARM64EC: u16 = 0xa641;

    #[test]
    fn only_a_native_or_x86_child_on_an_x86_64_host_has_a_verified_layout() {
        for child in [MACHINE_UNKNOWN, MACHINE_I386] {
            assert_eq!(supported_layout(child, MACHINE_AMD64), Ok(()));
        }
        // An x64 process reported by name is an emulated one on another host.
        for child in [MACHINE_AMD64, ARM64, ARM64EC, 0x01c4, 0x1234] {
            let text = supported_layout(child, MACHINE_AMD64).unwrap_err();
            assert!(
                text.contains("child") && text.contains("unverified"),
                "{}",
                text
            );
        }
        for host in [ARM64, ARM64EC, MACHINE_I386, MACHINE_UNKNOWN, 0x1234] {
            for child in [MACHINE_UNKNOWN, MACHINE_I386, MACHINE_AMD64, ARM64] {
                let text = supported_layout(child, host).unwrap_err();
                assert!(
                    text.contains("not x86_64"),
                    "{:#x}/{:#x}: {}",
                    host,
                    child,
                    text
                );
            }
        }
        assert!(supported_layout(MACHINE_UNKNOWN, ARM64)
            .unwrap_err()
            .contains("arm64"));
    }

    fn machines_of(process: HANDLE) -> (u16, u16) {
        let api = KERNEL32_WOW.as_ref().expect("IsWow64Process2 on this host");
        let (mut process_machine, mut native_machine) = (0u16, 0u16);
        // SAFETY: live locals, and a handle the caller keeps open.
        let ok =
            unsafe { (api.IsWow64Process2)(process, &mut process_machine, &mut native_machine) };
        assert_ne!(ok, 0, "{}", IoError::last_os_error());
        (process_machine, native_machine)
    }

    /// The OS classification of real children on this machine decides the guard.
    /// This host is native x86_64, so a native child and a SysWOW64 child pass.
    #[test]
    fn real_children_are_classified_by_the_os() {
        // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no closing.
        let (_, host) = machines_of(unsafe { GetCurrentProcess() });
        for (dir, expected) in [("System32", MACHINE_UNKNOWN), ("SysWOW64", MACHINE_I386)] {
            let exe = system_exe(dir, "cmd.exe");
            if !exe.exists() {
                continue;
            }
            let child = suspended_ignoring_child(exe);
            let (process_machine, native) = machines_of(child.handle());
            assert_eq!(native, host);
            if host == MACHINE_AMD64 {
                assert_eq!(process_machine, expected, "{}", dir);
                assert_eq!(check_architecture(child.handle()), Ok(()), "{}", dir);
            } else {
                assert!(check_architecture(child.handle()).is_err(), "{}", dir);
            }
        }
    }

    /// A refused architecture reads nothing and writes nothing. A null handle
    /// would turn any attempt into a failed block, so `NotAttempted` shows that
    /// the memory functions were never reached; a real child shows that its
    /// flags did not change.
    #[test]
    fn a_refused_architecture_reads_and_writes_nothing() {
        let attempted = release_when(std::ptr::null_mut(), |_| Ok(()));
        assert!(
            matches!(attempted, CtrlCRelease::Attempted { .. }),
            "{:?}",
            attempted
        );
        let child = suspended_ignoring_child(system_exe("System32", "cmd.exe"));
        let before = read_console_flags(child.handle()).unwrap();
        assert_eq!(before.native & CONSOLE_IGNORE_CTRL_C, CONSOLE_IGNORE_CTRL_C);
        for (process_machine, native_machine) in [
            (MACHINE_AMD64, ARM64),
            (ARM64, ARM64),
            (ARM64EC, ARM64),
            (MACHINE_UNKNOWN, ARM64),
            (MACHINE_I386, ARM64),
            (MACHINE_UNKNOWN, 0x1234),
        ] {
            let refusal = || supported_layout(process_machine, native_machine);
            for handle in [std::ptr::null_mut(), child.handle()] {
                let release = release_when(handle, |_| refusal());
                assert!(
                    matches!(release, CtrlCRelease::NotAttempted(_)),
                    "{:?}",
                    release
                );
                assert_eq!(release.cleared_blocks(), 0);
                let text = release.diagnostic().unwrap();
                assert!(text.contains("nothing was read or written"), "{}", text);
                assert!(text.contains("unverified"), "{}", text);
            }
            assert_eq!(read_console_flags(child.handle()).unwrap(), before);
        }
        // A check that cannot even be made is a refusal too.
        let release = release_when(child.handle(), |_| Err("the answer was unavailable".into()));
        assert!(
            matches!(release, CtrlCRelease::NotAttempted(_)),
            "{:?}",
            release
        );
        assert_eq!(read_console_flags(child.handle()).unwrap(), before);
        // Control: the same child, accepted, is released.
        let release = release_when(child.handle(), |_| {
            supported_layout(MACHINE_UNKNOWN, MACHINE_AMD64)
        });
        assert_eq!(release.cleared_blocks(), 1, "{:?}", release);
        assert_eq!(release.diagnostic(), None);
    }

    /// The creator's attribute is never written, however many children are
    /// cleared at once, so no thread can ever observe a transient creator state.
    #[test]
    fn concurrent_clearing_never_changes_the_creator() {
        let baseline = own_flags();
        let stop = Arc::new(AtomicBool::new(false));
        let sampler = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut seen = BTreeSet::new();
                while !stop.load(Ordering::Acquire) {
                    let flags = own_flags();
                    seen.insert((flags.native, flags.wow32));
                }
                seen
            })
        };
        let workers: Vec<_> = (0..4)
            .map(|_| {
                std::thread::spawn(|| {
                    for _ in 0..8 {
                        let child = suspended_ignoring_child(system_exe("System32", "cmd.exe"));
                        let release = clear_inherited_ctrl_c_ignore(child.handle());
                        assert_eq!(release.cleared_blocks(), 1, "{:?}", release);
                        let after = read_console_flags(child.handle()).unwrap();
                        assert_eq!(after.native & CONSOLE_IGNORE_CTRL_C, 0);
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        stop.store(true, Ordering::Release);
        let seen = sampler.join().unwrap();
        assert_eq!(seen, BTreeSet::from([(baseline.native, baseline.wow32)]));
        assert_eq!(own_flags(), baseline);
    }

    /// Re-runs one test of this binary in a child created in its own process
    /// group, exactly the state `bash.exe` passes on, with `variable` set.
    /// A creator cannot be simulated inside this process without touching
    /// every other test. Returns the inner test's stdout.
    fn run_inside_ignoring_creator(inner_test: &str, variable: &str, marker: &str) {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", inner_test, "--nocapture", "--test-threads=1"])
            .env(variable, "1")
            .creation_flags(CREATE_NEW_PROCESS_GROUP)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "{}\n{}",
            stdout,
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            stdout.contains(marker),
            "inner test did not run: {}",
            stdout
        );
    }

    /// The real spawn path, with a creator that carries the attribute.
    #[test]
    fn pty_children_are_released_from_an_ignoring_creator() {
        run_inside_ignoring_creator(
            "win::consoleflags::tests::inner_pty_children_are_released",
            INNER,
            "creator-ignore-verified",
        );
    }

    #[test]
    fn inner_pty_children_are_released() {
        if std::env::var_os(INNER).is_none() {
            return;
        }
        let creator = own_flags();
        assert_eq!(
            creator.native & CONSOLE_IGNORE_CTRL_C,
            CONSOLE_IGNORE_CTRL_C,
            "precondition: this process must carry the attribute"
        );
        let stop = Arc::new(AtomicBool::new(false));
        let sampler = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut seen = BTreeSet::new();
                while !stop.load(Ordering::Acquire) {
                    seen.insert(own_flags().native);
                }
                seen
            })
        };
        let workers: Vec<_> = (0..3)
            .map(|_| {
                std::thread::spawn(|| {
                    for _ in 0..3 {
                        let pair = native_pty_system().openpty(PtySize::default()).unwrap();
                        let mut reader = pair.master.try_clone_reader().unwrap();
                        let mut writer = pair.master.take_writer().unwrap();
                        // Service ConPTY's startup cursor request while the child runs.
                        std::thread::spawn(move || {
                            let mut bytes = Vec::new();
                            let mut chunk = [0; 8192];
                            while let Ok(n) = reader.read(&mut chunk) {
                                if n == 0 {
                                    break;
                                }
                                bytes.extend_from_slice(&chunk[..n]);
                                if bytes.windows(4).any(|b| b == b"\x1b[6n") {
                                    let _ = writer.write_all(b"\x1b[1;1R");
                                    let _ = writer.flush();
                                    bytes.clear();
                                }
                            }
                        });
                        let mut command = CommandBuilder::new(system_exe("System32", "cmd.exe"));
                        command.args(["/d", "/c", "exit 0"]);
                        let guard = pair.slave.spawn_command_suspended(command).unwrap();
                        assert_eq!(guard.ctrl_c_release_diagnostic(), None);
                        let handle = subc_jobobject::ProcessHandle::handle(&guard).unwrap();
                        let flags = read_console_flags(handle as HANDLE).unwrap();
                        assert_eq!(flags.native & CONSOLE_IGNORE_CTRL_C, 0, "{:?}", flags);
                        println!("pty-child-flags {:#x}", flags.native);
                        let job = subc_jobobject::JobObject::new().unwrap();
                        let mut child = guard.resume_in_job(&job).unwrap();
                        assert!(crate::Child::wait(&mut *child).unwrap().success());
                        drop(pair.slave);
                        let master = pair.master;
                        std::thread::spawn(move || drop(master));
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        stop.store(true, Ordering::Release);
        let seen = sampler.join().unwrap();
        assert_eq!(
            seen,
            BTreeSet::from([creator.native]),
            "creator state moved"
        );
        assert_eq!(own_flags(), creator);
        println!("creator-ignore-verified");
    }

    /// A pseudoconsole whose output is drained on a thread that also answers
    /// ConPTY's startup cursor request. Dropping the console closes it.
    struct ServicedConsole {
        con: Option<PsuedoCon>,
        input: Arc<Mutex<FileDescriptor>>,
        output: Arc<Mutex<Vec<u8>>>,
    }

    impl ServicedConsole {
        fn new() -> Self {
            let stdin = Pipe::new().unwrap();
            let stdout = Pipe::new().unwrap();
            let con = PsuedoCon::new(COORD { X: 80, Y: 24 }, stdin.read, stdout.write).unwrap();
            let input = Arc::new(Mutex::new(stdin.write));
            let output = Arc::new(Mutex::new(Vec::new()));
            let (answer, seen) = (Arc::clone(&input), Arc::clone(&output));
            let mut reader = stdout.read;
            std::thread::spawn(move || {
                let mut chunk = [0; 8192];
                while let Ok(n) = reader.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    let mut all = seen.lock().unwrap();
                    all.extend_from_slice(&chunk[..n]);
                    if chunk[..n].windows(4).any(|b| b == b"\x1b[6n") {
                        let mut writer = answer.lock().unwrap();
                        let _ = writer.write_all(b"\x1b[1;1R");
                        let _ = writer.flush();
                    }
                }
            });
            Self {
                con: Some(con),
                input,
                output,
            }
        }

        fn con(&self) -> &PsuedoCon {
            self.con.as_ref().unwrap()
        }

        fn send(&self, bytes: &[u8]) {
            let mut writer = self.input.lock().unwrap();
            writer.write_all(bytes).unwrap();
            writer.flush().unwrap();
        }

        fn wait_for_output(&self, needle: &str, within: Duration) {
            let deadline = Instant::now() + within;
            while Instant::now() < deadline {
                if String::from_utf8_lossy(&self.output.lock().unwrap()).contains(needle) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            panic!(
                "no {needle:?} in {:?}",
                String::from_utf8_lossy(&self.output.lock().unwrap())
            );
        }
    }

    impl Drop for ServicedConsole {
        fn drop(&mut self) {
            // ClosePseudoConsole can block on pending output; keep it off this thread.
            if let Some(con) = self.con.take() {
                std::thread::spawn(move || drop(con));
            }
        }
    }

    fn ping_command() -> CommandBuilder {
        let mut command = CommandBuilder::new(system_exe("System32", "ping.exe"));
        command.args(["-n", "30", "127.0.0.1"]);
        command
    }

    /// With the release failing, the child keeps what its creator passed on, so
    /// 0x03 cannot interrupt it. Stopping it then must still be bounded and the
    /// failure must be reported. The control with the real release, inside the
    /// same ignoring creator, is interrupted at once.
    #[test]
    fn a_failed_release_keeps_ctrl_c_ignored_and_stopping_the_child_stays_bounded() {
        run_inside_ignoring_creator(
            "win::consoleflags::tests::inner_failed_release_stays_bounded",
            INNER_FAILED_RELEASE,
            "failed-release-verified",
        );
    }

    #[test]
    fn inner_failed_release_stays_bounded() {
        if std::env::var_os(INNER_FAILED_RELEASE).is_none() {
            return;
        }
        assert_eq!(
            own_flags().native & CONSOLE_IGNORE_CTRL_C,
            CONSOLE_IGNORE_CTRL_C,
            "precondition: this process must carry the attribute"
        );

        // Control: the real release lets 0x03 interrupt the child (STATUS_CONTROL_C_EXIT).
        let console = ServicedConsole::new();
        let guard = console
            .con()
            .spawn_command_suspended(ping_command())
            .unwrap();
        assert_eq!(guard.ctrl_c_release_diagnostic(), None);
        let job = subc_jobobject::JobObject::new().unwrap();
        let mut control = guard.resume_in_job(&job).unwrap();
        console.wait_for_output("Pinging", Duration::from_secs(10));
        console.send(&[3]);
        let started = Instant::now();
        while control.try_wait().unwrap().is_none() {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "control not interrupted"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(control.wait().unwrap().exit_code(), 0xC000_013A);
        drop(console);

        // Failure: the injected release leaves the attribute, and says so.
        let console = ServicedConsole::new();
        let guard = console
            .con()
            .spawn_command_suspended_with(ping_command(), |_| CtrlCRelease::Attempted {
                native: BlockOutcome::Failed("injected release failure".into()),
                wow32: None,
            })
            .unwrap();
        let text = guard
            .ctrl_c_release_diagnostic()
            .expect("failure must be reported");
        assert!(text.contains("injected release failure"), "{}", text);
        let handle = subc_jobobject::ProcessHandle::handle(&guard).unwrap();
        let flags = read_console_flags(handle as HANDLE).unwrap();
        assert_eq!(
            flags.native & CONSOLE_IGNORE_CTRL_C,
            CONSOLE_IGNORE_CTRL_C,
            "{:?}",
            flags
        );
        let job = subc_jobobject::JobObject::new().unwrap();
        let mut child = guard.resume_in_job(&job).unwrap();
        console.wait_for_output("Pinging", Duration::from_secs(10));
        console.send(&[3]);
        std::thread::sleep(Duration::from_millis(1500));
        assert!(
            child.try_wait().unwrap().is_none(),
            "0x03 must not interrupt a child that kept the ignore attribute"
        );
        // Stopping it stays bounded: terminate, then the exit is observed.
        let stopping = Instant::now();
        let _ = child.kill();
        while child.try_wait().unwrap().is_none() {
            assert!(
                stopping.elapsed() < Duration::from_secs(5),
                "child not stopped"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!child.wait().unwrap().success());
        assert_eq!(
            job.process_count().unwrap(),
            0,
            "no process may remain in the job"
        );
        println!("failed-release-verified");
    }

    fn failing_release(_: HANDLE) -> CtrlCRelease {
        CtrlCRelease::Attempted {
            native: BlockOutcome::Failed("injected release failure".into()),
            wow32: Some(BlockOutcome::Failed("injected 32-bit failure".into())),
        }
    }

    /// Fail-open: a failed release is kept on the guard, and the child still
    /// starts, runs and exits with its own status.
    #[test]
    fn a_failed_release_is_reported_and_the_child_still_runs_to_completion() {
        let console = ServicedConsole::new();
        let mut command = CommandBuilder::new(system_exe("System32", "cmd.exe"));
        command.args(["/d", "/c", "exit 7"]);
        let guard = console
            .con()
            .spawn_command_suspended_with(command, failing_release)
            .unwrap();
        let text = guard.ctrl_c_release_diagnostic().unwrap();
        assert!(
            text.contains("native block: injected release failure"),
            "{}",
            text
        );
        assert!(
            text.contains("32-bit block: injected 32-bit failure"),
            "{}",
            text
        );
        let status = guard.process_status().unwrap();
        assert!(
            !status.has_exited().unwrap(),
            "the child must not have run yet"
        );
        let job = subc_jobobject::JobObject::new().unwrap();
        let mut child = guard.resume_in_job(&job).unwrap();
        assert_eq!(
            child.wait().unwrap().exit_code(),
            7,
            "the status is the child's own"
        );
        assert!(status.has_exited().unwrap());
    }

    /// The seam used by the transport's regression test: the real release code,
    /// refused by the OS because the handle may only query. Needs
    /// `--features uuldana-test-hooks`.
    #[cfg(feature = "uuldana-test-hooks")]
    #[test]
    fn the_test_hook_makes_the_real_release_fail_through_the_os() {
        let console = ServicedConsole::new();
        let mut command = CommandBuilder::new(system_exe("System32", "cmd.exe"));
        command.args(["/d", "/c", "exit 7"]);
        let guard = console
            .con()
            .spawn_command_suspended_with_failing_release(command)
            .unwrap();
        let text = guard
            .ctrl_c_release_diagnostic()
            .expect("the refusal must be reported");
        assert!(text.contains("native block"), "{}", text);
        assert!(text.contains("os error 5"), "{}", text);
        assert!(
            text.contains("interactive CTRL+C may not reach the child"),
            "{}",
            text
        );
        let job = subc_jobobject::JobObject::new().unwrap();
        let mut child = guard.resume_in_job(&job).unwrap();
        assert_eq!(child.wait().unwrap().exit_code(), 7);
    }

    /// Ownership after a failed release: dropping the guard without a resume
    /// still terminates the never-run child within the guard's own bound.
    #[test]
    fn an_abandoned_guard_after_a_failed_release_terminates_the_child_in_bound() {
        let console = ServicedConsole::new();
        let guard = console
            .con()
            .spawn_command_suspended_with(ping_command(), failing_release)
            .unwrap();
        assert!(guard.ctrl_c_release_diagnostic().is_some());
        let status = guard.process_status().unwrap();
        assert!(!status.has_exited().unwrap());
        let started = Instant::now();
        drop(guard);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "guard cleanup took {:?}",
            started.elapsed()
        );
        assert!(
            status.has_exited().unwrap(),
            "the abandoned child must be gone"
        );
    }
}
