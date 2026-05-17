$p = Get-Process retina-tag -ErrorAction SilentlyContinue
if ($p) {
    $v = (Get-Item 'C:\Users\dede_\AppData\Local\RetinaTag\retina-tag.exe').VersionInfo.ProductVersion
    $mem = '{0:N0}' -f ($p.WorkingSet64/1MB)
    $up = ((Get-Date) - $p.StartTime).ToString('hh\:mm\:ss')
    Write-Host ("PID {0} v{1} Resp={2} Mem={3}MB Uptime={4}" -f $p.Id, $v, $p.Responding, $mem, $up)
} else {
    Write-Host 'NOT RUNNING'
}
