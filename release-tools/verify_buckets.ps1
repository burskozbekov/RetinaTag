$root = 'D:\Fotograflar'
Write-Host "Unknown folder: " (Test-Path (Join-Path $root 'Unknown'))
Write-Host ""
Write-Host "YYYY / MM buckets with files:"
Get-ChildItem $root -Directory | Where-Object Name -match '^\d{4}$' | Sort-Object Name | ForEach-Object {
    $y = $_.Name
    Get-ChildItem $_.FullName -Directory -ErrorAction SilentlyContinue | ForEach-Object {
        $files = (Get-ChildItem $_.FullName -File -ErrorAction SilentlyContinue | Measure-Object).Count
        if ($files -gt 0) {
            [PSCustomObject]@{ Year = $y; Month = $_.Name; Files = $files }
        }
    }
} | Sort-Object Year, Month | Format-Table -AutoSize
