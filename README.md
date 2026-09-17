# Wingman

<p align="center">
  <img src="assets/brand/wingman-mark.png" alt="Wingman logo" width="120">
</p>

**It's the Copilot key. It's actually yours now.**

Wingman is a native Windows 11 tray app that lives behind the Copilot key.
Press it, Wingman looks at your screen and hands back one thing: a verdict, a
draft, a filled form, an answer. One press, one action, one card. There is no
chat window, no conversation, no follow-up question: if you want a back and
forth, ChatGPT and Ollama's own app already do that well.

> **Phase 0, in progress.** The crate, exe and config folder are being
> renamed from `copilot-ask` to `wingman` (tracked in issue #1). The commands
> and paths in this README are the ones that work in this checkout today,
> which still say `copilot-ask`; once the rename lands they become `wingman`
> and this file gets updated in the same commit. Everywhere else, the product
> is called Wingman.

## Why not Copilot or Recall?

Sourced from [`docs/positioning.md`](docs/positioning.md); every claim below
about Wingman is checked against the code in this repo, not aspirational.

| | Wingman | Copilot on Windows |
|---|---|---|
| **Footprint** | A tray icon, about 2 MB, 0% CPU until you press the key. No taskbar button, no notifications, no sign-in nags | A taskbar surface, an account wall, a WebView2 app, periodic prompts |
| **Verifiable** | Offline mode refuses any non-loopback network call in code, enforced before a socket ever opens (see [`docs/offline.md`](docs/offline.md)). Open source under MIT: read the code yourself | Copilot Vision sends the screen to Microsoft's servers. Windows Recall shipped plaintext screenshots and had to be pulled twice before relaunching opt-in |
| **Extensible** (planned) | An action will be a small, readable definition: a prompt, an input list and an executor. The framework is the product, actions are the contribution surface | Closed |
| **Never the final button** | Fills, drafts and proposes. Never presses Send, Submit, Buy or Pay: a permanent rule, not a version-1 limit to be relaxed later | Copilot Actions and other "agent" products sell autonomy |
| **Not a chatbot** | One press, one action, one card, done. No conversation view, no follow-up question | Copilot is a chat pane first, everything else second |
| **Bring your own model** | OpenAI, Anthropic, Gemini or a local Ollama model today; any OpenAI-compatible endpoint is planned. No account or subscription required for the app itself | Recall and Click to Do require a 40+ TOPS NPU; the strongest models sit behind Copilot Pro |

## What works today

One action, built and running: **Check my work**. Press the key, Wingman
screenshots your active monitor, sends it to a vision model with a prompt
tuned for checking a hand-worked physics or statistics problem, and shows the
verdict as a small GDI card in the corner, with a 1-10 (or U) difficulty
rating. Click the card for the full working; click anywhere else and it goes
away.

That is the whole app today: one hotkey, one screenshot, one cloud model call
(OpenAI or Anthropic, your key), one read-only card. No local models, no
confirm-and-execute loop, no other actions yet.

## Where it's going

The [expansion plan](docs/superpowers/specs/2026-09-16-expansion-plan-design.md)
is the detailed roadmap; the short version is that every future action
follows the same four-step loop, and none of steps 2 through 4 exist in code
yet:

```
   LOOK          PROPOSE              CONFIRM            DO
 screenshot ──▶ model returns a ──▶ preview card, ──▶ executor runs it,
 selection      typed proposal      Enter / Esc        result card
 clipboard      (calendar entry,    (edit fields)      (undo where
 context        form values, ...)                       possible)
```

Read-only actions (like today's Check my work) skip Confirm and Do and just
show the card. Anything that would write to your screen or a connected
service always waits for your Enter or click first, and Wingman never
presses these four buttons for you:

**Send. Submit. Buy. Pay.**

It fills, drafts and proposes; you finish. That rule is permanent, not a
version-1 limitation to be relaxed later.

Planned and not yet built: local models via Ollama, a Cloud/Local/Auto/Offline
mode switch, the actions-as-data framework, the confirm-and-execute loop,
executors (form fill, calendar add, text replace), connectors, and awareness.
Track them in [GitHub Issues](https://github.com/Raaif-Yousuf/Wingman/issues).

## Install

```powershell
.\install.ps1
```

Builds it, puts the `.exe` in `%LOCALAPPDATA%\Programs\copilot-ask`, registers
it with Windows, and switches on start-with-Windows. Re-run it to upgrade in
place. One UAC prompt the first time, to trust the certificate it signs with;
none after that.

Registering is what puts the app in **Start ▸ All apps**, in
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

The `.exe` is standalone and runs from anywhere with no runtime dependency.
You lose the app lists and the Copilot key picker, and start-with-Windows only
if you tick it yourself in Settings.

## First run

Launch it. A tray icon appears and `%APPDATA%\copilot-ask\config.toml` is
created. Left-click the tray icon to open **Settings**, and paste your key
into **OpenAI API key** or **Anthropic API key**, both in the Providers group
at the top. Everything else lives there too: model, effort, capture size,
card timeout, text size, the difficulty toggle and the prompt.

There is no local-model or offline option yet: every request today goes to
whichever cloud provider you configure.

The config file is still there (`Open config.toml` in the tray menu) if you
want to edit it by hand.

`OPENAI_API_KEY` / `ANTHROPIC_API_KEY` in the environment override the file,
so you can keep keys out of the config entirely if you prefer.

Configure one provider or both. With both set, Anthropic is the automatic
fallback when OpenAI fails: a provider with an empty key is skipped, not
treated as an error.

Switch models from the tray at any time; the choice is written straight back
to the config. To offer a model that isn't listed, add it to `models` under
the relevant provider; no rebuild needed.

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

Turn it off with **Show difficulty rating** in Settings. Off means the rating
is never requested, not merely hidden, so it costs nothing.

## Hotkeys

Two bindings, both live at once:

| | default | notes |
|---|---|---|
| primary | the Copilot key | Windows emits it as `Win+Shift+F23` |
| secondary | `Ctrl+Shift+/` | works on any keyboard |

If the Copilot key does nothing, use **Set Copilot key...** in the tray menu
and press it once: whatever your firmware actually emits gets captured and
saved. Same for the secondary binding.

### Through Windows Settings instead

Once installed, Wingman can also be made the Copilot key's target the
official way:

**Settings ▸ Bluetooth & devices ▸ Keyboard ▸ Customize Copilot key on
keyboard ▸ Custom ▸ copilot-ask**

(That entry still says `copilot-ask` in the picker until the Phase 0 rename
lands; it is the same app.) You have to click that yourself. Windows protects
the setting so no app can make itself the target, which is the right call,
and means an installer cannot do it for you however much it would like to.

Both routes work at once and do the same thing. The difference is that this
one is Windows launching the app, so it works regardless of what the keyboard
hook sees; the hook is still what serves the secondary binding.

One gap worth knowing: if Wingman is **not already running**, a key press
starts it and stops there rather than asking. That is deliberate: the same
bare launch is what runs at login, and asking on it would mean a billed API
call every boot. With start-with-Windows on it is always running anyway.

## Tray menu

- **Ask now**: trigger without the hotkey
- **Settings...**: the settings window (left-clicking the tray icon opens
  this too)
- **Copy last answer**: headline and working to the clipboard
- **Provider ▸**: which service answers (ChatGPT or Claude)
- **ChatGPT model ▸** / **Claude model ▸**: switch models, saved immediately
- **Set Copilot key...** / **Set secondary key...**: rebind by pressing the
  key
- **Edit settings**: opens `config.toml`
- **Reload settings**: re-read the file without restarting
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
| `ui.prompt` | see file | the system prompt, edit it to change what it checks |

Roughly 1-2 cents and 3-8 seconds per check at the defaults.

## Start with Windows

`install.ps1` turns this on for you. Otherwise, tick **Start with Windows** in
Settings; same entry, same effect. It writes a per-user entry to
`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, no admin prompt, and it
shows up in Task Manager's Startup tab like any other startup app. Untick to
remove it.

The checkbox reads the registry when Settings opens, so it always reflects
reality rather than a mirrored setting that can drift. If you move or rebuild
the exe somewhere else, the entry is repaired to the new path on next launch.

## Only one instance

Launching it again while it is already running does not start a second copy:
the new process hands over to the running one, which opens its Settings
window, then exits. Without that, two instances mean two tray icons, two
keyboard hooks and **two billed API calls per keypress**, with nothing in the
UI to hint at it.

If you kill it with Task Manager rather than **Quit**, the tray icon can
linger as a ghost until you mouse over it: the process never got to remove
it.

## Uninstall

```powershell
.\uninstall.ps1
```

Stops it, unregisters the package, clears the autostart entry, hands the
Copilot key back to Search if it was pointed here, deletes
`%LOCALAPPDATA%\Programs\copilot-ask`, and removes the signing certificate
from both stores. One UAC prompt, for that last part; `-KeepCertificate`
skips it, which is what you want if you are about to reinstall.

Since it is a registered app you can also use **Settings ▸ Apps ▸ Installed
apps ▸ copilot-ask ▸ Uninstall**. That removes the package but leaves the
autostart entry, the certificate and the installed folder behind, so the
script is the tidier route.

Either way your config is left alone, deliberately: it holds your API keys
and your prompt, and throwing those away is your call:

```powershell
Remove-Item "$env:APPDATA\copilot-ask" -Recurse -Force
```

Rotate any API key that was in it; deleting the file does not invalidate it.

Then delete the project folder. Nothing else is written anywhere: no Program
Files, no services, no scheduled tasks, no shell extensions.

## Promises

What Wingman commits to, and how each promise is enforced or verifiable
today: [`PROMISES.md`](PROMISES.md).

## Privacy and security

What leaves the machine, when, and how to wipe local state is in
[`PRIVACY.md`](PRIVACY.md). To report a security issue, see
[`SECURITY.md`](SECURITY.md).

## Contributing

Wingman is meant to grow by community-contributed actions: a prompt, an
input list and (usually) an existing executor. See
[`CONTRIBUTING.md`](CONTRIBUTING.md) for the "add an action" walkthrough, and
[`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md) for how we expect people to treat
each other here.

## Design

Architecture, request shapes and the Win32 details of what is built today are
in
[`docs/superpowers/specs/2026-09-14-copilot-ask-design.md`](docs/superpowers/specs/2026-09-14-copilot-ask-design.md).
Packaging and install internals are in the
[packaging spec](docs/superpowers/specs/2026-09-15-packaging-and-install-design.md).
What comes next, in full, is the
[expansion plan](docs/superpowers/specs/2026-09-16-expansion-plan-design.md).
Per-provider request shapes, keys and retry behaviour are in
[`docs/providers.md`](docs/providers.md); the four modes and the Offline
guard are in [`docs/offline.md`](docs/offline.md).

## License

MIT, see [`LICENSE`](LICENSE). Third-party dependencies and their licenses are
listed in [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md).
