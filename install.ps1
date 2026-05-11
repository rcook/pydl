# pydl installer (Windows) — fetches the latest release archive and unpacks
# the `pydl.exe` binary into $env:PYDL_INSTALL_DIR (default:
# %LOCALAPPDATA%\Programs\pydl).
#
# Usage:
#   irm https://raw.githubusercontent.com/rcook/pydl/main/install.ps1 | iex
#   $env:PYDL_INSTALL_DIR = 'C:\tools\pydl'; iwr ... -OutFile install.ps1; .\install.ps1
#
# Supported host: x86_64 Windows (matches what release.yaml publishes today).

$ErrorActionPreference = 'Stop'

$repo       = 'rcook/pydl'
$releases   = "https://github.com/$repo/releases"
$latestApi  = "https://api.github.com/repos/$repo/releases/latest"
$installDir = if ($env:PYDL_INSTALL_DIR) { $env:PYDL_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\pydl' }

# Only AMD64 archives are published. Refuse to silently install the wrong
# binary on x86 / ARM hosts.
$arch = $env:PROCESSOR_ARCHITECTURE
if ($arch -ne 'AMD64') {
    Write-Error "pydl-install: unsupported host architecture '$arch'. See $releases for available archives."
    exit 1
}

Write-Host "pydl-install: querying $latestApi"
$release = Invoke-RestMethod -Uri $latestApi -Headers @{ 'User-Agent' = 'pydl-install' }
$asset = $release.assets |
    Where-Object { $_.name -like '*x86_64-pc-windows-msvc.zip' } |
    Select-Object -First 1
if (-not $asset) {
    Write-Error "pydl-install: no x86_64-pc-windows-msvc.zip asset in latest release. See $releases."
    exit 1
}

$tmpZip = Join-Path $env:TEMP $asset.name
Write-Host "pydl-install: downloading $($asset.browser_download_url)"
Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $tmpZip -UseBasicParsing

if (-not (Test-Path -LiteralPath $installDir)) {
    New-Item -ItemType Directory -Path $installDir -Force | Out-Null
}
Expand-Archive -Path $tmpZip -DestinationPath $installDir -Force
Remove-Item -LiteralPath $tmpZip

$exe = Join-Path $installDir 'pydl.exe'
Write-Host "pydl-install: installed $exe"

# PATH hint.
$pathDirs = ($env:Path -split ';') | Where-Object { $_ -ne '' }
if (-not ($pathDirs -contains $installDir)) {
    Write-Host "pydl-install: $installDir is not on your PATH. Add it with:"
    Write-Host "  setx PATH `"$env:Path;$installDir`""
}
