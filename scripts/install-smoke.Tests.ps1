<#
.SYNOPSIS
  Pester tests for the pure parts of scripts/install-smoke.ps1. Nothing here
  installs or launches Wingman.
  Run with: Invoke-Pester scripts/install-smoke.Tests.ps1
#>

BeforeAll {
    . (Join-Path $PSScriptRoot 'install-smoke.ps1')
    $script:me = 'S-1-5-21-1-2-3-1001'
}

Describe 'Get-OwnerOnlyAclProblem' {
    It 'accepts one allow entry for the owner with inheritance cut' {
        $rules = @([pscustomobject]@{ Sid = $me; Type = 'Allow' })
        @(Get-OwnerOnlyAclProblem -IsProtected $true -Rules $rules -OwnerSid $me).Count | Should -Be 0
    }

    It 'flags inherited entries' {
        $rules = @([pscustomobject]@{ Sid = $me; Type = 'Allow' })
        Get-OwnerOnlyAclProblem -IsProtected $false -Rules $rules -OwnerSid $me | Should -Match 'inheritance'
    }

    It 'flags access for anyone else' {
        $rules = @(
            [pscustomobject]@{ Sid = $me; Type = 'Allow' },
            [pscustomobject]@{ Sid = 'S-1-5-32-545'; Type = 'Allow' }
        )
        Get-OwnerOnlyAclProblem -IsProtected $true -Rules $rules -OwnerSid $me | Should -Match 'S-1-5-32-545'
        $rules = @([pscustomobject]@{ Sid = 'S-1-1-0'; Type = 'Allow' }, $rules[0])
        Get-OwnerOnlyAclProblem -IsProtected $true -Rules $rules -OwnerSid $me | Should -Match 'S-1-1-0'
    }

    It 'allows SYSTEM and Administrators beside the owner' {
        $rules = @(
            [pscustomobject]@{ Sid = 'S-1-5-18'; Type = 'Allow' },
            [pscustomobject]@{ Sid = 'S-1-5-32-544'; Type = 'Allow' },
            [pscustomobject]@{ Sid = $me; Type = 'Allow' }
        )
        @(Get-OwnerOnlyAclProblem -IsProtected $true -Rules $rules -OwnerSid $me).Count | Should -Be 0
    }

    It 'flags a list with no owner entry or no entries' {
        $other = @([pscustomobject]@{ Sid = 'S-1-5-18'; Type = 'Allow' })
        (Get-OwnerOnlyAclProblem -IsProtected $true -Rules $other -OwnerSid $me) -join ';' | Should -Match 'no allow entry|current user'
        Get-OwnerOnlyAclProblem -IsProtected $true -Rules @() -OwnerSid $me | Should -Match 'no allow entry'
    }

    It 'ignores deny entries when looking for extra access' {
        $rules = @(
            [pscustomobject]@{ Sid = $me; Type = 'Allow' },
            [pscustomobject]@{ Sid = 'S-1-1-0'; Type = 'Deny' }
        )
        @(Get-OwnerOnlyAclProblem -IsProtected $true -Rules $rules -OwnerSid $me).Count | Should -Be 0
    }
}

Describe 'the zip file set' {
    It 'matches the files release.yml puts in the zip' {
        $yml = (Get-Content -LiteralPath (Join-Path $PSScriptRoot '..\.github\workflows\release.yml') -Raw) -replace "`r", ''
        $yml | Should -Not -BeNullOrEmpty
        $quoted = ($ReleaseZipExtras | ForEach-Object { "'$_'" }) -join ', '
        $yml.Contains("foreach (`$f in $quoted)") | Should -BeTrue
    }
}

Describe 'the safety rails' {
    It 'refuses to run off a CI runner' {
        $text = (Get-Content -LiteralPath (Join-Path $PSScriptRoot 'install-smoke.ps1') -Raw) -replace "`r", ''
        $text | Should -Not -BeNullOrEmpty
        $text.Contains('if (-not $env:CI -and -not $AllowLocal)') | Should -BeTrue
    }
}
