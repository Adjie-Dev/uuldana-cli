# Builds the test-only Windows process discovery helper from source and points UULDANA_TEST_PROCINFO at it.
# Run it, or dot-source it, in the session that will run the Windows tests, after the toolchain is active.
# The helper lives outside the workspace on purpose; see docs/TEST_PROCESS_HELPER.md.
& {
    param([string]$Root)
    $previousPreference = $ErrorActionPreference
    $oldPath = $env:PATH
    $ErrorActionPreference = 'Stop'
    try {
        if ($env:OS -ne 'Windows_NT') { throw 'The process discovery helper is Windows only.' }
        $env:PATH = (Join-Path $env:USERPROFILE '.cargo/bin') + ';' + $env:PATH
        foreach ($name in @('RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS')) {
            if ([Environment]::GetEnvironmentVariable($name)) {
                throw "$name is set. Clear compiler overrides before building the test helper."
            }
        }
        if (!(Get-Command cargo -ErrorAction SilentlyContinue)) { throw 'cargo is missing from PATH.' }
        $manifest = Join-Path $Root 'tools/uuldana-test-procinfo/Cargo.toml'
        $targetDir = Join-Path $Root 'target/test-helper'
        # Cargo writes progress to stderr. Its exit status is authoritative.
        $ErrorActionPreference = 'Continue'
        & cargo build --release --locked --offline --manifest-path $manifest --target-dir $targetDir 2>&1 | ForEach-Object { $_.ToString() }
        $code = $LASTEXITCODE
        $ErrorActionPreference = 'Stop'
        if ($code -ne 0) { throw "Test helper build failed: exit $code." }
        $exe = Join-Path $targetDir 'release/uuldana-test-procinfo.exe'
        if (!(Test-Path -LiteralPath $exe)) { throw "Test helper was not produced: $exe" }
        # A process id that cannot exist must be reported as gone, and this shell must be listable.
        & $exe alive 4000000000 cmd.exe | Out-Null
        if ($LASTEXITCODE -ne 1) { throw "Test helper self-check failed (alive): exit $LASTEXITCODE." }
        & $exe tree $PID | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "Test helper self-check failed (tree): exit $LASTEXITCODE." }
        $env:UULDANA_TEST_PROCINFO = (Resolve-Path -LiteralPath $exe).Path
        Write-Host "UULDANA_TEST_PROCINFO=$env:UULDANA_TEST_PROCINFO"
    } finally {
        $env:PATH = $oldPath
        $ErrorActionPreference = $previousPreference
    }
} (Split-Path -Parent $PSScriptRoot)
