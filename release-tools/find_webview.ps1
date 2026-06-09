$retinaPid = (Get-Process retina-tag -ErrorAction SilentlyContinue | Select-Object -First 1).Id
Write-Host ("RetinaTag PID: {0}" -f $retinaPid)

# Walk parent tree to find all descendants
function Get-DescendantPids($parentPid) {
    $allPids = @($parentPid)
    $changed = $true
    while ($changed) {
        $changed = $false
        $children = Get-CimInstance Win32_Process | Where-Object { $_.ParentProcessId -in $allPids -and ($_.ProcessId -notin $allPids) }
        foreach ($c in $children) {
            $allPids += $c.ProcessId
            $changed = $true
        }
    }
    return $allPids
}

$descendantPids = Get-DescendantPids -parentPid $retinaPid
Write-Host ("Descendant PIDs: {0}" -f ($descendantPids -join ', '))

Write-Host ''
Write-Host '=== Process tree ==='
foreach ($pid_ in $descendantPids) {
    $p = Get-Process -Id $pid_ -ErrorAction SilentlyContinue
    if ($p) {
        $mb = [math]::Round($p.WorkingSet64/1MB, 1)
        $cmi = Get-CimInstance Win32_Process -Filter "ProcessId=$pid_" -ErrorAction SilentlyContinue
        $cmd = if ($cmi) { $cmi.CommandLine } else { '' }
        # Truncate command line
        if ($cmd.Length -gt 120) { $cmd = $cmd.Substring(0, 117) + '...' }
        Write-Host ("  PID {0,6}  {1,-22} Resp={2,-5}  Mem={3,7}MB  Th={4,3}" -f $p.Id, $p.ProcessName, $p.Responding, $mb, $p.Threads.Count)
        Write-Host ("           cmd: {0}" -f $cmd)
    }
}
