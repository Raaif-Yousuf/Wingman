<#
.SYNOPSIS
  Build, sign, install and register copilot-ask.

.DESCRIPTION
  Puts copilot-ask.exe in %LOCALAPPDATA%\Programs\copilot-ask, installs a signed
  sparse MSIX package so Windows knows about it, and turns on start-with-Windows.

  That package is what puts copilot-ask in Start > All apps, in Settings > Apps >
  Installed apps, and in the app picker under Settings > Bluetooth & devices >
  Keyboard > Customize Copilot key on keyboard > Custom. Windows lists only
  packaged, signed apps there, which is the whole reason for the ceremony.

  Re-run it to upgrade in place. It never reads, writes or deletes
  %APPDATA%\copilot-ask\config.toml, so API keys and settings survive.

  One UAC prompt, the first time only, to trust the self-signed certificate.
  Design: docs/superpowers/specs/2026-09-15-packaging-and-install-design.md

.PARAMETER SkipBuild
  Use the existing target\release\copilot-ask.exe instead of running cargo.

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

$Repo        = $PSScriptRoot
$InstallDir  = Join-Path $env:LOCALAPPDATA 'Programs\copilot-ask'
$StageDir    = Join-Path $Repo 'target\msix'
$CertSubject = 'CN=Raaif Yousuf, O=copilot-ask'
$PackageName = 'RaaifYousuf.CopilotAsk'
$RunKey      = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$RunValue    = 'copilot-ask'

function Step($m) { Write-Host "==> $m" -ForegroundColor Cyan }
function Note($m) { Write-Host "    $m" -ForegroundColor DarkGray }

