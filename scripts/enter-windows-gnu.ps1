param(
    [string]$InstallRoot = (Join-Path $env:LOCALAPPDATA 'uuldana/toolchains/msys2-mingw64-20261012')
)
$bin = Join-Path $InstallRoot 'mingw64/bin'
foreach ($tool in @('as.exe', 'dlltool.exe')) {
    if (!(Test-Path -LiteralPath (Join-Path $bin $tool))) {
        throw "Missing $tool in $bin. Run ./scripts/install-windows-gnu.ps1 first."
    }
}
$env:PATH = (Join-Path $env:USERPROFILE '.cargo/bin') + ';' + $bin + ';' + $env:PATH
Write-Host "GNU toolchain PATH activated: $bin"
