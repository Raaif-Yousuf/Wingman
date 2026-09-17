# copilot-ask — design spec

A Windows 11 tray utility. Press the Copilot key (or a configurable secondary
hotkey); it screenshots the active monitor, sends it to a vision LLM, and shows a
one-line verdict in a notification card. Click the card to expand the full working.

Target: single ~2 MB `copilot-ask.exe`, under 10 MB idle RSS, no runtime dependency.

## Threading model

One process, three kinds of thread:

- **Main thread** — owns the only Win32 message loop. Hosts the tray icon, the
  notification card window, and the `WH_KEYBOARD_LL` hook (a low-level hook
  requires a message loop on its installing thread, so it lives here).
- **Worker thread** — spawned per request. Captures, calls the provider chain,
  then hands the result back with `PostMessage(WM_APP_RESULT, 0, Box::into_raw(..))`.
- No async runtime. `ureq` is blocking by design; that is why it was chosen over
  `reqwest` + `tokio`.

All cross-thread delivery is `PostMessage` with a boxed payload. The worker never
touches a `HWND` beyond posting to it.

## Modules

| file | owns | must not |
|---|---|---|
| `src/config.rs` | `Config` struct, TOML load/save, defaults, path resolution | know about Win32 or HTTP |
| `src/capture.rs` | active-monitor screenshot to downscaled PNG bytes | know about providers |
| `src/provider/mod.rs` | `Answer`, `Provider` trait, `Chain` fallback | know about Win32 |
| `src/provider/openai.rs` | OpenAI Responses API impl | — |
| `src/provider/anthropic.rs` | Anthropic Messages API impl | — |
| `src/hotkey.rs` | `WH_KEYBOARD_LL` hook, chord matching, learn mode | know about providers |
| `src/ui/tray.rs` | `Shell_NotifyIconW` icon + context menu | know about providers |
| `src/ui/settings.rs` | the GUI settings window (modal) | persist anything itself |
| `src/dismiss.rs` | `WH_MOUSE_LL` click-anywhere-to-close watcher | know about the card |
| `src/single_instance.rs` | named-mutex guard; a duplicate exits | own any UI |
| `src/autostart.rs` | the `Run` key entry behind "Start with Windows" | mirror state into `Config` |
| `src/ui/card.rs` | notification card window (collapsed + expanded) | know about providers |
| `src/app.rs` | window proc, state machine, wiring | contain business logic |
| `src/main.rs` | entry point, `#![windows_subsystem = "windows"]` | — |

## Shared contracts

These types are the seams between modules. They are fixed, so modules can be
written against them independently.

```rust
// src/provider/mod.rs
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct Answer {
    /// <= 90 chars. Leads with the final value or the correction.
    pub headline: String,
    /// <= 700 chars of plain-text working. May be empty.
    pub detail: String,
}

#[derive(Debug)]
pub struct Shot {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;
    fn ask(&self, shot: &Shot, prompt: &str, want_difficulty: bool) -> anyhow::Result<Answer>;
}

/// Shown as a badge in the card's bottom-right corner. Pure data — the
/// green-to-red gradient lives in the card.
pub enum Difficulty {
    Level(u8), // 1..=10
    Ultra,     // "a professor would struggle"
}
```

### Difficulty

Off by config (`ui.show_difficulty`). When off the property is absent from the
schema and the rubric is absent from the prompt, so it costs nothing — the
toggle is not merely a rendering switch.

| value | anchor |
|---|---|
| 1 | easy high-school |
| 3 | easy university intro course |
| 5 | medium university |
| 7 | hard university |
| 9 | very hard for an undergraduate |
| 10 | a PhD student would struggle |
| U | a professor would struggle |

`difficulty` sits **after** `headline` in the schema, so the rating is formed
once the problem has actually been worked through. An unparseable value
degrades to no badge, never to a wrong badge.

```rust
// src/hotkey.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct Chord {
    pub vk: u32,      // virtual-key code of the trigger key
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub win: bool,
}
```

## Window messages

```rust
pub const WM_APP_TRAY:    u32 = WM_APP + 1; // tray icon callback
pub const WM_APP_HOTKEY:  u32 = WM_APP + 2; // hook fired; wparam = 1 primary, 2 secondary
pub const WM_APP_RESULT:  u32 = WM_APP + 3; // lparam = *mut Result<Answer, String>
pub const WM_APP_LEARNED: u32 = WM_APP + 4; // lparam = *mut Chord (learn mode captured a key)
```

## Config

