# Windows development

The workspace uses Rust edition 2024. Local validation uses Windows 11 x64,
Rust 1.96.0 (`stable-x86_64-pc-windows-gnu`), and GNU binutils from MSYS2.
There is no declared minimum supported Rust version. Windows CI jobs select GNU
and MSVC toolchains explicitly; Linux and macOS run the workspace checks.
CI configuration is not evidence that those hosted jobs have already passed.

Windows launch containment differs by transport. The pipe transport uses
`subc-jobobject::spawn_contained`: `std::process::Command` creates the process
suspended, then the launcher assigns it to a kill-on-close Job and discovers its
primary thread through a Toolhelp thread snapshot before resuming it. The
patched ConPTY launcher instead retains the original primary-thread handle
returned by `CreateProcessW` and does not discover it by PID. The distinction
matters under high concurrent pipe startup. The stress runner below exercises it.

## GNU setup without administrator privileges

Install Rust with [rustup](https://rust-lang.org/tools/install/). To reproduce
the tested compiler version, in PowerShell from the repository root:

```powershell
$env:PATH = "$env:USERPROFILE/.cargo/bin;$env:PATH"
rustup toolchain install 1.96.0-x86_64-pc-windows-gnu --profile minimal --component rustfmt --component clippy
rustup override set 1.96.0-x86_64-pc-windows-gnu
./scripts/install-windows-gnu.ps1
```

The installer downloads versioned packages from the official
[MSYS2 repository](https://www.msys2.org/docs/mirrors/), verifies every archive
against the pinned SHA256 values in
[`windows-gnu-packages.json`](../scripts/windows-gnu-packages.json), and extracts
them under `%LOCALAPPDATA%/uuldana/toolchains/msys2-mingw64-20261012`.
It requires Windows `tar.exe` with zstd support. Windows 11's tested tar is
bsdtar 3.8.8 with libzstd 1.5.7. Extraction errors fail installation.
The archives remain in `packages/`, and `installed-packages.json` records the
installed set. No registry entry or permanent PATH change is created.

This is a native binutils bundle, not a complete MSYS2 installation managed by
Pacman. It includes the full runtime dependency closure of
[MSYS2's binutils package](https://packages.msys2.org/packages/mingw-w64-x86_64-binutils):
gettext-runtime, libwinpthread, zlib, zstd, cc-libs, libiconv, libgcc, libstdc++,
and tzdata. Rust supplies the compiler and bundled GNU linker support. The
missing prerequisite for this workspace was GNU `dlltool.exe` plus GNU `as.exe`
and their runtime DLLs. The installer runs both tools to check that they load.

For each new PowerShell session, activate the installed tools:

```powershell
. ./scripts/enter-windows-gnu.ps1
rustc -vV
as --version
dlltool --version
```

Both setup scripts accept `-InstallRoot` to use another installation directory.
For an existing complete MSYS2 installation, install binutils using its package
manager (`pacman -S mingw-w64-x86_64-binutils`) and put its `mingw64/bin` directory
on PATH. Do not mix the MSYS `/usr/bin` assembler with the native MinGW tools.

## Standard verification

After activation, these are ordinary Cargo commands with no `RUSTFLAGS`,
`CARGO_ENCODED_RUSTFLAGS`, assembler wrapper, or custom linker selection:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo check --workspace --locked
cargo test --workspace --locked
cargo test --workspace --all-targets --locked
cargo build -p uuldana-transport --example uuldana-test-child --locked
cargo run -p uuldana-transport --example pty_probe --locked -- --scripted
```

`./scripts/verify-windows.ps1` runs the same commands, stopping at the first
failure and writing each command's output to `target/verification/*.log`.
It checks tool availability and prints the active Rust host and binutils paths.
It also compiles and runs a small system-import and named-thread probe before
checking the workspace, catching incomplete SDKs and broken import libraries.
It temporarily adds the default installed binutils directory to PATH when the
Rust host is GNU; `-MingwBin` selects another complete installation. PATH is
restored on exit. The script rejects compiler flag overrides so its results
represent the documented standard configuration.
Use `./scripts/verify-windows.ps1 -PrerequisitesOnly` to diagnose installation
without running the workspace checks; its output explicitly states that those
checks were not run.

## Parallel and repeated stress verification

After activating the same toolchain, run:

```powershell
. ./scripts/enter-windows-gnu.ps1
./scripts/stress.ps1
```

The stress runner first executes
`cargo test -p uuldana-runtime -p uuldana-transport --no-run --locked --message-format=json`.
It discovers the exact current test executables from Cargo's JSON artifacts and
requires all six expected targets. It then runs the executables directly, so
parallel test processes do not contend for Cargo's build lock.

Each runtime library, lifecycle, reliability, probe, transport library, and
transport process target runs in a batch of 12 simultaneous processes with
`--test-threads=4`. The runtime library includes the exhaustion watchdog
regression. Lifecycle, reliability, and transport process tests also run 10
sequential repeats each with `--test-threads=1`. The default run therefore
requires 102 successful test process executions.

Every process has an external 60-second deadline. A timeout triggers process
tree termination and fails the run. Nonzero exit codes, missing test artifacts,
missing successful test summaries, and ignored tests also fail verification.
The runner reports progress at each executable batch and writes build output,
individual stdout/stderr logs, and a machine-readable `results.jsonl` under
`target/verification/stress/<timestamp>-<pid>/`. It stops after a failed batch
and does not report a partial run as successful.

`-ParallelRuns`, `-SequentialRuns`, and `-DeadlineSeconds` allow explicitly
selected diagnostic workloads. For example, `./scripts/stress.ps1
-ParallelRuns 2 -SequentialRuns 1` runs a smaller check. Reduced workloads do
not establish that the default 102-execution stress gate passed. The build
always runs; stale test executables cannot be selected by a skip-build option.
Executable paths must resolve inside the workspace's `target` directory, so
remove custom Cargo target-directory overrides before running this script.

## MSVC alternative

Install the MSVC Rust toolchain plus Visual Studio Build Tools' **Desktop
development with C++** workload, including an x64 MSVC toolset and Windows SDK.
Microsoft documents these components in
[C++ Build Tools installation](https://learn.microsoft.com/en-us/cpp/build/building-on-the-command-line?view=msvc-170).
Select `stable-x86_64-pc-windows-msvc` using a repository rustup override and run
the verification commands from an x64 Developer PowerShell. MSVC requires the
Windows SDK's system libraries; having `link.exe` alone is insufficient.
The GNU bundle is not needed for MSVC.

## Failure diagnosis and maintenance

- `error calling dlltool 'dlltool.exe': program not found`: activate the GNU
  binutils directory and confirm `Get-Command dlltool.exe` resolves there.
- GNU dlltool reports that its assembler cannot run: check `as.exe --version`
  and reinstall the pinned bundle, including dependencies.
- MSVC cannot find `kernel32.lib`: install the Windows SDK and use the matching
  developer shell. The local VS2019 installation lacked this SDK; subsequent MSVC
  verification used isolated official SDK import libraries under `target/`.
- A SHA256 mismatch or unavailable pinned archive fails installation. Review
  current official package metadata and its transitive dependencies before
  updating the manifest. Never bypass hash verification or silently substitute
  an unpinned package. This bundle is maintained by updating the pinned set;
  Pacman cannot update it in place.

An earlier local workaround used GNU dlltool with a custom clang assembler and
compiler flags. A standalone LLVM dlltool attempt produced executables that
failed to load `SetThreadDescription` correctly. Those attempts do not establish
standard Cargo compatibility. The documented GNU setup replaces both approaches
with official native binutils. Results from a local run are separate from CI jobs that have not been executed.

The setup was exercised on 2026-10-12: initial installation and a repeat
installation both returned exit 0, and every cached archive matched its pinned
hash. Plain `rustc` compiled and ran the system-import/named-thread probe.
An isolated Cargo package under `target/verification/toolchain/cargo-smoke`
then passed `cargo test --all-targets` (1 test) and `cargo run`, without compiler
flag overrides. These smoke checks establish toolchain operation; the workspace
checks above are the separate integration gate.

## Native teardown observer

The probe descendant test has a **two-second whole-probe deadline**, covering worker
creation, launch, readiness, native exit and teardown/output collection. Error
cleanup has a **separate three-second bound**. The native observer starts the
250 ms reader-progress budget after owned lifecycle evidence establishes
teardown completion. Zero membership, ACTIVE_PROCESS_ZERO and EOF alone are
insufficient. Missing notifications use bounded snapshot/handle polling; missing
coverage remains a diagnostic. See the
[native patch audit](../vendor/subc-jobobject/UULDANA_PATCH.md).
