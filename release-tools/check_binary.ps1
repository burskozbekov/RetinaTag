$exe = 'C:\Users\dede_\AppData\Local\RetinaTag\retina-tag.exe'
$bytes = [System.IO.File]::ReadAllBytes($exe)
$text = [System.Text.Encoding]::ASCII.GetString($bytes)

$has1 = $text.IndexOf('backdrop-filter:blur') -ge 0
$has3 = $text.IndexOf('will-change:opacity') -ge 0
Write-Host ("Binary version: {0}" -f (Get-Item $exe).VersionInfo.ProductVersion)
Write-Host ("Contains backdrop-filter:blur : {0}  -- should be False" -f $has1)
Write-Host ("Contains will-change:opacity  : {0}  -- should be True" -f $has3)

$idx = 0
$count = 0
while (($idx = $text.IndexOf('backdrop-filter', $idx)) -ge 0) {
    $count++
    $start = [Math]::Max(0, $idx - 20)
    $end = [Math]::Min($text.Length, $idx + 50)
    $ctx = $text.Substring($start, $end - $start) -replace '[^ -~]', '.'
    Write-Host ("  match #{0} @ {1}: {2}" -f $count, $idx, $ctx)
    $idx++
    if ($count -gt 10) { break }
}
Write-Host ("Total backdrop-filter strings: {0}" -f $count)
