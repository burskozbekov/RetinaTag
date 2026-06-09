# Test DB read speed directly. If this is slow, the freeze is disk/SQLite, not the app.
$db = 'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db'

# Try to find sqlite3.exe — bundled with Windows 10/11 in some paths, or in PATH
$sqlite = (Get-Command sqlite3.exe -ErrorAction SilentlyContinue).Source
if (-not $sqlite) {
    # Fall back to .NET System.Data.SQLite via PowerShell (may not be available)
    Write-Host 'sqlite3.exe not found in PATH'
    Write-Host 'Falling back to file-only check'

    Write-Host ''
    Write-Host '=== File timestamps ==='
    Get-ChildItem 'C:\Users\dede_\AppData\Roaming\com.retinatag.app\' -Filter 'retina*' | ForEach-Object {
        $mb = [math]::Round($_.Length/1MB, 2)
        Write-Host ("  {0,-20} {1,8} MB  mtime {2}  atime {3}" -f $_.Name, $mb, $_.LastWriteTime, $_.LastAccessTime)
    }

    Write-Host ''
    Write-Host '=== File locks ==='
    try {
        $stream = [System.IO.File]::Open($db, 'Open', 'Read', 'None')
        Write-Host 'DB readable with exclusive open'
        $stream.Close()
    } catch {
        Write-Host ("DB cannot be exclusively opened: {0}" -f $_.Exception.Message)
    }
    return
}

Write-Host ("Using sqlite3 at: {0}" -f $sqlite)
$t0 = Get-Date
$cnt = & $sqlite $db 'SELECT COUNT(*) FROM photos;' 2>&1
$ms = ((Get-Date) - $t0).TotalMilliseconds
Write-Host ("COUNT(*) photos = {0}  in {1:N0} ms" -f $cnt, $ms)

$t0 = Get-Date
$cnt = & $sqlite $db 'SELECT COUNT(*) FROM tags;' 2>&1
$ms = ((Get-Date) - $t0).TotalMilliseconds
Write-Host ("COUNT(*) tags = {0}  in {1:N0} ms" -f $cnt, $ms)

# Try a heavy query similar to get_photos
$t0 = Get-Date
$result = & $sqlite $db 'SELECT COUNT(*) FROM photos p LEFT JOIN tags t ON p.id = t.photo_id;' 2>&1
$ms = ((Get-Date) - $t0).TotalMilliseconds
Write-Host ("photos+tags JOIN count = {0}  in {1:N0} ms" -f $result, $ms)

Write-Host ''
Write-Host '=== WAL info ==='
$pragma = & $sqlite $db 'PRAGMA wal_checkpoint(PASSIVE);' 2>&1
Write-Host ("PRAGMA wal_checkpoint result: {0}" -f $pragma)
