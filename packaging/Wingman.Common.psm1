<#
.SYNOPSIS
  Logic shared by install.ps1, uninstall.ps1 and packaging\Build-Msix.ps1.

.DESCRIPTION
  Most of this module is pure -- a function of its parameters only: no
  registry, no process control, no certificate store. That is what makes it
  possible to unit-test the naming and cleanup-planning logic with Pester
  without ever touching this machine's real install, and it is why
  install.ps1 and uninstall.ps1 both import this module rather than each
  keeping their own copy (issue #1's rename already drifted once between the
  two scripts before this module existed).

  Two identities exist side by side while the rename is in progress: the
  pre-rename `copilot-ask` install (still on disk on any machine that ran the
  old install.ps1) and the current `Wingman` install. Every function that
  names "what to clean up" returns BOTH unless told otherwise, so
  uninstall.ps1 clears a machine regardless of which install.ps1 last ran on
  it.

  Two exceptions to "pure", added by issue #164 because install.ps1 and
  packaging\Build-Msix.ps1 had copy-pasted them verbatim: `Find-SdkTool`
  (read-only filesystem probing for makeappx.exe/signtool.exe, injectable via
  -SdkRoots so Pester can point it at a fake $TestDrive layout) and
  `Build-Logos` (draws PNGs with System.Drawing into a caller-supplied
  -Destination). Neither touches a registry key, a process, a certificate
  store or any path the caller did not hand it, so they are still safe for
  both install.ps1 (a real machine) and Build-Msix.ps1 (a CI runner) to share
  -- the "no filesystem writes" claim above is otherwise still true.
#>

Set-StrictMode -Version Latest

# --- identity table (issue #1's plan, table in the expansion plan's Section 2) ---
# The single place old vs. new names are written down, so install.ps1 and
# uninstall.ps1 can never drift against each other again.
function Get-WingmanIdentity {
    [CmdletBinding()]
    param()
    [pscustomobject]@{
        Current = [pscustomobject]@{
            ExeName        = 'wingman.exe'
            ProcessName    = 'wingman'
            InstallDirName = 'Wingman'
            PackageName    = 'RaaifYousuf.Wingman'
            DisplayName    = 'Wingman'
            RunValue       = 'Wingman'
            CertSubject    = 'CN=Raaif Yousuf, O=Wingman'
            ExtensionId    = 'Wingman'
        }
        Legacy = [pscustomobject]@{
            ExeName        = 'copilot-ask.exe'
            ProcessName    = 'copilot-ask'
            InstallDirName = 'copilot-ask'
            PackageName    = 'RaaifYousuf.CopilotAsk'
            DisplayName    = 'copilot-ask'
            RunValue       = 'copilot-ask'
            CertSubject    = 'CN=Raaif Yousuf, O=copilot-ask'
            ExtensionId    = 'CopilotAsk'
        }
    }
}

# --- version -----------------------------------------------------------------
# Split from install.ps1's old Get-CargoVersion so the string transform (the
# part with a bug surface: regex, missing file, malformed version) can be
# unit-tested without reading a real Cargo.toml.
function Get-CargoVersionString {
    [CmdletBinding()]
    param([Parameter(Mandatory)][string]$CargoTomlPath)
    if (-not (Test-Path $CargoTomlPath)) { throw "No Cargo.toml at $CargoTomlPath" }
    $line = Select-String -Path $CargoTomlPath -Pattern '^\s*version\s*=\s*"([^"]+)"' |
            Select-Object -First 1
    if (-not $line) { throw "No version found in $CargoTomlPath" }
    $line.Matches[0].Groups[1].Value
}

# MSIX wants four parts and reserves the last for the store; Cargo gives three.
# Pure string -> string, so this is the part Pester actually exercises.
function ConvertTo-MsixVersion {
    [CmdletBinding()]
    param([Parameter(Mandatory)][string]$CargoVersion)
    if ($CargoVersion -notmatch '^\d+\.\d+\.\d+$') {
        throw "Unexpected Cargo version '$CargoVersion'"
    }
    "$CargoVersion.0"
}

