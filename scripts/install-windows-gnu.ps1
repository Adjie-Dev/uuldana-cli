param(
    [string]$InstallRoot = (Join-Path $env:LOCALAPPDATA 'uuldana/toolchains/msys2-mingw64-20261012')
)
# Install only native MSYS2 binutils and its runtime dependencies, per user.
# No administrator privileges, registry changes, or compiler flag overrides.
$ErrorActionPreference = 'Stop'
$oldProgressPreference = $ProgressPreference
$ProgressPreference = 'SilentlyContinue'
try {
    if (![Environment]::Is64BitOperatingSystem -or $env:OS -ne 'Windows_NT') {
        throw 'This installer requires 64-bit Windows.'
    }
    $tar = Get-Command tar.exe -ErrorAction Stop
    $packages = Get-Content -Raw (Join-Path $PSScriptRoot 'windows-gnu-packages.json') | ConvertFrom-Json
    $install = [IO.Path]::GetFullPath($InstallRoot)
    $cache = Join-Path $install 'packages'
    New-Item -ItemType Directory -Force -Path $cache | Out-Null
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    foreach ($package in $packages) {
        $archive = Join-Path $cache $package.file
        if (!(Test-Path -LiteralPath $archive)) {
            $url = 'https://repo.msys2.org/mingw/mingw64/' + [Uri]::EscapeDataString($package.file)
            Write-Host "Download $url"
            Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $archive
        }
        $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $archive).Hash
        if ($actual -ne $package.sha256) {
            throw "SHA256 mismatch: $archive. Remove this archive and retry."
        }
        Write-Host "Extract $($package.file) (SHA256 verified)"
        & $tar.Source -xf $archive -C $install
        if ($LASTEXITCODE -ne 0) { throw "Extraction failed for $archive (tar needs zstd support)." }
    }
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'windows-gnu-packages.json') -Destination (Join-Path $install 'installed-packages.json') -Force
    $bin = Join-Path $install 'mingw64/bin'
    foreach ($tool in @('as.exe', 'dlltool.exe')) {
        & (Join-Path $bin $tool) --version
        if ($LASTEXITCODE -ne 0) { throw "$tool cannot run; installation is incomplete." }
    }
    $env:PATH = (Join-Path $env:USERPROFILE '.cargo/bin') + ';' + $bin + ';' + $env:PATH
    Write-Host "Installed in $install"
    Write-Host 'Cargo and GNU binutils are on PATH for this PowerShell session.'
    Write-Host 'For later sessions, run: . ./scripts/enter-windows-gnu.ps1'
} finally {
    $ProgressPreference = $oldProgressPreference
}
