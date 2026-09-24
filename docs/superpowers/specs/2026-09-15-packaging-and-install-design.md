# copilot-ask — packaging, install and Copilot-key registration

Supplements [`2026-09-14-copilot-ask-design.md`](2026-09-14-copilot-ask-design.md),
which covers the running app. This one covers how it gets onto the machine and
how Windows comes to know about it.

## The problem

Built from source, `copilot-ask` is a bare `target\release\copilot-ask.exe`.
Nothing registers it, so:

- it never starts at login — `src/autostart.rs` works, but its `HKCU\…\Run`
  value is only written when the Settings checkbox is ticked;
- it appears in no app list — no Start Menu entry, no `Uninstall` registry key;
- it cannot be chosen in **Settings ▸ Bluetooth & devices ▸ Keyboard ▸
  Customize Copilot key on keyboard ▸ Custom**, which lists only packaged,
  signed apps.

It also lives in a build-output directory, so `cargo clean` would break any
path registered against it.

## Install layout

Current, post-rename (issues #1 and #10, both closed; identity from
`packaging\Wingman.Common.psm1`'s `Get-WingmanIdentity`):

| What | Where |
|---|---|
| executable | `%LOCALAPPDATA%\Programs\Wingman\wingman.exe` |
| config | `%APPDATA%\Wingman\config.toml` (renamed from `%APPDATA%\copilot-ask\config.toml`, issue #1; the old file is left in place and copied forward once, on first run) |
| signing certificate | `%LOCALAPPDATA%\Programs\Wingman\wingman.cer` |
| sparse package | installed by identity (`RaaifYousuf.Wingman`); no payload on disk |

As originally written (below, and the "Confirmed on 2026-09-15" measurement
and the manifest example further down), this document used the pre-rename
`%LOCALAPPDATA%\Programs\copilot-ask\copilot-ask.exe` / `RaaifYousuf.CopilotAsk`
identity, since the rename (issue #1) and the identity/icon work (issue #10)
had not yet landed when it was written. Both are done now; `uninstall.ps1`
still cleans up that pre-rename `copilot-ask` install (the `Legacy` identity
in `Get-WingmanIdentity`) wherever it is still found on a machine.

`%LOCALAPPDATA%\Programs` matches where other per-user apps on this machine
install, needs no admin, and is stable across rebuilds.

Install, upgrade and uninstall never read, write or delete `config.toml`. The
API keys in it survive all three, and are the user's to remove.

## Sparse package, not full MSIX

The package declares `uap10:AllowExternalContent` and is installed with
`Add-AppxPackage -ExternalLocation`, so the executable stays at a real path
outside `WindowsApps`. Two consequences, both of them the reason for the choice:

- **Autostart keeps working untouched.** `HKCU\…\Run` holds a genuine, stable
  path. A full MSIX installs under a per-version `WindowsApps` directory whose
  name changes on every upgrade, which would break the Run value and force
  `src/autostart.rs` to be rewritten around the `windows.startupTask`
  extension.
- **No write virtualization.** A full-trust MSIX app has its `%APPDATA%`
  writes redirected into a package-private store, which would move
  `config.toml` somewhere the tray's *Open config.toml* no longer points.
  Sparse packages leave the filesystem alone.

The cost is that the external executable must carry the same signature as the
package.

**Confirmed on 2026-09-15**: sparse packages *are* enumerated as Copilot key
providers. Querying the same catalog the Settings picker reads —

```powershell
[Windows.ApplicationModel.AppExtensions.AppExtensionCatalog,Windows.ApplicationModel,ContentType=WindowsRuntime]::
    Open("com.microsoft.windows.copilotkeyprovider").FindAllAsync()
```

— returns `DisplayName='copilot-ask' Id='CopilotAsk'
Package=RaaifYousuf.CopilotAsk_pa8sd8xv631fa`. It is the *only* entry;
Microsoft Copilot itself is special-cased by the shell rather than registered
through this extension. The full-MSIX fallback was therefore never needed.
(This measurement predates the 2026-09-16 rename, issues #1 and #10, both
closed; the mechanism it confirms -- sparse packages are enumerated as
Copilot key providers -- still holds, but a fresh query today would return
`DisplayName='Wingman' Id='Wingman' Package=RaaifYousuf.Wingman_...` instead
of the `copilot-ask`/`CopilotAsk` values shown here.)

## Manifest

Identity publisher must match the signing certificate subject exactly, or
deployment fails with `0x800B0109`. Shown here with the current,
post-rename identity (`packaging\AppxManifest.xml.in`); the pre-rename
manifest used `Name="RaaifYousuf.CopilotAsk"` and `DisplayName="copilot-ask"`
in the same two places.

```xml
<Identity Name="RaaifYousuf.Wingman" Publisher="CN=Raaif Yousuf" Version="0.1.0.0" />
```

Three things the manifest carries beyond the basics:

- `<uap10:AllowExternalContent>true</uap10:AllowExternalContent>` — the sparse
  package opt-in.
- `<Application … EntryPoint="Windows.FullTrustApplication">` with
  `runFullTrust` — this is a Win32 app, not UWP.
- the registration that makes the picker list it:

```xml
<uap3:Extension Category="windows.appExtension">
  <uap3:AppExtension Name="com.microsoft.windows.copilotkeyprovider"
                     Id="Wingman" DisplayName="Wingman"
                     Description="Check what's on screen with a vision model"
                     PublicFolder="Public" />
</uap3:Extension>
```

All four attributes are required. `PublicFolder` names a folder the shell may
read through a broker; it is declared but empty.

## Signing

A self-signed code-signing certificate, created in `Cert:\CurrentUser\My` and
installed into `LocalMachine\TrustedPeople`. The second step is the only part
of the install that needs administrator rights, so `install.ps1` self-elevates
for exactly that step and nothing else — one UAC prompt per machine, not per
install.

Both the `.exe` and the `.msix` are signed with it. An unsigned external
executable inside a sparse package is rejected at deployment.

## Launch semantics

The picker activates the app by AUMID with no arguments, and offers no way to
supply any. With the app already in the tray that reaches
`single_instance::poke_existing()`, which today opens Settings — wrong for a
key-press. So argument handling is added, and the default inverted:

| Invocation | Args | Already running → |
|---|---|---|
| Copilot key via picker | none | **ask** |
| Start Menu entry | none | **ask** |
| tray icon left-click | — | settings (unchanged) |
| `copilot-ask --settings` | `--settings` | settings |
| autostart at login | none | n/a — it is the first instance |

A tray app has no main window, so "ask" is the only meaningful reading of
"launch me" once it is running; Settings remains a left-click on the tray icon
away. `--settings` exists so that path stays scriptable and testable.

Mechanism: a new `WM_APP_ACTIVATE` posted to the owner window and handled
beside the existing tray commands, rather than spoofing a `WM_LBUTTONUP`
through `Tray::on_tray_message`. The existing message is a tray notification;
overloading it to mean "a second process launched" would tie two unrelated
things to one code path.

`app.ask()` is already re-entrancy safe — `App::busy` drops triggers while a
request is in flight — so a second launch during a request is ignored, as a
second key-press is.

## What the keyboard hook is for now

`src/hotkey.rs` is not removed. The picker only remaps the key at the shell
level; the secondary binding (`Ctrl+Shift+/`) still needs the low-level hook,
and the hook remains the working path if the picker does not list the package.
Retiring the primary binding and its synthetic-Ctrl-tap Start-menu suppression
is a separate change, to be made only once the picker route is confirmed on
this hardware.

## Where each list entry comes from

| List | Source |
|---|---|
| Start ▸ All apps | package app entry |
| Settings ▸ Apps ▸ Installed apps | package identity |
| Task Manager ▸ Startup | `HKCU\…\Run` value, as before |
| Copilot key picker | `copilotkeyprovider` extension |

No `Uninstall` registry key is written; package identity supplies that entry.

## Install sequence: build/register/cleanup, not cleanup/build/register (issue #165)

Until 2026-09-16, `install.ps1` removed the pre-rename `copilot-ask` install
(process, package, Run value, install dir) *before* building, signing,
packing and registering the new one. Under `$ErrorActionPreference = 'Stop'`,
any failure between those two things — a `cargo build` error, a signing
hiccup, `MakeAppx` rejecting the manifest, `Add-AppxPackage` refusing the
deployment (Developer Mode off, a stale registration, `0x800B0109`) — threw
past the point where the old install still existed, leaving the owner with
neither app running nor autostarting and only a script error to recover from.
None of those failure modes are about the rename itself; `Add-AppxPackage` in
particular is exactly the kind of step that can fail on a machine's first run
of a new install script even when everything upstream worked.

The script now runs in three phases, in this order:

1. **Build, stage, sign, pack.** `cargo build --release`, sign
   `target\release\wingman.exe` directly (not an `$InstallDir` copy — this
   phase never touches `$InstallDir` at all), build the package layout and
   logos, `MakeAppx pack`, sign the `.msix`. No process is stopped, no
   package is (un)registered, no registry value is touched, no install
   directory is deleted. A failure anywhere in this phase leaves the machine
   exactly as it was found — old and new installs both still whatever they
   were before the script ran.
2. **Register.** Stop the old `copilot-ask` process if running, then stop a
   running copy of the *current* `wingman` process if one exists (an
   in-place Wingman-to-Wingman upgrade) — the old one first, always, so the
   two low-level keyboard hooks are never both live and the new one is never
   started while the old one still owns the key. If a Wingman exe already
   exists at `$InstallDir` (the upgrade/re-run case), back it up to
   `<exe>.bak` before it is touched. Copy the now-signed exe into
   `$InstallDir`, `Add-AppxPackage -ExternalLocation`, verify the
   registration actually took (`Get-AppxPackage -Name RaaifYousuf.Wingman`
   must return something — `Add-AppxPackage` completing without throwing is
   not itself proof), then write the new `Run` value unless `-NoAutostart`;
   on success the backup is deleted. If anything in this phase throws,
   including the explicit verification check, it is rolled back: the
   `.bak` file, if one was taken, is copied back over `$InstallDir`'s exe
   *first* — before any process is restarted, so rollback never offers to
   relaunch a half-written or partially upgraded binary as "the previous"
   one — then the new package is unregistered if it got that far, the new
   `Run` value is removed if it got written, the old `copilot-ask.exe` is
   restarted if it was running before this phase stopped it and its install
   dir is still present, and (issue #173) the previous `wingman.exe` is
   restarted the same way if IT was running before this phase stopped it and
   its exe path still exists. The script then throws a message naming what
   failed and stating that whatever was running before was left in place
   (and restarted, if applicable) — never a silent partial state.

   **Why back up and restore, not "stage the new exe aside and swap only
   after" (issue #173's other option):** `Add-AppxPackage -ExternalLocation`
   reads the exe from `$InstallDir` at registration time, so the new signed
   exe has to already be at its final path *before* that call — staging it
   under a different name and renaming it in only once `Add-AppxPackage`
   returns would just move the same "what if the rename itself fails"
   problem one step later, not remove it. Backing up first means any
   failure from here on — including a `Copy-Item` that fails partway and
   leaves `$InstallDir`'s exe truncated, `THEORY (unverified)`: nothing rules
   out `File.Copy` failing mid-write on Windows — can restore the exact
   previous binary rather than whatever a failed copy left behind.

   **AppX upgrade atomicity, `THEORY (unverified on this machine)`:** if
   `Add-AppxPackage -ForceUpdateFromAnyVersion` throws before returning, the
   previously registered `RaaifYousuf.Wingman` package (whatever version)
   is expected to remain registered exactly as it was — Windows' deployment
   service is documented to apply an MSIX/sparse-package upgrade as a single
   transaction, not in place over the old registration. That is why
   `$state.NewPackageRegistered` only flips `$true` once `Add-AppxPackage`
   returns without throwing: in the ordinary "it threw" case there is no
   "new" package for the rollback to unregister, and `Get-AppxPackage` during
   rollback would still report the old version, which must never be touched.
   Not exercised against a real deployment failure in this session — see the
   "Owed" list below.
3. **Remove the legacy install.** Only reached if phase 2 returned
   successfully. Same shape as before (stop process, unregister package,
   remove Run value, delete install dir), and still never touches the
   certificate store or `%APPDATA%\copilot-ask`, matching
   `uninstall.ps1 -KeepCertificate`.

The phase-2 orchestration (`Invoke-PackageRegistrationPhase`) and the pure
decision it calls on failure (`Get-RollbackPlan`) live in
`packaging\Wingman.Common.psm1`, so Pester can mock every side-effecting
cmdlet (`Get-Process`, `Add-AppxPackage`, `Get-AppxPackage`, `Start-Process`,
…) and drive a simulated registration failure end to end — including
asserting the legacy package is never named by the rollback path — without
touching a real machine.

A first version of this (`Get-InstallPhaseOrder`) named the intended step
order as a hard-coded list, but nothing ever read it back against
`install.ps1`, so the list and the real script could drift from each other
in either direction without a test noticing (issue #168). It was replaced by
`Test-InstallPhaseOrder` (and its helper `Get-TopLevelPhaseMarkers`), which
parses `install.ps1`'s own AST and checks the order of its real top-level,
side-effecting statements — the build (`cargo`), the registration call
(`Invoke-PackageRegistrationPhase`) and the legacy-removal call
(`Remove-LegacyInstall`) — ignoring any same-named call that only appears
nested inside a function body. A regression back to "legacy removed before
the new one is proven" now fails a fast unit test run against the real file,
not a hand-synced description of it. See `packaging\Wingman.Common.Tests.ps1`.

**Owed:** this reordering has not been run against the owner's real machine
(that run is explicitly out of scope for an unattended session — no script in
this repo may touch the real registry, certificate store, installed packages
or processes). The owner's manual upgrade check is filed against issue #165.

**Rollback matrix (issue #173):** `packaging\Wingman.Common.Tests.ps1`'s
"Invoke-PackageRegistrationPhase rollback matrix" `Describe` block drives the
same phase-2 failure through three starting states — (a) a running legacy
`copilot-ask` and nothing current, (b) a running current `wingman.exe`
(the upgrade/re-run case), (c) nothing installed — crossed with failing at
`Add-AppxPackage`, at `Get-AppxPackage` verification, at writing the `Run`
value, and at the `Copy-Item` that stages the new exe itself. Each case
asserts which process (if any) is restarted, from which path, and that the
`.bak` exe is copied back before that restart. Also owed against a real
machine: whether `Add-AppxPackage` throwing mid-upgrade truly leaves the
previous package version registered (the atomicity theory above), and
whether a genuinely interrupted `Copy-Item` (killed process, full disk)
leaves a truncated exe the way the backup step assumes it might.

## Verification

`cargo test` covers argument parsing. Everything else is observable state, and
was checked directly on 2026-09-15 (Windows 11 build 26200):

| | Result |
|---|---|
| package registered | `RaaifYousuf.CopilotAsk_0.1.0.0_x64__pa8sd8xv631fa`, `Status: Ok` |
| listed as a Copilot key provider | yes, via the extension catalog above |
| Start ▸ All apps | yes — AUMID `RaaifYousuf.CopilotAsk_pa8sd8xv631fa!CopilotAsk` |
| `AllowExternalContent` honoured | `true` in the deployed manifest |
| autostart | `HKCU\…\Run` → `"%LOCALAPPDATA%\Programs\copilot-ask\copilot-ask.exe"`, other entries intact |
| no-argument launch while running | one process throughout; spinner then a real card with a model answer |
| `config.toml` | untouched by install |
| `wingman.exe --settings` as the FIRST instance (nothing running yet) | Settings window opens once the tray icon and hook exist, before message pumping starts (issue #149). Owed: not yet checked by hand on this machine. `cargo test single_instance` covers only the pure argv decision (`first_launch_action`); a bare `wingman.exe` launch under the same conditions must NOT open Settings -- check both. |
| phase ordering (issues #165, #168): legacy install removed only after the new one registers and verifies, checked against install.ps1's real AST, not a hand-synced list | `Invoke-Pester -Path packaging` — `Test-InstallPhaseOrder` parses `install.ps1` and asserts the top-level `cargo` build call sorts before the `Invoke-PackageRegistrationPhase` call, which sorts before the `Remove-LegacyInstall` call; synthetic-fixture tests prove it also catches a planted violation of that order |
| phase-2 rollback on a simulated registration failure | `Invoke-Pester -Path packaging` — `Invoke-PackageRegistrationPhase`'s "simulates a registration-verification failure" test: mocks `Add-AppxPackage`/`Get-AppxPackage`/`Get-Process`/`Start-Process`, asserts the function throws, `Remove-AppxPackage` is never called against the legacy package, and the old `copilot-ask` process is restarted |
| old process stopped before the new one is (re)started, and the two low-level keyboard hooks are never simultaneously live | `Invoke-PackageRegistrationPhase`'s "stops the old process before attempting to register" test, asserting call order via a mocked `Stop-Process`/`Add-AppxPackage` sequence. Owed: not checked against a real simultaneous-processes machine state -- `install.ps1` itself was never run for real in this session |
| a real interrupted upgrade on the owner's machine (kill `powershell.exe` mid `Add-AppxPackage`, or run with Developer Mode off, and confirm `copilot-ask` is still running/autostarting afterward) | **Owed** -- explicitly out of scope for an unattended session; see issue #165 |
| phase-2 rollback restarts the previous WINGMAN process too, not just legacy copilot-ask, and never overwrites the previous exe before registration succeeds (issue #173) | `Invoke-Pester -Path packaging` — "Invoke-PackageRegistrationPhase rollback matrix" crosses three starting states (legacy running / current Wingman running (re-run) / nothing installed) with four failing steps (`Add-AppxPackage`, verification, the `Run` value write, the exe `Copy-Item` itself); each case asserts the right process is restarted from the right path and that a `.bak` exe is restored first when one was taken. Mutation-checked: disabling the current-process restart call breaks exactly the 4 tests that exercise it. Owed: not checked against a real interrupted upgrade -- see the two THEORY notes above |

## Setting the key is the user's, not the installer's

`HKCU\Software\Microsoft\Windows\Shell\BrandedKey` is documented as readable,
and it is: `BrandedKeyChoiceType` and `AppAumid` can be queried to find out
whether this app currently owns the key. Writing them fails with
*Attempted to perform an unauthorized operation* even though the values sit in
HKCU, because the shell protects them — an app cannot make itself the Copilot
key target. That is deliberate, and matches Microsoft's guidance that apps
should not push users to change the selection. `install.ps1` prints the path
through Settings rather than pretending it can do it.

`uninstall.ps1` *can* still hand the key back, because setting
`BrandedKeyChoiceType` to `Search` is a narrowing of a choice rather than a
claim on it.

## Risks

- **A self-signed certificate is trusted machine-wide.** It is generated
  locally, used only for this app, and removed by `uninstall.ps1`. Worth
  stating plainly because "install this certificate" is otherwise exactly what
  malware asks for.
- **Upgrades must bump `Version` in the manifest**, or `Add-AppxPackage`
  refuses the install. `install.ps1` derives it from `Cargo.toml` so the two
  cannot drift.
- **A cold launch does not ask.** If no instance is running, a Copilot key
  press starts the tray app and stops there, because the same bare invocation
  is what autostart uses at login — making it ask would fire a billed API call
  on every boot. With autostart on, an instance is essentially always running.
- **`--` is illegal inside an XML comment.** `makeappx` reports it only as
  `error C00CEE23 … '>' expected` with a line and column, which reads like a
  malformed tag. Cost twenty minutes; noted in the manifest itself.
