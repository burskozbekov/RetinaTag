# v1.5.224 — Snapshot the PC's retina.db to the shared library root
# so Mac can read it over SMB. NOT a live-shared DB — this is a copy.
# The PC keeps writing to the real DB in AppData\Roaming. Re-run this
# script (or wire it to app shutdown / a hotkey) when Mac needs a fresh
# snapshot.

param(
    [string]$Src  = 'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db',
    [string]$DstRoot = 'D:\Fotograflar',
    [string]$DstName = 'retina-pc.db'
)

$dstDir  = Join-Path $DstRoot '.retinatag-state'
$dstFile = Join-Path $dstDir $DstName

if (-not (Test-Path $dstDir)) {
    New-Item -ItemType Directory -Path $dstDir -Force | Out-Null
    # Mark the dir hidden on Windows so it doesn't pollute Explorer
    # views of the library root. macOS already hides dot-prefixed
    # paths by default.
    attrib +h $dstDir
}

Copy-Item -Path $Src -Destination $dstFile -Force

$info = Get-Item $dstFile
$mb = [math]::Round($info.Length / 1MB, 1)
Write-Host ('Snapshot OK')
Write-Host ('  Source: ' + $Src)
Write-Host ('  Dest:   ' + $dstFile)
Write-Host ('  Size:   ' + $mb + ' MB')
Write-Host ('  Time:   ' + $info.LastWriteTime)
