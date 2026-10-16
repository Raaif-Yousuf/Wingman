<#
.SYNOPSIS
  Launches the release exe a few times and fails when the median startup time
  or the median idle memory is over its budget in perf-budgets.json. Prints
  every run and the medians, pass or fail.

.DESCRIPTION
  This runs the real app: it creates the tray icon, installs the keyboard
  hook and takes the single-instance mutex. That is fine on a throwaway CI
  runner and wrong on a dev machine, where a second launch would poke the
  running instance. So the script refuses to start when the mutex name is
  already held. Each run also gets its own empty APPDATA and LOCALAPPDATA, so
  it never reads a real config or writes next to one.

  Startup time: from process start until the owner window exists and the
  process is waiting for input (the message loop is running, so the tray,
  hook and palette are all created).
  Idle memory: the working set two seconds after that.

  Shared runners are noisy, so the budgets in perf-budgets.json are set far
  above what CI measures, and the gate uses the median of several runs. It
  catches a large regression, not a small drift.

  Usage: pwsh scripts/perf-runtime.ps1 [-ExePath target\release\wingman.exe]
                                       [-BudgetsPath perf-budgets.json]
                                       [-Runs 5]
#>
[CmdletBinding()]
param(
    [string]$ExePath = 'target\release\wingman.exe',
    [string]$BudgetsPath = (Join-Path $PSScriptRoot '..\perf-budgets.json'),
    [int]$Runs = 5
)

# Both names are in the source (src/app.rs, src/single_instance.rs); a Pester
# test checks they still match.
$OwnerWindowClass = 'Wingman.Owner.Window.4d1b62f0'
$InstanceMutexName = 'Local\Wingman.SingleInstance.4d1b62f0'

function Get-Median {
    param([Parameter(Mandatory)] [double[]] $Values)
    if (-not $Values -or $Values.Count -eq 0) { throw 'no values' }
    $s = @($Values | Sort-Object)
    $mid = [int][math]::Floor($s.Count / 2)
    if ($s.Count % 2 -eq 1) { return $s[$mid] }
    return ($s[$mid - 1] + $s[$mid]) / 2
}

# True when a named mutex already exists (an instance of the app is running).
function Test-MutexHeld {
    param([Parameter(Mandatory)] [string] $Name)
    $m = $null
    if ([System.Threading.Mutex]::TryOpenExisting($Name, [ref]$m)) {
        $m.Dispose()
        return $true
    }
    return $false
}

# Dot-sourcing (the Pester tests do) loads the functions without running.
if ($MyInvocation.InvocationName -eq '.') { return }

. (Join-Path $PSScriptRoot 'perf-budget.ps1')

if (-not (Test-Path -LiteralPath $ExePath)) { throw "exe not found: $ExePath" }
if (Test-MutexHeld -Name $InstanceMutexName) {
    throw 'Wingman is already running. This script launches the real app, so it only runs when none is.'
}

Add-Type -Namespace Perf -Name Win -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("user32.dll", CharSet = System.Runtime.InteropServices.CharSet.Unicode)]
public static extern System.IntPtr FindWindowW(string cls, string title);
'@

# One launch. Returns startup milliseconds and idle working set in bytes.
function Measure-Launch {
    param([string] $Exe)
    $data = Join-Path ([IO.Path]::GetTempPath()) ("wingman-perf-" + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $data | Out-Null
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = (Resolve-Path -LiteralPath $Exe).Path
    $psi.UseShellExecute = $false
    $psi.Environment['APPDATA'] = $data
    $psi.Environment['LOCALAPPDATA'] = $data
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $proc = [System.Diagnostics.Process]::Start($psi)
    try {
        while ([Perf.Win]::FindWindowW($OwnerWindowClass, [NullString]::Value) -eq [IntPtr]::Zero) {
            if ($proc.HasExited) { throw "wingman exited with code $($proc.ExitCode) before its window appeared" }
            if ($sw.Elapsed.TotalSeconds -gt 30) { throw 'no owner window within 30 s' }
            Start-Sleep -Milliseconds 5
        }
        if (-not $proc.WaitForInputIdle(30000)) { throw 'not idle within 30 s' }
        $startup = $sw.Elapsed.TotalMilliseconds
        Start-Sleep -Seconds 2
        $proc.Refresh()
        if ($proc.HasExited) { throw "wingman exited with code $($proc.ExitCode) while idle" }
        [pscustomobject]@{ StartupMs = $startup; RssBytes = $proc.WorkingSet64 }
    }
    finally {
        if (-not $proc.HasExited) { $proc.Kill(); $proc.WaitForExit(10000) | Out-Null }
        $proc.Dispose()
        Remove-Item -LiteralPath $data -Recurse -Force -ErrorAction SilentlyContinue
    }
}

$results = foreach ($i in 1..$Runs) { Measure-Launch -Exe $ExePath }
$n = 0
foreach ($r in $results) {
    $n++
    "run ${n}: startup $([math]::Round($r.StartupMs)) ms, idle RSS $($r.RssBytes) bytes"
}

$budgets = Get-Content -LiteralPath $BudgetsPath -Raw | ConvertFrom-Json
$measured = @{
    startup_ms = [long][math]::Round((Get-Median -Values @($results | ForEach-Object { $_.StartupMs })))
    idle_rss_bytes = [long][math]::Round((Get-Median -Values @($results | ForEach-Object { [double]$_.RssBytes })))
}

$failed = $false
foreach ($r in Get-BudgetResult -Budgets $budgets -Measured $measured -Names 'startup_ms', 'idle_rss_bytes') {
    $status = if ($r.Passed) { 'ok' } else { 'OVER BUDGET' }
    "$($r.Name): median $($r.Measured), budget $($r.Max) ($status)"
    if (-not $r.Passed) { $failed = $true }
}
if ($failed) { exit 1 }
