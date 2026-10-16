<#
.SYNOPSIS
  Turns a cargo-llvm-cov JSON summary into a markdown table for the job
  summary: total line coverage, then pure logic and Win32-heavy code apart.

.DESCRIPTION
  Informational only. This script never fails on a low number.

  Win32-heavy means files that mostly drive windows, hooks, the clipboard or
  UI Automation. Those are checked by hand (AGENTS.md rule 8), so mixing them
  into one number hides how well the pure logic is covered. The split is a
  path pattern ($script:Win32Pattern), so it is easy to adjust.

  Usage: pwsh scripts/coverage-summary.ps1 -SummaryJson coverage\summary.json
#>
[CmdletBinding()]
param(
    [string]$SummaryJson = 'coverage\summary.json'
)

$script:Win32Pattern = '(^|/)src/(ui/(?!(palette_model|card_placement|card_buttons|pending_status|send_gate|text)\.rs)|hotkey\.rs|app\.rs|main\.rs|capture\.rs|ocr\.rs|speech\.rs|single_instance\.rs|autostart\.rs|dpapi\.rs|activation\.rs|executors/(target|uia_guard|clipboard|image_clipboard|replace_text|fill_form)\.rs|inputs/uia)'

function Test-Win32Path {
    param([Parameter(Mandatory)][string]$Path)
    return (($Path -replace '\\', '/') -match $script:Win32Pattern)
}

function Get-CoverageGroup {
    param([Parameter(Mandatory)][object[]]$Files)
    $groups = [ordered]@{ 'All code' = @(); 'Pure logic' = @(); 'Win32-heavy' = @() }
    foreach ($f in $Files) {
        $groups['All code'] += $f
        if (Test-Win32Path -Path $f.filename) { $groups['Win32-heavy'] += $f } else { $groups['Pure logic'] += $f }
    }
    foreach ($name in $groups.Keys) {
        $files = $groups[$name]
        $lc = [long](($files | ForEach-Object { $_.summary.lines.count } | Measure-Object -Sum).Sum)
        $lv = [long](($files | ForEach-Object { $_.summary.lines.covered } | Measure-Object -Sum).Sum)
        $fc = [long](($files | ForEach-Object { $_.summary.functions.count } | Measure-Object -Sum).Sum)
        $fv = [long](($files | ForEach-Object { $_.summary.functions.covered } | Measure-Object -Sum).Sum)
        [pscustomobject]@{
            Group        = $name
            Files        = @($files).Count
            Lines        = $lc
            LinesCovered = $lv
            LinePercent  = if ($lc -gt 0) { [math]::Round(100.0 * $lv / $lc, 2) } else { 0 }
            FuncPercent  = if ($fc -gt 0) { [math]::Round(100.0 * $fv / $fc, 2) } else { 0 }
        }
    }
}

function Format-CoverageMarkdown {
    param([Parameter(Mandatory)][object[]]$Rows)
    $out = @('| Group | Files | Lines | Covered | Line % | Function % |', '|---|---:|---:|---:|---:|---:|')
    foreach ($r in $Rows) {
        $out += "| $($r.Group) | $($r.Files) | $($r.Lines) | $($r.LinesCovered) | $($r.LinePercent) | $($r.FuncPercent) |"
    }
    return ($out -join "`n")
}

# Dot-sourcing (the Pester tests do) loads the functions without running.
if ($MyInvocation.InvocationName -eq '.') { return }

if (-not (Test-Path -LiteralPath $SummaryJson)) { throw "summary not found: $SummaryJson" }
$data = Get-Content -LiteralPath $SummaryJson -Raw | ConvertFrom-Json
$rows = @(Get-CoverageGroup -Files $data.data[0].files)
Format-CoverageMarkdown -Rows $rows
