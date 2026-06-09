$exe = 'C:\Users\dede_\AppData\Local\RetinaTag\retina-tag.exe'
$bytes = [System.IO.File]::ReadAllBytes($exe)

# Search both ASCII and UTF-16 LE
$ascii = [System.Text.Encoding]::ASCII.GetString($bytes)
$utf16 = [System.Text.Encoding]::Unicode.GetString($bytes)

$tests = @(
    'backdrop-filter:blur',
    'modal-overlay',
    'RetinaTag',
    'gridWrap',
    'AI Photo Tagger'
)
foreach ($t in $tests) {
    $a = $ascii.IndexOf($t)
    $u = $utf16.IndexOf($t)
    Write-Host ("{0,-28} ASCII:{1,-8} UTF16:{2}" -f $t, $a, $u)
}

# Bundle DataStore at .data section can be Brotli-compressed
$brotliMarker = $ascii.IndexOf([char]0x1b)
Write-Host ("brotli marker: {0}" -f $brotliMarker)

# Tauri embeds via include_bytes! - the html is usually in .data section
Write-Host ("Binary size: {0:N0} bytes" -f $bytes.Length)
