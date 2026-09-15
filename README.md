# copilot-ask

A tray app for the Dell XPS 14. Press the Copilot key, and whatever is on your
screen gets checked by a vision model. The verdict arrives as a notification card
in the corner; click it for the full working.

Built for one job: verifying a hand-worked physics or statistics problem before
you commit the numbers.

```
      ╭───╮                  ╭──────────────────────────────────────────╮
      │ ◜ │   ── then ──▶    │ 53.3 rad/s^2; your 26.7 uses hoop inertia │
      ╰───╯                  ╰──────────────────────────────────────────╯
     spinner                       click it for the full working
```

Click anywhere else and it goes away. Clicks while the spinner is up are ignored,
so you can keep working while it thinks.

## Install

```powershell
.\install.ps1
```

Builds it, puts the `.exe` in `%LOCALAPPDATA%\Programs\copilot-ask`, registers it
with Windows, and switches on start-with-Windows. Re-run it to upgrade in place.
One UAC prompt the first time, to trust the certificate it signs with; none after
that.

Registering is what puts copilot-ask in **Start ▸ All apps**, in
**Settings ▸ Apps ▸ Installed apps**, and in the Copilot key picker described
below. Windows lists only packaged, signed apps in that picker, which is the
whole reason the install does more than copy a file.

`-SkipBuild` uses the existing `target\release\copilot-ask.exe` instead of
running cargo; `-NoAutostart` installs without the login entry.

Nothing in install, upgrade or uninstall reads, writes or deletes your
`config.toml`, so your keys and prompt survive all three.

### Or don't install it

```powershell
cargo build --release
# target\release\copilot-ask.exe
```

The `.exe` is standalone and runs from anywhere with no runtime dependency. You
lose the app lists and the Copilot key picker, and start-with-Windows only if you
tick it yourself in Settings.

## First run

Launch it. A tray icon appears and `%APPDATA%\copilot-ask\config.toml` is created.
Left-click the tray icon to open **Settings**, and paste your key into
**OpenAI API key** or **Anthropic API key** — both are in the Providers group at
the top. Everything else lives there too: model, effort, capture size, card
timeout, text size, the difficulty toggle and the prompt.

The config file is still there (`Open config.toml` in the tray menu) if you want
to edit it by hand.

`OPENAI_API_KEY` / `ANTHROPIC_API_KEY` in the environment override the file, so
you can keep keys out of the config entirely if you prefer.

Configure one provider or both. With both set, Anthropic is the automatic
fallback when OpenAI fails — a provider with an empty key is skipped, not treated
as an error.

Switch models from the tray at any time; the choice is written straight back to
the config. To offer a model that isn't listed, add it to `models` under the
relevant provider — no rebuild needed.

## Difficulty rating

Each answer carries a 1-10 rating of the problem in the card's bottom-right
corner, green through amber to red, with a purple **U** above 10.

| | |
|---|---|
| 1 | easy high-school |
| 3 | easy university intro course |
| 5 | medium university |
| 7 | hard university, typically graduate coursework |
| 9 | very hard for an undergraduate |
| 10 | a PhD student would struggle |
| U | a professor would struggle |

Turn it off with **Show difficulty rating** in Settings. Off means the rating is
never requested, not merely hidden, so it costs nothing.

## Hotkeys

Two bindings, both live at once:

| | default | notes |
|---|---|---|
| primary | the Copilot key | Windows emits it as `Win+Shift+F23` |
| secondary | `Ctrl+Shift+/` | works on any keyboard |

If the Copilot key does nothing, use **Set Copilot key…** in the tray menu and
press it once — whatever your firmware actually emits gets captured and saved.
Same for the secondary binding.

### Through Windows Settings instead

Once installed, copilot-ask can also be made the Copilot key's target the
official way:

**Settings ▸ Bluetooth & devices ▸ Keyboard ▸ Customize Copilot key on keyboard
▸ Custom ▸ copilot-ask**

You have to click that yourself. Windows protects the setting so no app can make
itself the target — which is the right call, and means an installer cannot do it
for you however much it would like to.

