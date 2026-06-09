$exe = 'C:\Users\dede_\AppData\Local\RetinaTag\retina-tag.exe'
Write-Host ("On-disk version: {0}" -f (Get-Item $exe).VersionInfo.ProductVersion)
Start-Process $exe
Start-Sleep -Seconds 10
$p = Get-Process retina-tag -ErrorAction SilentlyContinue
if ($p) {
    Write-Host ("Running PID {0} mem {1}MB threads {2}" -f $p.Id, [math]::Round($p.WorkingSet64/1MB, 1), $p.Threads.Count)
}
# Wait ANOTHER 60s and check tags
Write-Host 'Waiting 60s more for auto-import...'
Start-Sleep -Seconds 60
$p2 = Get-Process retina-tag -ErrorAction SilentlyContinue
if ($p2) {
    Write-Host ("After 70s: PID {0} mem {1}MB threads {2}" -f $p2.Id, [math]::Round($p2.WorkingSet64/1MB, 1), $p2.Threads.Count)
}
