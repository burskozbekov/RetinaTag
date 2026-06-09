Get-Process retina-tag -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 3
$src = 'C:\Users\dede_\AppData\Roaming\com.retinatag.app'
$dst = "$env:TEMP\retina_probe"
if (Test-Path $dst) { Remove-Item -Recurse -Force $dst }
New-Item -ItemType Directory -Path $dst | Out-Null
Copy-Item -LiteralPath "$src\retina.db" -Destination "$dst\retina.db" -Force
if (Test-Path "$src\retina.db-wal") { Copy-Item -LiteralPath "$src\retina.db-wal" -Destination "$dst\retina.db-wal" -Force }
if (Test-Path "$src\retina.db-shm") { Copy-Item -LiteralPath "$src\retina.db-shm" -Destination "$dst\retina.db-shm" -Force }
Write-Host ("Copied DB + WAL to {0}" -f $dst)
Get-ChildItem $dst | ForEach-Object {
    $mb = [math]::Round($_.Length/1MB, 2)
    Write-Host ("  {0,-20}  {1,8} MB" -f $_.Name, $mb)
}
