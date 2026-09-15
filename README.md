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

## Build

```powershell
cargo build --release
# target\release\copilot-ask.exe
```

No runtime dependency — the `.exe` is standalone.

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

Tick **Start with Windows** in Settings. It writes a per-user entry to
`HKCU\Software\Microsoft\Windows\CurrentVersion\Run` — no admin prompt, and
it shows up in Task Manager's Startup tab like any other startup app. Untick to
remove it.

The checkbox reads the registry when Settings opens, so it always reflects
reality rather than a mirrored setting that can drift. If you move or rebuild
the exe somewhere else, the entry is repaired to the new path on next launch.

## Uninstall

There is no installer, so there is nothing in Add/Remove Programs. Three things
to remove:

```powershell
# 1. stop it (or use Quit in the tray menu)
Get-Process copilot-ask -ErrorAction SilentlyContinue | Stop-Process -Force

# 2. remove the autostart entry, if you ticked Start with Windows
Remove-ItemProperty "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" `
  -Name "copilot-ask" -ErrorAction SilentlyContinue

# 3. delete the config -- this holds your API keys
Remove-Item "$env:APPDATA\copilot-ask" -Recurse -Force
```

Then delete the project folder itself. Nothing else is written anywhere: no
Program Files, no services, no scheduled tasks, no shell extensions.

Rotate any API key that was in the config, since deleting the file does not
invalidate it.

## Design

Architecture, request shapes, and the Win32 details are in
[`docs/superpowers/specs/2026-09-14-copilot-ask-design.md`](docs/superpowers/specs/2026-09-14-copilot-ask-design.md).
