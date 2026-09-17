<#
.SYNOPSIS
  Remove everything install.ps1 (current or pre-rename) put on the machine.

.DESCRIPTION
  Stops the app, unregisters the package, clears the autostart entry, deletes
  the install directory, and removes the signing certificate from both
  certificate stores. Cleans up BOTH the current Wingman install and any
  pre-rename copilot-ask leftovers (old install.ps1 versions, or a
  Remove-LegacyInstall that never ran), so this script is safe to run on any
  machine regardless of which install.ps1 last touched it.

  It deliberately does NOT delete %APPDATA%\Wingman\config.toml or the
  pre-rename %APPDATA%\copilot-ask\config.toml. Those files hold your API
  keys and your prompt; deciding to destroy them is yours, not an
  uninstaller's. The command to do it is printed at the end.

  One UAC prompt, to remove the machine-wide certificate(s). -KeepCertificate
  skips it, which is what you want if you are about to reinstall.

.PARAMETER KeepCertificate
  Leave the signing certificate(s) in place, so a later install.ps1 needs no
  elevation.
#>
[CmdletBinding()]
param(
    [switch]$KeepCertificate
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

Import-Module (Join-Path $PSScriptRoot 'packaging\Wingman.Common.psm1') -Force

$Identity   = Get-WingmanIdentity
$InstallDir = Get-InstallDirPath -LocalAppData $env:LOCALAPPDATA -InstallDirName $Identity.Current.InstallDirName
$LegacyDir  = Get-InstallDirPath -LocalAppData $env:LOCALAPPDATA -InstallDirName $Identity.Legacy.InstallDirName
$RunKey     = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$BrandedKey = 'HKCU:\Software\Microsoft\Windows\Shell\BrandedKey'

function Step($m) { Write-Host "==> $m" -ForegroundColor Cyan }
function Note($m) { Write-Host "    $m" -ForegroundColor DarkGray }

# --- stop it -------------------------------------------------------------
# Killed rather than asked to Quit, so the tray icon can linger as a ghost
# until the notification area is next hovered. Cosmetic, and unavoidable
# without a running message loop to remove it.
foreach ($identityLevel in @($Identity.Current, $Identity.Legacy)) {
    $running = Get-Process -Name $identityLevel.ProcessName -ErrorAction SilentlyContinue
    if ($running) {
        Step "Stopping $($identityLevel.ProcessName)"
        $running | Stop-Process -Force
        $running | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
    }
}

# --- Copilot key -----------------------------------------------------------
# Hand the key back before the package(s) go, or Windows is left pointing at
# an app that no longer exists and the key does nothing at all.
if (Test-Path $BrandedKey) {
    $aumid = (Get-ItemProperty $BrandedKey -Name AppAumid -ErrorAction SilentlyContinue).AppAumid
    if (Test-AumidBelongsToWingman -Aumid $aumid -Identity $Identity) {
        Step "Returning the Copilot key to Search"
        Set-ItemProperty $BrandedKey -Name BrandedKeyChoiceType -Value 'Search'
    }
}

# --- package(s) --------------------------------------------------------------
foreach ($identityLevel in @($Identity.Current, $Identity.Legacy)) {
    $pkg = Get-AppxPackage -Name $identityLevel.PackageName -ErrorAction SilentlyContinue
    if ($pkg) {
        Step "Unregistering $($identityLevel.PackageName)"
        Remove-AppxPackage -Package $pkg.PackageFullName
    } else {
        Note "$($identityLevel.PackageName) not installed"
    }
}

# --- autostart ---------------------------------------------------------------
foreach ($identityLevel in @($Identity.Current, $Identity.Legacy)) {
    if (Get-ItemProperty -Path $RunKey -Name $identityLevel.RunValue -ErrorAction SilentlyContinue) {
        Step "Removing the '$($identityLevel.RunValue)' autostart entry"
        Remove-ItemProperty -Path $RunKey -Name $identityLevel.RunValue
    }
}

# --- files -------------------------------------------------------------------
# Both paths are fixed, computed directories (never user input, never a
# wildcard), so this cannot walk outside either install directory.
foreach ($dir in @($InstallDir, $LegacyDir)) {
    if (Test-Path $dir) {
        Step "Deleting $dir"
        Remove-Item $dir -Recurse -Force
    }
}

# --- certificate(s) -----------------------------------------------------------
if (-not $KeepCertificate) {
    foreach ($identityLevel in @($Identity.Current, $Identity.Legacy)) {
        $mine = Get-ChildItem Cert:\CurrentUser\My | Where-Object { $_.Subject -eq $identityLevel.CertSubject }
        foreach ($c in $mine) {
            Step "Removing the certificate from your personal store ($($identityLevel.CertSubject))"
            Remove-Item $c.PSPath -Force
        }

        $trusted = Get-ChildItem Cert:\LocalMachine\TrustedPeople -ErrorAction SilentlyContinue |
                   Where-Object { $_.Subject -eq $identityLevel.CertSubject }
        if ($trusted) {
            Step "Removing the machine-wide certificate (one UAC prompt)"
            foreach ($c in $trusted) {
                $inner = "Get-ChildItem Cert:\LocalMachine\TrustedPeople | Where-Object { `$_.Thumbprint -eq '$($c.Thumbprint)' } | Remove-Item -Force"
                $p = Start-Process powershell.exe -Verb RunAs -Wait -PassThru `
                     -ArgumentList '-NoProfile', '-NonInteractive', '-Command', $inner
                if ($p.ExitCode -ne 0) { Write-Warning "Could not remove certificate $($c.Thumbprint); remove it by hand in certlm.msc under Trusted People." }
            }
        }
    }
} else {
    Note "certificate(s) kept (-KeepCertificate)"
}

Write-Host ""
Step "Removed"
Note "Your config and API keys were left alone, at:"
Note "  $env:APPDATA\Wingman\config.toml"
Note "  $env:APPDATA\copilot-ask\config.toml (pre-rename, if present)"
Write-Host ""
Write-Host "  To delete those too:" -ForegroundColor Yellow
Write-Host "  Remove-Item `"`$env:APPDATA\Wingman`" -Recurse -Force" -ForegroundColor Yellow
Write-Host "  Remove-Item `"`$env:APPDATA\copilot-ask`" -Recurse -Force" -ForegroundColor Yellow
Write-Host "  Rotate any API key they held -- deleting the file does not revoke it." -ForegroundColor Yellow
Write-Host ""
