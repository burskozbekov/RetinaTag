$p = Get-Process retina-tag -ErrorAction SilentlyContinue
if ($p) {
    $mb = [math]::Round($p.WorkingSet64/1MB, 1)
    Write-Host ("PID {0} Responding={1} Mem={2}MB Threads={3} Title='{4}'" -f $p.Id, $p.Responding, $mb, $p.Threads.Count, $p.MainWindowTitle)
} else {
    Write-Host 'DEAD'
}
