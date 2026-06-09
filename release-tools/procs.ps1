Get-Process | Where-Object { $_.ProcessName -match 'retina|msedge|WebView' } | ForEach-Object {
    $mb = [math]::Round($_.WorkingSet64/1MB, 1)
    Write-Host ("{0,6} {1,-30} Responding={2,-5} Mem={3,7} MB Threads={4,3}" -f $_.Id, $_.ProcessName, $_.Responding, $mb, $_.Threads.Count)
}