# --- Windows SDK tools -------------------------------------------------------
# makeappx and signtool are not on PATH by default; pick the newest SDK that has
# both rather than hard-coding a version that a machine may not have.
function Find-SdkTools {
    $roots = @(
        "${env:ProgramFiles(x86)}\Windows Kits\10\bin",
        "$env:ProgramFiles\Windows Kits\10\bin"
    ) | Where-Object { Test-Path $_ }

    foreach ($root in $roots) {
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
    throw "Windows SDK not found. makeappx.exe and signtool.exe are needed to package the app. Install the Windows SDK, or the 'MSVC v143 build tools' workload in the Visual Studio Installer."
}

# --- version -----------------------------------------------------------------
# Derived from Cargo.toml: Add-AppxPackage refuses an install whose Version has
# not increased, so a manually maintained manifest version would silently block
# upgrades the first time someone forgot to bump it.
function Get-CargoVersion {
    $line = Select-String -Path (Join-Path $Repo 'Cargo.toml') -Pattern '^\s*version\s*=\s*"([^"]+)"' |
            Select-Object -First 1
    if (-not $line) { throw "No version found in Cargo.toml" }
    $v = $line.Matches[0].Groups[1].Value
    # MSIX wants four parts and reserves the last for the store; Cargo gives three.
    if ($v -notmatch '^\d+\.\d+\.\d+$') { throw "Unexpected Cargo version '$v'" }
    "$v.0"
}

# --- package logos -----------------------------------------------------------
# Derived from assets\icon.ico rather than checked in, so the tray icon and the
# Start menu tile can never drift apart -- change the .ico and both follow.
function Build-Logos($dest) {
    Add-Type -AssemblyName System.Drawing
    $ico = New-Object System.Drawing.Icon((Join-Path $Repo 'assets\icon.ico'), 256, 256)
    $src = $ico.ToBitmap()
    try {
        foreach ($spec in @(
            @{ Name = 'Square44x44Logo';   Size = 44  },
            @{ Name = 'Square150x150Logo'; Size = 150 },
            @{ Name = 'StoreLogo';         Size = 50  }
        )) {
            $bmp = New-Object System.Drawing.Bitmap($spec.Size, $spec.Size)
            $g = [System.Drawing.Graphics]::FromImage($bmp)
            $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
            $g.PixelOffsetMode   = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
            $g.Clear([System.Drawing.Color]::Transparent)
            $g.DrawImage($src, 0, 0, $spec.Size, $spec.Size)
            $g.Dispose()
            $bmp.Save((Join-Path $dest "$($spec.Name).png"), [System.Drawing.Imaging.ImageFormat]::Png)
            $bmp.Dispose()
        }
    } finally {
        $src.Dispose()
        $ico.Dispose()
    }
}

# --- certificate -------------------------------------------------------------
function Get-SigningCert {
    $cert = Get-ChildItem Cert:\CurrentUser\My |
            Where-Object { $_.Subject -eq $CertSubject -and $_.NotAfter -gt (Get-Date) } |
            Sort-Object NotAfter -Descending | Select-Object -First 1
    if ($cert) {
        Note "reusing certificate $($cert.Thumbprint)"
        return $cert
    }
    Step "Creating a self-signed code-signing certificate"
    Note "local to this machine, used only for copilot-ask, removed by uninstall.ps1"
    New-SelfSignedCertificate -Type Custom -Subject $CertSubject `
        -KeyUsage DigitalSignature -FriendlyName 'copilot-ask package signing' `
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

# =============================================================================
$sdk     = Find-SdkTools
$version = Get-CargoVersion
Note "version $version"
Note "sdk     $(Split-Path $sdk.MakeAppx -Parent)"

# --- build -------------------------------------------------------------------
$built = Join-Path $Repo 'target\release\copilot-ask.exe'
if (-not $SkipBuild) {
    Step "Building (cargo build --release)"
    & cargo build --release --manifest-path (Join-Path $Repo 'Cargo.toml')
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
}
if (-not (Test-Path $built)) { throw "No executable at $built. Run without -SkipBuild." }

# --- stop a running copy -----------------------------------------------------
# The file is locked while it runs, and a stale instance would keep the old
# keyboard hook and tray icon alive next to the new one.
$running = Get-Process copilot-ask -ErrorAction SilentlyContinue
if ($running) {
    Step "Stopping the running copy"
    $running | Stop-Process -Force
    $running | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
}

# --- place the executable ----------------------------------------------------
Step "Installing to $InstallDir"
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
Copy-Item $built (Join-Path $InstallDir 'copilot-ask.exe') -Force

# --- sign --------------------------------------------------------------------
$cert    = Get-SigningCert
$cerPath = Join-Path $InstallDir 'copilot-ask.cer'
Ensure-CertTrusted $cert $cerPath

Step "Signing the executable"
# A sparse package's external executable must carry the package's signature;
# an unsigned one is rejected at deployment time.
& $sdk.SignTool sign /fd SHA256 /sha1 $cert.Thumbprint /s My `
    (Join-Path $InstallDir 'copilot-ask.exe') | Out-Null
if ($LASTEXITCODE -ne 0) { throw "signing the executable failed" }

# --- stage and pack ----------------------------------------------------------
Step "Building the package"
if (Test-Path $StageDir) { Remove-Item $StageDir -Recurse -Force }
New-Item -ItemType Directory -Force -Path (Join-Path $StageDir 'layout\Assets') | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $StageDir 'layout\Public')  | Out-Null

Build-Logos (Join-Path $StageDir 'layout\Assets')
# PublicFolder must exist in the package; makeappx drops empty directories.
Set-Content -Path (Join-Path $StageDir 'layout\Public\README.txt') -Encoding utf8 `
    -Value 'Declared by PublicFolder in the manifest. Intentionally empty.'

$manifest = Get-Content (Join-Path $Repo 'packaging\AppxManifest.xml.in') -Raw
$manifest = $manifest.Replace('@VERSION@', $version).Replace('@PUBLISHER@', $cert.Subject)
Set-Content -Path (Join-Path $StageDir 'layout\AppxManifest.xml') -Value $manifest -Encoding utf8

$msix = Join-Path $StageDir 'copilot-ask.msix'
& $sdk.MakeAppx pack /d (Join-Path $StageDir 'layout') /p $msix /nv /o | Out-Null
if ($LASTEXITCODE -ne 0) { throw "makeappx failed" }

& $sdk.SignTool sign /fd SHA256 /sha1 $cert.Thumbprint /s My $msix | Out-Null
if ($LASTEXITCODE -ne 0) { throw "signing the package failed" }

# --- deploy ------------------------------------------------------------------
Step "Registering the package with Windows"
# -ExternalLocation is what makes this sparse: the executable stays at the real
# path above rather than being copied into WindowsApps, which keeps the
# autostart entry valid across upgrades and stops %APPDATA% being virtualized.
Add-AppxPackage -Path $msix -ExternalLocation $InstallDir -ForceUpdateFromAnyVersion

# --- autostart ---------------------------------------------------------------
# Written directly rather than through the app's own Settings checkbox so a
# fresh install is already enabled. Must match the format src/autostart.rs
# writes -- a quoted absolute path -- or repair_if_stale rewrites it on launch.
if (-not $NoAutostart) {
    Step "Enabling start with Windows"
    # Never New-Item -Force this key: on an existing registry key that recreates
    # it, silently deleting every other startup entry the user has.
    if (-not (Test-Path $RunKey)) { New-Item -Path $RunKey | Out-Null }
    Set-ItemProperty -Path $RunKey -Name $RunValue -Value "`"$InstallDir\copilot-ask.exe`""
}

# --- report ------------------------------------------------------------------
$pkg = Get-AppxPackage -Name $PackageName
Write-Host ""
Step "Installed"
Note "package   $($pkg.PackageFullName)"
Note "exe       $InstallDir\copilot-ask.exe"
Note "autostart $(if ($NoAutostart) { 'skipped' } else { 'on' })"
Note "config    $env:APPDATA\copilot-ask\config.toml (untouched)"
Write-Host ""
Write-Host "  Set the Copilot key to it:" -ForegroundColor Yellow
Write-Host "  Settings > Bluetooth & devices > Keyboard > Customize Copilot key" -ForegroundColor Yellow
Write-Host "  on keyboard > Custom > copilot-ask" -ForegroundColor Yellow
Write-Host ""

Step "Starting it"
Start-Process (Join-Path $InstallDir 'copilot-ask.exe') -ArgumentList '--settings'
