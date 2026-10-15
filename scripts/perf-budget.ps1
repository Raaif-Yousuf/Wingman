<#
.SYNOPSIS
  Fails when the release exe is larger than its budget in perf-budgets.json.
  Prints the measured number on every run, pass or fail.

.DESCRIPTION
  Only the exe size is gated. It is deterministic. Startup time and idle
  memory need the app to run (tray icon, keyboard hook, single-instance
  mutex), which a CI runner cannot do without side effects, and they are
  too noisy on shared runners to gate tightly, so they are not measured
  here.

  Usage: pwsh scripts/perf-budget.ps1 [-ExePath target\release\wingman.exe]
                                      [-BudgetsPath perf-budgets.json]
#>
[CmdletBinding()]
param(
    [string]$ExePath = 'target\release\wingman.exe',
    [string]$BudgetsPath = (Join-Path $PSScriptRoot '..\perf-budgets.json')
)

# One result per budget: Name, Measured, Max, Passed.
function Get-BudgetResult {
    param(
        [Parameter(Mandatory)] $Budgets,
        [Parameter(Mandatory)] [hashtable] $Measured
    )
    foreach ($p in $Budgets.PSObject.Properties) {
        if (-not $Measured.ContainsKey($p.Name)) {
            throw "no measurement for budget '$($p.Name)'"
        }
        [pscustomobject]@{
            Name     = $p.Name
            Measured = [long]$Measured[$p.Name]
            Max      = [long]$p.Value.max
            Passed   = ([long]$Measured[$p.Name] -le [long]$p.Value.max)
        }
    }
}

# Dot-sourcing (the Pester tests do) loads the function without running.
if ($MyInvocation.InvocationName -eq '.') { return }

if (-not (Test-Path -LiteralPath $ExePath)) { throw "exe not found: $ExePath" }
$budgets = Get-Content -LiteralPath $BudgetsPath -Raw | ConvertFrom-Json
$measured = @{ exe_size_bytes = (Get-Item -LiteralPath $ExePath).Length }

$failed = $false
foreach ($r in Get-BudgetResult -Budgets $budgets -Measured $measured) {
    $status = if ($r.Passed) { 'ok' } else { 'OVER BUDGET' }
    "$($r.Name): measured $($r.Measured), budget $($r.Max) ($status)"
    if (-not $r.Passed) { $failed = $true }
}
if ($failed) { exit 1 }
