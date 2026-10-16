<#
.SYNOPSIS
  Smoke test for the release exe: it starts, a second launch hands off to the
  first and exits, and the first exits cleanly when asked. Fails with a plain
  message on the first step that does not hold.

.DESCRIPTION
  Like scripts/perf-runtime.ps1 this runs the real app, with the real mutex
  and window class names, so it is for a throwaway CI runner and refuses to
  start when a Wingman is already running. The process gets its own empty
  APPDATA and LOCALAPPDATA, so it never reads a real config.

  Steps:
    1. Launch. The owner window must appear and the process must go idle.
    2. Launch again with --settings. A bare duplicate launch would start an
       ask (a billed call on a machine with a key), so the smoke run asks for
       the settings window instead. The duplicate must exit with code 0 while
       the first process is still running. A duplicate that could not find
       the first instance would carry on running, so exiting is the proof of
       the hand-off.
    3. Post WM_CLOSE to the owner window. The first process must exit with
       code 0 and release the single-instance name.

  Every wait polls a condition with a short timeout, no fixed sleeps.

  Installing the package first is a later step (it needs a certificate).

  Usage: pwsh scripts/smoke-launch.ps1 [-ExePath target\release\wingman.exe]
#>
[CmdletBinding()]
param(
    [string]$ExePath = 'target\release\wingman.exe'
)

# Window class, mutex name and Test-MutexHeld, shared with perf-runtime.ps1
# (dot-sourcing it only loads them).
. (Join-Path $PSScriptRoot 'perf-runtime.ps1')

# From src/ui/settings.rs; a Pester test checks it still matches.
$SettingsWindowClass = 'Wingman.Settings.Window.9d4e2b71'

# Polls $Condition every $PollMs until it returns true or $TimeoutMs passes.
# The condition is always checked at least once.
function Wait-Until {
    param(
        [Parameter(Mandatory)] [scriptblock] $Condition,
        [int] $TimeoutMs = 10000,
        [int] $PollMs = 20
    )
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    while ($true) {
        if (& $Condition) { return $true }
        if ($sw.ElapsedMilliseconds -ge $TimeoutMs) { return $false }
        Start-Sleep -Milliseconds $PollMs
    }
}

# Dot-sourcing (the Pester tests do) loads the functions without running.
if ($MyInvocation.InvocationName -eq '.') { return }

function Exit-Smoke {
    param([string] $Message)
    throw "smoke test failed: $Message"
}

if (-not (Test-Path -LiteralPath $ExePath)) { throw "exe not found: $ExePath" }
if (Test-MutexHeld -Name $InstanceMutexName) {
    throw 'Wingman is already running. This script launches the real app, so it only runs when none is.'
}

Add-Type -Namespace Smoke -Name Win -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("user32.dll", CharSet = System.Runtime.InteropServices.CharSet.Unicode)]
public static extern System.IntPtr FindWindowW(string cls, string title);
[System.Runtime.InteropServices.DllImport("user32.dll")]
public static extern bool PostMessageW(System.IntPtr hwnd, uint msg, System.UIntPtr wparam, System.IntPtr lparam);
'@

$exe = (Resolve-Path -LiteralPath $ExePath).Path
$data = Join-Path ([IO.Path]::GetTempPath()) ('wingman-smoke-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $data | Out-Null

function Invoke-Wingman {
    param([string[]] $Arguments = @())
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $exe
    foreach ($a in $Arguments) { $psi.ArgumentList.Add($a) }
    $psi.UseShellExecute = $false
    $psi.Environment['APPDATA'] = $data
    $psi.Environment['LOCALAPPDATA'] = $data
    [System.Diagnostics.Process]::Start($psi)
}

$first = $null
$second = $null
try {
    # 1. It starts.
    $first = Invoke-Wingman
    $up = Wait-Until -TimeoutMs 30000 -Condition {
        if ($first.HasExited) { Exit-Smoke "first launch exited early with code $($first.ExitCode)" }
        [Smoke.Win]::FindWindowW($OwnerWindowClass, [NullString]::Value) -ne [IntPtr]::Zero
    }
    if (-not $up) { Exit-Smoke 'no owner window within 30 s' }
    if (-not $first.WaitForInputIdle(30000)) { Exit-Smoke 'first launch not idle within 30 s' }
    if (-not (Test-MutexHeld -Name $InstanceMutexName)) { Exit-Smoke 'first launch does not hold the single-instance name' }
    'ok: first launch is up and holds the single-instance name'

    # 2. A second launch hands off and exits.
    $second = Invoke-Wingman -Arguments @('--settings')
    if (-not $second.WaitForExit(15000)) { Exit-Smoke 'second launch did not exit within 15 s' }
    if ($second.ExitCode -ne 0) { Exit-Smoke "second launch exited with code $($second.ExitCode)" }
    $first.Refresh()
    if ($first.HasExited) { Exit-Smoke "first launch died during the hand-off (code $($first.ExitCode))" }
    $settings = [IntPtr]::Zero
    $opened = Wait-Until -TimeoutMs 10000 -Condition {
        $script:settings = [Smoke.Win]::FindWindowW($SettingsWindowClass, [NullString]::Value)
        $script:settings -ne [IntPtr]::Zero
    }
    if (-not $opened) { Exit-Smoke 'the first instance never opened the settings window it was asked for' }
    'ok: second launch handed off and exited 0, first opened settings and is still running'

    # 3. It exits cleanly when asked. The settings window is modal and the
    # owner window ignores WM_CLOSE while it is open, so close it first.
    $WM_CLOSE = 0x0010
    if (-not [Smoke.Win]::PostMessageW($script:settings, $WM_CLOSE, [UIntPtr]::Zero, [IntPtr]::Zero)) { Exit-Smoke 'could not post WM_CLOSE to settings' }
    $closed = Wait-Until -TimeoutMs 10000 -Condition {
        [Smoke.Win]::FindWindowW($SettingsWindowClass, [NullString]::Value) -eq [IntPtr]::Zero
    }
    if (-not $closed) { Exit-Smoke 'settings window did not close within 10 s' }
    $hwnd =[Smoke.Win]::FindWindowW($OwnerWindowClass, [NullString]::Value)
    if ($hwnd -eq [IntPtr]::Zero) { Exit-Smoke 'owner window vanished before the close' }
    if (-not [Smoke.Win]::PostMessageW($hwnd,$WM_CLOSE, [UIntPtr]::Zero, [IntPtr]::Zero)) { Exit-Smoke 'could not post WM_CLOSE' }
    if (-not $first.WaitForExit(15000)) { Exit-Smoke 'first launch did not exit within 15 s of WM_CLOSE' }
    if ($first.ExitCode -ne 0) { Exit-Smoke "first launch exited with code $($first.ExitCode)" }
    if (-not (Wait-Until -TimeoutMs 5000 -Condition { -not (Test-MutexHeld -Name $InstanceMutexName) })) {
        Exit-Smoke 'single-instance name still held after exit'
    }
    'ok: closed cleanly with code 0 and released the single-instance name'
}
finally {
    foreach ($p in @($first, $second)) {
        if ($p) {
            if (-not $p.HasExited) { $p.Kill(); $p.WaitForExit(10000) | Out-Null }
            $p.Dispose()
        }
    }
    Remove-Item -LiteralPath $data -Recurse -Force -ErrorAction SilentlyContinue
}
