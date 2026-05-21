# Anything modified on D:\Fotograflar in the last 2 hours, anywhere
$cut = (Get-Date).AddHours(-2)
Get-ChildItem 'D:\Fotograflar' -Recurse -Force -ErrorAction SilentlyContinue |
    Where-Object { $_.LastWriteTime -gt $cut -and -not $_.PSIsContainer } |
    Sort-Object LastWriteTime -Descending |
    Select-Object -First 25 LastWriteTime, FullName, @{Name='KB';Expression={[math]::Round($_.Length/1KB,1)}} |
    Format-Table -AutoSize
