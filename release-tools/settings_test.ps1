Add-Type @"
using System;
using System.Runtime.InteropServices;
public class WinApi {
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);
    [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint lpdwProcessId);
    [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint idAttach, uint idAttachTo, bool fAttach);
    [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
    [DllImport("user32.dll", SetLastError = true)] public static extern uint SendInput(uint nInputs, INPUT[] pInputs, int cbSize);

    [StructLayout(LayoutKind.Sequential)]
    public struct INPUT {
        public uint type;
        public InputUnion U;
    }

    [StructLayout(LayoutKind.Explicit)]
    public struct InputUnion {
        [FieldOffset(0)] public KEYBDINPUT ki;
    }

    [StructLayout(LayoutKind.Sequential)]
    public struct KEYBDINPUT {
        public ushort wVk;
        public ushort wScan;
        public uint dwFlags;
        public uint time;
        public IntPtr dwExtraInfo;
    }
}
"@

function Send-Key([ushort]$vk, [bool]$keyUp) {
    $inp = New-Object WinApi+INPUT
    $inp.type = 1  # INPUT_KEYBOARD
    $inp.U.ki.wVk = $vk
    $inp.U.ki.dwFlags = $(if ($keyUp) { 2 } else { 0 })
    $arr = @($inp)
    [void][WinApi]::SendInput(1, $arr, [System.Runtime.InteropServices.Marshal]::SizeOf($inp))
}

# Step 1: Bring RetinaTag forward
$p = Get-Process retina-tag -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
if (-not $p) { Write-Host 'NO HANDLE'; return }
$h = $p.MainWindowHandle
$fgWin = [WinApi]::GetForegroundWindow()
$fgThreadId = 0
[void][WinApi]::GetWindowThreadProcessId($fgWin, [ref]$fgThreadId)
$currentTid = [WinApi]::GetCurrentThreadId()
[WinApi]::AttachThreadInput($fgThreadId, $currentTid, $true) | Out-Null
[WinApi]::ShowWindow($h, 9) | Out-Null
[WinApi]::BringWindowToTop($h) | Out-Null
[WinApi]::SetForegroundWindow($h) | Out-Null
[WinApi]::AttachThreadInput($fgThreadId, $currentTid, $false) | Out-Null
Start-Sleep -Milliseconds 800
$fg = [WinApi]::GetForegroundWindow()
Write-Host ("Foreground match: {0}" -f ($h -eq $fg))

# Step 2: Send Ctrl+S keystroke via SendInput
# VK_CONTROL = 0x11, VK_S = 0x53
Send-Key 0x11 $false   # Ctrl down
Start-Sleep -Milliseconds 50
Send-Key 0x53 $false   # S down
Start-Sleep -Milliseconds 50
Send-Key 0x53 $true    # S up
Start-Sleep -Milliseconds 50
Send-Key 0x11 $true    # Ctrl up

Write-Host 'Ctrl+S sent. Sleeping 3s before health check...'
Start-Sleep -Seconds 3

# Step 3: Check responsiveness
$p2 = Get-Process retina-tag -ErrorAction SilentlyContinue
if ($p2) {
    $mb = [math]::Round($p2.WorkingSet64/1MB, 1)
    Write-Host ("After Ctrl+S: PID {0} Responding={1} Mem={2}MB Threads={3}" -f $p2.Id, $p2.Responding, $mb, $p2.Threads.Count)
}
