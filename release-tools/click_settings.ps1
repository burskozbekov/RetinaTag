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
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT lpRect);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint dwFlags, uint dx, uint dy, uint cButtons, IntPtr dwExtraInfo);

    [StructLayout(LayoutKind.Sequential)]
    public struct RECT {
        public int Left; public int Top; public int Right; public int Bottom;
    }
}
"@

$p = Get-Process retina-tag -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
if (-not $p) { Write-Host 'NO HANDLE'; exit }
$h = $p.MainWindowHandle

# Force foreground
$fgWin = [WinApi]::GetForegroundWindow()
$fgThreadId = 0
[void][WinApi]::GetWindowThreadProcessId($fgWin, [ref]$fgThreadId)
$currentTid = [WinApi]::GetCurrentThreadId()
[WinApi]::AttachThreadInput($fgThreadId, $currentTid, $true) | Out-Null
[WinApi]::ShowWindow($h, 9) | Out-Null
[WinApi]::BringWindowToTop($h) | Out-Null
[WinApi]::SetForegroundWindow($h) | Out-Null
[WinApi]::AttachThreadInput($fgThreadId, $currentTid, $false) | Out-Null
Start-Sleep -Milliseconds 1000

# Get window rect
$rect = New-Object WinApi+RECT
[void][WinApi]::GetWindowRect($h, [ref]$rect)
$winLeft = $rect.Left
$winTop = $rect.Top
$winW = $rect.Right - $rect.Left
$winH = $rect.Bottom - $rect.Top
Write-Host ("Window: ({0},{1}) {2}x{3}" -f $winLeft, $winTop, $winW, $winH)

$fg = [WinApi]::GetForegroundWindow()
Write-Host ("Foreground match: {0}" -f ($h -eq $fg))

# Settings button is in bottom-left of sidebar. From earlier UI tree it was at
# (1823, 1177) when window was at default position. Compute relative offset.
# Window is ~1456x939. Settings button is at relative offset roughly:
#   x = 166 (within 172-wide sidebar)
#   y = ~880 (near bottom of sidebar)
# Actually from earlier snapshot the absolute was (1823, 1177) and the window
# was 1456x939. The window position wasn't shown but from the maximized claude
# context it was probably roughly at (1655, 286) to (3111, 1225). So relative
# button offset = (1823-1655, 1177-286) = (168, 891).
$btnX = $winLeft + 168
$btnY = $winTop + 891
Write-Host ("Click target: ({0},{1})" -f $btnX, $btnY)

# Move cursor and click
[void][WinApi]::SetCursorPos($btnX, $btnY)
Start-Sleep -Milliseconds 200
# MOUSEEVENTF_LEFTDOWN = 0x0002, MOUSEEVENTF_LEFTUP = 0x0004
[WinApi]::mouse_event(0x0002, 0, 0, 0, [IntPtr]::Zero)
Start-Sleep -Milliseconds 50
[WinApi]::mouse_event(0x0004, 0, 0, 0, [IntPtr]::Zero)
Write-Host 'Click sent at Settings button location'

# Watch process for 10s
for ($i = 0; $i -lt 5; $i++) {
    Start-Sleep -Seconds 2
    $p2 = Get-Process -Id $p.Id -ErrorAction SilentlyContinue
    if ($p2) {
        $mb = [math]::Round($p2.WorkingSet64/1MB, 1)
        Write-Host ("[{0}s] PID {1} Responding={2} Mem={3}MB Threads={4}" -f (($i+1)*2), $p2.Id, $p2.Responding, $mb, $p2.Threads.Count)
    } else {
        Write-Host "DEAD at $i"; break
    }
}

# Screenshot
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$bitmap = New-Object System.Drawing.Bitmap($bounds.Width, $bounds.Height)
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
$graphics.CopyFromScreen(0, 0, 0, 0, $bitmap.Size)
$out = 'C:\Users\dede_\Desktop\RetinaTag\.claude\worktrees\beautiful-blackwell\release-tools\after_settings2.png'
$bitmap.Save($out)
$graphics.Dispose()
$bitmap.Dispose()
Write-Host $out
