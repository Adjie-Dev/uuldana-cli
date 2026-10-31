param([string]$MingwBin, [switch]$PrerequisitesOnly)
# Run from any directory. Ordinary Cargo commands; no compiler flag overrides.
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$oldPath = $env:PATH
Push-Location $root
try {
    $env:PATH = (Join-Path $env:USERPROFILE '.cargo/bin') + ';' + $env:PATH
    foreach ($name in @('RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS')) {
        if ([Environment]::GetEnvironmentVariable($name)) {
            throw "$name is set. Clear it before standard verification; see docs/WINDOWS_DEVELOPMENT.md."
        }
    }
    foreach ($tool in @('rustc', 'cargo', 'rustup')) {
        if (!(Get-Command $tool -ErrorAction SilentlyContinue)) {
            throw "Missing $tool. Install Rust using rustup; see docs/WINDOWS_DEVELOPMENT.md."
        }
    }
    $rustInfo = & rustc -vV
    if ($LASTEXITCODE -ne 0) { throw 'rustc -vV failed' }
    Write-Host ($rustInfo -join [Environment]::NewLine)
    $hostLine = $rustInfo | Where-Object { $_ -like 'host: *' }
    if ($hostLine -eq 'host: x86_64-pc-windows-gnu') {
        if (!$MingwBin) {
            $localBin = Join-Path $env:LOCALAPPDATA 'uuldana/toolchains/msys2-mingw64-20261012/mingw64/bin'
            if (Test-Path -LiteralPath $localBin) { $MingwBin = $localBin }
        }
        if ($MingwBin) {
            $env:PATH = (Resolve-Path -LiteralPath $MingwBin).Path + ';' + $env:PATH
        }
        foreach ($tool in @('as.exe', 'dlltool.exe')) {
            $command = Get-Command $tool -ErrorAction SilentlyContinue
            if (!$command) {
                throw "Missing GNU $tool. Run ./scripts/install-windows-gnu.ps1; see docs/WINDOWS_DEVELOPMENT.md."
            }
            Write-Host "$tool : $($command.Source)"
            $version = & $command.Source --version
            if ($LASTEXITCODE -ne 0) { throw "$tool cannot run; install its runtime dependencies." }
            Write-Host ($version -join [Environment]::NewLine)
            if (($version -join ' ') -notmatch 'GNU') {
                throw "$tool is not GNU binutils. Select the complete native MinGW bundle; see docs/WINDOWS_DEVELOPMENT.md."
            }
        }
    } elseif ($hostLine -ne 'host: x86_64-pc-windows-msvc') {
        throw "Unsupported verification host: $hostLine. This script supports Windows x64 GNU or MSVC."
    }
    $logs = Join-Path $root 'target/verification'
    New-Item -ItemType Directory -Force -Path $logs | Out-Null
    # Exercise a real system import, linker/SDK libraries, and named std thread.
    # This detects incomplete SDKs and dlltool outputs that compile but cannot run.
    $probeSource = @'
#[link(name="kernel32", kind="raw-dylib")]
unsafe extern "system" { fn GetCurrentProcessId() -> u32; }
fn main() {
    assert_ne!(unsafe { GetCurrentProcessId() }, 0);
    std::thread::Builder::new().name("toolchain-probe".into())
        .spawn(|| ()).unwrap().join().unwrap();
}
'@
    $probePath = Join-Path $logs 'toolchain-probe.rs'
    $probeExe = Join-Path $logs 'toolchain-probe.exe'
    [IO.File]::WriteAllText($probePath, $probeSource, [Text.UTF8Encoding]::new($false))
    & rustc --edition 2024 $probePath -o $probeExe
    if ($LASTEXITCODE -ne 0) { throw 'Windows toolchain link/import probe failed. See docs/WINDOWS_DEVELOPMENT.md.' }
    & $probeExe
    if ($LASTEXITCODE -ne 0) { throw "Windows toolchain executable probe failed: exit $LASTEXITCODE." }
    Write-Host 'RESULT toolchain-probe exit=0'
    if ($PrerequisitesOnly) {
        Write-Host 'Prerequisites verified; workspace checks were not run (-PrerequisitesOnly).'
        return
    }
    # The Windows process tests find their discovery helper through UULDANA_TEST_PROCINFO.
    . (Join-Path $PSScriptRoot 'prepare-test-helper.ps1')
    function Invoke-CargoCheck([string]$Name, [string[]]$CargoArgs) {
        Write-Host ('cargo ' + ($CargoArgs -join ' '))
        # Native Cargo progress uses stderr. Only its exit code decides success.
        $ErrorActionPreference = 'Continue'
        & cargo @CargoArgs 2>&1 | ForEach-Object { $_.ToString() } | Tee-Object -FilePath (Join-Path $logs ($Name + '.log'))
        $code = $LASTEXITCODE
        if ($code -ne 0) { throw "cargo $Name failed: exit $code (see target/verification/$Name.log)" }
        Write-Host "RESULT $Name exit=0"
    }
    Invoke-CargoCheck 'fmt' @('fmt', '--all', '--', '--check')
    Invoke-CargoCheck 'clippy' @('clippy', '--workspace', '--all-targets', '--locked', '--', '-D', 'warnings')
    Invoke-CargoCheck 'check' @('check', '--workspace', '--locked')
    Invoke-CargoCheck 'test' @('test', '--workspace', '--locked')
    Invoke-CargoCheck 'test-all-targets' @('test', '--workspace', '--all-targets', '--locked')
    Invoke-CargoCheck 'fixture-build' @('build', '-p', 'uuldana-transport', '--example', 'uuldana-test-child', '--locked')
    Invoke-CargoCheck 'pty-probe' @('run', '-p', 'uuldana-transport', '--example', 'pty_probe', '--locked', '--', '--scripted')
} finally {
    $env:PATH = $oldPath
    Pop-Location
}
