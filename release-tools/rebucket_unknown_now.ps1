# v1.5.221 — One-shot Unknown/Unknown rescue.
# Walks D:\Fotograflar\Unknown\Unknown\, reads each file's EXIF
# DateTimeOriginal (System.Drawing fallback to mtime), and moves
# the file into D:\Fotograflar\<YYYY>\<MM-Month>\. Logs every move
# so we can sync the SQLite DB afterwards.

param(
    [string]$Root = 'D:\Fotograflar',
    [string]$LogPath = 'C:\Users\dede_\Desktop\RetinaTag\.claude\worktrees\beautiful-blackwell\release-tools\rebucket_moves.tsv'
)

Add-Type -AssemblyName System.Drawing

$months = @{
    1='January'; 2='February'; 3='March';     4='April';
    5='May';     6='June';     7='July';      8='August';
    9='September'; 10='October'; 11='November'; 12='December';
}

function Read-ExifDate($path) {
    try {
        $img = [System.Drawing.Image]::FromFile($path)
        try {
            # PropertyTag 0x9003 = DateTimeOriginal
            # PropertyTag 0x9004 = DateTimeDigitized
            # PropertyTag 0x0132 = DateTime (mtime within EXIF)
            foreach ($tag in 0x9003, 0x9004, 0x0132) {
                try {
                    $prop = $img.GetPropertyItem($tag)
                    if ($prop -and $prop.Value -and $prop.Value.Length -ge 19) {
                        $s = [System.Text.Encoding]::ASCII.GetString($prop.Value, 0, 19)
                        # Format: "YYYY:MM:DD HH:MM:SS"
                        if ($s -match '^(\d{4}):(\d{2}):(\d{2}) ') {
                            $y = [int]$matches[1]
                            $m = [int]$matches[2]
                            if ($y -ge 1980 -and $y -le 2100 -and $m -ge 1 -and $m -le 12) {
                                return @{ Year = $y; Month = $m; Source = 'EXIF' }
                            }
                        }
                    }
                } catch { }
            }
        } finally {
            $img.Dispose()
        }
    } catch { }
    return $null
}

$unknown = Join-Path $Root 'Unknown\Unknown'
if (-not (Test-Path $unknown)) {
    Write-Host "No $unknown folder — nothing to do."
    exit 0
}

# Reset log file
"OLD_PATH`tNEW_PATH`tSOURCE" | Out-File -FilePath $LogPath -Encoding UTF8

$files = Get-ChildItem -Path $unknown -File -Recurse -ErrorAction SilentlyContinue
$total = $files.Count
Write-Host "Found $total files in $unknown"

$moved = 0
$mtimeFallback = 0
$failed = 0
$i = 0

foreach ($f in $files) {
    $i++
    if ($i % 100 -eq 0) {
        $pct = [int](100 * $i / $total)
        Write-Host "  $i / $total ($pct%) · moved=$moved · mtime=$mtimeFallback · failed=$failed"
    }

    $oldPath = $f.FullName
    $ext = $f.Extension.ToLower()

    # Skip junk
    if ($f.Name -eq 'Thumbs.db' -or $f.Name -eq '.DS_Store') { continue }

    # Read EXIF for image files only; videos go straight to mtime
    $info = $null
    if ($ext -in '.jpg', '.jpeg', '.png', '.tif', '.tiff', '.heic') {
        $info = Read-ExifDate $oldPath
    }
    if (-not $info) {
        # mtime fallback
        $t = $f.LastWriteTime
        $info = @{ Year = $t.Year; Month = $t.Month; Source = 'mtime' }
        $mtimeFallback++
    }

    $monthName = $months[$info.Month]
    $bucket = Join-Path $Root ("{0:D4}\{1:D2}-{2}" -f $info.Year, $info.Month, $monthName)
    if (-not (Test-Path $bucket)) {
        New-Item -ItemType Directory -Path $bucket -Force | Out-Null
    }

    $newPath = Join-Path $bucket $f.Name
    if (Test-Path $newPath) {
        # Collision: if same size, treat as dup and remove the stranded copy
        $existSize = (Get-Item $newPath).Length
        if ($existSize -eq $f.Length) {
            Remove-Item $oldPath -Force -ErrorAction SilentlyContinue
            # Still log so DB row gets pointed at the existing destination
            "$oldPath`t$newPath`tdup-$($info.Source)" | Out-File -FilePath $LogPath -Encoding UTF8 -Append
            $moved++
            continue
        }
        # Different content — append _1, _2, ...
        $stem = [System.IO.Path]::GetFileNameWithoutExtension($f.Name)
        $extKeep = $f.Extension
        $n = 1
        do {
            $candidate = Join-Path $bucket ("{0}_{1}{2}" -f $stem, $n, $extKeep)
            $n++
        } while ((Test-Path $candidate) -and $n -lt 9999)
        $newPath = $candidate
    }

    try {
        Move-Item -LiteralPath $oldPath -Destination $newPath -Force
        "$oldPath`t$newPath`t$($info.Source)" | Out-File -FilePath $LogPath -Encoding UTF8 -Append
        $moved++
    } catch {
        $failed++
        Write-Host "  FAIL: $oldPath -> $($_.Exception.Message)"
    }
}

# Prune now-empty Unknown subtree
Remove-Item -Path (Join-Path $Root 'Unknown\Unknown') -Recurse -Force -ErrorAction SilentlyContinue
Remove-Item -Path (Join-Path $Root 'Unknown') -Force -ErrorAction SilentlyContinue

Write-Host ""
Write-Host "===== Done ====="
Write-Host "Total:           $total"
Write-Host "Moved:           $moved"
Write-Host "  via EXIF:      $($moved - $mtimeFallback)"
Write-Host "  via mtime:     $mtimeFallback"
Write-Host "Failed:          $failed"
Write-Host "Move log:        $LogPath"
