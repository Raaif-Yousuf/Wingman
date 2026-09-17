<#
.SYNOPSIS
  Build, sign, install and register Wingman.

.DESCRIPTION
  Puts wingman.exe in %LOCALAPPDATA%\Programs\Wingman, installs a signed
  sparse MSIX package so Windows knows about it, and turns on start-with-Windows.

  That package is what puts Wingman in Start > All apps, in Settings > Apps >
  Installed apps, and in the app picker under Settings > Bluetooth & devices >
  Keyboard > Customize Copilot key on keyboard > Custom. Windows lists only
  packaged, signed apps there, which is the whole reason for the ceremony.

  Re-run it to upgrade in place. It never reads, writes or deletes
  %APPDATA%\Wingman\config.toml or the pre-rename
  %APPDATA%\copilot-ask\config.toml, so API keys and settings survive.

  Runs in three phases so a failure never leaves the machine with neither app
  (issue #165): (1) build, stage, sign and pack the new package -- no system
  side effects, nothing here can leave the machine worse off; (2) stop the
  old process and any running copy of the new one, register the new package,
  write the new Run value, and verify the registration actually took; only
  once that is verified (3) remove a pre-rename `copilot-ask` install if one
  is found (old package, old process, old install dir, old Run value), the
  same way uninstall.ps1 -KeepCertificate would: the certificate store is
  never touched here (issue #10). If phase 2 fails, it is rolled back --
  the new package/Run value it managed to add are undone, and the old
  `copilot-ask.exe` is restarted if it was running -- and the script throws
  before phase 3 ever runs, so the pre-rename install is left intact. A
  pre-rename config.toml is left alone either way -- src/config.rs migrates
  it forward on first run of the new exe.

  One UAC prompt, the first time only, to trust the self-signed certificate.
  Design: docs/superpowers/specs/2026-09-15-packaging-and-install-design.md

.PARAMETER SkipBuild
  Use the existing target\release\wingman.exe instead of running cargo.

.PARAMETER NoAutostart
  Install without ticking start-with-Windows.
#>
[CmdletBinding()]
param(
    [switch]$SkipBuild,
    [switch]$NoAutostart
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

Import-Module (Join-Path $PSScriptRoot 'packaging\Wingman.Common.psm1') -Force

$Repo       = $PSScriptRoot
$Identity   = Get-WingmanIdentity
$InstallDir = Get-InstallDirPath -LocalAppData $env:LOCALAPPDATA -InstallDirName $Identity.Current.InstallDirName
$LegacyDir  = Get-InstallDirPath -LocalAppData $env:LOCALAPPDATA -InstallDirName $Identity.Legacy.InstallDirName
$StageDir   = Join-Path $Repo 'target\msix'
$RunKey     = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'

function Step($m) { Write-Host "==> $m" -ForegroundColor Cyan }
function Note($m) { Write-Host "    $m" -ForegroundColor DarkGray }

# --- Windows SDK tools, package logos ---------------------------------------
# Find-SdkTool and Build-Logos used to be duplicated here and in
# packaging\Build-Msix.ps1; both now live in Wingman.Common.psm1 (issue #164)
# so the two scripts cannot drift against each other again.

# --- certificate ---------------------------------------------------------
# Deliberately not $InstallDir\wingman.cer: phase 1 (this function's caller)
# must not touch $InstallDir at all, so a failure here can never look like a
# half-installed Wingman. The .cer file is only ever read back by
# Import-Certificate a few lines below; nothing later depends on where it sits.
function Get-SigningCert {
    $cert = Get-ChildItem Cert:\CurrentUser\My |
            Where-Object { $_.Subject -eq $Identity.Current.CertSubject -and $_.NotAfter -gt (Get-Date) } |
            Sort-Object NotAfter -Descending | Select-Object -First 1
    if ($cert) {
        Note "reusing certificate $($cert.Thumbprint)"
        return $cert
    }
    Step "Creating a self-signed code-signing certificate"
    Note "local to this machine, used only for Wingman, removed by uninstall.ps1"
    New-SelfSignedCertificate -Type Custom -Subject $Identity.Current.CertSubject `
        -KeyUsage DigitalSignature -FriendlyName 'Wingman package signing' `
        -CertStoreLocation 'Cert:\CurrentUser\My' -NotAfter (Get-Date).AddYears(5) `
        -TextExtension @('2.5.29.37={text}1.3.6.1.5.5.7.3.3', '2.5.29.19={text}')
}

# Windows' deployment service runs as SYSTEM, so it cannot see a per-user store:
# the certificate has to reach LocalMachine\TrustedPeople, and that needs admin.
# This is the only elevated step, and only the first time.
function Ensure-CertTrusted($cert, $cerPath) {
    $already = Get-ChildItem Cert:\LocalMachine\TrustedPeople -ErrorAction SilentlyContinue |
               Where-Object { $_.Thumbprint -eq $cert.Thumbprint }
    if ($already) { Note "certificate already trusted machine-wide"; return }

    Step "Trusting the certificate (one UAC prompt)"
    Export-Certificate -Cert $cert -FilePath $cerPath -Force | Out-Null
    $inner = "Import-Certificate -FilePath '$cerPath' -CertStoreLocation Cert:\LocalMachine\TrustedPeople | Out-Null"
    $p = Start-Process -FilePath 'powershell.exe' -Verb RunAs -Wait -PassThru `
         -ArgumentList '-NoProfile', '-NonInteractive', '-Command', $inner
    if ($p.ExitCode -ne 0) { throw "Trusting the certificate failed (exit $($p.ExitCode)). Without it Windows will refuse the package." }

    $ok = Get-ChildItem Cert:\LocalMachine\TrustedPeople | Where-Object { $_.Thumbprint -eq $cert.Thumbprint }
    if (-not $ok) { throw "Certificate did not land in LocalMachine\TrustedPeople." }
}

# --- pre-rename cleanup (phase 3; issue #165 moved this from first to last) -
# Only ever called after Invoke-PackageRegistrationPhase has returned
# successfully, i.e. after the new package is registered, verified and (unless
# -NoAutostart) autostarting -- so a failure anywhere before that point can
# never reach this function at all. Never touches the certificate store --
# that is what makes this equivalent to uninstall.ps1 -KeepCertificate. The
# old and new certificate subjects differ ("O=copilot-ask" vs "O=Wingman"), so
# the old certificate is simply unused from here on, not removed; removing it
# is uninstall.ps1's job if the user asks for it explicitly.
function Remove-LegacyInstall {
    [CmdletBinding(SupportsShouldProcess)]
    param()

    $running = Get-Process -Name $Identity.Legacy.ProcessName -ErrorAction SilentlyContinue
    $pkg = Get-AppxPackage -Name $Identity.Legacy.PackageName -ErrorAction SilentlyContinue
    $runValue = Get-ItemProperty -Path $RunKey -Name $Identity.Legacy.RunValue -ErrorAction SilentlyContinue
    $dirPresent = Test-Path $LegacyDir

    $plan = Get-CleanupPlan -ProcessRunning ([bool]$running) -PackageInstalled ([bool]$pkg) `
        -RunValuePresent ([bool]$runValue) -InstallDirPresent $dirPresent

    if (-not ($plan.StopProcess -or $plan.RemovePackage -or $plan.RemoveRunValue -or $plan.RemoveInstallDir)) {
        return
    }

    if (-not $PSCmdlet.ShouldProcess('the pre-rename copilot-ask install', 'Remove')) { return }

    Step "Removing the pre-rename copilot-ask install"

    if ($plan.StopProcess) {
        Note "stopping the running copilot-ask process"
        $running | Stop-Process -Force
        $running | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
    }

    if ($plan.RemovePackage) {
        Note "unregistering $($Identity.Legacy.PackageName)"
        Remove-AppxPackage -Package $pkg.PackageFullName
    }

    if ($plan.RemoveRunValue) {
        Note "removing the old '$($Identity.Legacy.RunValue)' Run value"
        Remove-ItemProperty -Path $RunKey -Name $Identity.Legacy.RunValue -ErrorAction SilentlyContinue
    }

    if ($plan.RemoveInstallDir) {
        # $LegacyDir is a fixed, computed path (never user input, never a
        # wildcard), so this cannot walk outside the old install directory.
        Note "deleting $LegacyDir"
        Remove-Item $LegacyDir -Recurse -Force
    }
}

# =============================================================================
# Phase 1 -- build, stage, sign, pack. No system side effects: nothing here
# touches a process, the registry, an installed package or $InstallDir, so a
# failure anywhere in this phase leaves the machine exactly as it was found.
# =============================================================================
$sdk     = Find-SdkTool
$version = ConvertTo-MsixVersion -CargoVersion (Get-CargoVersionString -CargoTomlPath (Join-Path $Repo 'Cargo.toml'))
Note "version $version"
Note "sdk     $(Split-Path $sdk.MakeAppx -Parent)"

$built = Join-Path $Repo "target\release\$($Identity.Current.ExeName)"
if (-not $SkipBuild) {
    Step "Building (cargo build --release)"
    & cargo build --release --manifest-path (Join-Path $Repo 'Cargo.toml')
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
}
if (-not (Test-Path $built)) { throw "No executable at $built. Run without -SkipBuild." }

# --- sign the build output directly, not an $InstallDir copy -----------------
# Keeps this phase free of $InstallDir entirely; phase 2 copies the already-
# signed exe in once it has stopped whatever might be locking that path.
$cert = Get-SigningCert
if (Test-Path $StageDir) { Remove-Item $StageDir -Recurse -Force }
New-Item -ItemType Directory -Force -Path $StageDir | Out-Null
Ensure-CertTrusted $cert (Join-Path $StageDir 'wingman.cer')

Step "Signing the executable"
# A sparse package's external executable must carry the package's signature;
# an unsigned one is rejected at deployment time.
& $sdk.SignTool sign /fd SHA256 /sha1 $cert.Thumbprint /s My $built | Out-Null
if ($LASTEXITCODE -ne 0) { throw "signing the executable failed" }

# --- stage and pack ----------------------------------------------------------
Step "Building the package"
New-Item -ItemType Directory -Force -Path (Join-Path $StageDir 'layout\Assets') | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $StageDir 'layout\Public')  | Out-Null

Build-Logos -IconPath (Join-Path $Repo 'assets\icon.ico') -Destination (Join-Path $StageDir 'layout\Assets')
# PublicFolder must exist in the package; makeappx drops empty directories.
Set-Content -Path (Join-Path $StageDir 'layout\Public\README.txt') -Encoding utf8 `
    -Value 'Declared by PublicFolder in the manifest. Intentionally empty.'

$manifest = Get-Content (Join-Path $Repo 'packaging\AppxManifest.xml.in') -Raw
$manifest = $manifest.Replace('@VERSION@', $version).Replace('@PUBLISHER@', $cert.Subject)
Set-Content -Path (Join-Path $StageDir 'layout\AppxManifest.xml') -Value $manifest -Encoding utf8

$msix = Join-Path $StageDir 'wingman.msix'
& $sdk.MakeAppx pack /d (Join-Path $StageDir 'layout') /p $msix /nv /o | Out-Null
if ($LASTEXITCODE -ne 0) { throw "makeappx failed" }

& $sdk.SignTool sign /fd SHA256 /sha1 $cert.Thumbprint /s My $msix | Out-Null
if ($LASTEXITCODE -ne 0) { throw "signing the package failed" }

# =============================================================================
# Phase 2 -- stop the old and current processes, register the new package,
# write the new Run value, verify the registration. The only phase with
# system side effects that can fail partway through; Invoke-PackageRegistration-
# Phase rolls itself back and throws rather than leaving a half-upgrade, so a
# failure here never reaches phase 3.
# =============================================================================
Step "Registering the package with Windows"
# -ExternalLocation is what makes this sparse: the executable stays at the real
# path above rather than being copied into WindowsApps, which keeps the
# autostart entry valid across upgrades and stops %APPDATA% being virtualized.
$registration = Invoke-PackageRegistrationPhase -Identity $Identity -BuiltExePath $built `
    -InstallDir $InstallDir -LegacyDir $LegacyDir -MsixPath $msix -RunKeyPath $RunKey `
    -NoAutostart:$NoAutostart
$pkg = $registration.Package

# =============================================================================
# Phase 3 -- only once phase 2 is verified: remove the pre-rename install.
# =============================================================================
Remove-LegacyInstall

# --- report ------------------------------------------------------------------
Write-Host ""
Step "Installed"
Note "package   $($pkg.PackageFullName)"
Note "exe       $InstallDir\$($Identity.Current.ExeName)"
Note "autostart $(if ($NoAutostart) { 'skipped' } else { 'on' })"
Note "config    $env:APPDATA\Wingman\config.toml (untouched; a pre-rename copilot-ask\config.toml, if any, is migrated forward on first run)"
Write-Host ""
Write-Host "  Set the Copilot key to it:" -ForegroundColor Yellow
Write-Host "  Settings > Bluetooth & devices > Keyboard > Customize Copilot key" -ForegroundColor Yellow
Write-Host "  on keyboard > Custom > Wingman" -ForegroundColor Yellow
Write-Host ""

Step "Starting it"
Start-Process (Join-Path $InstallDir $Identity.Current.ExeName) -ArgumentList '--settings'
