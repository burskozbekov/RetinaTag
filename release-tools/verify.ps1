$exe = 'C:\Users\dede_\AppData\Local\RetinaTag\retina-tag.exe'
$f = Get-Item $exe
Write-Host ("Binary path: {0}" -f $f.FullName)
Write-Host ("Binary size: {0:N0} bytes ({1:N1} MB)" -f $f.Length, ($f.Length/1MB))
Write-Host ("Binary mtime: {0}" -f $f.LastWriteTime)
Write-Host ("Version: {0}" -f $f.VersionInfo.ProductVersion)

# Build artifact comparison
$built = 'C:\Users\dede_\Desktop\RetinaTag\.claude\worktrees\beautiful-blackwell\src-tauri\target\release\retina-tag.exe'
if (Test-Path $built) {
    $b = Get-Item $built
    Write-Host ''
    Write-Host ("Built artifact: {0}" -f $b.FullName)
    Write-Host ("Built size: {0:N0} bytes" -f $b.Length)
    Write-Host ("Built mtime: {0}" -f $b.LastWriteTime)
    Write-Host ("Match: {0}" -f ($f.Length -eq $b.Length))
}

# Check WebView2 data cache
$wvCache = 'C:\Users\dede_\AppData\Local\com.retinatag.app\EBWebView'
if (Test-Path $wvCache) {
    $items = Get-ChildItem $wvCache -Recurse -File -ErrorAction SilentlyContinue | Measure-Object Length -Sum
    Write-Host ''
    Write-Host ("WebView2 cache: {0} files, {1:N1} MB" -f $items.Count, ($items.Sum/1MB))
}
