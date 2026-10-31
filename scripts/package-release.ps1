<#
.SYNOPSIS
  Builds the release binary and packs it with its documentation into an archive
  and a SHA256 checksum file under target\dist.

.DESCRIPTION
  Builds locally only. It publishes nothing and contacts no service.

.PARAMETER Target
  Rust target triple. Defaults to the host triple.
#>
param([string]$Target = '')

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

# An explicit --target switches Cargo to cross-compilation mode. Without one the
# build is native and uses the toolchain exactly as `cargo build` does.
$explicit = [bool]$Target
if (-not $Target) {
    $Target = ((rustc -vV) | Where-Object { $_ -like 'host:*' }) -replace '^host:\s*', ''
}
$version = (Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version = "(.+)"' |
    Select-Object -First 1).Matches[0].Groups[1].Value
if (-not $version) { throw 'could not read the workspace version from Cargo.toml' }

# Cargo reports progress on stderr, which Windows PowerShell 5.1 turns into
# errors under 'Stop', so only the exit code decides here.
$ErrorActionPreference = 'Continue'
$targetArgs = if ($explicit) { @('--target', $Target) } else { @() }
. (Join-Path $PSScriptRoot 'release-env.ps1')
cargo build --release --locked -p uuldana-cli @targetArgs 2>&1 | ForEach-Object { "$_" }
$buildExit = $LASTEXITCODE
$ErrorActionPreference = 'Stop'
if ($buildExit -ne 0) { throw "release build failed with exit code $buildExit" }

$exeName = if ($Target -like '*windows*') { 'uuldana.exe' } else { 'uuldana' }
$outDir = if ($explicit) { "target\$Target\release" } else { 'target\release' }
$binary = Join-Path $root "$outDir\$exeName"
if (-not (Test-Path $binary)) { throw "missing build output: $binary" }
Assert-NoBuilderPaths $binary

$name = "uuldana-$version-$Target"
$dist = Join-Path $root 'target\dist'
$stage = Join-Path $dist $name
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force -Path $stage | Out-Null

Copy-Item $binary $stage
foreach ($file in 'README.md', 'LICENSE-MIT', 'THIRD_PARTY_NOTICES.md', 'SECURITY.md') {
    Copy-Item (Join-Path $root $file) $stage
}
New-Item -ItemType Directory -Force -Path (Join-Path $stage 'docs') | Out-Null
foreach ($file in 'CONFIGURATION.md', 'PLUGIN_COMPATIBILITY.md', 'PROVIDER_SDK.md') {
    Copy-Item (Join-Path $root "docs\$file") (Join-Path $stage 'docs')
}
Copy-Item -Recurse (Join-Path $root 'examples') $stage

$archive = Join-Path $dist "$name.zip"
if (Test-Path $archive) { Remove-Item -Force $archive }
Compress-Archive -Path $stage -DestinationPath $archive

$hash = (Get-FileHash -Algorithm SHA256 $archive).Hash.ToLowerInvariant()
$sums = Join-Path $dist "$name.sha256"
"$hash  $name.zip" | Out-File -Encoding ascii $sums

Write-Host "archive : $archive"
Write-Host "sha256  : $hash"
