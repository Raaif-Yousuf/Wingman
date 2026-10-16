<#
.SYNOPSIS
  Pester tests for scripts/coverage-summary.ps1's grouping and totals. Pure:
  no coverage run is needed, the input is a hand-built file list.
  Run with: Invoke-Pester scripts/coverage-summary.Tests.ps1
#>

BeforeAll {
    . (Join-Path $PSScriptRoot 'coverage-summary.ps1')
    function Get-TestFileEntry([string]$Name, [long]$Count, [long]$Covered) {
        [pscustomobject]@{
            filename = $Name
            summary  = [pscustomobject]@{
                lines     = [pscustomobject]@{ count = $Count; covered = $Covered }
                functions = [pscustomobject]@{ count = 2; covered = 1 }
            }
        }
    }
}

Describe 'Test-Win32Path' {
    It 'treats window and hook code as Win32-heavy' {
        Test-Win32Path 'C:\r\Wingman\src\ui\card.rs' | Should -BeTrue
        Test-Win32Path 'C:\r\Wingman\src\hotkey.rs' | Should -BeTrue
        Test-Win32Path 'C:\r\Wingman\src\executors\target.rs' | Should -BeTrue
    }

    It 'treats parsers and models as pure' {
        Test-Win32Path 'C:\r\Wingman\src\config.rs' | Should -BeFalse
        Test-Win32Path 'C:\r\Wingman\src\ui\palette_model.rs' | Should -BeFalse
        Test-Win32Path 'C:\r\Wingman\src\provider\ollama.rs' | Should -BeFalse
    }
}

Describe 'Get-CoverageGroup' {
    It 'sums lines per group and keeps All code equal to the two parts' {
        $files = @(
            (Get-TestFileEntry 'x/src/config.rs' 100 80),
            (Get-TestFileEntry 'x/src/ui/card.rs' 100 20)
        )
        $rows = @(Get-CoverageGroup -Files $files)
        ($rows | Where-Object Group -eq 'All code').LinePercent | Should -Be 50
        ($rows | Where-Object Group -eq 'Pure logic').LinePercent | Should -Be 80
        ($rows | Where-Object Group -eq 'Win32-heavy').LinePercent | Should -Be 20
    }

    It 'reports 0 rather than dividing by zero for an empty group' {
        $rows = @(Get-CoverageGroup -Files @((Get-TestFileEntry 'x/src/config.rs' 10 10)))
        ($rows | Where-Object Group -eq 'Win32-heavy').LinePercent | Should -Be 0
    }
}

Describe 'Format-CoverageMarkdown' {
    It 'writes a header plus one row per group' {
        $rows = @(Get-CoverageGroup -Files @((Get-TestFileEntry 'x/src/config.rs' 10 5)))
        (Format-CoverageMarkdown -Rows $rows) -split "`n" | Should -HaveCount 5
    }
}