Both routes work at once and do the same thing. The difference is that this one
is Windows launching the app, so it works regardless of what the keyboard hook
sees; the hook is still what serves the secondary binding.

One gap worth knowing: if copilot-ask is **not already running**, a key press
starts it and stops there rather than asking. That is deliberate — the same bare
launch is what runs at login, and asking on it would mean a billed API call every
boot. With start-with-Windows on it is always running anyway.

## Tray menu

- **Ask now** — trigger without the hotkey
- **Settings…** — the settings window (left-clicking the tray icon opens this too)
- **Copy last answer** — headline and working to the clipboard
- **Provider ▸** — which service answers (ChatGPT or Claude)
- **ChatGPT model ▸** / **Claude model ▸** — switch models, saved immediately
- **Set Copilot key…** / **Set secondary key…** — rebind by pressing the key
- **Edit settings** — opens `config.toml`
- **Reload settings** — re-read the file without restarting
- **Quit**

## Settings worth knowing

| key | default | what it does |
|---|---|---|
| `capture.max_edge` | `1568` | long-edge downscale before upload; lower cuts cost |
| `capture.monitor` | `"active"` | `"active"` follows the focused window; `"primary"` pins it |
| `providers.order` | `["openai", "anthropic"]` | fallback order |
| `providers.*.effort` | `"low"` | reasoning depth; raise for harder problems |
| `ui.card_seconds` | `12` | auto-dismiss for the collapsed card; `0` = never |
| `ui.text_scale` | `1.0` | multiplies the card's font size; lower is smaller |
| `ui.show_difficulty` | `true` | the 1-10/U badge; when off it is not requested at all |
| `providers.*.models` | see file | what the tray's model submenu offers |
| `ui.prompt` | see file | the system prompt — edit it to change what it checks |

Roughly 1-2 cents and 3-8 seconds per check at the defaults.

## Start with Windows

`install.ps1` turns this on for you. Otherwise, tick **Start with Windows** in
Settings — same entry, same effect. It writes a per-user entry to
`HKCU\Software\Microsoft\Windows\CurrentVersion\Run` — no admin prompt, and
it shows up in Task Manager's Startup tab like any other startup app. Untick to
remove it.

The checkbox reads the registry when Settings opens, so it always reflects
reality rather than a mirrored setting that can drift. If you move or rebuild
the exe somewhere else, the entry is repaired to the new path on next launch.

## Only one instance

Launching it again while it is already running does not start a second copy —
the new process hands over to the running one, which opens its Settings window,
then exits. Without that, two instances mean two tray icons, two keyboard hooks
and **two billed API calls per keypress**, with nothing in the UI to hint at it.

If you kill it with Task Manager rather than **Quit**, the tray icon can linger
as a ghost until you mouse over it — the process never got to remove it.

## Uninstall

```powershell
.\uninstall.ps1
```

Stops it, unregisters the package, clears the autostart entry, hands the Copilot
key back to Search if it was pointed here, deletes
`%LOCALAPPDATA%\Programs\copilot-ask`, and removes the signing certificate from
both stores. One UAC prompt, for that last part; `-KeepCertificate` skips it,
which is what you want if you are about to reinstall.

Since it is a registered app you can also use **Settings ▸ Apps ▸ Installed apps
▸ copilot-ask ▸ Uninstall**. That removes the package but leaves the autostart
entry, the certificate and the installed folder behind, so the script is the
tidier route.

Either way your config is left alone, deliberately — it holds your API keys and
your prompt, and throwing those away is your call:

```powershell
Remove-Item "$env:APPDATA\copilot-ask" -Recurse -Force
```

Rotate any API key that was in it; deleting the file does not invalidate it.

Then delete the project folder. Nothing else is written anywhere: no Program
Files, no services, no scheduled tasks, no shell extensions.

## Design

Architecture, request shapes, and the Win32 details are in
[`docs/superpowers/specs/2026-09-14-copilot-ask-design.md`](docs/superpowers/specs/2026-09-14-copilot-ask-design.md).
