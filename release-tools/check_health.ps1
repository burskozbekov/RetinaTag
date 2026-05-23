$p = Get-Process retina-tag -ErrorAction SilentlyContinue
if ($p) {
    $v = (Get-Item 'C:\Users\dede_\AppData\Local\RetinaTag\retina-tag.exe').VersionInfo.ProductVersion
    $mem = [int]($p.WorkingSet64 / 1MB)
    Write-Host ('PID {0}  v{1}  Resp={2}  Mem={3}MB' -f $p.Id, $v, $p.Responding, $mem)
} else {
    Write-Host 'NOT RUNNING'
}
