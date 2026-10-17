<#
.SYNOPSIS
  Install smoke test (#128): installs the release build the way a person
  would, starts it, checks it came up and wrote its config, quits it, and
  removes it. Fails with a plain message on the first step that does not hold.

.DESCRIPTION
  Two install paths, one per run (-Mode):

    Zip     The release zip from the README: the same files release.yml puts
            in it, zipped, unzipped into a fresh folder, and run from there
            with no portable marker, so the real config path is used.
    Sparse  install.ps1's path: self-signed certificate, trust in
            LocalMachine\TrustedPeople, makeappx and signtool, the sparse
            package registered with -ExternalLocation, the Run value. Then
            uninstall.ps1 and a check that it left nothing behind except the
            config, which it promises to keep.

  Both modes then check the same things: the owner window exists (it owns
  the tray icon), the single-instance name is held, config.toml was created
  and is owner-only (no inherited entries, one allowed entry, the current
  user), and a WM_CLOSE exits the process with code 0.

  It needs no API key and no network: nothing here asks a model. The child
  gets no *_API_KEY variables, and a bare launch does not ask.

  This runs the real app with the real mutex and window class names and
  writes the real %APPDATA%\Wingman, and Sparse mode installs a package and
  a certificate. It is for a throwaway CI runner only: it refuses to run
  unless CI is set or -AllowLocal is passed, and refuses to start when a
  Wingman is already running or %APPDATA%\Wingman already exists.

  Usage: pwsh scripts/install-smoke.ps1 -Mode Zip|Sparse [-ExePath target\release\wingman.exe]
#>
[CmdletBinding()]
param(
    [ValidateSet('Zip', 'Sparse')]
    [string]$Mode = 'Zip',
    [string]$ExePath = 'target\release\wingman.exe',
    [switch]$AllowLocal
)

# Wait-Until, the window class names and Test-MutexHeld come from
# smoke-launch.ps1 (dot-sourcing it only loads them). Dot-sourcing re-runs its
# param() block, which resets $ExePath and $AllowLocal, so keep ours.
$installSmokeExePath = $ExePath
$installSmokeAllowLocal = $AllowLocal
. (Join-Path $PSScriptRoot 'smoke-launch.ps1')
$ExePath = $installSmokeExePath
$AllowLocal = $installSmokeAllowLocal

# The files release.yml puts next to wingman.exe in the zip. A Pester test
# checks this list still matches the workflow.
$ReleaseZipExtras = @('LICENSE', 'THIRD_PARTY_NOTICES.md', 'PRIVACY.md', 'config.example.toml', 'actions.example.toml')

# What is wrong with a config file's access list, as plain sentences; empty
# when it is owner-only. Pure: takes the facts, not the file.
#   $IsProtected  inheritance is cut (icacls /inheritance:r)
#   $Rules        objects with Sid (string) and Type ('Allow' or 'Deny')
#   $AlsoAllowed  SIDs that may hold an entry besides the owner. Default:
#                 none. The app removes the SYSTEM and Administrators entries
#                 an elevated creator leaves behind (#966).
function Get-OwnerOnlyAclProblem {
    param(
        [Parameter(Mandatory)] [bool] $IsProtected,
        [AllowEmptyCollection()] [object[]] $Rules = @(),
        [Parameter(Mandatory)] [string] $OwnerSid,
        [string[]] $AlsoAllowed = @()
    )
    $problems = @()
    if (-not $IsProtected) { $problems += 'inheritance is not cut, so the folder entries apply too' }
    $allow = @($Rules | Where-Object { $_.Type -eq 'Allow' })
    if ($allow.Count -eq 0) { $problems += 'no allow entry at all' }
    foreach ($r in $allow) {
        if ($r.Sid -ne $OwnerSid -and $AlsoAllowed -notcontains $r.Sid) { $problems += "access is also granted to $($r.Sid)" }
    }
    if ($allow.Count -gt 0 -and -not ($allow | Where-Object { $_.Sid -eq $OwnerSid })) {
        $problems += 'the current user has no allow entry'
    }
    $problems
}

# The README's command for running a script of ours, as an argument list for
# powershell.exe (#1050): no profile, Bypass for a Restricted default policy,
# -File. Pure.
function Get-DocumentedScriptArgs {
    param(
        [Parameter(Mandatory)] [string] $Script,
        [string[]] $ScriptArgs = @()
    )
    @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $Script) + $ScriptArgs
}

