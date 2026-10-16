<#
.SYNOPSIS
  Pester tests for scripts/perf-budget.ps1's pass/fail logic. Pure: the real
  script is only run against a stub file under $TestDrive.
  Run with: Invoke-Pester scripts/perf-budget.Tests.ps1
#>

BeforeAll {
    $script:Script = Join-Path $PSScriptRoot 'perf-budget.ps1'
    . $script:Script
    function Get-TestBudget([long]$Max) {
        '{"exe_size_bytes":{"max":' + $Max + ',"measured":1,"measured_on":"x","reason":"x"}}' | ConvertFrom-Json
    }
}

Describe 'Get-BudgetResult' {
    It 'passes when the measurement is under the budget' {
        $r = Get-BudgetResult -Budgets (Get-TestBudget 100) -Measured @{ exe_size_bytes = 99 }
        $r.Passed | Should -BeTrue
    }

    It 'passes exactly at the budget' {
        $r = Get-BudgetResult -Budgets (Get-TestBudget 100) -Measured @{ exe_size_bytes = 100 }
        $r.Passed | Should -BeTrue
    }

    It 'fails one byte over and reports the measured number' {
        $r = Get-BudgetResult -Budgets (Get-TestBudget 100) -Measured @{ exe_size_bytes = 101 }
        $r.Passed | Should -BeFalse
        $r.Measured | Should -Be 101
        $r.Max | Should -Be 100
    }

    It 'checks only the named budgets and ignores the rest' {
        $b = '{"exe_size_bytes":{"max":100},"startup_ms":{"max":5}}' | ConvertFrom-Json
        $r = @(Get-BudgetResult -Budgets $b -Measured @{ exe_size_bytes = 99 } -Names 'exe_size_bytes')
        $r.Count | Should -Be 1
        $r[0].Name | Should -Be 'exe_size_bytes'
    }

    It 'throws when a named budget is not in the budgets file' {
        { Get-BudgetResult -Budgets (Get-TestBudget 100) -Measured @{ startup_ms = 1 } -Names 'startup_ms' } | Should -Throw
    }

    It 'throws when a budget has no measurement' {
        { Get-BudgetResult -Budgets (Get-TestBudget 100) -Measured @{} } | Should -Throw
    }
}

Describe 'perf-budget.ps1 end to end' {
    BeforeEach {
        $script:exe = Join-Path $TestDrive 'stub.exe'
        [IO.File]::WriteAllBytes($script:exe, (New-Object byte[] 50))
        $script:budgets = Join-Path $TestDrive 'budgets.json'
    }

    It 'exits 0 and prints the number when under budget' {
        '{"exe_size_bytes":{"max":100}}' | Set-Content $script:budgets
        $out = & pwsh -NoProfile -File $script:Script -ExePath $script:exe -BudgetsPath $script:budgets
        $LASTEXITCODE | Should -Be 0
        ($out -join "`n") | Should -Match 'measured 50, budget 100'
    }

    It 'exits 1 and prints the number when over budget' {
        '{"exe_size_bytes":{"max":1}}' | Set-Content $script:budgets
        $out = & pwsh -NoProfile -File $script:Script -ExePath $script:exe -BudgetsPath $script:budgets
        $LASTEXITCODE | Should -Be 1
        ($out -join "`n") | Should -Match 'measured 50, budget 1 \(OVER BUDGET\)'
    }
}
