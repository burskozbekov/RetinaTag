# RetinaTag Windows release script
#
# Usage:
#   cd C:\Users\dede_\Desktop\RetinaTag\.claude\worktrees\beautiful-blackwell
#   .\release-tools\release.ps1
#
# What it does:
#   1. Reads the version from Cargo.toml.
#   2. Builds the Tauri release bundle (NSIS + updater bundle, signed).
#   3. Uploads installer + signature to the rolling `windows-latest` GitHub
#      Release (replaces previous build's artifacts).
#   4. Patches the cross-platform `latest.json` (preserves the existing
#      darwin-aarch64 entry, swaps the windows-x86_64 signature/url) and
#      uploads it to `mac-latest` so /releases/latest/download/latest.json
#      serves the new Windows update without breaking Mac auto-update.
#
# Requirements:
#   - retinatag.key on Desktop (Tauri signing private key, empty password).
#   - gh CLI authenticated (gh auth status).
#   - Cargo.toml + tauri.conf.json + src/lib.rs version comment updated.

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $repoRoot

# 1. Pull version from Cargo.toml so we don't drift between files.
$cargoToml = Get-Content "src-tauri\Cargo.toml" -Raw
if ($cargoToml -notmatch '(?m)^version\s*=\s*"([\d.]+)"') {
    throw "Could not parse version from src-tauri/Cargo.toml"
}
$version = $Matches[1]
Write-Host "Releasing RetinaTag v$version" -ForegroundColor Cyan

# 2. Build with signing.
$env:TAURI_SIGNING_PRIVATE_KEY_PATH = "C:\Users\dede_\Desktop\retinatag.key"
$env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = ""

Write-Host "Building (this takes ~5 min)..." -ForegroundColor Cyan
npm run tauri build -- --bundles updater
if ($LASTEXITCODE -ne 0) { throw "Tauri build failed" }

$nsisDir = "src-tauri\target\release\bundle\nsis"
$exePath = Join-Path $nsisDir "RetinaTag_${version}_x64-setup.exe"
$sigPath = "${exePath}.sig"

if (-not (Test-Path $exePath)) { throw "Installer not found: $exePath" }
if (-not (Test-Path $sigPath)) { throw "Signature not found: $sigPath. Check signing key env vars." }

# Stable, version-less aliases so the public download URL never changes
# across releases. Updater manifest also references these.
$exeAlias = Join-Path $nsisDir "RetinaTag-setup.exe"
$sigAlias = "${exeAlias}.sig"
Copy-Item -Force $exePath $exeAlias
Copy-Item -Force $sigPath $sigAlias

# 3. Upload to windows-latest rolling release (overwrites previous).
# Both versioned (audit trail / direct-download fallback) and aliased
# (the URL we publish on the website) artifacts are uploaded.
Write-Host "Uploading to windows-latest..." -ForegroundColor Cyan
gh release upload windows-latest $exePath $sigPath $exeAlias $sigAlias --clobber --repo burskozbekov/RetinaTag
if ($LASTEXITCODE -ne 0) { throw "gh release upload (windows-latest) failed" }

# Update title + notes for the windows-latest release.
gh release edit windows-latest `
    --repo burskozbekov/RetinaTag `
    --title "RetinaTag Windows $version" `
    --prerelease

# 4. Build the cross-platform latest.json.
$signature = Get-Content $sigPath -Raw
# Get-Content -Raw can include a trailing newline; minisign sigs end with \n
# already, so we trim and add exactly one newline back.
$signature = $signature.TrimEnd("`r", "`n") + "`n"

$pubDate = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")

# Preserve the existing darwin-aarch64 platform from the live manifest so we
# don't accidentally clobber Mac auto-update with a Windows-only file.
Write-Host "Fetching current latest.json to preserve macOS entry..." -ForegroundColor Cyan
$current = Invoke-RestMethod -Uri "https://github.com/burskozbekov/RetinaTag/releases/latest/download/latest.json"
$darwinPlatform = $current.platforms.'darwin-aarch64'

if (-not $darwinPlatform) {
    Write-Warning "No existing darwin-aarch64 in current latest.json — Mac users will lose auto-update if you publish this manifest. Aborting."
    throw "Refusing to publish a Windows-only manifest."
}

$manifest = [ordered]@{
    version  = $version
    notes    = "RetinaTag $version — see release pages for details."
    pub_date = $pubDate
    platforms = [ordered]@{
        'darwin-aarch64' = [ordered]@{
            signature = $darwinPlatform.signature
            url       = $darwinPlatform.url
        }
        'windows-x86_64' = [ordered]@{
            signature = $signature
            url       = "https://github.com/burskozbekov/RetinaTag/releases/download/windows-latest/RetinaTag-setup.exe"
        }
    }
}

$manifestPath = "release-tools\latest.json"
$manifest | ConvertTo-Json -Depth 6 | Out-File -FilePath $manifestPath -Encoding utf8

# 5. Upload manifest to mac-latest (the release flagged "latest" by GitHub).
Write-Host "Uploading multi-platform latest.json to mac-latest..." -ForegroundColor Cyan
gh release upload mac-latest $manifestPath --clobber --repo burskozbekov/RetinaTag
if ($LASTEXITCODE -ne 0) { throw "gh release upload (mac-latest manifest) failed" }

Write-Host ""
Write-Host "Done. v$version is live." -ForegroundColor Green
Write-Host "Verify: https://github.com/burskozbekov/RetinaTag/releases/latest/download/latest.json"
