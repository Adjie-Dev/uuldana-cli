<#
.SYNOPSIS
  Sets RUSTFLAGS so a release binary does not carry the paths of the machine
  that built it (the user's home directory, the Rust and Cargo folders, the
  checkout).

.DESCRIPTION
  Dot-source it before `cargo build --release`:

    . .\scripts\release-env.ps1

  Rust writes the source path of every panic message into the binary, so
  without this the user name of whoever built it is readable inside the
  published executable. `trim-paths` would do the same without a script but is
  not stable in Cargo yet. scripts\package-release.ps1 does this itself, and
  packaging\npm\build.ps1 refuses a binary that still holds such a path.
#>
$releaseRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..')).TrimEnd('\')
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
if (-not (Get-Command rustc -ErrorAction SilentlyContinue)) { throw 'rustc is not on PATH; activate the toolchain first (see WINDOWS_DEVELOPMENT.md)' }
$sysroot = (rustc --print sysroot).Trim()
$remaps = @(
    "--remap-path-prefix=$($releaseRoot)=uuldana",
    "--remap-path-prefix=$($cargoHome.TrimEnd('\'))=cargo",
    "--remap-path-prefix=$($sysroot)=rustc"
)
$present = @($env:RUSTFLAGS -split ' ')
$env:RUSTFLAGS = ((@($env:RUSTFLAGS) + ($remaps | Where-Object { $present -notcontains $_ })) | Where-Object { $_ }) -join ' '
Write-Host "RUSTFLAGS: $env:RUSTFLAGS"

function Assert-NoBuilderPaths {
    param([Parameter(Mandatory)][string]$Binary)
    $ErrorActionPreference = 'Stop'
    $bytes = [IO.File]::ReadAllBytes($Binary)
    $views = @(
        [Text.Encoding]::GetEncoding(28591).GetString($bytes),
        [Text.Encoding]::Unicode.GetString($bytes),
        [Text.Encoding]::Unicode.GetString($bytes, 1, $bytes.Length - 1)
    )
    foreach ($needle in @($env:USERNAME, $env:USERPROFILE, $releaseRoot)) {
        if (-not $needle) { continue }
        foreach ($view in $views) {
            if ($view.IndexOf($needle, [StringComparison]::OrdinalIgnoreCase) -ge 0) {
                throw "$Binary contains '$needle', a path or name of the machine that built it. Build with RUSTFLAGS from scripts\release-env.ps1 and try again."
            }
        }
    }
    Write-Host 'no user name or build path found inside the binary'
}
