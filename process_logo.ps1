# RetinaTag — Logo Background Remover
# Saves the logo image as src-tauri/icons/logo-raw.png, then run this script.
# Output: dist/logo.png  (transparent background, multiple sizes)

param(
    [string]$InputPath = "$PSScriptRoot\src-tauri\icons\logo-raw.png"
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

if (-not (Test-Path $InputPath)) {
    Write-Error "Logo bulunamadi: $InputPath`nResmi once 'src-tauri\icons\logo-raw.png' olarak kaydedin."
    exit 1
}

function Remove-WhiteBackground {
    param($bmp, $tolerance = 30)

    $result = New-Object System.Drawing.Bitmap($bmp.Width, $bmp.Height, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($result)
    $g.DrawImage($bmp, 0, 0)
    $g.Dispose()

    for ($y = 0; $y -lt $result.Height; $y++) {
        for ($x = 0; $x -lt $result.Width; $x++) {
            $px = $result.GetPixel($x, $y)
            # If pixel is close to white, make it transparent
            if ($px.R -gt (255 - $tolerance) -and
                $px.G -gt (255 - $tolerance) -and
                $px.B -gt (255 - $tolerance)) {
                # Smooth edges: partial transparency based on how "white" the pixel is
                $whiteness = [Math]::Min($px.R, [Math]::Min($px.G, $px.B))
                $alpha = [Math]::Max(0, 255 - $whiteness)
                $newPx = [System.Drawing.Color]::FromArgb($alpha, $px.R, $px.G, $px.B)
                $result.SetPixel($x, $y, $newPx)
            }
        }
    }
    return $result
}

function Resize-Bitmap {
    param($bmp, $size)
    $resized = New-Object System.Drawing.Bitmap($size, $size)
    $g = [System.Drawing.Graphics]::FromImage($resized)
    $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $g.SmoothingMode     = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.DrawImage($bmp, 0, 0, $size, $size)
    $g.Dispose()
    return $resized
}

Write-Host "Logo isleniyor: $InputPath" -ForegroundColor Cyan

$original = New-Object System.Drawing.Bitmap($InputPath)
$transparent = Remove-WhiteBackground $original 25

# ── Outputs ──────────────────────────────────────────────────────────────────
$distDir   = "$PSScriptRoot\dist"
$iconsDir  = "$PSScriptRoot\src-tauri\icons"

# 1. dist/logo.png — sidebar logo (80px)
$logo80 = Resize-Bitmap $transparent 80
$logo80.Save("$distDir\logo.png", [System.Drawing.Imaging.ImageFormat]::Png)
Write-Host "  -> dist\logo.png (80x80)" -ForegroundColor Green

# 2. Tauri icon sizes
$sizes = @(32, 128, 256)
foreach ($s in $sizes) {
    $bmp = Resize-Bitmap $transparent $s
    # White background version for .ico compatibility
    $flat = New-Object System.Drawing.Bitmap($s, $s, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $fg   = [System.Drawing.Graphics]::FromImage($flat)
    $fg.Clear([System.Drawing.Color]::Transparent)
    $fg.DrawImage($bmp, 0, 0, $s, $s)
    $fg.Dispose()

    if ($s -eq 32)  { $flat.Save("$iconsDir\32x32.png",        [System.Drawing.Imaging.ImageFormat]::Png); Write-Host "  -> icons\32x32.png" -ForegroundColor Green }
    if ($s -eq 128) { $flat.Save("$iconsDir\128x128.png",      [System.Drawing.Imaging.ImageFormat]::Png); Write-Host "  -> icons\128x128.png" -ForegroundColor Green }
    if ($s -eq 256) { $flat.Save("$iconsDir\128x128@2x.png",   [System.Drawing.Imaging.ImageFormat]::Png); Write-Host "  -> icons\128x128@2x.png" -ForegroundColor Green }
    $bmp.Dispose(); $flat.Dispose()
}

# 3. dist/logo-white.png — inverted for any light-bg use
$logo80.Dispose()

$original.Dispose()
$transparent.Dispose()

Write-Host "`nLogo hazir! Artik uygulamayi yeniden derleyebilirsiniz." -ForegroundColor Green
