<#
.SYNOPSIS
  Packs the npm package for the current Windows x64 release binary into
  target\npm as a .tgz file.

.DESCRIPTION
  Builds nothing and publishes nothing. Run scripts\package-release.ps1 or
  `. .\scripts\release-env.ps1; cargo build --release -p uuldana-cli --locked`
  first. The tarball is installed with:

    npm install -g target\npm\uuldana-cli-<version>.tgz

  The command it installs is `uuldana`.
#>
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)

$version = (Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version = "(.+)"' |
    Select-Object -First 1).Matches[0].Groups[1].Value
if (-not $version) { throw 'could not read the workspace version from Cargo.toml' }

$binary = Join-Path $root 'target\release\uuldana.exe'
if (-not (Test-Path $binary)) { throw "missing $binary, build the release binary first (after `. .\scripts\release-env.ps1`)" }
. (Join-Path $root 'scripts\release-env.ps1')
Assert-NoBuilderPaths $binary

$out = Join-Path $root 'target\npm'
if (Test-Path $out) { Remove-Item -Recurse -Force $out }
New-Item -ItemType Directory -Force -Path $out | Out-Null

$licenses = 'LICENSE-MIT', 'THIRD_PARTY_NOTICES.md'

# npm and Node read package.json fine without a byte order mark; keep it out.
function Write-Json($value, $path) {
    [IO.File]::WriteAllText($path, ($value | ConvertTo-Json -Depth 5), (New-Object Text.UTF8Encoding($false)))
}

# One package: the shim and the binary next to it, version from Cargo.toml.
$main = Join-Path $out 'uuldana-cli'
Copy-Item -Recurse (Join-Path $PSScriptRoot 'uuldana') $main
Copy-Item $binary (Join-Path $main 'bin\uuldana.exe')
foreach ($file in $licenses) { Copy-Item (Join-Path $root $file) $main }
$package = Get-Content -Raw (Join-Path $main 'package.json') | ConvertFrom-Json
$package.version = $version
Write-Json $package (Join-Path $main 'package.json')

Push-Location $main
try {
    npm pack --pack-destination $out --silent
    if ($LASTEXITCODE -ne 0) { throw "npm pack failed in $main" }
} finally { Pop-Location }

Get-ChildItem $out -Filter *.tgz | ForEach-Object { Write-Host $_.FullName }
