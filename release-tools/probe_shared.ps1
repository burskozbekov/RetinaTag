$db = 'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db'

# DB locked by retina-tag — copy it to temp and inspect
$tmp = "$env:TEMP\retina_probe.db"
try {
    Copy-Item $db $tmp -Force
} catch {
    Write-Host "DB busy — kill retina-tag briefly"
    Get-Process retina-tag -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep 2
    Copy-Item $db $tmp -Force
}

# Use System.Data.Sqlite if available; otherwise SQLite via P/Invoke
# Simpler: use Python sqlite3 if installed, else parse manually via System.Data
$python = (Get-Command python -ErrorAction SilentlyContinue).Source
if (-not $python) { $python = (Get-Command python3 -ErrorAction SilentlyContinue).Source }
if ($python) {
    & $python -c "import sqlite3; c=sqlite3.connect(r'$tmp'); cur=c.cursor(); print('--- watch folders ---'); [print(r) for r in cur.execute('SELECT path, enabled, auto_tag FROM watch_folders LIMIT 20')]; print(); print('--- distinct folders (top 30 by photo count) ---'); [print(r) for r in cur.execute('SELECT folder, COUNT(*) as c FROM photos GROUP BY folder ORDER BY c DESC LIMIT 30')]; print(); print('--- relevant settings ---'); [print(r) for r in cur.execute(\"SELECT key, value FROM settings WHERE key LIKE '%xmp%' OR key LIKE '%metadata%' OR key LIKE '%sidecar%' OR key LIKE '%embed%' OR key LIKE '%auto%'\")]"
    return
}
Write-Host "Python not found. Will list folders via app instead."
