# Windows test process helper

Status: test-only support for `crates/uuldana-transport/tests/process.rs`. No production code depends on it.

## Why it exists

The transport process tests must ask the operating system which processes are children of a given process and whether a
specific process is still alive. The workspace sets `unsafe_code = "forbid"`, and the standard library has no
parent/child or process-identity API, so the Win32 calls live in a separate crate that is **not** a workspace member:
`tools/uuldana-test-procinfo`. It has no dependencies and declares its own empty `[workspace]` table, which keeps the
workspace lint from applying to it. The tests previously started `powershell.exe` (CIM) and `tasklist.exe` for every
question; the helper answers the same questions with Toolhelp32, `GetProcessTimes` and `QueryFullProcessImageNameW`.

## Behavior

- `tree <ppid>` prints one tab separated row per direct child: pid, creation FILETIME, image name, image path, machine.
  Children that started before their parent are dropped, because the parent id was recycled.
- `alive <pid> <image> [creation]` exits 0 (alive), 1 (not running), 2 (usage) or 3 (enumeration failed).
  When the test saw the process earlier, it passes the creation time, so a recycled pid is reported as a different
  process. A process that exists but cannot be opened is reported alive (fail closed).
- A failed enumeration is never reported as an empty list or as "alive". The helper exits non-zero with a message, and
  the test panics with it. A missing helper or an unset variable also panics.

## Build and discovery

```powershell
. ./scripts/enter-windows-gnu.ps1      # GNU only; use an x64 developer shell for MSVC
./scripts/prepare-test-helper.ps1
```

The script builds the helper from source (`cargo build --release --locked --offline`) into `target/test-helper`,
runs a two command self-check and sets `UULDANA_TEST_PROCINFO` to the absolute path of the executable for the current
session. No prebuilt executable is stored in the repository. The same executable serves test binaries built by either
toolchain; switching toolchains rebuilds it. The built binary is not byte-for-byte reproducible between builds
(the PE header carries a timestamp), but it is rebuilt from the same source and lock file.

`scripts/verify-windows.ps1` and the Windows jobs in `.github/workflows/ci.yml` run the script before the tests.

## Running the stress runner

`scripts/stress.ps1` is unchanged. It builds the test executables with `cargo test ... --no-run` and starts them
with `Start-Process`, which passes the session environment on. Prepare the session first:

```powershell
. ./scripts/enter-windows-gnu.ps1
./scripts/prepare-test-helper.ps1
./scripts/stress.ps1
```

Parallel and sequential counts, test threads, deadlines and the 15 second and two second test bounds are unchanged.
The extra step is an addition to the documented preparation, not to the workload. The bare commands listed under
"Standard verification" in `WINDOWS_DEVELOPMENT.md` need the same preparation on Windows; without it the process
tests that use the helper fail immediately with `UULDANA_TEST_PROCINFO must name the uuldana-test-procinfo helper`.
Whether this preparation step is accepted for the official campaigns is an owner decision.

## Known differences from the previous query helpers

- A process id outside the real range (for example 4000000000) is reported as not running. The previous `tasklist`
  filter printed an error and was read as "running".
- Direct children of a process that cannot be opened (for example PID 4) are listed without the start time filter.
  This can only add processes to verify, never remove them.
- Windows only, x64 tested. ARM64 is not claimed.
