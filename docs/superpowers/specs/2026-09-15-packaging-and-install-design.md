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

| What | Where |
|---|---|
| executable | `%LOCALAPPDATA%\Programs\copilot-ask\copilot-ask.exe` |
| config (unchanged) | `%APPDATA%\copilot-ask\config.toml` |
| signing certificate | `%LOCALAPPDATA%\Programs\copilot-ask\copilot-ask.cer` |
| sparse package | installed by identity; no payload on disk |

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

## Manifest

Identity publisher must match the signing certificate subject exactly, or
deployment fails with `0x800B0109`.

```xml
<Identity Name="RaaifYousuf.CopilotAsk" Publisher="CN=Raaif Yousuf" Version="0.1.0.0" />
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
                     Id="CopilotAsk" DisplayName="copilot-ask"
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
