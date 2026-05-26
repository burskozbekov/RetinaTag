# Downloads libmpv-2.dll for Windows x64 from zhongfly/mpv-winbuild and
# drops it under src-tauri/third-party/mpv/.  The dll is 97 MB and
# .gitignored — every fresh clone needs to run this once before
# `cargo build --release` succeeds with the libmpv link path.
#
# Idempotent: skips the download if the dll is already present.  Pass
# -Force to redownload (e.g. to pull in an upstream mpv update).
[CmdletBinding()]
param(
    [switch]$Force
)

$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
$mpvDir   = Join-Path $repoRoot 'src-tauri\third-party\mpv'
$dllPath  = Join-Path $mpvDir   'libmpv-2.dll'

if ((Test-Path $dllPath) -and -not $Force) {
    $mb = [math]::Round((Get-Item $dllPath).Length / 1MB, 1)
    Write-Host "libmpv-2.dll already present ($mb MB).  Pass -Force to redownload."
    return
}

New-Item -ItemType Directory -Path $mpvDir -Force | Out-Null

Write-Host "Looking up the latest mpv-dev-lgpl-x86_64 archive from zhongfly/mpv-winbuild..."
$api   = 'https://api.github.com/repos/zhongfly/mpv-winbuild/releases/latest'
$rel   = Invoke-RestMethod $api -Headers @{ 'User-Agent' = 'RetinaTag-Build' }
$asset = $rel.assets | Where-Object { $_.name -like 'mpv-dev-lgpl-x86_64-v3-*.7z' } | Select-Object -First 1
if (-not $asset) {
    $asset = $rel.assets | Where-Object { $_.name -like 'mpv-dev-lgpl-x86_64-*.7z' } | Select-Object -First 1
}
if (-not $asset) {
    throw 'No mpv-dev-lgpl-x86_64 asset found in latest release.'
}

$archive = Join-Path $mpvDir $asset.name
Write-Host "Downloading $($asset.name) ($([math]::Round($asset.size/1MB,1)) MB)..."
Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $archive -UseBasicParsing

# Find 7-Zip — required to extract .7z archives.
$sevenZ = @(
    'C:\Program Files\7-Zip\7z.exe',
    'C:\Program Files (x86)\7-Zip\7z.exe',
    "$env:LOCALAPPDATA\Programs\7-Zip\7z.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1

if (-not $sevenZ) {
    throw 'Could not find 7z.exe.  Install 7-Zip from https://www.7-zip.org/ and re-run.'
}

$extractDir = Join-Path $mpvDir '.extract'
& $sevenZ x $archive "-o$extractDir" -y | Out-Null

# Move just the files we need into the third-party/mpv root.
Move-Item -Force (Join-Path $extractDir 'libmpv-2.dll')  $dllPath
$importLib = Join-Path $extractDir 'libmpv.dll.a'
if (Test-Path $importLib) {
    Move-Item -Force $importLib (Join-Path $mpvDir 'libmpv.dll.a')
}
$includeSrc = Join-Path $extractDir 'include'
if (Test-Path $includeSrc) {
    if (Test-Path (Join-Path $mpvDir 'include')) {
        Remove-Item (Join-Path $mpvDir 'include') -Recurse -Force
    }
    Move-Item -Force $includeSrc (Join-Path $mpvDir 'include')
}

# Clean up
Remove-Item $extractDir -Recurse -Force
Remove-Item $archive -Force

$finalMb = [math]::Round((Get-Item $dllPath).Length / 1MB, 1)
Write-Host "Done.  libmpv-2.dll ($finalMb MB) installed at $dllPath"
