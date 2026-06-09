$retinaPid = (Get-Process retina-tag -ErrorAction SilentlyContinue | Select-Object -First 1).Id
function Get-DescendantPids($parentPid) {
    $allPids = @($parentPid)
    $changed = $true
    while ($changed) {
        $changed = $false
        $children = Get-CimInstance Win32_Process | Where-Object { $_.ParentProcessId -in $allPids -and ($_.ProcessId -notin $allPids) }
        foreach ($c in $children) { $allPids += $c.ProcessId; $changed = $true }
    }
    return $allPids
}
$desc = Get-DescendantPids -parentPid $retinaPid

# Find the renderer
$renderer = $null
foreach ($pid_ in $desc) {
    $cmi = Get-CimInstance Win32_Process -Filter "ProcessId=$pid_" -ErrorAction SilentlyContinue
    if ($cmi -and $cmi.CommandLine -like '*--type=renderer*') {
        $renderer = $pid_
        break
    }
}
if (-not $renderer) { Write-Host 'No renderer found'; exit }
Write-Host ("Renderer PID: {0}" -f $renderer)

# Sample CPU
Write-Host 'Sampling 5 seconds...'
$samples = @()
for ($i = 0; $i -lt 10; $i++) {
    $p = Get-Process -Id $renderer -ErrorAction SilentlyContinue
    if (-not $p) { Write-Host 'DEAD'; break }
    $samples += $p.CPU
    Start-Sleep -Milliseconds 500
}
$delta = $samples[-1] - $samples[0]
Write-Host ("Renderer CPU delta over 5s: {0:N2}s of CPU time" -f $delta)
Write-Host ("=> CPU% (single core) ~ {0:N1}%" -f ($delta/5*100))

# Also count handles to see if many are held open
$p = Get-Process -Id $renderer -ErrorAction SilentlyContinue
Write-Host ("Renderer HandleCount: {0}" -f $p.HandleCount)
Write-Host ("Renderer Threads: {0}" -f $p.Threads.Count)
Write-Host ("Renderer PeakWorkingSet: {0:N1} MB" -f ($p.PeakWorkingSet64/1MB))
Write-Host ("Renderer Responding: {0}" -f $p.Responding)
