param(
    [ValidateRange(1, 128)][int]$ParallelRuns = 12,
    [ValidateRange(1, 1000)][int]$SequentialRuns = 10,
    [ValidateRange(1, 600)][int]$DeadlineSeconds = 60
)
# Build once, discover Cargo's exact test artifacts, then execute them directly.
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$oldPath = $env:PATH
$runStamp = [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssfffZ') + '-' + $PID
$logRoot = Join-Path $root ('target/verification/stress/' + $runStamp)
$summary = Join-Path $logRoot 'results.jsonl'
$completed = [Collections.Generic.List[object]]::new()
Push-Location $root
try {
    $env:PATH = (Join-Path $env:USERPROFILE '.cargo/bin') + ';' + $env:PATH
    if ($env:OS -ne 'Windows_NT') { throw 'This stress runner requires Windows PowerShell.' }
    foreach ($name in @('RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS')) {
        if ([Environment]::GetEnvironmentVariable($name)) {
            throw "$name is set. Clear compiler overrides before standard stress verification."
        }
    }
    if (!(Get-Command cargo -ErrorAction SilentlyContinue)) { throw 'cargo is missing from PATH.' }
    if (!(Get-Command taskkill.exe -ErrorAction SilentlyContinue)) { throw 'taskkill.exe is required for watchdog cleanup.' }
    New-Item -ItemType Directory -Force -Path $logRoot | Out-Null
    # Diagnostic samples never gate, skip or reclassify workload results.
    function Write-HostSample([string]$Stage) {
        $sample = [ordered]@{ stage = $Stage; utc = [DateTime]::UtcNow.ToString('o') }
        try {
            $cpu = @(Get-CimInstance Win32_Processor -ErrorAction Stop)
            $sample.cpu_load_percent = ($cpu | Measure-Object LoadPercentage -Average).Average
            $osInfo = Get-CimInstance Win32_OperatingSystem -ErrorAction Stop
            $sample.available_memory_mib = [Math]::Round($osInfo.FreePhysicalMemory / 1024, 1)
            $processes = @(Get-Process -ErrorAction Stop)
            $sample.process_count = $processes.Count
            $sample.powershell_count = @($processes | Where-Object { $_.ProcessName -match '^(powershell|pwsh)$' }).Count
            $sample.test_process_count = @($processes | Where-Object { $_.ProcessName -match '^(lifecycle|reliability|probe|process|uuldana_runtime|uuldana_transport)-' }).Count
            $sample.mock_process_count = @($processes | Where-Object { $_.ProcessName -eq 'uuldana-mock' }).Count
        } catch { $sample.measurement_error = $_.Exception.Message }
        $sample | ConvertTo-Json -Compress | Add-Content -LiteralPath (Join-Path $logRoot 'host-preflight.jsonl')
        Write-Host ("HOST " + ($sample | ConvertTo-Json -Compress))
    }
    Write-HostSample 'pre-build'
    $buildJson = Join-Path $logRoot 'build.jsonl'
    $buildErrors = Join-Path $logRoot 'build.stderr.log'
    Write-Host 'BUILD cargo test -p uuldana-runtime -p uuldana-transport --no-run --locked --message-format=json'
    # Cargo writes progress to stderr. Preserve it; its exit status is authoritative.
    $ErrorActionPreference = 'Continue'
    & cargo test -p uuldana-runtime -p uuldana-transport --no-run --locked --message-format=json 1> $buildJson 2> $buildErrors
    $buildCode = $LASTEXITCODE
    $ErrorActionPreference = 'Stop'
    if ($buildCode -ne 0) { throw "Stress build failed: exit $buildCode. See $buildErrors and $buildJson." }

    $specs = @(
        @{ Name = 'runtime-lib'; Target = 'uuldana_runtime'; Kind = 'lib'; Repeat = $false },
        @{ Name = 'runtime-lifecycle'; Target = 'lifecycle'; Kind = 'test'; Repeat = $true },
        @{ Name = 'runtime-reliability'; Target = 'reliability'; Kind = 'test'; Repeat = $true },
        @{ Name = 'runtime-probe'; Target = 'probe'; Kind = 'test'; Repeat = $false },
        @{ Name = 'transport-lib'; Target = 'uuldana_transport'; Kind = 'lib'; Repeat = $false },
        @{ Name = 'transport-process'; Target = 'process'; Kind = 'test'; Repeat = $true }
    )
    $artifacts = @(Get-Content -LiteralPath $buildJson | ForEach-Object {
        $message = $_ | ConvertFrom-Json
        if ($message.reason -eq 'compiler-artifact' -and $message.profile.test -and $message.executable) { $message }
    })
    $targetRoot = [IO.Path]::GetFullPath((Join-Path $root 'target')) + [IO.Path]::DirectorySeparatorChar
    foreach ($spec in $specs) {
        $matches = @($artifacts | Where-Object {
            $_.target.name -eq $spec.Target -and $_.target.kind -contains $spec.Kind
        } | Select-Object -ExpandProperty executable -Unique)
        if ($matches.Count -ne 1) { throw "Expected exactly one $($spec.Name) test artifact; found $($matches.Count). See $buildJson." }
        $exe = (Resolve-Path -LiteralPath $matches[0] -ErrorAction Stop).Path
        if (![IO.File]::Exists($exe) -or [IO.Path]::GetExtension($exe) -ne '.exe' -or
            !$exe.StartsWith($targetRoot, [StringComparison]::OrdinalIgnoreCase)) {
            throw "Refusing test executable outside the workspace target directory: $exe. Remove custom Cargo target-directory overrides."
        }
        $spec.Executable = $exe
        Write-Host "ARTIFACT $($spec.Name): $exe"
    }

    function Stop-StressProcess($Run) {
        if (!$Run.Process.HasExited) {
            $killLog = $Run.Stdout + '.watchdog.log'
            $ErrorActionPreference = 'Continue'
            & taskkill.exe /PID $Run.Process.Id /T /F 2>&1 | ForEach-Object { $_.ToString() } | Out-File -LiteralPath $killLog
            $killCode = $LASTEXITCODE
            if ($killCode -ne 0) { Write-Warning "Process-tree termination returned $killCode; see $killLog" }
            if (!$Run.Process.WaitForExit(5000)) {
                Stop-Process -Id $Run.Process.Id -Force -ErrorAction Stop
                if (!$Run.Process.WaitForExit(5000)) { throw "Cannot terminate stress process $($Run.Process.Id)." }
            }
        }
    }

    function Invoke-StressBatch($Spec, [string]$Stage, [int]$Count, [int]$Threads, [int]$Offset = 0) {
        Write-HostSample ("$Stage-$($Spec.Name)")
        Write-Host "START $Stage $($Spec.Name): $Count process(es), test-threads=$Threads, deadline=${DeadlineSeconds}s each"
        $runs = [Collections.Generic.List[object]]::new()
        $batchFailed = $false
        try {
            for ($index = 1; $index -le $Count; $index++) {
                $number = $Offset + $index
                $stem = Join-Path $logRoot ("$Stage-$($Spec.Name)-" + $number.ToString('D3'))
                $stdout = $stem + '.stdout.log'
                $stderr = $stem + '.stderr.log'
                $process = Start-Process -FilePath $Spec.Executable -ArgumentList @("--test-threads=$Threads", '--nocapture') -WorkingDirectory $root -WindowStyle Hidden -RedirectStandardOutput $stdout -RedirectStandardError $stderr -PassThru
                # Retain the handle before a short test executable can exit.
                $null = $process.Handle
                $runs.Add(@{ Process = $process; Clock = [Diagnostics.Stopwatch]::StartNew(); Number = $number; Stdout = $stdout; Stderr = $stderr; Done = $false; TimedOut = $false })
            }
            while (@($runs | Where-Object { !$_.Done }).Count -gt 0) {
                foreach ($run in $runs) {
                    if ($run.Done) { continue }
                    $run.Process.Refresh()
                    if (!$run.Process.HasExited -and $run.Clock.Elapsed.TotalSeconds -ge $DeadlineSeconds) {
                        $run.TimedOut = $true
                        Stop-StressProcess $run
                    }
                    if (!$run.Process.HasExited) { continue }
                    $run.Process.WaitForExit()
                    $exitCode = $run.Process.ExitCode
                    if ($null -eq $exitCode) { throw "Cannot read exit status for process $($run.Process.Id)." }
                    $run.Clock.Stop()
                    $processSeconds = ($run.Process.ExitTime - $run.Process.StartTime).TotalSeconds
                    if ($processSeconds -gt $DeadlineSeconds) { $run.TimedOut = $true }
                    $stdoutText = Get-Content -LiteralPath $run.Stdout -Raw
                    $testSummary = $stdoutText -match 'test result: ok\. [1-9][0-9]* passed; 0 failed; 0 ignored;'
                    $passed = !$run.TimedOut -and $exitCode -eq 0 -and $testSummary
                    $result = [ordered]@{
                        stage = $Stage; target = $Spec.Name; run = $run.Number; threads = $Threads
                        process_id = $run.Process.Id; exit_code = $exitCode; timed_out = $run.TimedOut
                        seconds = [Math]::Round($processSeconds, 3); passed = $passed
                        stdout = $run.Stdout; stderr = $run.Stderr
                    }
                    $completed.Add($result)
                    $result | ConvertTo-Json -Compress | Add-Content -LiteralPath $summary
                    $run.Done = $true
                    if (!$passed) {
                        $batchFailed = $true
                        Write-Warning "FAILED $Stage $($Spec.Name) run=$($run.Number) exit=$exitCode watchdog=$($run.TimedOut) test-summary=$testSummary; see $($run.Stdout) and $($run.Stderr)"
                    }
                }
                if (@($runs | Where-Object { !$_.Done }).Count -gt 0) { Start-Sleep -Milliseconds 100 }
            }
        } finally {
            foreach ($run in $runs) {
                if (!$run.Process.HasExited) { Stop-StressProcess $run }
                $run.Process.Dispose()
            }
        }
        if ($batchFailed) { throw "Stress batch failed: $Stage $($Spec.Name). See $summary." }
        Write-Host "PASS $Stage $($Spec.Name): $Count/$Count process(es)"
    }

    # A separately labelled, bounded timing sample; it contributes no success
    # to the official 102-execution workload and cannot suppress its failures.
    $probeSpec = $specs | Where-Object { $_.Name -eq 'runtime-probe' }
    $probeOut = Join-Path $logRoot 'preflight-probe.stdout.log'
    $probeErr = Join-Path $logRoot 'preflight-probe.stderr.log'
    $probeClock = [Diagnostics.Stopwatch]::StartNew()
    $probeProcess = Start-Process -FilePath $probeSpec.Executable -ArgumentList @('version_probe_captures_stdout_and_status', '--exact', '--test-threads=1', '--nocapture') -WorkingDirectory $root -WindowStyle Hidden -RedirectStandardOutput $probeOut -RedirectStandardError $probeErr -PassThru
    $null = $probeProcess.Handle
    try {
        $probeTimedOut = !$probeProcess.WaitForExit(20000)
        if ($probeTimedOut) { Stop-StressProcess @{ Process = $probeProcess; Stdout = $probeOut } }
        $probeClock.Stop()
        [ordered]@{
            stage = 'preflight-probe'; utc = [DateTime]::UtcNow.ToString('o')
            probe_execution_seconds = [Math]::Round($probeClock.Elapsed.TotalSeconds, 3)
            exit_code = $probeProcess.ExitCode; timed_out = $probeTimedOut
            stdout = $probeOut; stderr = $probeErr
        } | ConvertTo-Json -Compress | Add-Content -LiteralPath (Join-Path $logRoot 'host-preflight.jsonl')
    } finally { $probeProcess.Dispose() }

    foreach ($spec in $specs) { Invoke-StressBatch $spec 'parallel' $ParallelRuns 4 }
    foreach ($spec in $specs | Where-Object { $_.Repeat }) {
        Write-Host "SEQUENTIAL $($spec.Name): $SequentialRuns repeat(s)"
        for ($iteration = 0; $iteration -lt $SequentialRuns; $iteration++) {
            Invoke-StressBatch $spec 'sequential' 1 1 $iteration
        }
    }
    $expected = 6 * $ParallelRuns + 3 * $SequentialRuns
    if ($completed.Count -ne $expected) { throw "Expected $expected stress executions; recorded $($completed.Count)." }
    Write-Host "RESULT stress exit=0 executions=$($completed.Count) logs=$logRoot"
} finally {
    $env:PATH = $oldPath
    Pop-Location
}
