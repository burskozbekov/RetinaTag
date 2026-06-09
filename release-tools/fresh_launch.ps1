Get-Process retina-tag -ErrorAction SilentlyContinue | ForEach-Object {
    Write-Host ("Killing PID {0}" -f $_.Id)
    Stop-Process -Id $_.Id -Force
}
Start-Sleep -Seconds 2
Get-Process retina-tag -ErrorAction SilentlyContinue | ForEach-Object {
    Write-Host ("Still alive PID {0}, forcing again" -f $_.Id)
    Stop-Process -Id $_.Id -Force
}
Start-Sleep -Seconds 1
Write-Host '--- All killed, launching fresh ---'
$exe = 'C:\Users\dede_\AppData\Local\RetinaTag\retina-tag.exe'
Start-Process $exe
Start-Sleep -Seconds 6
$p = Get-Process retina-tag -ErrorAction SilentlyContinue
if ($p) {
    Write-Host ("FRESH: PID {0} Responding={1} Threads={2}" -f $p.Id, $p.Responding, $p.Threads.Count)
} else {
    Write-Host 'FAILED TO LAUNCH'
}
