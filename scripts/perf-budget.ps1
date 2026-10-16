<#
.SYNOPSIS
  Fails when the release exe is larger than its budget in perf-budgets.json.
  Prints the measured number on every run, pass or fail.

.DESCRIPTION
  Only the exe size is gated here. It is deterministic. Startup time and idle
  memory need the app to run, so scripts/perf-runtime.ps1 gates those.

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
        [Parameter(Mandatory)] [hashtable] $Measured,
        # Check only these budgets (perf-runtime.ps1 owns the other two).
        [string[]] $Names
    )
    $props = $Budgets.PSObject.Properties
    if ($Names) {
        foreach ($n in $Names) {
            if (-not $Budgets.PSObject.Properties[$n]) { throw "no budget named '$n'" }
        }
        $props = $props | Where-Object { $Names -contains $_.Name }
    }
    foreach ($p in $props) {
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
foreach ($r in Get-BudgetResult -Budgets $budgets -Measured $measured -Names 'exe_size_bytes') {
    $status = if ($r.Passed) { 'ok' } else { 'OVER BUDGET' }
    "$($r.Name): measured $($r.Measured), budget $($r.Max) ($status)"
    if (-not $r.Passed) { $failed = $true }
}
if ($failed) { exit 1 }
