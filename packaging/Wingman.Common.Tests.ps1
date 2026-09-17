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

Describe 'Get-InstallPhaseOrder' {
    It 'orders every legacy-removal step after registration is verified (issue #165)' {
        $order = Get-InstallPhaseOrder
        $order.IndexOf('VerifyRegistration') | Should -BeGreaterThan -1
        foreach ($legacyStep in 'RemoveLegacyPackage', 'RemoveLegacyRunValue', 'RemoveLegacyInstallDir') {
            $order.IndexOf($legacyStep) | Should -BeGreaterThan $order.IndexOf('VerifyRegistration')
        }
    }

    It 'stops the old process before the current one is (re)started' {
        $order = Get-InstallPhaseOrder
        $order.IndexOf('StopOldProcess') | Should -BeLessThan $order.IndexOf('RegisterPackage')
    }

    It 'never lists a legacy-removal step before RegisterPackage' {
        $order = Get-InstallPhaseOrder
        foreach ($legacyStep in 'RemoveLegacyPackage', 'RemoveLegacyRunValue', 'RemoveLegacyInstallDir') {
            $order.IndexOf($legacyStep) | Should -BeGreaterThan $order.IndexOf('RegisterPackage')
        }
    }
}

Describe 'Get-RollbackPlan' {
    It 'undoes everything phase 2 finished and restarts the old process when it was running' {
        $plan = Get-RollbackPlan -NewPackageRegistered $true -NewRunValueWritten $true `
            -OldProcessWasRunning $true -OldInstallStillPresent $true
        $plan.UnregisterNewPackage | Should -BeTrue
        $plan.RemoveNewRunValue | Should -BeTrue
        $plan.RestartOldProcess | Should -BeTrue
    }

    It 'does not try to unregister a package that never got registered' {
        $plan = Get-RollbackPlan -NewPackageRegistered $false -NewRunValueWritten $false `
            -OldProcessWasRunning $true -OldInstallStillPresent $true
        $plan.UnregisterNewPackage | Should -BeFalse
        $plan.RemoveNewRunValue | Should -BeFalse
    }

    It 'does not restart the old process when it was never running' {
        $plan = Get-RollbackPlan -NewPackageRegistered $true -NewRunValueWritten $false `
            -OldProcessWasRunning $false -OldInstallStillPresent $true
        $plan.RestartOldProcess | Should -BeFalse
    }

    It 'does not try to restart the old process when its install dir/exe is already gone' {
        $plan = Get-RollbackPlan -NewPackageRegistered $true -NewRunValueWritten $true `
            -OldProcessWasRunning $true -OldInstallStillPresent $false
        $plan.RestartOldProcess | Should -BeFalse
    }

    It 'plans nothing when phase 2 had not done anything yet' {
        $plan = Get-RollbackPlan -NewPackageRegistered $false -NewRunValueWritten $false `
            -OldProcessWasRunning $false -OldInstallStillPresent $true
        $plan.UnregisterNewPackage | Should -BeFalse
        $plan.RemoveNewRunValue | Should -BeFalse
        $plan.RestartOldProcess | Should -BeFalse
    }
}

Describe 'Invoke-PackageRegistrationPhase' {
    BeforeAll {
        $script:id = Get-WingmanIdentity
        $script:installDir = 'TestDrive:\Install\Wingman'
        $script:legacyDir  = 'TestDrive:\Install\copilot-ask'
        $script:msixPath   = 'TestDrive:\stage\wingman.msix'
        $script:builtExe   = 'TestDrive:\build\wingman.exe'
        $script:runKey     = 'TestDrive:\Run'
    }

    BeforeEach {
        Mock -ModuleName Wingman.Common Get-Process { $null }
        Mock -ModuleName Wingman.Common Stop-Process { }
        Mock -ModuleName Wingman.Common Wait-Process { }
        Mock -ModuleName Wingman.Common New-Item { }
        Mock -ModuleName Wingman.Common Copy-Item { }
        Mock -ModuleName Wingman.Common Test-Path { $false }
        Mock -ModuleName Wingman.Common Add-AppxPackage { }
        Mock -ModuleName Wingman.Common Get-AppxPackage { $null }
        Mock -ModuleName Wingman.Common Set-ItemProperty { }
        Mock -ModuleName Wingman.Common Remove-ItemProperty { }
        Mock -ModuleName Wingman.Common Remove-AppxPackage { }
        Mock -ModuleName Wingman.Common Start-Process { }
    }

    It 'reports success and never touches the legacy package when registration verifies' {
        Mock -ModuleName Wingman.Common Get-AppxPackage { [pscustomobject]@{ PackageFullName = 'RaaifYousuf.Wingman_1.0.0.0_x64__abc' } }

        $result = Invoke-PackageRegistrationPhase -Identity $id -BuiltExePath $builtExe `
            -InstallDir $installDir -LegacyDir $legacyDir -MsixPath $msixPath -RunKeyPath $runKey

        $result.Success | Should -BeTrue
        Should -Invoke -ModuleName Wingman.Common Remove-AppxPackage -Times 0
        Should -Invoke -ModuleName Wingman.Common Start-Process -Times 0
    }

    It 'simulates a registration-verification failure and asserts the legacy install is never removed' {
        # Add-AppxPackage "succeeds" but Get-AppxPackage then finds nothing --
        # the exact shape issue #165 is about: a step downstream of
        # Remove-LegacyInstall throws after the legacy install is gone.
        Mock -ModuleName Wingman.Common Get-Process {
            if ($Name -eq $id.Legacy.ProcessName) { [pscustomobject]@{ Id = 4242 } } else { $null }
        }
        Mock -ModuleName Wingman.Common Test-Path { $true } # old exe still present
        Mock -ModuleName Wingman.Common Get-AppxPackage { $null }

        { Invoke-PackageRegistrationPhase -Identity $id -BuiltExePath $builtExe `
            -InstallDir $installDir -LegacyDir $legacyDir -MsixPath $msixPath -RunKeyPath $runKey } |
            Should -Throw

        # The legacy package/Run value/install dir are never named by this
        # function at all -- asserting that is the strongest form of "never
        # removed" available without wiring in Remove-LegacyInstall itself.
        Should -Invoke -ModuleName Wingman.Common Remove-AppxPackage -Times 0 -ParameterFilter {
            $Package -like "*CopilotAsk*"
        }
        # Rollback undid the new package registration attempt and restarted
        # the still-running-before-we-stopped-it old process.
        Should -Invoke -ModuleName Wingman.Common Start-Process -Times 1
        Should -Invoke -ModuleName Wingman.Common Stop-Process -Times 1
    }

    It 'does not restart the old process on failure if it was never running' {
        Mock -ModuleName Wingman.Common Get-Process { $null }
        Mock -ModuleName Wingman.Common Add-AppxPackage { throw 'deployment refused: 0x80073CF2' }

        { Invoke-PackageRegistrationPhase -Identity $id -BuiltExePath $builtExe `
            -InstallDir $installDir -LegacyDir $legacyDir -MsixPath $msixPath -RunKeyPath $runKey } |
            Should -Throw

        Should -Invoke -ModuleName Wingman.Common Start-Process -Times 0
    }

    It 'stops the old process before attempting to register the new package' {
        $script:order = [System.Collections.Generic.List[string]]::new()
        Mock -ModuleName Wingman.Common Get-Process {
            if ($Name -eq $id.Legacy.ProcessName) { [pscustomobject]@{ Id = 99 } } else { $null }
        }
        Mock -ModuleName Wingman.Common Stop-Process { $script:order.Add('StopOldProcess') }
        Mock -ModuleName Wingman.Common Add-AppxPackage { $script:order.Add('RegisterPackage') }
        Mock -ModuleName Wingman.Common Get-AppxPackage { [pscustomobject]@{ PackageFullName = 'x' } }

        Invoke-PackageRegistrationPhase -Identity $id -BuiltExePath $builtExe `
            -InstallDir $installDir -LegacyDir $legacyDir -MsixPath $msixPath -RunKeyPath $runKey | Out-Null

        $order | Should -Be @('StopOldProcess', 'RegisterPackage')
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

Describe 'Find-SdkTool (issue #164: shared with packaging\Build-Msix.ps1)' {
    # -SdkRoots is injectable specifically so this never has to depend on (or
    # search) the real Windows Kits install on the test machine. The helper
    # is defined in BeforeAll (not inline in the Describe body) because
    # Pester 6 runs each It block in its own scope, which cannot see a
    # function only defined in the Describe block's own scope.
    BeforeAll {
        function New-FakeSdkVersion {
            param([string]$Root, [string]$Version, [string[]]$Arches, [bool]$Complete = $true)
            foreach ($arch in $Arches) {
                $bin = Join-Path $Root "$Version\$arch"
                New-Item -ItemType Directory -Force -Path $bin | Out-Null
                Set-Content -Path (Join-Path $bin 'makeappx.exe') -Value 'stub' -Encoding utf8
                if ($Complete) {
                    Set-Content -Path (Join-Path $bin 'signtool.exe') -Value 'stub' -Encoding utf8
                }
            }
        }
    }

    It 'picks the newest version that has both tools' {
        $root = Join-Path $TestDrive 'sdk-newest'
        New-FakeSdkVersion -Root $root -Version '10.0.19041.0' -Arches @('x64')
        New-FakeSdkVersion -Root $root -Version '10.0.22621.0' -Arches @('x64')

        $tools = Find-SdkTool -SdkRoots @($root)
        $tools.MakeAppx | Should -Be (Join-Path $root '10.0.22621.0\x64\makeappx.exe')
        $tools.SignTool | Should -Be (Join-Path $root '10.0.22621.0\x64\signtool.exe')
    }

    It 'skips a newer version whose tools are incomplete in favor of an older complete one' {
        $root = Join-Path $TestDrive 'sdk-incomplete'
        New-FakeSdkVersion -Root $root -Version '10.0.19041.0' -Arches @('x64') -Complete $true
        New-FakeSdkVersion -Root $root -Version '10.0.26100.0' -Arches @('x64') -Complete $false

        $tools = Find-SdkTool -SdkRoots @($root)
        $tools.MakeAppx | Should -Be (Join-Path $root '10.0.19041.0\x64\makeappx.exe')
    }

    It 'falls back from x64 to x86 when only x86 has both tools' {
        $root = Join-Path $TestDrive 'sdk-x86-only'
        New-FakeSdkVersion -Root $root -Version '10.0.22621.0' -Arches @('x86')

        $tools = Find-SdkTool -SdkRoots @($root)
        $tools.MakeAppx | Should -Be (Join-Path $root '10.0.22621.0\x86\makeappx.exe')
    }

    It 'checks a second root when the first has no usable version' {
        $emptyRoot = Join-Path $TestDrive 'sdk-empty'
        New-Item -ItemType Directory -Force -Path $emptyRoot | Out-Null
        $realRoot = Join-Path $TestDrive 'sdk-real'
        New-FakeSdkVersion -Root $realRoot -Version '10.0.22621.0' -Arches @('x64')

        $tools = Find-SdkTool -SdkRoots @($emptyRoot, $realRoot)
        $tools.MakeAppx | Should -Be (Join-Path $realRoot '10.0.22621.0\x64\makeappx.exe')
    }

    It 'throws a message naming what is missing when no root has usable tools' {
        $root = Join-Path $TestDrive 'sdk-none'
        New-Item -ItemType Directory -Force -Path $root | Out-Null
        { Find-SdkTool -SdkRoots @($root) } | Should -Throw '*Windows SDK not found*'
    }

    It 'throws when given no roots at all (e.g. neither ProgramFiles path exists)' {
        { Find-SdkTool -SdkRoots @() } | Should -Throw
    }
}

Describe 'Get-LogoSpecs (issue #164)' {
    It 'names exactly the three logos AppxManifest.xml.in references' {
        $specs = Get-LogoSpecs
        ($specs | Select-Object -ExpandProperty Name | Sort-Object) |
            Should -Be @('Square150x150Logo', 'Square44x44Logo', 'StoreLogo')
    }

    It 'matches the sizes install.ps1 and Build-Msix.ps1 always rendered' {
        $specs = Get-LogoSpecs
        ($specs | Where-Object Name -eq 'Square44x44Logo').Size   | Should -Be 44
        ($specs | Where-Object Name -eq 'Square150x150Logo').Size | Should -Be 150
        ($specs | Where-Object Name -eq 'StoreLogo').Size         | Should -Be 50
    }
}

Describe 'Build-Logos (issue #164)' {
    It 'renders one correctly-sized PNG per Get-LogoSpecs entry from the real icon' {
        $iconPath = Join-Path $PSScriptRoot '..\assets\icon.ico'
        $dest = Join-Path $TestDrive 'logos'
        New-Item -ItemType Directory -Force -Path $dest | Out-Null

        Build-Logos -IconPath $iconPath -Destination $dest

        foreach ($spec in Get-LogoSpecs) {
            $pngPath = Join-Path $dest "$($spec.Name).png"
            Test-Path $pngPath | Should -BeTrue
            Add-Type -AssemblyName System.Drawing
            $img = [System.Drawing.Image]::FromFile($pngPath)
            try {
                $img.Width  | Should -Be $spec.Size
                $img.Height | Should -Be $spec.Size
            } finally {
                $img.Dispose()
            }
        }
    }
}
