<#
.SYNOPSIS
  Packs the npm packages for the current Windows x64 release binary into
  target\npm as .tgz files.

.DESCRIPTION
  Builds nothing and publishes nothing. Run scripts\package-release.ps1 or
  `. .\scripts\release-env.ps1; cargo build --release -p uuldana-cli` first. The two tarballs are installed
  together with:

    npm install -g target\npm\uuldana-win32-x64-<version>.tgz target\npm\uuldana-<version>.tgz
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

# Platform package: the binary and nothing else.
$platform = Join-Path $out 'uuldana-win32-x64'
New-Item -ItemType Directory -Force -Path (Join-Path $platform 'bin') | Out-Null
Copy-Item $binary (Join-Path $platform 'bin\uuldana.exe')
foreach ($file in $licenses) { Copy-Item (Join-Path $root $file) $platform }
$manifest = [ordered]@{
    name        = '@uuldana/win32-x64'
    version     = $version
    description = 'The uuldana binary for Windows x64.'
    license     = 'MIT'
    author      = 'Ang Hae Jhie (Anicca Code)'
    bugs        = [ordered]@{ email = 'aniccacodestudio@gmail.com' }
    os          = @('win32')
    cpu         = @('x64')
    files       = @('bin', 'LICENSE-MIT', 'THIRD_PARTY_NOTICES.md')
}
Write-Json $manifest (Join-Path $platform 'package.json')

# Main package: the shim, with the version taken from Cargo.toml.
$main = Join-Path $out 'uuldana'
Copy-Item -Recurse (Join-Path $PSScriptRoot 'uuldana') $main
foreach ($file in $licenses) { Copy-Item (Join-Path $root $file) $main }
$package = Get-Content -Raw (Join-Path $main 'package.json') | ConvertFrom-Json
$package.version = $version
$package.optionalDependencies.'@uuldana/win32-x64' = $version
Write-Json $package (Join-Path $main 'package.json')

foreach ($dir in $platform, $main) {
    Push-Location $dir
    try {
        npm pack --pack-destination $out --silent
        if ($LASTEXITCODE -ne 0) { throw "npm pack failed in $dir" }
    } finally { Pop-Location }
}

Get-ChildItem $out -Filter *.tgz | ForEach-Object { Write-Host $_.FullName }