# --- paths ---------------------------------------------------------------
# Pure path-joining, injectable so Pester never resolves against the real
# %LOCALAPPDATA%.
function Get-InstallDirPath {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string]$LocalAppData,
        [Parameter(Mandatory)][string]$InstallDirName
    )
    Join-Path $LocalAppData (Join-Path 'Programs' $InstallDirName)
}

function Get-ConfigDirPath {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string]$AppData,
        [Parameter(Mandatory)][string]$ConfigDirName
    )
    Join-Path $AppData $ConfigDirName
}

# --- cleanup planning ------------------------------------------------------
# Decides WHAT to clean up from a snapshot of what is present, handed in as
# plain booleans/strings rather than discovered here. install.ps1 and
# uninstall.ps1 gather the snapshot (Get-Process, Get-AppxPackage, Test-Path,
# registry reads); this function only decides, so the decision itself is
# testable without any of those calls.
function Get-CleanupPlan {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][bool]$ProcessRunning,
        [Parameter(Mandatory)][bool]$PackageInstalled,
        [Parameter(Mandatory)][bool]$RunValuePresent,
        [Parameter(Mandatory)][bool]$InstallDirPresent
    )
    [pscustomobject]@{
        StopProcess    = $ProcessRunning
        RemovePackage  = $PackageInstalled
        RemoveRunValue = $RunValuePresent
        RemoveInstallDir = $InstallDirPresent
    }
}

# Whether a `BrandedKey` AUMID (as read from the registry) belongs to either
# identity, so uninstall.ps1 can hand the Copilot key back to Search
# regardless of which install last owned it. Pure string matching.
function Test-AumidBelongsToWingman {
    [CmdletBinding()]
    param(
        [AllowNull()][AllowEmptyString()][string]$Aumid,
        [Parameter(Mandatory)]$Identity
    )
    if ([string]::IsNullOrEmpty($Aumid)) { return $false }
    ($Aumid -like "$($Identity.Current.PackageName)*") -or
    ($Aumid -like "$($Identity.Legacy.PackageName)*")
}

# --- packaging build helpers (issue #164) -------------------------------------
# Shared by install.ps1 (a real machine) and packaging\Build-Msix.ps1 (a CI
# runner headless build). Both used to keep byte-identical copies of these two
# functions; see the module header for why they live here despite touching a
# (caller-supplied) filesystem path.

# makeappx and signtool are not on PATH by default; pick the newest SDK that
# has both rather than hard-coding a version a machine may not have.
# -SdkRoots is injectable so Pester can point this at a fake $TestDrive
# layout instead of the real Windows Kits install -- the search/selection
# logic is what has a bug surface (version sorting, arch fallback, "nothing
# found"), and that is now testable without a real SDK on the test machine.
function Find-SdkTool {
    [CmdletBinding()]
    param(
        [string[]]$SdkRoots = (@(
            "${env:ProgramFiles(x86)}\Windows Kits\10\bin",
            "$env:ProgramFiles\Windows Kits\10\bin"
        ) | Where-Object { Test-Path $_ })
    )

    foreach ($root in $SdkRoots) {
        if (-not (Test-Path $root)) { continue }
        $vers = Get-ChildItem $root -Directory -ErrorAction SilentlyContinue |
                Where-Object { $_.Name -match '^10\.' } |
                Sort-Object { [version]$_.Name } -Descending
        foreach ($v in $vers) {
            foreach ($arch in 'x64', 'x86') {
                $bin = Join-Path $v.FullName $arch
                if ((Test-Path "$bin\makeappx.exe") -and (Test-Path "$bin\signtool.exe")) {
                    return [pscustomobject]@{
                        MakeAppx = "$bin\makeappx.exe"
                        SignTool = "$bin\signtool.exe"
                    }
                }
            }
        }
    }
    throw "Windows SDK not found under $($SdkRoots -join ', '). makeappx.exe and signtool.exe are needed to package the app. Install the Windows SDK, or the 'MSVC v143 build tools' workload in the Visual Studio Installer -- the windows-latest GitHub Actions runner ships one already."
}

