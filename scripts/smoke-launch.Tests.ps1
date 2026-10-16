<#
.SYNOPSIS
  Pester tests for the pure parts of scripts/smoke-launch.ps1. Nothing here
  launches Wingman.
  Run with: Invoke-Pester scripts/smoke-launch.Tests.ps1
#>

BeforeAll {
    . (Join-Path $PSScriptRoot 'smoke-launch.ps1')
}

Describe 'Wait-Until' {
    It 'returns true as soon as the condition holds' {
        $script:n = 0
        Wait-Until -TimeoutMs 2000 -PollMs 1 -Condition { $script:n++; $script:n -ge 3 } | Should -BeTrue
        $script:n | Should -Be 3
    }

    It 'returns false when the condition never holds' {
        Wait-Until -TimeoutMs 50 -PollMs 5 -Condition { $false } | Should -BeFalse
    }

    It 'checks once even with a zero timeout' {
        Wait-Until -TimeoutMs 0 -PollMs 1 -Condition { $true } | Should -BeTrue
    }
}

Describe 'the second launch' {
    It 'uses --settings, because a bare duplicate launch would start an ask' {
        $text = (Get-Content -LiteralPath (Join-Path $PSScriptRoot 'smoke-launch.ps1') -Raw) -replace "`r", ''
        $text | Should -Not -BeNullOrEmpty
        $text.Contains("'--settings'") | Should -BeTrue
    }
}

Describe 'the settings window class' {
    It 'matches the class in the source' {
        $src = (Get-Content -LiteralPath (Join-Path $PSScriptRoot '..\src\ui\settings.rs') -Raw) -replace "`r", ''
        $src | Should -Not -BeNullOrEmpty
        $src.Contains('const CLASS_NAME: &str = "' + $SettingsWindowClass + '";') | Should -BeTrue
    }
}
