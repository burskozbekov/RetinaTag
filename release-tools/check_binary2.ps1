$exe = 'C:\Users\dede_\AppData\Local\RetinaTag\retina-tag.exe'
$bytes = [System.IO.File]::ReadAllBytes($exe)
$text = [System.Text.Encoding]::ASCII.GetString($bytes)

$tests = @(
    'backdrop-filter',
    'will-change',
    'modal-overlay',
    'v1.5.97',
    'v1.5.95',
    'v1.5.92'
)
foreach ($t in $tests) {
    $cnt = 0
    $idx = 0
    while (($idx = $text.IndexOf($t, $idx)) -ge 0) { $cnt++; $idx++; if ($cnt -gt 50) { break } }
    Write-Host ("{0,-20} : {1} occurrences" -f $t, $cnt)
}
