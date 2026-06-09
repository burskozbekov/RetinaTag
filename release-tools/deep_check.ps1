$exe = 'C:\Users\dede_\AppData\Local\RetinaTag\retina-tag.exe'
$built = 'C:\Users\dede_\Desktop\RetinaTag\.claude\worktrees\beautiful-blackwell\src-tauri\target\release\retina-tag.exe'

Write-Host '=== Installed vs Built ==='
$f1 = Get-Item $exe
$f2 = Get-Item $built
Write-Host ("Installed: {0:N0} bytes  mtime {1}  version {2}" -f $f1.Length, $f1.LastWriteTime, $f1.VersionInfo.ProductVersion)
Write-Host ("Built    : {0:N0} bytes  mtime {1}" -f $f2.Length, $f2.LastWriteTime)
$h1 = Get-FileHash $exe -Algorithm MD5
$h2 = Get-FileHash $built -Algorithm MD5
Write-Host ("Installed MD5: {0}" -f $h1.Hash)
Write-Host ("Built MD5    : {0}" -f $h2.Hash)
Write-Host ("Match: {0}" -f ($h1.Hash -eq $h2.Hash))

Write-Host ''
Write-Host '=== retina-tag processes ==='
$procs = Get-Process retina-tag -ErrorAction SilentlyContinue
foreach ($p in $procs) {
    $mb = [math]::Round($p.WorkingSet64/1MB, 1)
    $cpu = if ($p.CPU) { [math]::Round($p.CPU, 1) } else { 0 }
    Write-Host ("PID {0}  StartTime {1}  Responding={2}  CPU={3}s  Mem={4}MB  Threads={5}" -f $p.Id, $p.StartTime, $p.Responding, $cpu, $mb, $p.Threads.Count)
}

Write-Host ''
Write-Host '=== Database files ==='
$dbDir = 'C:\Users\dede_\AppData\Roaming\com.retinatag.app'
if (Test-Path $dbDir) {
    Get-ChildItem $dbDir -Filter 'retina*' | ForEach-Object {
        $mb = [math]::Round($_.Length/1MB, 2)
        Write-Host ("  {0,-20}  {1,8} MB  {2}" -f $_.Name, $mb, $_.LastWriteTime)
    }
}

Write-Host ''
Write-Host '=== Disk pressure on DB dir ==='
try {
    $drv = Get-PSDrive C
    Write-Host ("C: Free {0:N1} GB / Total {1:N1} GB" -f ($drv.Free/1GB), (($drv.Used+$drv.Free)/1GB))
} catch {}