# The three MSIX logo files AppxManifest.xml.in references (Square150x150Logo
# and Square44x44Logo in <uap:VisualElements>, StoreLogo in <Properties>).
# Pure and separated from Build-Logos below purely so the spec/sizes list
# itself -- the part that actually drifts when the manifest changes -- is
# unit-testable without System.Drawing or a real .ico file.
function Get-LogoSpecs {
    [CmdletBinding()]
    param()
    @(
        [pscustomobject]@{ Name = 'Square44x44Logo';   Size = 44  }
        [pscustomobject]@{ Name = 'Square150x150Logo'; Size = 150 }
        [pscustomobject]@{ Name = 'StoreLogo';         Size = 50  }
    )
}

# Renders assets\icon.ico into the sizes Get-LogoSpecs names. Derived from the
# .ico rather than checked in, so the tray icon and the Start menu tile can
# never drift apart -- change the .ico and both follow.
# NOTE (issue #10): this still draws from the pre-rename icon.ico; a new icon
# set is issue #10's own scope.
function Build-Logos {
    [CmdletBinding()]
    [Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSUseSingularNouns', '',
        Justification = 'It genuinely builds a set of logos (three files, one per Get-LogoSpecs entry); a singular Build-Logo would misdescribe what one call does.')]
    param(
        [Parameter(Mandatory)][string]$IconPath,
        [Parameter(Mandatory)][string]$Destination
    )
    Add-Type -AssemblyName System.Drawing
    $ico = New-Object System.Drawing.Icon($IconPath, 256, 256)
    $src = $ico.ToBitmap()
    try {
        foreach ($spec in Get-LogoSpecs) {
            $bmp = New-Object System.Drawing.Bitmap($spec.Size, $spec.Size)
            $g = [System.Drawing.Graphics]::FromImage($bmp)
            $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
            $g.PixelOffsetMode   = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
            $g.Clear([System.Drawing.Color]::Transparent)
            $g.DrawImage($src, 0, 0, $spec.Size, $spec.Size)
            $g.Dispose()
            $bmp.Save((Join-Path $Destination "$($spec.Name).png"), [System.Drawing.Imaging.ImageFormat]::Png)
            $bmp.Dispose()
        }
    } finally {
        $src.Dispose()
        $ico.Dispose()
    }
}

# --- phase ordering (issue #165) --------------------------------------------
# The fixed sequence install.ps1 follows. Purely declarative -- nothing here
# runs a step -- so a test can assert the ordering itself never regresses back
# to issue #165's shape: the legacy install removed before the new one is
# proven to work. install.ps1 does not read this list to drive its own
# control flow (a linear script does not need to); it exists so the invariant
# has one written-down, testable place to live.
function Get-InstallPhaseOrder {
    [CmdletBinding()]
    param()
    @(
        'Build',
        'StageAndSignExecutable',
        'PackAndSignMsix',
        'StopOldProcess',
        'StopCurrentProcess',
        'DeployExecutable',
        'RegisterPackage',
        'VerifyRegistration',
        'WriteRunValue',
        'RemoveLegacyPackage',
        'RemoveLegacyRunValue',
        'RemoveLegacyInstallDir'
    )
}

# --- rollback decision (issue #165) ------------------------------------------
# Phase 2 (registering the new package) is the only phase with both system
# side effects and a way to fail partway through. This decides what to undo
# from a snapshot of what phase 2 had actually finished when it failed, so the
# decision -- not the Remove-AppxPackage / Start-Process calls that carry it
# out -- is what Pester exercises directly.
function Get-RollbackPlan {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][bool]$NewPackageRegistered,
        [Parameter(Mandatory)][bool]$NewRunValueWritten,
        [Parameter(Mandatory)][bool]$OldProcessWasRunning,
        [Parameter(Mandatory)][bool]$OldInstallStillPresent
    )
    [pscustomobject]@{
        UnregisterNewPackage = $NewPackageRegistered
        RemoveNewRunValue    = $NewRunValueWritten
        RestartOldProcess    = $OldProcessWasRunning -and $OldInstallStillPresent
    }
}

