<#
.SYNOPSIS
  Pester tests for the pure parts of scripts/perf-runtime.ps1. Nothing here
  launches Wingman.
  Run with: Invoke-Pester scripts/perf-runtime.Tests.ps1
#>

BeforeAll {
    $script:Script = Join-Path $PSScriptRoot 'perf-runtime.ps1'
    . $script:Script
}

Describe 'Get-Median' {
    It 'returns the middle of an odd count' {
        Get-Median -Values 5, 1, 3 | Should -Be 3
    }

    It 'averages the middle two of an even count' {
        Get-Median -Values 4, 1, 3, 2 | Should -Be 2.5
    }

    It 'ignores one outlier' {
        Get-Median -Values 100, 110, 5000, 105, 108 | Should -Be 108
    }

    It 'throws on no values' {
        { Get-Median -Values @() } | Should -Throw
    }
}

Describe 'the names the script waits on' {
    It 'match the window class and mutex in the source' {
        $app = (Get-Content -LiteralPath (Join-Path $PSScriptRoot '..\src\app.rs') -Raw) -replace "`r", ''
        $si = (Get-Content -LiteralPath (Join-Path $PSScriptRoot '..\src\single_instance.rs') -Raw) -replace "`r", ''
        $app | Should -Not -BeNullOrEmpty
        $si | Should -Not -BeNullOrEmpty
        $app.Contains('w!("' + $OwnerWindowClass + '")') | Should -BeTrue
        $si.Contains('w!("' + $InstanceMutexName.Replace('\', '\\') + '")') | Should -BeTrue
    }
}

Describe 'Test-MutexHeld' {
    It 'is false when nothing holds the name' {
        Test-MutexHeld -Name 'Local\Wingman.PerfRuntimeTest.00000000' | Should -BeFalse
    }

    It 'is true while something holds the name' {
        $name = 'Local\Wingman.PerfRuntimeTest.11111111'
        $m = New-Object System.Threading.Mutex($false, $name)
        try { Test-MutexHeld -Name $name | Should -BeTrue }
        finally { $m.Dispose() }
    }
}