# Runs install.ps1 or uninstall.ps1 the way the README tells a person to: in
# a child powershell.exe (Windows PowerShell 5.1, not this pwsh). The child
# starts from a Restricted policy (the default on a client Windows), so the
# script only runs because of -ExecutionPolicy Bypass; a runner is not
# Restricted, so without this the flag would never be checked. Output goes
# straight to the log; a non-zero exit fails the smoke.
function Invoke-DocumentedScript {
    param(
        [Parameter(Mandatory)] [string] $Script,
        [string[]] $ScriptArgs = @()
    )
    $before = $env:PSExecutionPolicyPreference
    try {
        $env:PSExecutionPolicyPreference = 'Restricted'
        & powershell.exe @(Get-DocumentedScriptArgs -Script $Script -ScriptArgs $ScriptArgs) | Out-Host
        $code = $LASTEXITCODE
    }
    finally {
        $env:PSExecutionPolicyPreference = $before
    }
    if ($code -ne 0) { Exit-InstallSmoke "$(Split-Path -Leaf $Script) exited with code $code" }
}

# Dot-sourcing (the Pester tests do) loads the functions without running.
if ($MyInvocation.InvocationName -eq '.') { return }

$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path

function Exit-InstallSmoke {
    param([string] $Message)
    throw "install smoke failed ($Mode): $Message"
}

function Write-Fact {
    param([string] $Message)
    Write-Output $Message
}

if (-not $env:CI -and -not $AllowLocal) {
    throw 'This script installs and runs the real app. Run it on a CI runner (CI is set) or pass -AllowLocal.'
}
$exeSource = (Resolve-Path -LiteralPath $ExePath).Path
if (Test-MutexHeld -Name $InstanceMutexName) { throw 'Wingman is already running; this only runs when none is.' }
$configDir = Join-Path $env:APPDATA 'Wingman'
if (Test-Path -LiteralPath $configDir) { throw "$configDir already exists; this only runs on a clean machine." }
$configFile = Join-Path $configDir 'config.toml'

Add-Type -Namespace InstallSmoke -Name Win -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("user32.dll", CharSet = System.Runtime.InteropServices.CharSet.Unicode)]
public static extern System.IntPtr FindWindowW(string cls, string title);
[System.Runtime.InteropServices.DllImport("user32.dll")]
public static extern bool PostMessageW(System.IntPtr hwnd, uint msg, System.UIntPtr wparam, System.IntPtr lparam);
'@
$WM_CLOSE = 0x0010

function Find-Window([string] $Class) {
    [InstallSmoke.Win]::FindWindowW($Class, [NullString]::Value)
}

function Get-OwnerOnlyConfigProblem {
    $acl = Get-Acl -LiteralPath $configFile
    $me = [System.Security.Principal.WindowsIdentity]::GetCurrent().User
    $rules = @($acl.Access | ForEach-Object {
        $sid = $_.IdentityReference.Translate([System.Security.Principal.SecurityIdentifier]).Value
        [pscustomobject]@{ Sid = $sid; Type = "$($_.AccessControlType)" }
    })
    # Not Write-Fact: its output would become part of this function's result.
    $script:aclFacts = "config acl: protected=$($acl.AreAccessRulesProtected) entries=" + (($rules | ForEach-Object { "$($_.Type):$($_.Sid)" }) -join ',')
    Get-OwnerOnlyAclProblem -IsProtected $acl.AreAccessRulesProtected -Rules $rules -OwnerSid $me.Value
}

# Waits for the running app to be fully up: owner window, single-instance
# name held, config.toml written, config owner-only.
function Assert-Up {
    param([System.Diagnostics.Process] $Process)
    $watched = $Process
    $up = Wait-Until -TimeoutMs 30000 -Condition {
        if ($watched -and $watched.HasExited) { Exit-InstallSmoke "the app exited early with code $($watched.ExitCode)" }
        (Find-Window $OwnerWindowClass) -ne [IntPtr]::Zero
    }
    if (-not $up) { Exit-InstallSmoke 'no owner window (the tray icon owner) within 30 s' }
    if (-not (Wait-Until -TimeoutMs 5000 -Condition { Test-MutexHeld -Name $InstanceMutexName })) {
        Exit-InstallSmoke 'the app does not hold the single-instance name'
    }
    Write-Fact 'ok: owner window exists and the single-instance name is held'
    if (-not (Wait-Until -TimeoutMs 15000 -Condition { Test-Path -LiteralPath $configFile })) {
        Exit-InstallSmoke "config.toml was not created at $configFile"
    }
    $problems = @(Get-OwnerOnlyConfigProblem)
    Write-Fact $script:aclFacts
    if ($problems.Count -gt 0) { Exit-InstallSmoke ("config.toml is not owner-only: " + ($problems -join '; ')) }
    Write-Fact 'ok: config.toml exists and is owner-only'
}

