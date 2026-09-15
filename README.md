# copilot-ask

A tray app for the Dell XPS 14. Press the Copilot key, and whatever is on your
screen gets checked by a vision model. The verdict arrives as a notification card
in the corner; click it for the full working.

Built for one job: verifying a hand-worked physics or statistics problem before
you commit the numbers.

```
┌──────────────────────────────────┐
│ ✗ 1.35 m/s² up the slope —       │
│   you used cos 30°, not sin 30°  │
│                click for working │
└──────────────────────────────────┘
```

## Build

```powershell
cargo build --release
# target\release\copilot-ask.exe
```

No runtime dependency — the `.exe` is standalone.

## First run

Launch it. A tray icon appears and `%APPDATA%\copilot-ask\config.toml` is created.
Open it from the tray (**Edit settings**) and paste your API key:

```toml
[providers.openai]
api_key = "sk-proj-..."
```

`OPENAI_API_KEY` / `ANTHROPIC_API_KEY` in the environment override the file, so
you can keep keys out of the config entirely if you prefer.

Configure one provider or both. With both set, Anthropic is the automatic
fallback when OpenAI fails — a provider with an empty key is skipped, not treated
as an error.

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

- **Ask now** — trigger without the hotkey (left-click does this too)
- **Copy last answer** — headline and working to the clipboard
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
| `ui.prompt` | see file | the system prompt — edit it to change what it checks |

Roughly 1-2 cents and 3-8 seconds per check at the defaults.

## Start with Windows

There is no installer. Drop a shortcut in the Startup folder:

```powershell
$s = (New-Object -ComObject WScript.Shell).CreateShortcut(
  "$env:APPDATA\Microsoft\Windows\Start Menu\Programs\Startup\copilot-ask.lnk")
$s.TargetPath = "$PWD\target\release\copilot-ask.exe"
$s.Save()
```

## Design

Architecture, request shapes, and the Win32 details are in
[`docs/superpowers/specs/2026-09-14-copilot-ask-design.md`](docs/superpowers/specs/2026-09-14-copilot-ask-design.md).
