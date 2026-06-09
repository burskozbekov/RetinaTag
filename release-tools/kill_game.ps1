Get-Process | Where-Object { $_.MainWindowTitle -match 'Slay|Spire' -or $_.ProcessName -match 'SlayThe|Spire' } | ForEach-Object {
    Write-Host ("Killing PID {0} ({1}): {2}" -f $_.Id, $_.ProcessName, $_.MainWindowTitle)
    Stop-Process -Id $_.Id -Force
}
Start-Sleep -Milliseconds 500
Get-Process | Where-Object { $_.MainWindowTitle -match 'Slay|Spire' -or $_.ProcessName -match 'SlayThe|Spire' } | ForEach-Object {
    Write-Host ("Still alive PID {0}, again" -f $_.Id)
    Stop-Process -Id $_.Id -Force
}
