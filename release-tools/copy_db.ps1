Get-Process retina-tag -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 2
$src = 'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db'
$dst = "$env:TEMP\retina_probe.db"
Copy-Item -LiteralPath $src -Destination $dst -Force
Write-Host ("Copied to {0}" -f $dst)