# WM_CLOSE like Quit does. While the settings window is open the owner window
# does not act on WM_CLOSE (#673), so close that first if it is there.
function Invoke-CleanQuit {
    param([System.Diagnostics.Process] $Process)
    $settings = Find-Window $SettingsWindowClass
    if ($settings -ne [IntPtr]::Zero) {
        [void][InstallSmoke.Win]::PostMessageW($settings, $WM_CLOSE, [UIntPtr]::Zero, [IntPtr]::Zero)
        if (-not (Wait-Until -TimeoutMs 10000 -Condition { (Find-Window $SettingsWindowClass) -eq [IntPtr]::Zero })) {
            Exit-InstallSmoke 'the settings window did not close within 10 s'
        }
    }
    $hwnd = Find-Window $OwnerWindowClass
    if ($hwnd -eq [IntPtr]::Zero) { Exit-InstallSmoke 'the owner window vanished before the close' }
    if (-not [InstallSmoke.Win]::PostMessageW($hwnd, $WM_CLOSE, [UIntPtr]::Zero, [IntPtr]::Zero)) {
        Exit-InstallSmoke 'could not post WM_CLOSE to the owner window'
    }
    if (-not $Process.WaitForExit(15000)) { Exit-InstallSmoke 'the app did not exit within 15 s of WM_CLOSE' }
    # A process found with Get-Process (the sparse install started it, not us)
    # reports an empty exit code, MEASURED 2026-10-17 on a runner. Only a code
    # that is there and not 0 fails; the exit itself is still checked above.
    if ($null -ne $Process.ExitCode -and "$($Process.ExitCode)" -ne '' -and $Process.ExitCode -ne 0) {
        Exit-InstallSmoke "the app exited with code $($Process.ExitCode)"
    }
    if (-not (Wait-Until -TimeoutMs 5000 -Condition { -not (Test-MutexHeld -Name $InstanceMutexName) })) {
        Exit-InstallSmoke 'the single-instance name is still held after exit'
    }
    Write-Fact 'ok: closed cleanly with code 0 and released the single-instance name'
}

function Invoke-BareLaunch {
    param([string] $Exe)
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $Exe
    $psi.UseShellExecute = $false
    foreach ($k in @($psi.Environment.Keys | Where-Object { $_ -like '*_API_KEY' })) {
        $psi.Environment.Remove($k) | Out-Null
    }
    [System.Diagnostics.Process]::Start($psi)
}

