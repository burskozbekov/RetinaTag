$exe = 'C:\Users\dede_\AppData\Local\RetinaTag\retina-tag.exe'
if (Test-Path $exe) {
    $v = (Get-Item $exe).VersionInfo.ProductVersion
    $mtime = (Get-Item $exe).LastWriteTime
    Write-Host ("Installed: v{0}  mtime {1}" -f $v, $mtime)
} else {
    Write-Host 'NOT INSTALLED'
}
