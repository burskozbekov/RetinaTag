$retinaPid = (Get-Process retina-tag -ErrorAction SilentlyContinue | Select-Object -First 1).Id

# Check current PS elevation
$isElevated = ([System.Security.Principal.WindowsPrincipal][System.Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([System.Security.Principal.WindowsBuiltInRole]::Administrator)
Write-Host ("PowerShell elevated: {0}" -f $isElevated)

# Check retina-tag elevation via Win32 token
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class TokenInfo {
    [DllImport("kernel32.dll")] public static extern IntPtr OpenProcess(int dwDesiredAccess, bool bInheritHandle, int dwProcessId);
    [DllImport("kernel32.dll")] public static extern bool CloseHandle(IntPtr hObject);
    [DllImport("advapi32.dll", SetLastError = true)] public static extern bool OpenProcessToken(IntPtr ProcessHandle, int DesiredAccess, out IntPtr TokenHandle);
    [DllImport("advapi32.dll", SetLastError = true)] public static extern bool GetTokenInformation(IntPtr TokenHandle, int TokenInformationClass, IntPtr TokenInformation, int TokenInformationLength, out int ReturnLength);
}
"@

$h = [TokenInfo]::OpenProcess(0x1000, $false, $retinaPid)  # PROCESS_QUERY_LIMITED_INFORMATION
if ($h -eq [IntPtr]::Zero) {
    Write-Host "Could not open process token (likely retina-tag is at higher integrity)"
} else {
    $tok = [IntPtr]::Zero
    $ok = [TokenInfo]::OpenProcessToken($h, 0x0008, [ref]$tok)  # TOKEN_QUERY
    if ($ok) {
        $size = 4
        $ptr = [System.Runtime.InteropServices.Marshal]::AllocHGlobal($size)
        $ret = 0
        $ok2 = [TokenInfo]::GetTokenInformation($tok, 20, $ptr, $size, [ref]$ret)  # TokenElevation
        if ($ok2) {
            $val = [System.Runtime.InteropServices.Marshal]::ReadInt32($ptr)
            Write-Host ("retina-tag elevated: {0}" -f ($val -ne 0))
        }
        [System.Runtime.InteropServices.Marshal]::FreeHGlobal($ptr)
    }
    [void][TokenInfo]::CloseHandle($h)
}

# Also check integrity level via wmic-style
$proc = Get-CimInstance Win32_Process -Filter "ProcessId=$retinaPid"
Write-Host ("retina-tag SessionId: {0}" -f $proc.SessionId)

# Get PowerShell process info
$psPid = $PID
$psProc = Get-CimInstance Win32_Process -Filter "ProcessId=$psPid"
Write-Host ("PowerShell SessionId: {0}" -f $psProc.SessionId)
