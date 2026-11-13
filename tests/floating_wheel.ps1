param([int]$TestPid, [string]$ExpectedExe, [ValidateSet('content','title','outside')][string]$Area='content')
$ErrorActionPreference='Stop'
Add-Type @'
using System;
using System.Runtime.InteropServices;
public class FloatWheel {
 [StructLayout(LayoutKind.Sequential)] public struct RECT { public int left,top,right,bottom; }
 [StructLayout(LayoutKind.Sequential)] public struct POINT { public int x,y; }
 [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
 [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h,out RECT r);
 [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h,ref POINT p);
 [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr h);
 [DllImport("user32.dll",EntryPoint="PostMessageW")] public static extern bool PostMessage(IntPtr h,uint msg,IntPtr w,IntPtr l);
}
'@
[void][FloatWheel]::SetProcessDPIAware()
$p=Get-Process -Id $TestPid
if($p.Path -ne $ExpectedExe){throw 'Not the owned test executable'}
$h=$p.MainWindowHandle;if($h -eq 0){throw 'Test window not ready'}
$r=New-Object FloatWheel+RECT;[void][FloatWheel]::GetClientRect($h,[ref]$r)
$scale=[FloatWheel]::GetDpiForWindow($h)/96.0
$at=New-Object FloatWheel+POINT
$at.x=[int](($r.right-$r.left)*0.5)
$at.y=[int](($r.bottom-$r.top)*0.5)
if($Area -eq 'outside'){$at.x=[int](($r.right-$r.left)*0.05)}
if($Area -eq 'title'){$at.y=[int](36*$scale + (($r.bottom-$r.top)-36*$scale)*0.2 + 10*$scale)}
[void][FloatWheel]::PostMessage($h,0x200,[IntPtr]0,[IntPtr]($at.x -bor ($at.y -shl 16)))
[void][FloatWheel]::ClientToScreen($h,[ref]$at)
Start-Sleep -Milliseconds 150
[void][FloatWheel]::PostMessage($h,0x20A,[IntPtr](120 -shl 16),[IntPtr]($at.x -bor ($at.y -shl 16)))
Start-Sleep -Milliseconds 500
Write-Output "WHEEL area=$Area test_pid=$TestPid"