`%APPDATA%\Wingman\config.toml`, created on first run with owner-only ACLs.
Renamed from `%APPDATA%\copilot-ask\config.toml` (issue #1); `Config::migrate_from`
copies the old file forward once, on first run, and leaves the old file alone.
Keys may also come from the `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` environment
variables, which take precedence over the file.

```toml
[hotkeys]
# Copilot key. Windows emits it as LeftWin + LeftShift + F23 (VK 0x86).
primary   = { vk = 0x86, ctrl = false, shift = true, alt = false, win = true }
# Secondary, always active alongside the primary.
secondary = { vk = 0xBF, ctrl = true, shift = true, alt = false, win = false }  # Ctrl+Shift+/

[capture]
max_edge = 1568        # long-edge downscale target
monitor  = "active"    # active | primary

[providers]
order = ["openai", "anthropic"]

[providers.openai]
model   = "gpt-5.5"
effort  = "low"
api_key = ""

[providers.anthropic]
model   = "claude-opus-5"
effort  = "low"
api_key = ""

[ui]
card_seconds = 12      # auto-dismiss for the collapsed card; 0 = never
prompt = "..."         # system prompt, editable
```

Both hotkeys are live simultaneously. Either one triggers the same flow. Neither
may be disabled by deleting it — a missing binding falls back to the default.

## Provider request shapes

The OpenAI shape below is **verified working** against the live API. The
Anthropic shape comes from the `claude-api` skill and is unverified until a key
is supplied.

### OpenAI — `POST https://api.openai.com/v1/responses`

```json
{
  "model": "gpt-5.5",
  "instructions": "<system prompt>",
  "input": [{"role": "user", "content": [
    {"type": "input_text",  "text": "<user prompt>"},
    {"type": "input_image", "image_url": "data:image/png;base64,...", "detail": "high"}
  ]}],
  "reasoning": {"effort": "low"},
  "max_output_tokens": 2500,
  "text": {"format": {
    "type": "json_schema", "name": "answer", "strict": true,
    "schema": {"type": "object",
      "properties": {"headline": {"type": "string"}, "detail": {"type": "string"}},
      "required": ["headline", "detail"], "additionalProperties": false}}}
}
```

Parse: concatenate `output[].content[].text` where `output[].type == "message"`,
then `serde_json::from_str::<Answer>`. Note that reasoning models emit a
`reasoning` entry in `output[]` first — skipping non-`message` entries is required,
not optional.

### Anthropic — `POST https://api.anthropic.com/v1/messages`

Headers: `x-api-key`, `anthropic-version: 2023-06-01`, `content-type: application/json`.

```json
{
  "model": "claude-opus-5",
  "max_tokens": 4000,
  "system": "<system prompt>",
  "output_config": {
    "effort": "low",
    "format": {"type": "json_schema",
      "schema": {"type": "object",
        "properties": {"headline": {"type": "string"}, "detail": {"type": "string"}},
        "required": ["headline", "detail"], "additionalProperties": false}}},
  "messages": [{"role": "user", "content": [
    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "<b64>"}},
    {"type": "text", "text": "<user prompt>"}
  ]}]
}
```

Details that matter: it is `output_config.format`, **not** the deprecated
`output_format`. Anthropic's `format` object takes `schema` directly — no `name`
and no `strict`; those are the OpenAI spelling. Thinking is on by default on
Opus 5, so do not send `budget_tokens` (400) and do not prefill the assistant
turn (400). Check `stop_reason == "refusal"` before reading `content`.
Parse: first `content[]` block with `type == "text"`, then `from_str::<Answer>`.

### Chain semantics

Try providers in `order`. Fall through to the next on transport error, non-2xx,
or unparseable body. If every provider fails, surface the **first** error. A
provider with an empty key is skipped, not failed — so the app works with only
one key configured.

## Capture

1. Enumerate monitors. "Active" is the monitor containing the foreground window
   (`GetForegroundWindow` then `MonitorFromWindow(MONITOR_DEFAULTTONEAREST)`);
   fall back to the primary monitor when there is no foreground window.
2. Grab RGBA, downscale so the long edge is `max_edge` (Lanczos3), skipping the
   resize when it is already smaller.
3. Encode PNG.

The app's own card window is hidden before capture, or it lands in the screenshot.

## UI: the notification card

One layered, borderless, always-on-top, non-activating window
(`WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE`, `WS_POPUP`), rendering
itself with GDI. `WS_EX_TOOLWINDOW` keeps it off the taskbar and out of Alt-Tab.

Three states:

- **Pending** — small card, bottom-right, above the taskbar, showing "Thinking…".
  It appears immediately on hotkey, so there is feedback during the 3-8 s wait.
- **Collapsed** — the headline, word-wrapped, at most 3 lines. A hint line reads
  "click for working" when `detail` is non-empty. Auto-dismisses after
  `card_seconds`.
- **Expanded** — grows in place (anchored bottom-right) to fit the detail text,
  up to 60% of work-area height, then scrolls. Takes focus on expand so that
  `WM_KILLFOCUS` can close it. Esc also closes.

DPI: per-monitor-v2 aware (`SetProcessDpiAwarenessContext`); every metric scales
from the card's current monitor DPI. Fonts come from
`SystemParametersInfo(SPI_GETNONCLIENTMETRICS)` so the card matches the shell.

Colors follow the system light/dark setting, read at startup from
`HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize\AppsUseLightTheme`.

## UI: tray

`Shell_NotifyIconW` with `NIM_ADD`. The icon loads from an embedded resource and
falls back to `IDI_APPLICATION`. Right-click opens a `TrackPopupMenu`:

- **Ask now** — trigger the flow without the hotkey (left-click on the icon
  opens **Settings** instead: the icon is easy to hit by accident, and firing a
  paid API call on a stray click is worse than opening a window)
- **Provider ▸** — which service answers; reorders `providers.order` rather
  than dropping the other, so the fallback survives a switch
- **Copy last answer** — headline + detail to the clipboard via `arboard`
- separator
- **Set Copilot key…** — learn mode for the primary binding
- **Set secondary key…** — learn mode for the secondary binding
- **Edit settings** — `ShellExecuteW("open", config.toml)`
- **Reload settings** — re-read the TOML without restarting
- separator
- **Quit**

Left-click is **Ask now**. The tooltip shows the active provider.

## Hotkeys

`SetWindowsHookExW(WH_KEYBOARD_LL, ...)`. On each `WM_KEYDOWN` / `WM_SYSKEYDOWN`,
build the current `Chord` from the event's vk plus `GetAsyncKeyState` for the
modifiers, then compare against both bindings.

On a match: `PostMessage(hwnd, WM_APP_HOTKEY, which, 0)` and **return 1** to
swallow the key, so Copilot or Search does not also open.

**The Win-key release problem.** Swallowing F23 leaves `LWin` logically down; its
eventual keyup with nothing in between opens the Start menu. Mitigation: on a
primary match, `SendInput` a `VK_CONTROL` down+up pair before returning 1. A Ctrl
tap cancels the pending Start-menu activation and is otherwise inert. This is the
known-fragile spot — if Start still flickers, the fallback is to also swallow the
`LWin` keyup that immediately follows a consumed F23.

**Learn mode.** A tray menu item arms it. The next keydown is captured as a
`Chord` rather than matched, posted as `WM_APP_LEARNED`, swallowed, and written to
the config. This is what makes the app work even if Dell's firmware emits
something other than the documented combo. Learn mode times out after 10 s. A bare
modifier keydown (Shift, Ctrl, Alt or Win alone) is ignored while learning, so the
chord captures the real trigger key rather than the modifier that preceded it.

## Error handling

Every failure path ends in a card — never a silent no-op, never a dialog box:

| failure | headline shown |
|---|---|
| no API key configured | `No API key — open Edit settings` |
| capture failed | `Couldn't capture the screen` |
| all providers failed | `<provider>: <first error, truncated>` |
| model returned unparseable JSON | `Bad response from <provider>` |

The detail pane carries the full error text so it can be copied. Nothing panics on
the main thread; the worker catches everything and posts a `Result`.

## Testing

Pure logic is unit-tested; Win32 is exercised manually.

- `config.rs` — round-trip TOML, defaults, env-var precedence, malformed file
- `provider/*` — request-body construction and response parsing against recorded
  fixtures in `tests/fixtures/`; no network in tests
- `provider::Chain` — fallback order, skip-on-empty-key, first-error propagation
- `capture.rs` — the downscale math (`fit_long_edge`) as a pure function
- `hotkey.rs` — `Chord` matching and the learn-mode state machine as pure
  functions, isolated from the hook callback

Manual checklist: tray appears; both hotkeys fire; the Start menu does not flash;
the card renders in light and dark; click expands; focus loss dismisses; learn mode
rebinds; provider fallback works with a deliberately bad OpenAI key.

## Out of scope

Conversation history, follow-up questions, OCR preprocessing, auto-start
(documented as a manual Startup-folder shortcut), packaging and installer, a
settings GUI (the TOML is the settings UI), streaming responses.
