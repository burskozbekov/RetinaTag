# RetinaTag — Face Recognition Model Downloader
# Run this script once before building to download InsightFace buffalo_s models.
# Models are placed in src-tauri/models/ and bundled into the installer.

$ErrorActionPreference = "Stop"

$modelsDir = "$PSScriptRoot\src-tauri\models"
$zipUrl = "https://github.com/deepinsight/insightface/releases/download/v0.7/buffalo_s.zip"
$zipPath = "$env:TEMP\buffalo_s.zip"

$detPath = "$modelsDir\det_500m.onnx"
$embPath = "$modelsDir\w600k_mbf.onnx"

if ((Test-Path $detPath) -and (Test-Path $embPath)) {
    Write-Host "Models already present in $modelsDir" -ForegroundColor Green
    exit 0
}

Write-Host "Downloading InsightFace buffalo_s models (~5 MB)..." -ForegroundColor Cyan
New-Item -ItemType Directory -Force -Path $modelsDir | Out-Null

try {
    $ProgressPreference = 'SilentlyContinue'
    Invoke-WebRequest -Uri $zipUrl -OutFile $zipPath -UseBasicParsing
    Write-Host "Download complete. Extracting..." -ForegroundColor Cyan

    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [System.IO.Compression.ZipFile]::OpenRead($zipPath)

    foreach ($entry in $zip.Entries) {
        if ($entry.Name -eq "det_500m.onnx") {
            [System.IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $detPath, $true)
            Write-Host "  Extracted det_500m.onnx" -ForegroundColor Green
        }
        if ($entry.Name -eq "w600k_mbf.onnx") {
            [System.IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $embPath, $true)
            Write-Host "  Extracted w600k_mbf.onnx" -ForegroundColor Green
        }
    }
    $zip.Dispose()
    Remove-Item $zipPath -ErrorAction SilentlyContinue

    if ((Test-Path $detPath) -and (Test-Path $embPath)) {
        Write-Host "`nFace recognition models ready!" -ForegroundColor Green
        Write-Host "  $detPath" -ForegroundColor Gray
        Write-Host "  $embPath" -ForegroundColor Gray
    } else {
        Write-Error "Extraction failed — model files not found in zip."
    }
} catch {
    Write-Error "Download failed: $_"
    exit 1
}
