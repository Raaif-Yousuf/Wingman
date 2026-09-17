<#
.SYNOPSIS
  Pester tests for Wingman.Common.psm1 -- pure logic only, nothing here reads
  or writes the real registry, filesystem outside $TestDrive, process list or
  certificate store. Run with: Invoke-Pester packaging/Wingman.Common.Tests.ps1
#>

BeforeAll {
    Import-Module (Join-Path $PSScriptRoot 'Wingman.Common.psm1') -Force
}

Describe 'Get-WingmanIdentity' {
    It 'names the current identity as Wingman, not copilot-ask' {
        $id = Get-WingmanIdentity
        $id.Current.ExeName | Should -Be 'wingman.exe'
        $id.Current.PackageName | Should -Be 'RaaifYousuf.Wingman'
        $id.Current.RunValue | Should -Be 'Wingman'
    }

    It 'keeps the legacy identity as copilot-ask so cleanup can still find it' {
        $id = Get-WingmanIdentity
        $id.Legacy.ExeName | Should -Be 'copilot-ask.exe'
        $id.Legacy.PackageName | Should -Be 'RaaifYousuf.CopilotAsk'
        $id.Legacy.RunValue | Should -Be 'copilot-ask'
    }

    It 'gives current and legacy distinct process names so both are ever stopped' {
        $id = Get-WingmanIdentity
        $id.Current.ProcessName | Should -Not -Be $id.Legacy.ProcessName
    }
}

Describe 'ConvertTo-MsixVersion' {
    It 'appends the MSIX-reserved fourth part to a three-part Cargo version' {
        ConvertTo-MsixVersion -CargoVersion '0.2.0' | Should -Be '0.2.0.0'
    }

    It 'rejects a two-part version' {
        { ConvertTo-MsixVersion -CargoVersion '0.2' } | Should -Throw
    }

    It 'rejects a pre-release suffix' {
        { ConvertTo-MsixVersion -CargoVersion '0.2.0-beta' } | Should -Throw
    }

    It 'rejects an empty string' {
        { ConvertTo-MsixVersion -CargoVersion '' } | Should -Throw
    }
}

Describe 'Get-CargoVersionString' {
    It 'reads the version out of a real Cargo.toml under $TestDrive' {
        $path = Join-Path $TestDrive 'Cargo.toml'
        Set-Content -Path $path -Value @('[package]', 'name = "wingman"', 'version = "0.3.1"') -Encoding utf8
        Get-CargoVersionString -CargoTomlPath $path | Should -Be '0.3.1'
    }

    It 'throws when the file does not exist' {
        { Get-CargoVersionString -CargoTomlPath (Join-Path $TestDrive 'missing.toml') } | Should -Throw
    }

    It 'throws when no version line is present' {
        $path = Join-Path $TestDrive 'NoVersion.toml'
        Set-Content -Path $path -Value @('[package]', 'name = "wingman"') -Encoding utf8
        { Get-CargoVersionString -CargoTomlPath $path } | Should -Throw
    }
}

Describe 'Get-InstallDirPath / Get-ConfigDirPath' {
    It 'joins LocalAppData, Programs and the install dir name' {
        Get-InstallDirPath -LocalAppData 'C:\Users\test\AppData\Local' -InstallDirName 'Wingman' |
            Should -Be 'C:\Users\test\AppData\Local\Programs\Wingman'
    }

    It 'joins AppData and the config dir name' {
        Get-ConfigDirPath -AppData 'C:\Users\test\AppData\Roaming' -ConfigDirName 'Wingman' |
            Should -Be 'C:\Users\test\AppData\Roaming\Wingman'
    }

    It 'gives current and legacy distinct install dirs so uninstall never deletes one for the other' {
        $id = Get-WingmanIdentity
        $current = Get-InstallDirPath -LocalAppData 'C:\LA' -InstallDirName $id.Current.InstallDirName
        $legacy = Get-InstallDirPath -LocalAppData 'C:\LA' -InstallDirName $id.Legacy.InstallDirName
        $current | Should -Not -Be $legacy
    }
}

Describe 'Get-CleanupPlan' {
    It 'plans every step when everything is present' {
        $plan = Get-CleanupPlan -ProcessRunning $true -PackageInstalled $true `
            -RunValuePresent $true -InstallDirPresent $true
        $plan.StopProcess | Should -BeTrue
        $plan.RemovePackage | Should -BeTrue
        $plan.RemoveRunValue | Should -BeTrue
        $plan.RemoveInstallDir | Should -BeTrue
    }

    It 'plans nothing when nothing is present, so a clean machine is a no-op' {
        $plan = Get-CleanupPlan -ProcessRunning $false -PackageInstalled $false `
            -RunValuePresent $false -InstallDirPresent $false
        $plan.StopProcess | Should -BeFalse
        $plan.RemovePackage | Should -BeFalse
        $plan.RemoveRunValue | Should -BeFalse
        $plan.RemoveInstallDir | Should -BeFalse
    }

    It 'plans steps independently -- a stray Run value with no install dir still gets removed' {
        $plan = Get-CleanupPlan -ProcessRunning $false -PackageInstalled $false `
            -RunValuePresent $true -InstallDirPresent $false
        $plan.RemoveRunValue | Should -BeTrue
        $plan.RemoveInstallDir | Should -BeFalse
    }
}

Describe 'Test-AumidBelongsToWingman' {
    BeforeAll { $script:id = Get-WingmanIdentity }

    It 'matches the current package AUMID' {
        Test-AumidBelongsToWingman -Aumid 'RaaifYousuf.Wingman_pa8sd8xv631fa!Wingman' -Identity $id |
            Should -BeTrue
    }

    It 'matches the legacy package AUMID, so uninstall can hand the key back on an old install' {
        Test-AumidBelongsToWingman -Aumid 'RaaifYousuf.CopilotAsk_pa8sd8xv631fa!CopilotAsk' -Identity $id |
            Should -BeTrue
    }

    It 'does not match an unrelated AUMID' {
        Test-AumidBelongsToWingman -Aumid 'Microsoft.Copilot_8wekyb3d8bbwe!Copilot' -Identity $id |
            Should -BeFalse
    }

    It 'does not match null or empty' {
        Test-AumidBelongsToWingman -Aumid $null -Identity $id | Should -BeFalse
        Test-AumidBelongsToWingman -Aumid '' -Identity $id | Should -BeFalse
    }
}
