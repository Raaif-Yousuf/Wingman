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

![Wingman's card in the corner of the screen, answering a physics problem](docs/screenshots/card-on-screen.png)

One press on the problem above, and the card in the corner says 5.94 m/s. Click
it and it [shows the working](docs/screenshots/card-expanded.png) it used to get
there. That run went to a local Ollama model, so the screenshot never left the
machine.

## Why not Copilot or Recall?

Sourced from [`docs/positioning.md`](docs/positioning.md); every claim below
about Wingman is checked against the code in this repo, not aspirational.

| | Wingman | Copilot on Windows |
|---|---|---|
| **Footprint** | A tray icon, about 2 MB, 0% CPU until you press the key. No taskbar button, no notifications, no sign-in nags | A taskbar surface, an account wall, a WebView2 app, periodic prompts |
| **Verifiable** | Offline mode refuses any non-loopback network call in code, enforced before a socket ever opens (see [`docs/offline.md`](docs/offline.md)). Open source under MIT: read the code yourself | Copilot Vision sends the screen to Microsoft's servers. Windows Recall shipped plaintext screenshots and had to be pulled twice before relaunching opt-in |
| **Extensible** | An action is a small, readable definition: a prompt, an input list and an executor. Five built-in actions ship today (check my work, add event from screen, review this email, fill this form, copy text from screen); a hand-written `actions.toml` entry parses and shows in the palette but does not yet dispatch on confirm (issue #242). The framework is the product, actions are the contribution surface | Closed |
| **Never the final button** | Fills, drafts and proposes. Never presses Send, Submit, Buy or Pay: a permanent rule, not a version-1 limit to be relaxed later | Copilot Actions and other "agent" products sell autonomy |
| **Not a chatbot** | One press, one action, one card, done. No conversation view, no follow-up question | Copilot is a chat pane first, everything else second |
| **Bring your own model** | OpenAI, Anthropic, Gemini, a local Ollama model, or any OpenAI-compatible endpoint (OpenRouter, Groq, LM Studio, llama.cpp, vLLM, ...) today. No account or subscription required for the app itself | Recall and Click to Do require a 40+ TOPS NPU; the strongest models sit behind Copilot Pro |

## What works today

Five built-in actions, built and running, all reachable from the tray menu:

- **Check my work**: press the key, Wingman screenshots your active
  monitor, sends it to a vision model with a prompt tuned for checking a
  hand-worked physics or statistics problem, and shows the verdict as a
  small GDI card in the corner. Click the card for the full working; click
  anywhere else and it goes away. Read-only: it skips Confirm and Do.
- **Add event from screen**: screenshots, proposes a calendar entry, and on
  confirm writes a real `.ics` file via the ics connector.
- **Review this email**: screenshots, proposes edited text, and on confirm
  replaces the selected text via the `replace_text` executor.
- **Fill this form**: screenshots, proposes field values, and on confirm
  fills the form via the `fill_form` executor (with **Restore last form**
  to undo). Goes through the same Look, Propose, Confirm, Do loop as the
  others: `App::on_preview_decided` reads the card's confirmed proposal and
  calls `confirm::confirm(...)` before the executor runs.
- **Copy text from screen**: OCRs the active monitor and copies the text to
  the clipboard.

The confirm-and-execute loop, the executors (calendar add, form fill, text
replace, image-to-clipboard) and the actions framework itself
(`actions.toml`, five built-in actions loadable today) are built and wired,
not planned. What has landed alongside those actions:

- **Four providers plus any OpenAI-compatible endpoint**: OpenAI, Anthropic
  and Gemini (cloud, your key), Ollama (local, your own machine), and any
  OpenAI-compatible server (OpenRouter, Groq, LM Studio, llama.cpp, vLLM,
  ...). See [`docs/providers.md`](docs/providers.md) for request shapes and
  how to add Ollama, Gemini or a compat endpoint to the fallback order today
  (Settings only exposes OpenAI/Anthropic so far).
- **Modes**: Cloud, Local, Auto (default) or Offline, from the tray's
  **Mode** submenu. Offline refuses any non-loopback network call in code;
  see [`docs/offline.md`](docs/offline.md).
- **Pause**: 1 hour, until tomorrow, or until resumed, from the tray's
  **Pause** submenu. While paused the hotkey passes through untouched, no
  capture runs, no request can start.
- **Keys in Windows Credential Manager**, not in `config.toml`: a saved key
  never touches disk in plaintext. See [`docs/providers.md`](docs/providers.md#where-a-key-lives).
- **Retry with backoff**: a transport error or a 5xx retries automatically;
  a 429 retries once against the server's own `retry-after`, or shows a
  card naming the delay.

## Where it's going

The [expansion plan](docs/superpowers/specs/2026-09-16-expansion-plan-design.md)
is the detailed roadmap; the short version is that every action follows the
same four-step loop:

```
   LOOK          PROPOSE              CONFIRM            DO
 screenshot ──▶ model returns a ──▶ preview card, ──▶ executor runs it,
 selection      typed proposal      Enter / Esc        result card
 clipboard      (calendar entry,    (edit fields)      (undo where
 context        form values, ...)                       possible)
```

Read-only actions (like Check my work) skip Confirm and Do and just show the
card. Anything that would write to your screen or a connected service always
waits for your Enter or click first, and Wingman never presses these four
buttons for you:

**Send. Submit. Buy. Pay.**

It fills, drafts and proposes; you finish. That rule is permanent, not a
version-1 limitation to be relaxed later.

Planned and not yet built: dispatch for an arbitrary hand-written
`actions.toml` entry beyond the five built-in ids (issue #242), more
connectors beyond ics, and awareness. The actions-as-data framework, the
confirm-and-execute loop, the built executors and the ics connector are
built; see "What works today" above. Track what remains in
[GitHub Issues](https://github.com/Raaif-Yousuf/Wingman/issues).

## Install

```powershell
.\install.ps1
```

Builds it, puts the `.exe` in `%LOCALAPPDATA%\Programs\Wingman`, registers
it with Windows, and switches on start-with-Windows. Re-run it to upgrade in
place. One UAC prompt the first time, to trust the certificate it signs with;
none after that.

Registering is what puts the app in **Start ▸ All apps**, in
**Settings ▸ Apps ▸ Installed apps**, and in the Copilot key picker described
below. Windows lists only packaged, signed apps in that picker, which is the
whole reason the install does more than copy a file.

`-SkipBuild` uses the existing `target\release\wingman.exe` instead of
running cargo; `-NoAutostart` installs without the login entry.

Nothing in install, upgrade or uninstall reads, writes or deletes your
`config.toml`, so your keys and prompt survive all three.

### Or don't install it

```powershell
cargo build --release
# target\release\wingman.exe
```

The `.exe` is standalone and runs from anywhere with no runtime dependency.
You lose the app lists and the Copilot key picker, and start-with-Windows only
if you tick it yourself in Settings.

## First run

Launch it. A tray icon appears and `%APPDATA%\Wingman\config.toml` is
created. Left-click the tray icon to open **Settings**, and paste your key
into **OpenAI API key** or **Anthropic API key**, both in the Providers group
at the top. Everything else lives there too: model, effort, capture size,
card timeout, text size and the prompt. A saved key moves into Windows
Credential Manager, not the file; Settings shows only its last four
characters afterward.

Settings exposes OpenAI and Anthropic today; Gemini and Ollama both work but
need one hand-edit of `config.toml` to enable, since the fixed-layout
Settings dialog does not have a field group for either yet. See
[`docs/providers.md`](docs/providers.md#how-to-add-ollama-or-gemini-to-providersorder-by-hand-today)
for the exact steps and the limitation to know about (a Settings save can
discard the hand-edit).

Local (Ollama) and Offline are both real options now, via the tray's
**Mode** submenu: Cloud, Local, Auto (default: local first if Ollama is up,
otherwise cloud) or Offline (loopback only, enforced in code). See
[`docs/offline.md`](docs/offline.md) for exactly what each mode does.

The config file is still there (`Open config.toml` in the tray menu) if you
want to edit it by hand.

`OPENAI_API_KEY` / `ANTHROPIC_API_KEY` / `GEMINI_API_KEY` in the environment
override the file, so you can keep keys out of the config entirely if you
prefer.

Configure one cloud provider or several. With more than one set, the rest
are the automatic fallback in `providers.order` when the first fails: a
provider with an empty key is skipped, not treated as an error.

Switch models from the tray at any time; the choice is written straight back
to the config. To offer a model that isn't listed, add it to `models` under
the relevant provider; no rebuild needed.

## Hotkeys

Two bindings, both live at once:

| | default | notes |
|---|---|---|
| primary | the Copilot key | Windows emits it as `Win+Shift+F23` |
| secondary | `Ctrl+Shift+/` | works on any keyboard |

If the Copilot key does nothing, use **Set Copilot key...** in the tray menu
and press it once: whatever your firmware actually emits gets captured and
saved. Same for the secondary binding.

A third, optional chord toggles Pause: running -> paused until resumed;
paused, for any reason -> resume. Unlike primary/secondary it has no default
binding and no tray "Set..." entry yet, so set it by hand in `config.toml`:

```toml
[hotkeys.pause]
vk = 0x13    # VK_PAUSE
ctrl = false
shift = false
alt = false
win = false
```

then **Reload settings** (or restart) to pick it up. It is the one chord
that still works while paused.

### Through Windows Settings instead

Once installed, Wingman can also be made the Copilot key's target the
official way:

**Settings ▸ Bluetooth & devices ▸ Keyboard ▸ Customize Copilot key on
keyboard ▸ Custom ▸ Wingman**

You have to click that yourself. Windows protects
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
- **Mode ▸**: Cloud / Local / Auto / Offline, radio-checked
- **Pause ▸**: 1 hour / until tomorrow / until resumed; shown as **Resume**
  instead while already paused
- **Set Copilot key...** / **Set secondary key...**: rebind by pressing the
  key
- **Edit settings**: opens `config.toml`
- **Reload settings**: re-read the file without restarting
- **Quit**

## Settings worth knowing

| key | default | what it does |
|---|---|---|
| `mode` | `"auto"` | `cloud` / `local` / `auto` / `offline`; see [`docs/offline.md`](docs/offline.md) |
| `capture.max_edge` | `1568` | long-edge downscale before upload; lower cuts cost |
| `capture.monitor` | `"active"` | `"active"` follows the focused window; `"primary"` pins it |
| `providers.order` | `["openai", "anthropic"]` | fallback order; add `"ollama"`/`"gemini"` by hand, see [`docs/providers.md`](docs/providers.md) |
| `providers.*.effort` | `"low"` | reasoning depth; raise for harder problems |
| `providers.ollama.base_url` | `"http://127.0.0.1:11434"` | never `localhost`, see [`docs/providers.md`](docs/providers.md#ollama) |
| `ui.card_seconds` | `12` | auto-dismiss for the collapsed card; `0` = never |
| `ui.text_scale` | `1.0` | multiplies the card's font size; lower is smaller |
| `providers.*.models` | see file | what the tray's model submenu offers (Ollama has no list; it uses `providers.ollama.model` directly) |
| `ui.prompt` | see file | the system prompt, edit it to change what it checks |
| `hotkeys.pause` | unset (absent from the file) | optional third chord: toggles Pause until resumed / Resume; no default, no tray learn button yet, see [Hotkeys](#hotkeys) |

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
`%LOCALAPPDATA%\Programs\Wingman`, and removes the signing certificate
from both stores. One UAC prompt, for that last part; `-KeepCertificate`
skips it, which is what you want if you are about to reinstall. It also
cleans up a pre-rename `copilot-ask` install left over from before the
2026-09-16 rename, if one is still on the machine.

Since it is a registered app you can also use **Settings ▸ Apps ▸ Installed
apps ▸ Wingman ▸ Uninstall**. That removes the package but leaves the
autostart entry, the certificate and the installed folder behind, so the
script is the tidier route.

Either way your config is left alone, deliberately: it holds your API keys
and your prompt, and throwing those away is your call:

```powershell
Remove-Item "$env:APPDATA\Wingman" -Recurse -Force
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