# --- phase 2: register the new package (issue #165) -------------------------
# Everything here has a system side effect, which is exactly why it is its own
# function: Pester can mock every cmdlet it calls (Mock -ModuleName
# Wingman.Common) and drive a simulated failure without touching this
# machine's real processes, packages, registry or filesystem.
#
# On success the caller (install.ps1) may proceed to remove the legacy
# install. On failure this rolls back what it did (via Get-RollbackPlan) and
# throws, so the caller never reaches legacy removal at all -- the old install
# stays in place (and the old process, if it was running, is restarted).
function Invoke-PackageRegistrationPhase {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]$Identity,
        [Parameter(Mandatory)][string]$BuiltExePath,
        [Parameter(Mandatory)][string]$InstallDir,
        [Parameter(Mandatory)][string]$LegacyDir,
        [Parameter(Mandatory)][string]$MsixPath,
        [Parameter(Mandatory)][string]$RunKeyPath,
        [switch]$NoAutostart
    )

    $state = [pscustomobject]@{
        NewPackageRegistered = $false
        NewRunValueWritten   = $false
        OldProcessWasRunning = $false
    }

    try {
        # Old process first, then a running copy of the current identity (an
        # in-place Wingman-to-Wingman upgrade): with both possibly holding a
        # low-level keyboard hook, the old one must be gone before the new one
        # is ever (re)started, and its file must be unlocked before it is
        # overwritten below.
        $oldProcess = Get-Process -Name $Identity.Legacy.ProcessName -ErrorAction SilentlyContinue
        if ($oldProcess) {
            $state.OldProcessWasRunning = $true
            $oldProcess | Stop-Process -Force
            $oldProcess | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
        }

        $currentProcess = Get-Process -Name $Identity.Current.ProcessName -ErrorAction SilentlyContinue
        if ($currentProcess) {
            $currentProcess | Stop-Process -Force
            $currentProcess | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
        }

        New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
        Copy-Item $BuiltExePath (Join-Path $InstallDir $Identity.Current.ExeName) -Force

        Add-AppxPackage -Path $MsixPath -ExternalLocation $InstallDir -ForceUpdateFromAnyVersion
        $state.NewPackageRegistered = $true

        $pkg = Get-AppxPackage -Name $Identity.Current.PackageName
        if (-not $pkg) {
            throw "Add-AppxPackage completed but Get-AppxPackage -Name $($Identity.Current.PackageName) found nothing; registration did not verify."
        }

        if (-not $NoAutostart) {
            if (-not (Test-Path $RunKeyPath)) { New-Item -Path $RunKeyPath | Out-Null }
            Set-ItemProperty -Path $RunKeyPath -Name $Identity.Current.RunValue `
                -Value "`"$InstallDir\$($Identity.Current.ExeName)`""
            $state.NewRunValueWritten = $true
        }

        [pscustomobject]@{ Success = $true; Package = $pkg }
    }
    catch {
        $originalError = $_
        $oldExePresent = Test-Path (Join-Path $LegacyDir $Identity.Legacy.ExeName)
        $plan = Get-RollbackPlan -NewPackageRegistered $state.NewPackageRegistered `
            -NewRunValueWritten $state.NewRunValueWritten `
            -OldProcessWasRunning $state.OldProcessWasRunning `
            -OldInstallStillPresent $oldExePresent

        if ($plan.RemoveNewRunValue) {
            Remove-ItemProperty -Path $RunKeyPath -Name $Identity.Current.RunValue -ErrorAction SilentlyContinue
        }
        if ($plan.UnregisterNewPackage) {
            $newPkg = Get-AppxPackage -Name $Identity.Current.PackageName -ErrorAction SilentlyContinue
            if ($newPkg) { Remove-AppxPackage -Package $newPkg.PackageFullName -ErrorAction SilentlyContinue }
        }
        if ($plan.RestartOldProcess) {
            Start-Process -FilePath (Join-Path $LegacyDir $Identity.Legacy.ExeName) -ErrorAction SilentlyContinue
        }

        throw "Registering the new package failed: $($originalError.Exception.Message). The pre-rename copilot-ask install was left in place$(if ($plan.RestartOldProcess) { ' and restarted' }); re-run install.ps1 to try again."
    }
}

Export-ModuleMember -Function @(
    'Get-WingmanIdentity',
    'Get-CargoVersionString',
    'ConvertTo-MsixVersion',
    'Get-InstallDirPath',
    'Get-ConfigDirPath',
    'Get-CleanupPlan',
    'Test-AumidBelongsToWingman',
    'Get-InstallPhaseOrder',
    'Get-RollbackPlan',
    'Invoke-PackageRegistrationPhase',
    'Find-SdkTool',
    'Get-LogoSpecs',
    'Build-Logos'
)
