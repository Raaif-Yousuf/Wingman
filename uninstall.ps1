<#
.SYNOPSIS
  Remove everything install.ps1 put on the machine.

.DESCRIPTION
  Stops the app, unregisters the package, clears the autostart entry, deletes
  %LOCALAPPDATA%\Programs\copilot-ask, and removes the signing certificate from
  both certificate stores.

  It deliberately does NOT delete %APPDATA%\copilot-ask\config.toml. That file
  holds your API keys and your prompt; deciding to destroy it is yours, not an
  uninstaller's. The command to do it is printed at the end.

  One UAC prompt, to remove the machine-wide certificate. -KeepCertificate
  skips it, which is what you want if you are about to reinstall.

.PARAMETER KeepCertificate
  Leave the signing certificate in place, so a later install.ps1 needs no
  elevation.
#>
[CmdletBinding()]
param(
    [switch]$KeepCertificate
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$InstallDir  = Join-Path $env:LOCALAPPDATA 'Programs\copilot-ask'
$CertSubject = 'CN=Raaif Yousuf, O=copilot-ask'
$PackageName = 'RaaifYousuf.CopilotAsk'
$RunKey      = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$RunValue    = 'copilot-ask'
$BrandedKey  = 'HKCU:\Software\Microsoft\Windows\Shell\BrandedKey'

function Step($m) { Write-Host "==> $m" -ForegroundColor Cyan }
function Note($m) { Write-Host "    $m" -ForegroundColor DarkGray }

# --- stop it -----------------------------------------------------------------
$running = Get-Process copilot-ask -ErrorAction SilentlyContinue
if ($running) {
    Step "Stopping copilot-ask"
    # Killed rather than asked to Quit, so the tray icon can linger as a ghost
    # until the notification area is next hovered. Cosmetic, and unavoidable
    # without a running message loop to remove it.
    $running | Stop-Process -Force
    $running | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
}

# --- Copilot key -------------------------------------------------------------
# Hand the key back before the package goes, or Windows is left pointing at an
# app that no longer exists and the key does nothing at all.
if (Test-Path $BrandedKey) {
    $aumid = (Get-ItemProperty $BrandedKey -Name AppAumid -ErrorAction SilentlyContinue).AppAumid
    if ($aumid -and $aumid -like "$PackageName*") {
        Step "Returning the Copilot key to Search"
        Set-ItemProperty $BrandedKey -Name BrandedKeyChoiceType -Value 'Search'
    }
}

# --- package -----------------------------------------------------------------
$pkg = Get-AppxPackage -Name $PackageName
if ($pkg) {
    Step "Unregistering the package"
    Remove-AppxPackage -Package $pkg.PackageFullName
} else {
    Note "package not installed"
}

# --- autostart ---------------------------------------------------------------
if (Get-ItemProperty -Path $RunKey -Name $RunValue -ErrorAction SilentlyContinue) {
    Step "Removing the autostart entry"
    Remove-ItemProperty -Path $RunKey -Name $RunValue
}

# --- files -------------------------------------------------------------------
if (Test-Path $InstallDir) {
    Step "Deleting $InstallDir"
    Remove-Item $InstallDir -Recurse -Force
}

# --- certificate -------------------------------------------------------------
if (-not $KeepCertificate) {
    $mine = Get-ChildItem Cert:\CurrentUser\My | Where-Object { $_.Subject -eq $CertSubject }
    foreach ($c in $mine) {
        Step "Removing the certificate from your personal store"
        Remove-Item $c.PSPath -Force
    }

    $trusted = Get-ChildItem Cert:\LocalMachine\TrustedPeople -ErrorAction SilentlyContinue |
               Where-Object { $_.Subject -eq $CertSubject }
    if ($trusted) {
        Step "Removing the machine-wide certificate (one UAC prompt)"
        foreach ($c in $trusted) {
            $inner = "Get-ChildItem Cert:\LocalMachine\TrustedPeople | Where-Object { `$_.Thumbprint -eq '$($c.Thumbprint)' } | Remove-Item -Force"
            $p = Start-Process powershell.exe -Verb RunAs -Wait -PassThru `
                 -ArgumentList '-NoProfile', '-NonInteractive', '-Command', $inner
            if ($p.ExitCode -ne 0) { Write-Warning "Could not remove certificate $($c.Thumbprint); remove it by hand in certlm.msc under Trusted People." }
        }
    }
} else {
    Note "certificate kept (-KeepCertificate)"
}

Write-Host ""
Step "Removed"
Note "Your config and API keys were left alone, at:"
Note "  $env:APPDATA\copilot-ask\config.toml"
Write-Host ""
Write-Host "  To delete those too:" -ForegroundColor Yellow
Write-Host "  Remove-Item `"`$env:APPDATA\copilot-ask`" -Recurse -Force" -ForegroundColor Yellow
Write-Host "  Rotate any API key it held -- deleting the file does not revoke it." -ForegroundColor Yellow
Write-Host ""
