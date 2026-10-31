# Uuldana Windows containment patch

Upstream: `portable-pty` **0.9.0**, crates.io source archive, MIT license retained in `LICENSE.md`. `.cargo_vcs_info.json` records upstream revision. `UPSTREAM_HASHES.json` records SHA-256 of every copied file **before** modifications. Unix sources and ordinary upstream spawn behavior are unchanged. The workspace selects this dependency through `[patch.crates-io]` and excludes it from workspace membership; all Uuldana crates continue to inherit `unsafe_code = "forbid"`.

## Decision and API support

Microsoft supports two relevant mechanisms:

1. Set `PROC_THREAD_ATTRIBUTE_JOB_LIST` together with `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` in `STARTUPINFOEXW`. Job assignment then occurs during `CreateProcessW`, before execution. It is supported on Windows 10 / Server 2016 and newer. This also avoids a creator-crash window between suspended creation and assignment. The existing `subc-jobobject` safe API does not expose a borrowed Job handle for that attribute; implementing it here would require patching another dependency or replacing Job ownership.
2. Create with `CREATE_SUSPENDED`, assign to a Job using the process handle, then resume the **original primary-thread handle**. This is the selected implementation. It closes the execution/descendant escape window, keeps the existing Job implementation, and avoids thread discovery by PID. A helper/bootstrap executable would change the native process boundary and require a separate protocol/distribution surface, so is not used.

The ConPTY minimum remains **Windows 10 version 1809 / Windows Server 2019**. `CREATE_SUSPENDED`, `AssignProcessToJobObject`, `ResumeThread`, and zero-time `WaitForSingleObject` predate that floor. Nested Jobs are supported since Windows 8 / Server 2012. A restrictive ancestor Job can still reject assignment; failure terminates the never-run child instead of falling back to uncontained execution. Minimum-version compatibility is established from API documentation, not from executing an 1809 machine.

Sources (checked 2026-10-12):

