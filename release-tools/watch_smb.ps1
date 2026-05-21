# Show every file modified on the Fotograflar share in the last 10 minutes
$cut = (Get-Date).AddMinutes(-10)
Get-ChildItem 'D:\Fotograflar' -Recurse -Force -ErrorAction SilentlyContinue |
    Where-Object { $_.LastWriteTime -gt $cut } |
    Sort-Object LastWriteTime -Descending |
    Select-Object -First 30 LastWriteTime, FullName, @{Name='Bytes';Expression={$_.Length}} |
    Format-Table -AutoSize
