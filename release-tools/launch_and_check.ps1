Start-Process 'C:\Users\dede_\AppData\Local\RetinaTag\retina-tag.exe'
Start-Sleep -Seconds 8
$p = Get-Process retina-tag -ErrorAction SilentlyContinue
if ($p) {
    $v = (Get-Item 'C:\Users\dede_\AppData\Local\RetinaTag\retina-tag.exe').VersionInfo.ProductVersion
    Write-Host ("running PID {0} v{1} Resp={2}" -f $p.Id, $v, $p.Responding)
} else {
    Write-Host 'WONT LAUNCH'
}