- [Process creation flags](https://learn.microsoft.com/en-us/windows/win32/procthread/process-creation-flags)
- [ResumeThread](https://learn.microsoft.com/en-us/windows/desktop/api/processthreadsapi/nf-processthreadsapi-resumethread)
- [AssignProcessToJobObject](https://learn.microsoft.com/en-us/windows/win32/api/jobapi2/nf-jobapi2-assignprocesstojobobject)
- [Startup attribute list, including JOB_LIST](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-updateprocthreadattribute)
- [CreatePseudoConsole and cursor handshake](https://learn.microsoft.com/en-us/windows/console/createpseudoconsole)
- [Microsoft's comparison of suspended creation and Job-list creation](https://devblogs.microsoft.com/oldnewthing/20230209-00/?p=107812)

## Safe boundary and ownership review

`SlavePty::spawn_command_suspended` has a fail-closed default; ConPTY implements it. It returns `SuspendedChild`, whose constructor is private to the Windows backend. Successful `CreateProcessW` transfers its two returned handles exactly once into `OwnedHandle`; the process and thread are never looked up by PID. Only this internal creation path supplies `CREATE_SUSPENDED`.

`SuspendedChild::resume_in_job` is the sole public resume operation. Assignment must succeed before `ResumeThread` is called. The original thread handle is retained until this call returns. One call releases the creation suspension; a previous count greater than one is successful and leaves any other component's suspension intact. Stress testing observed a count of two; its external owner was not identified. The implementation never loops to consume someone else's suspension. A zero count or native failure is rejected. On assignment/resume failure or unwinding, the guard stays armed: Drop terminates the child by its owned handle and waits at most five seconds. On success it transfers the child and closes only the thread handle. The caller retains the kill-on-close Job in the transport tree. No breakaway flag or Job limit is enabled. Cleanup errors are logged; they are not hidden behind a successful launch.

The containment unsafe calls are isolated in `src/win/containment.rs`: `ResumeThread`, `TerminateProcess`, and two uses of `WaitForSingleObject`. Each uses an owned live kernel handle, with no caller-provided memory addresses. This statement covers `containment.rs` only. The later Ctrl+C release (see "Inherited Ctrl+C disposition") adds cross-process memory reads and writes in `src/win/consoleflags.rs`; those do use addresses taken from the child, and are described there. The finite Drop wait cannot deadlock the async executor because creation/resume execute on the blocking spawn worker. `ProcessStatus` duplicates a process handle through the existing safe filedescriptor API and uses a zero-time wait; it distinguishes an exited process with code 259 from a live process. A retained observer prevents PID-reuse-based classification. No new unsafe block or lint exception is added to a Uuldana workspace crate.

The added trait methods affect only this dependency, not the frozen Uuldana core/SDK contract 0.2. Raw terminal bytes, command construction, environment, current directory, ConPTY attributes, resize and Ctrl+C input paths stay with the original backend. `src/win/psuedocon.rs` changes the process creation flags, the ownership of the returned primary-thread handle, and, for the suspended path only, calls the Ctrl+C release on the never-run child before handing the guard back (see "Inherited Ctrl+C disposition"). The path that carries `0x03` to the pseudoconsole is unchanged. Ordinary `spawn_command` remains available for upstream compatibility; Uuldana uses only the suspended path on Windows.

## Limits and regression evidence

Suspended creation is execution-atomic containment, **not kernel-atomic creation-and-assignment**. Abrupt creator termination between `CreateProcessW` and Job assignment can leave a suspended, never-executed orphan. Rust unwinding and ordinary error/drop paths are guarded; arbitrary OS termination cannot run Drop. Job-list creation would eliminate that distinct crash window and remains a documented risk, also present in the existing suspended pipe launcher. This patch does not claim an OS sandbox or containment of a privileged adversary.

Workspace tests service the ConPTY cursor handshake during the suspended interval, assert no execution marker before assignment, then assert immediate parent+descendant Job membership. Removing `CREATE_SUSPENDED` makes that test fail. Other tests cover abandoned guards, native assignment rejection, repeated immediate descendants during force/drop, natural/graceful/forced supervisor cleanup, PTY input/output/resize/Ctrl+C and exit code 259. A dependency-local native test deliberately holds an additional suspension and verifies that launch assigns the Job, releases only its own suspension, then exits successfully when the test releases its hold. `Cargo.lock` pins that separate test build; use `CARGO_TARGET_DIR` under the workspace's ignored `target/`. 

## Native exit-status polling correction (2026-10-14)

`src/win/mod.rs` now checks the existing safe `ProcessStatus::has_exited()`
observer before treating a native exit code as final. Upstream `is_complete`
treated every code 259 as STILL_ACTIVE, including an already-signaled process
that actually exited with 259. A retained-handle regression reproduces that
failure independently of ConPTY and compares the backend with standard native
wait for 0, 259, `0xC000004B`, and `0xFFFFFFFF`. Another regression checks a
live, unsignaled process. The same polling path serves `try_wait`, `wait`'s
fast path, and Future polling. Handle duplication/query failures propagate as
I/O errors, and `wait` propagates polling errors instead of ignoring them.

The correction uses the existing safe observer and existing native query. It
adds no native API, unsafe block, launcher change, status normalization, or
contract change. It fixes confirmed 259 polling ambiguity; it does **not**
establish the cause of the historical unexpected `0xC000004B` status. The cause of the historical unexpected `0xC000004B` status is still open.

## Wait-error and async worker handling

`WinChild::wait_for_exit` accepts only `WAIT_OBJECT_0`
before querying final status. `WAIT_FAILED` returns the original OS error;
unexpected wait results return an I/O error. A queryable process handle lacking
SYNCHRONIZE rights tests ERROR_ACCESS_DENIED through both blocking wait and
the async worker, preventing a failed wait from becoming successful completion.

The Future uses one named wait thread with an owned duplicate process handle.
It stores the result and current waker behind a mutex, updates the waker on
subsequent polls, and wakes outside the lock after publishing the result.
Dropping the owner cannot close the worker's in-flight handle. Thread creation,
duplication, wait and query errors propagate. The worker regression covers owner
drop and waking the latest poller. The six-entry Windows-specific unit suite also retains
live-process polling, final 259 and DWORD-bit preservation controls, and the
child-fixture entry.

Test fixture paths now use the existing PID/time/atomic-sequence helper,
including the four early-containment tests. This prevents same-process clock
collisions while retaining their existing file and descendant cleanup.
Both the GNU and the MSVC toolchain pass workspace formatting, strict locked Clippy,
locked checks, workspace tests, the scripted PTY probe, the portable-pty unit
entries (six Windows-specific plus two command-builder tests) and the
subc-jobobject tests.

## Inherited Ctrl+C disposition

`SetConsoleCtrlHandler(NULL, TRUE)` and `CREATE_NEW_PROCESS_GROUP` set an
inheritable "ignore CTRL+C" attribute (bit 0 of `ConsoleFlags` in the process
parameters block of the PEB). The Windows documentation states that child
processes inherit it, and offers no call to read it back. An MSYS `bash.exe`
carries it, so a ConPTY child started from Git Bash inherited it and a `0x03`
written to the pseudoconsole never interrupted the child (the process-level
`Interrupt` step then waited until its deadline).

`SlavePty::spawn_command_suspended` now releases the child from that attribute
while it is still suspended and has executed nothing (`src/win/consoleflags.rs`).
Only bit 0 of the child's own parameter block is cleared, through
`NtQueryInformationProcess`, `ReadProcessMemory` and `WriteProcessMemory` on the
handle `CreateProcessW` returned, with a read-back check. A 32-bit child on
64-bit Windows has a second block (PEB32), which is cleared as well. The
creating process's own attribute is never read for modification, toggled, or
restored: no process-wide state is touched, so concurrent spawns, including
those made by code outside this crate, cannot observe a transient state.

### What the patch does to the child, exactly

For a suspended child, in this order, all on the handle `CreateProcessW` returned:

1. `IsWow64Process2` is asked for the child's and the host's machine type (see the architecture guard below). A refusal ends the release here.
2. `NtQueryInformationProcess` (`ProcessBasicInformation`, and `ProcessWow64Information`, class 26) locates the child's PEB, and for a 32-bit child its PEB32.
3. `ReadProcessMemory` reads the process parameters pointer, the block's `MaximumLength` and `Length`, and the stored `ConsoleFlags` word.
4. Only if the block is long enough, the stored value uses only bits 0 to 2, and bit 0 is set: one `WriteProcessMemory` of one `u32` to that address, with bit 0 cleared and the other bits unchanged, then a read-back that must match.

Nothing is written to the creating process, to any other process, or to any other field. The creator's own attribute is only ever read, by tests. No handle other than the child's is opened, no global console state is toggled, and no retry or repair is attempted after a failure. The block layout, the field offsets and the meaning of the bit are not documented by Microsoft (`ProcessBasicInformation` is documented in `winternl.h`; class 26 and the `ConsoleFlags` field are not). They are inferred from observation on one build, which is the reason for the guards and the diagnostics below.

### Reporting a failed release

A failure never stops the launch (fail-open): the child starts exactly as it
did before this change, and stopping it then relies on the later escalation
steps, bounded by the caller's grace period. It is not silent. The release
returns a result per block (cleared, already clear, or failed with the reason),
or "not attempted" with the reason when no block was touched.
`SuspendedChild::ctrl_c_release_diagnostic()` turns a failed, partial or
refused result into text, and the transport reads it before `resume_in_job` and
queues it as a `Frame::Diagnostic` ahead of the exit frame. The runtime
supervisor turns that frame into an `AgentEvent::Diagnostic`. Nothing here needs
a logging backend; the `log::warn!` that is also emitted is only a courtesy for
embedders that install one. A success, or an attribute that was already clear,
produces no diagnostic.

The two blocks are located and processed independently. If the native block
cannot be processed, the 32-bit block of a WOW64 child is still processed, and
the other way round; the diagnostic names which block failed and which was
released. A child whose WOW64 state cannot be determined is reported as a
failed 32-bit block, never assumed to be native only. A partial release does
not guarantee that Ctrl+C reaches a 32-bit child.

When the transport's wait worker itself fails, the delivery differs by path:

- `Process::next_frame` (corrected): diagnostics still queued, this one
  included, are delivered first as `Frame::Diagnostic`, then the worker failure
  is reported exactly once as an error, then the stream ends. No exit frame is
  invented. This holds at all three places where `next_frame` observes the
  failure (a completed wait result, a closed wait channel, and a failure that
  wakes a pending read).
- `Process::force_terminate` (known limitation, not corrected): when it
  observes the same worker failure it still calls `Process::wait_failed`, which
  retires the transport at once and returns the error. Diagnostics still queued
  at that moment are not delivered. The run still ends as a process failure and
  the runtime records the cleanup failure; only the secondary text is lost. No
  test covers this path.

### Runtime architecture guard

The offsets below exist only in builds for `x86_64`, but a build for `x86_64`
also runs on Windows on ARM, under emulation, where the child may be native
ARM64 or an emulated x64 or x86 process. Before step 2 above, the release asks
`IsWow64Process2` (kernel32, Windows 10 1709 and newer, loaded dynamically; if it
is missing the answer is a refusal) and proceeds only when the host is `x86_64`
(`IMAGE_FILE_MACHINE_AMD64`) and the child is either native (`UNKNOWN`) or a
32-bit x86 WOW64 process (`I386`). Every other answer, a failed query, a null
handle, or the pseudo-handle of the calling process is a refusal: nothing is
read or written, and the diagnostic names the reported machine types.

This is a conservative allow list, not a statement that ARM64 is broken. How
`IsWow64Process2` reports an emulated x64 process, and what its parameters block
looks like, was not established. No ARM64 build or ARM64 machine was used.
**No ARM64 compatibility is claimed.**

### What is and is not verified

The parameter block layout is not documented, and a check cannot prove it.
The offsets (`PEB+0x20`, block `+0x18`; PEB32 `+0x10`, block `+0x14`) are
verified for x86_64 Windows 11 build 10.0.26300 only. Two guards make a wrong
guess less likely to be written, and neither proves the layout:

- the block's `MaximumLength` and `Length` must cover `ConsoleFlags`;
- the stored value may only use bits 0 to 2, the bits seen in practice
  (0x0 and 0x1 in creators and suspended children, 0x6 and 0x7 in pseudoconsole
  children). A value with any other bit is reported and left untouched. This is
  an empirical limit from one Windows build, so a future build that defines
  another low bit would be reported instead of repaired.

Unsupported or unverified, so reported or simply not proven:

- Windows on ARM: native ARM64 children, emulated x64 and x86 children,
  ARM64EC. Refused at run time by the guard above; the refusal path is tested
  with machine values supplied by the test, not on ARM hardware.
- x86 hosts and 32-bit builds of this crate: compiled to the reported fallback
  (`target_arch` is not `x86_64`); nothing is read or written.
- Windows builds other than 10.0.26300 (x64), including every older Windows 10
  and Windows Server release; the layout and the observed bit values may differ.
- Children other than the ones tested (`ping`, `cmd`, test fixtures) in native
  and SysWOW64 form; interactive shells, agent CLIs, mintty and WSL are untested.
- Security software other than the Windows Defender configuration recorded in
  the evidence. Writing another process's memory while it is suspended is the
  same API pattern used by process tampering, so an EDR product may flag or
  block it. A block would surface as a failed release (access denied) and fall
  to the fail-open path above; it has not been observed or tested.
- Interruption of a 32-bit child was observed on the prototype build only, not
  rerun on the remediated code or with the architecture guard.

The ordinary non-suspended `spawn_command` is not changed because the child is
already running there; Uuldana uses only the suspended path on Windows.

### Tests

`win::consoleflags::tests`, including re-runs of the spawn path inside a
creator that carries the attribute, with the real release (children are
interrupted) and with an injected failing release (children keep the attribute,
`0x03` does not interrupt them, and stopping them stays bounded); the machine
type table and the refusal tests for the architecture guard.

The Cargo feature `uuldana-test-hooks` (off by default, enabled only through the
transport crate's `[dev-dependencies]`) adds one hidden `SlavePty` method that
runs the real release on a handle that can only query the child, so the OS
genuinely refuses the memory read. The transport's regression test uses it to
follow a real failure through `ctrl_c_release_diagnostic()`, the transport's
`suspended_ctrl_c_diagnostic` and the frame queue to the consumer. A normal
build does not contain the method.
