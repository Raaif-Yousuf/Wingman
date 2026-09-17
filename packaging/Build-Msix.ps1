<#
.SYNOPSIS
  Build the sparse MSIX package headlessly (CI-only companion to install.ps1).

.DESCRIPTION
  Produces target\msix\wingman.msix from an already-built
  target\release\wingman.exe, staging logos and substituting the manifest the
  same way install.ps1's inline packaging step does (see packaging\
  AppxManifest.xml.in and docs\superpowers\specs\2026-09-15-packaging-and-
  install-design.md). Unlike install.ps1, this script never elevates, never
  calls Add-AppxPackage, and never touches the machine-wide certificate trust
  store or the HKCU Run key: it only builds and (if a certificate is
  supplied) signs the two artifacts a GitHub release needs. It exists so
  release.yml can produce an MSIX on a CI runner with no interactive session
  and no admin rights, which install.ps1's Ensure-CertTrusted step requires.

  Find-SdkTools and Build-Logos are intentionally duplicated from install.ps1
  rather than shared, because Wingman.Common.psm1 documents itself as pure,
  machine- and filesystem-untouching logic only, and these two functions are
  neither (one shells out to makeappx/signtool, one draws bitmaps and writes
  PNGs). A tech-debt issue is filed proposing a third, impure shared module
  if the two copies ever drift, the way install.ps1 and uninstall.ps1 drifted
  once before Wingman.Common.psm1 existed (see that module's own header).

.PARAMETER ExePath
  Path to the already-built release executable. Defaults to
  target\release\wingman.exe (what `cargo build --release` produces).

.PARAMETER PfxPath
  Path to a PKCS#12 certificate file to sign the exe and the package with. If
  omitted, both are left unsigned -- CLAUDE.md rule 4 (sparse package) still
  applies to the manifest either way; signing only affects whether the
  artifact can be *deployed* with Add-AppxPackage, not whether it builds.

.PARAMETER PfxPassword
  SecureString password for -PfxPath. Required if -PfxPath is given.

.OUTPUTS
  Writes the signed-or-not executable to target\msix\layout\wingman.exe and
  the package to target\msix\wingman.msix, and prints both paths prefixed
  with "built-exe=" / "built-msix=" so a CI step can capture them without
  parsing PowerShell objects.
#>
[CmdletBinding()]
param(
    [string]$ExePath = (Join-Path $PSScriptRoot '..\target\release\wingman.exe'),
    [string]$PfxPath,
    [securestring]$PfxPassword
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

Import-Module (Join-Path $PSScriptRoot 'Wingman.Common.psm1') -Force

$Repo     = Split-Path $PSScriptRoot -Parent
$Identity = Get-WingmanIdentity
$StageDir = Join-Path $Repo 'target\msix'

function Step($m) { Write-Host "==> $m" -ForegroundColor Cyan }
function Note($m) { Write-Host "    $m" -ForegroundColor DarkGray }

if (-not (Test-Path $ExePath)) {
    throw "No executable at $ExePath. Run 'cargo build --release' first."
}

# --- Windows SDK tools -------------------------------------------------------
# Duplicated from install.ps1's Find-SdkTools; see the header note above.
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
    throw "Windows SDK not found. makeappx.exe and signtool.exe are needed to package the app. The windows-latest GitHub Actions runner ships one; a local build needs the 'MSVC v143 build tools' workload in the Visual Studio Installer."
}

# --- package logos -----------------------------------------------------------
# Duplicated from install.ps1's Build-Logos; see the header note above.
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

$sdk = Find-SdkTools
Note "sdk $(Split-Path $sdk.MakeAppx -Parent)"

# --- certificate (optional) ---------------------------------------------------
$cert = $null
if ($PfxPath) {
    if (-not $PfxPassword) { throw "-PfxPassword is required with -PfxPath." }
    Step "Importing the signing certificate"
    # CurrentUser store: no admin needed. Unlike install.ps1's Ensure-CertTrusted,
    # this never reaches LocalMachine\TrustedPeople, so it never elevates -- the
    # resulting package can be built and signed, just not deployed with
    # Add-AppxPackage on THIS machine (that needs the cert trusted machine-wide
    # too, which is a per-installer's job, not the release build's).
    $cert = Import-PfxCertificate -FilePath $PfxPath -CertStoreLocation Cert:\CurrentUser\My -Password $PfxPassword
    Note "thumbprint $($cert.Thumbprint), subject $($cert.Subject)"
} else {
    Note "No -PfxPath given: building UNSIGNED artifacts."
}
$publisher = if ($cert) { $cert.Subject } else { $Identity.Current.CertSubject }

# --- stage ---------------------------------------------------------------
Step "Staging the package"
if (Test-Path $StageDir) { Remove-Item $StageDir -Recurse -Force }
New-Item -ItemType Directory -Force -Path (Join-Path $StageDir 'layout\Assets') | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $StageDir 'layout\Public')  | Out-Null

Build-Logos (Join-Path $StageDir 'layout\Assets')
# PublicFolder must exist in the package; makeappx drops empty directories.
Set-Content -Path (Join-Path $StageDir 'layout\Public\README.txt') -Encoding utf8 `
    -Value 'Declared by PublicFolder in the manifest. Intentionally empty.'

$layoutExe = Join-Path $StageDir "layout\$($Identity.Current.ExeName)"
Copy-Item $ExePath $layoutExe -Force

$version  = ConvertTo-MsixVersion -CargoVersion (Get-CargoVersionString -CargoTomlPath (Join-Path $Repo 'Cargo.toml'))
$manifest = Get-Content (Join-Path $Repo 'packaging\AppxManifest.xml.in') -Raw
$manifest = $manifest.Replace('@VERSION@', $version).Replace('@PUBLISHER@', $publisher)
Set-Content -Path (Join-Path $StageDir 'layout\AppxManifest.xml') -Value $manifest -Encoding utf8
Note "version $version, publisher $publisher"

# --- sign the executable (before packing, same order as install.ps1) --------
if ($cert) {
    Step "Signing the executable"
    & $sdk.SignTool sign /fd SHA256 /sha1 $cert.Thumbprint /s My $layoutExe | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "signing the executable failed" }
}

# --- pack ----------------------------------------------------------------
$msix = Join-Path $StageDir 'wingman.msix'
Step "Packing $msix"
& $sdk.MakeAppx pack /d (Join-Path $StageDir 'layout') /p $msix /nv /o | Out-Null
if ($LASTEXITCODE -ne 0) { throw "makeappx failed" }

if ($cert) {
    Step "Signing the package"
    & $sdk.SignTool sign /fd SHA256 /sha1 $cert.Thumbprint /s My $msix | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "signing the package failed" }
}

Step "Built"
Note "exe  $layoutExe"
Note "msix $msix"
Write-Output "built-exe=$layoutExe"
Write-Output "built-msix=$msix"
