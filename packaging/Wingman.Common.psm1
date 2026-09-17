<#
.SYNOPSIS
  Pure, machine-untouching logic shared by install.ps1 and uninstall.ps1.

.DESCRIPTION
  Everything here is a function of its parameters only -- no registry, no
  filesystem writes, no process control, no certificate store. That is what
  makes it possible to unit-test the naming and cleanup-planning logic with
  Pester without ever touching this machine's real install, and it is why
  install.ps1 and uninstall.ps1 both import this module rather than each
  keeping their own copy (issue #1's rename already drifted once between the
  two scripts before this module existed).

  Two identities exist side by side while the rename is in progress: the
  pre-rename `copilot-ask` install (still on disk on any machine that ran the
  old install.ps1) and the current `Wingman` install. Every function that
  names "what to clean up" returns BOTH unless told otherwise, so
  uninstall.ps1 clears a machine regardless of which install.ps1 last ran on
  it.
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

Export-ModuleMember -Function @(
    'Get-WingmanIdentity',
    'Get-CargoVersionString',
    'ConvertTo-MsixVersion',
    'Get-InstallDirPath',
    'Get-ConfigDirPath',
    'Get-CleanupPlan',
    'Test-AumidBelongsToWingman'
)
