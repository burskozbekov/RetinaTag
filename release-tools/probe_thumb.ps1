$candidates = @(
    'D:\Fotograflar\RetinaTag\retina.db',
    "$env:USERPROFILE\Pictures\RetinaTag\retina.db",
    "$env:APPDATA\com.retinatag.app\retina.db"
)
$db = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $db) { Write-Host 'NO DB'; exit 1 }
Write-Host "DB: $db"

# Pull bearer token + addr+port from lan_peer_tokens via sqlite3
$sqlite = Get-Command sqlite3.exe -ErrorAction SilentlyContinue
if ($sqlite) {
    Write-Host "--- lan_peer_tokens ---"
    & sqlite3.exe $db "SELECT peer_name, addr, port, substr(token,1,16) FROM lan_peer_tokens;"
    Write-Host "--- token (full) ---"
    $token = & sqlite3.exe $db "SELECT token FROM lan_peer_tokens LIMIT 1;"
    $token = $token.Trim()
    Write-Host "Token: $($token.Substring(0,8))...$($token.Substring($token.Length-8))"

    # Try Mac /api/thumb/1 with bearer
    Write-Host "--- GET http://192.168.1.111:9876/api/thumb/1 ---"
    try {
        $r = Invoke-WebRequest -Uri 'http://192.168.1.111:9876/api/thumb/1' `
            -Headers @{ 'Authorization' = "Bearer $token" } `
            -UseBasicParsing -TimeoutSec 10
        Write-Host "Status: $($r.StatusCode)"
        Write-Host "Content-Type: $($r.Headers['Content-Type'])"
        Write-Host "Content-Length: $($r.RawContentLength)"
        $first16 = ($r.Content[0..15] | ForEach-Object { '{0:X2}' -f $_ }) -join ' '
        Write-Host "First 16 bytes: $first16"
    } catch {
        Write-Host "ERROR: $($_.Exception.Message)"
        if ($_.Exception.Response) {
            Write-Host "Status: $($_.Exception.Response.StatusCode)"
        }
    }

    # Also probe a few other photo IDs in case 1 doesn't exist on Mac
    Write-Host "--- Mac's /api/photos?vault_only=true&limit=3 (with token) ---"
    try {
        $r = Invoke-WebRequest -Uri 'http://192.168.1.111:9876/api/photos?vault_only=true&offset=0&limit=3' `
            -Headers @{ 'Authorization' = "Bearer $token" } `
            -UseBasicParsing -TimeoutSec 10
        Write-Host "Status: $($r.StatusCode)"
        Write-Host "Body (truncated): $(($r.Content | Out-String).Substring(0, [Math]::Min(800, $r.Content.Length)))"
    } catch {
        Write-Host "ERROR: $($_.Exception.Message)"
    }
} else {
    Write-Host 'sqlite3.exe not on PATH'
}