$work = Join-Path ([IO.Path]::GetTempPath()) ('wingman-install-smoke-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $work | Out-Null
$proc = $null
try {
    if ($Mode -eq 'Zip') {
        # Same contents as the release zip (release.yml, issue #830).
        $stage = Join-Path $work 'stage'
        New-Item -ItemType Directory -Path $stage | Out-Null
        Copy-Item -LiteralPath $exeSource -Destination (Join-Path $stage 'wingman.exe')
        foreach ($f in $ReleaseZipExtras) { Copy-Item -LiteralPath (Join-Path $repo $f) -Destination $stage }
        $zip = Join-Path $work 'wingman-windows-x64.zip'
        Compress-Archive -Path "$stage\*" -DestinationPath $zip
        $unzipped = Join-Path $work 'Wingman unzipped'
        Expand-Archive -LiteralPath $zip -DestinationPath $unzipped
        foreach ($f in @('wingman.exe') + $ReleaseZipExtras) {
            if (-not (Test-Path -LiteralPath (Join-Path $unzipped $f))) { Exit-InstallSmoke "the unzipped folder has no $f" }
        }
        if (Test-Path -LiteralPath (Join-Path $unzipped 'wingman.portable')) { Exit-InstallSmoke 'the zip must not hold a portable marker' }
        Write-Fact 'ok: zip unpacked with the release file set'

        $proc = Invoke-BareLaunch -Exe (Join-Path $unzipped 'wingman.exe')
        Assert-Up -Process $proc
        Invoke-CleanQuit -Process $proc
    }
    else {
        $isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
        $lua = (Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System' -Name EnableLUA -ErrorAction SilentlyContinue).EnableLUA
        Write-Fact "runner: admin=$isAdmin EnableLUA=$lua"
        Import-Module (Join-Path $repo 'packaging\Wingman.Common.psm1') -Force
        $id = Get-WingmanIdentity
        $sdk = Find-SdkTool
        Write-Fact "sdk: $sdk"
        $installDir = Get-InstallDirPath -LocalAppData $env:LOCALAPPDATA -InstallDirName $id.Current.InstallDirName
        $runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'

        # The runner is already an elevated administrator. install.ps1 makes and
        # trusts its own certificate here, which checks that the certificate step
        # skips RunAs when the caller is elevated (#965).

        # install.ps1 signs the exe in place and writes target\msix; it is
        # the real path, so it runs against the repo checkout as it is.
        # #1050: started the README way, in a child powershell.exe.
        Invoke-DocumentedScript -Script (Join-Path $repo 'install.ps1') -ScriptArgs @('-SkipBuild')
        Write-Fact 'ok: install.ps1 finished'

        if (-not (Get-AppxPackage -Name $id.Current.PackageName)) { Exit-InstallSmoke "package $($id.Current.PackageName) is not registered" }
        if (-not (Test-Path -LiteralPath (Join-Path $installDir $id.Current.ExeName))) { Exit-InstallSmoke "no exe in $installDir" }
        $run = (Get-ItemProperty $runKey -Name $id.Current.RunValue -ErrorAction SilentlyContinue).$($id.Current.RunValue)
        if ($run -ne ('"' + (Join-Path $installDir $id.Current.ExeName) + '"')) { Exit-InstallSmoke "Run value is '$run'" }
        if (-not (Get-ChildItem Cert:\LocalMachine\TrustedPeople | Where-Object { $_.Subject -eq $id.Current.CertSubject })) {
            Exit-InstallSmoke 'the certificate is not in LocalMachine\TrustedPeople'
        }
        Write-Fact 'ok: package registered, exe in place, Run value set, certificate trusted'

        # install.ps1 started the exe itself, with --settings.
        $proc = Get-Process -Name $id.Current.ProcessName -ErrorAction SilentlyContinue | Select-Object -First 1
        if (-not $proc) { Exit-InstallSmoke 'install.ps1 did not leave the app running' }
        Assert-Up -Process $proc
        Invoke-CleanQuit -Process $proc

        Invoke-DocumentedScript -Script (Join-Path $repo 'uninstall.ps1')
        Write-Fact 'ok: uninstall.ps1 finished'
        if (Get-AppxPackage -Name $id.Current.PackageName) { Exit-InstallSmoke 'the package is still registered after uninstall' }
        if (Test-Path -LiteralPath $installDir) { Exit-InstallSmoke "$installDir is still there after uninstall" }
        if (Get-ItemProperty $runKey -Name $id.Current.RunValue -ErrorAction SilentlyContinue) { Exit-InstallSmoke 'the Run value is still there after uninstall' }
        # uninstall.ps1 removes the machine-wide certificate through the same
        # self-elevation that does not work on a runner (see above), so it is
        # only warned about here and removed directly.
        $left = @(Get-ChildItem Cert:\LocalMachine\TrustedPeople | Where-Object { $_.Subject -eq $id.Current.CertSubject })
        if ($left.Count -gt 0) {
            Write-Warning 'uninstall.ps1 left the machine-wide certificate (its self-elevation does not work on a runner); removing it directly'
            $left | Remove-Item -Force
        }
        if (-not (Test-Path -LiteralPath $configFile)) { Exit-InstallSmoke 'uninstall deleted config.toml, which it promises to keep' }
        Write-Fact 'ok: uninstalled; nothing left but the config it promises to keep'
    }
    Write-Fact "install smoke ($Mode) passed"
}
finally {
    # Cleanup must not throw: it would replace the real failure.
    try { if ($proc -and -not $proc.HasExited) { $proc.Kill(); $proc.WaitForExit(10000) | Out-Null } } catch { Write-Warning "cleanup: $_" }
    try { Get-Process -Name wingman -ErrorAction SilentlyContinue | Stop-Process -Force } catch { Write-Warning "cleanup: $_" }
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $configDir -Recurse -Force -ErrorAction SilentlyContinue
}
