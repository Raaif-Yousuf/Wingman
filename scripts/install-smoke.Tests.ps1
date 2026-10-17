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

    It 'flags SYSTEM and Administrators beside the owner (#966)' {
        $rules = @(
            [pscustomobject]@{ Sid = 'S-1-5-18'; Type = 'Allow' },
            [pscustomobject]@{ Sid = 'S-1-5-32-544'; Type = 'Allow' },
            [pscustomobject]@{ Sid = $me; Type = 'Allow' }
        )
        $text = (Get-OwnerOnlyAclProblem -IsProtected $true -Rules $rules -OwnerSid $me) -join ';'
        $text | Should -Match 'S-1-5-18'
        $text | Should -Match 'S-1-5-32-544'
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

Describe 'the certificate step (#965)' {
    It 'lets install.ps1 make and trust the certificate, so the elevated path is exercised' {
        $smoke = (Get-Content -LiteralPath (Join-Path $PSScriptRoot 'install-smoke.ps1') -Raw) -replace "`r", ''
        $smoke | Should -Not -BeNullOrEmpty
        $smoke.Contains('New-SelfSignedCertificate') | Should -BeFalse
        $smoke.Contains('Import-Certificate') | Should -BeFalse
    }
}

Describe 'the documented install commands (#1021)' {
    BeforeAll {
        $script:readme = (Get-Content -LiteralPath (Join-Path $PSScriptRoot '..\README.md') -Raw) -replace "`r", ''
    }
    It 'runs install.ps1 so a default Restricted policy and a downloaded zip do not stop it' {
        $script:readme.Contains('powershell -NoProfile -ExecutionPolicy Bypass -File .\install.ps1') | Should -BeTrue
    }
    It 'runs uninstall.ps1 the same way' {
        $script:readme.Contains('powershell -NoProfile -ExecutionPolicy Bypass -File .\uninstall.ps1') | Should -BeTrue
    }
    It 'never tells the user to run either script bare' {
        $script:readme | Should -Not -Match '(?m)^\.\\(un)?install\.ps1'
    }
}

Describe 'the smoke runs the documented command (#1050)' {
    BeforeAll {
        $script:smoke = (Get-Content -LiteralPath (Join-Path $PSScriptRoot 'install-smoke.ps1') -Raw) -replace "`r", ''
    }
    It 'builds the README argument list: no profile, Bypass, -File, then the script and its switches' {
        Get-DocumentedScriptCommand -Script 'C:\x\install.ps1' -ScriptArgs @('-SkipBuild') |
            Should -Be @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', 'C:\x\install.ps1', '-SkipBuild')
        Get-DocumentedScriptCommand -Script 'C:\x\uninstall.ps1' |
            Should -Be @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', 'C:\x\uninstall.ps1')
    }
    It 'starts install.ps1 and uninstall.ps1 through it, in a child powershell.exe, never in process' {
        $script:smoke.Contains("Invoke-DocumentedScript -Script (Join-Path `$repo 'install.ps1')") | Should -BeTrue
        $script:smoke.Contains("Invoke-DocumentedScript -Script (Join-Path `$repo 'uninstall.ps1')") | Should -BeTrue
        $script:smoke | Should -Not -Match "(?m)^\s*& \(Join-Path \`$repo '(un)?install\.ps1'\)"
    }
    It 'makes the child start from a Restricted policy, so only the flag lets the script run' {
        $script:smoke.Contains("PSExecutionPolicyPreference = 'Restricted'") | Should -BeTrue
    }
}

Describe 'the safety rails' {
    It 'refuses to run off a CI runner' {
        $text = (Get-Content -LiteralPath (Join-Path $PSScriptRoot 'install-smoke.ps1') -Raw) -replace "`r", ''
        $text | Should -Not -BeNullOrEmpty
        $text.Contains('if (-not $env:CI -and -not $AllowLocal)') | Should -BeTrue
    }
}

Describe 'the stub run (#128)' {
    It 'posts the same message number the keyboard hook posts' {
        $src = (Get-Content -LiteralPath (Join-Path $PSScriptRoot '..\src\hotkey.rs') -Raw) -replace "`r", ''
        $src | Should -Not -BeNullOrEmpty
        $src.Contains('pub const WM_APP_HOTKEY: u32 = WM_APP + 2;') | Should -BeTrue
        $WM_APP_HOTKEY | Should -Be (0x8000 + 2)
    }

    It 'writes a config that keeps the run local, on the stub port, and runs Check my work at once' {
        $cfg = Get-StubSmokeConfig -Port 4242
        $cfg | Should -Match 'mode = "local"'
        $cfg | Should -Match 'base_url = "http://127\.0\.0\.1:4242"'
        $cfg | Should -Match 'primary_action = "check-my-work"'
        $cfg | Should -Match 'double_press_ms = 0'
        $cfg | Should -Not -Match 'api_key'
    }

    It 'names the action id the app uses for Check my work' {
        $src = (Get-Content -LiteralPath (Join-Path $PSScriptRoot '..\src\actions\mod.rs') -Raw) -replace "`r", ''
        $src | Should -Not -BeNullOrEmpty
        $src.Contains('pub const DEFAULT_ACTION_ID: &str = "check-my-work";') | Should -BeTrue
    }

    It 'answers over real HTTP on loopback, and records what it was asked' {
        $stub = Start-StubProvider
        try {
            $tags = Invoke-RestMethod -Uri "http://127.0.0.1:$($stub.Port)/api/tags"
            $tags.models[0].name | Should -Be $StubModel
            $big = '{"images":["' + ('A' * 200000) + '"]}'
            $chat = Invoke-RestMethod -Uri "http://127.0.0.1:$($stub.Port)/api/chat" -Method Post -Body $big -ContentType 'application/json'
            $chat.done | Should -BeTrue
            ($chat.message.content | ConvertFrom-Json).headline | Should -Be $StubHeadline
            $req = @($stub.Requests.ToArray() | Where-Object { $_.Path -eq '/api/chat' })
            $req.Count | Should -Be 1
            $req[0].Bytes | Should -BeGreaterThan 200000
            $req[0].Body.Contains('"images"') | Should -BeTrue
            { Invoke-RestMethod -Uri "http://127.0.0.1:$($stub.Port)/nope" } | Should -Throw
        }
        finally { Stop-StubProvider -Stub $stub }
    }

    It 'listens on the loopback address only' {
        (Get-Content -LiteralPath (Join-Path $PSScriptRoot 'stub-provider.ps1') -Raw) | Should -Match '\[System\.Net\.IPAddress\]::Loopback'
    }
}
