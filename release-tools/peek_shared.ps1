# Look at everything Mac might have left in shared folders
$root = 'D:\Fotograflar'
Get-ChildItem $root -Force -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -like '.retinatag*' -or $_.Name -like '.retina*' } |
    ForEach-Object {
        Write-Host ''
        Write-Host ('=== ' + $_.FullName + ' ===')
        Get-ChildItem $_.FullName -Recurse -Force -ErrorAction SilentlyContinue |
            Sort-Object LastWriteTime -Descending |
            Select-Object -First 30 FullName,
                @{Name='Bytes';Expression={$_.Length}},
                LastWriteTime |
            Format-Table -AutoSize
    }
