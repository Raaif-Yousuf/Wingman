# Wingman — expansion plan

Status: **planning, revised 2026-09-16 after owner decisions.** Nothing here is
implemented. The two existing specs
([running app](2026-09-14-copilot-ask-design.md),
[packaging](2026-09-15-packaging-and-install-design.md)) stay authoritative for
what is built today. This document says what changes and what is added, and
its § 17 is the GitHub issue map.

Owner decisions, 2026-09-16:

| decision | answer |
|---|---|
| name | **Wingman**; repo `github.com/Raaif-Yousuf/Wingman` |
| chat | **none.** One press, one action, one card. No conversations, no follow-ups, no threads. Anyone who wants a chat has ChatGPT or Ollama's own app. Owner decision, late 2026-09-16 |
| settings window | WebView2 via `wry`, opened rarely: settings, actions, memory, connections, profile, usage, egress log |
| API keys | Windows Credential Manager |
| awareness default | **on**, with a first-run consent screen (owner overrode the "off" recommendation) |
| copied code | MIT only; dependencies permissive |
| scope | **not a homework tool.** A universal assistant that acts on what is on screen: fill the order form, review the email before it goes, put the event on the calendar. Built one action at a time |
| ambition | a community project; the framework is the product and actions are the contribution surface |

---

## 1. What we are building

Today: press the Copilot key, a vision model checks the physics problem on
screen, a card shows the verdict. One job, done well, ~2 MB, no runtime.

Target: **the key you press when you want something on your screen dealt
with.** You are on a checkout page: it fills your details and hands back
before "Place order". You are about to send an email: it says "good to go" or
shows the three fixes and applies them on a click. An event is on screen: it
proposes a calendar entry, you press Enter, it is on your calendar. A problem
is on screen: it checks your answer. An error dialog: it explains it. Nothing
on screen fits: type one line in the palette and get one card.

**Wingman is not a chatbot.** One press, one action, one card, done. There
is no conversation view, no history of exchanges, no "ask a follow-up". If a
task needs a back-and-forth, it is not a Wingman task; open ChatGPT or
Ollama's own app for that. This is the design, not a gap: it keeps the tool
light, keeps every interaction under a few seconds, and keeps Wingman out of
the crowded field of local chat wrappers.

The loop is always the same four steps, and that is what makes it universal:

```
   LOOK          PROPOSE              CONFIRM            DO
 screenshot ──▶ model returns a ──▶ preview card, ──▶ executor runs it,
 selection      typed proposal      Enter / Esc        result card
 clipboard      (calendar entry,    (edit fields)      (undo where
 UIA tree       form values,                           possible)
 context        text edits...)
```

Read-only actions (review, explain, check) skip CONFIRM and show the result.
Anything that writes to your screen, your files or a connected service always
goes through CONFIRM. Wingman never presses Send, Submit, Buy or Pay itself.

Guiding constraints, carried over and extended:

| principle | consequence |
|---|---|
| **Instant** | Feedback within 100 ms of the key press. The palette is a pre-created window. No webview on the hot path. |
| **One shot** | One press, one action, one card. No chat, no threads, no follow-ups. The answer fits on a card (the expanded card scrolls) or it is not a Wingman task. |
| **Native** | One Rust `.exe`, Win32 for tray, hook, card, palette, capture, UI Automation. WebView2 (a Windows component) only for the rarely opened settings window. No Electron, no Node. |
| **Cheap to idle** | Under 15 MB RSS in the tray, zero CPU, never raises the timer resolution, no polling. Everything event-driven. |
| **Acts only with consent** | Every write goes through a preview. The user's click or Enter is the authorization, every time. No "auto mode" for writes. |
| **Private by default** | Nothing leaves the machine except the request the user just confirmed, to the provider they configured. Awareness data is local-only, encrypted, expiring. |
| **Offline-capable** | Offline mode refuses non-loopback hosts in code. Local models plus local executors still work; connectors do not. |
| **Bring your own model** | OpenAI, Anthropic, Gemini, any OpenAI-compatible endpoint, Ollama. The same action runs on all of them. |
| **Open source, MIT** | Copied code MIT-only and attributed; dependencies permissive; `cargo deny` in CI. |
| **Secrets never in the repo** | Keys and OAuth tokens in Credential Manager. `config.toml` is safe to share. |

---

## 2. Name: Wingman

Decided. The Copilot key now runs your wingman. Executable `wingman.exe`,
config `%APPDATA%\Wingman\`, package identity `RaaifYousuf.Wingman`, AUMID
`RaaifYousuf.Wingman_<hash>!Wingman`.

### Rename mechanics

Every binding of the current name, from `git grep -i 'copilot-ask\|copilot_ask\|CopilotAsk'`:

| where | today | change |
|---|---|---|
| `Cargo.toml` package and bin | `copilot-ask` | `wingman` |
| window classes, mutex (`app.rs`, `card.rs`, `settings.rs`, `single_instance.rs`) | `CopilotAsk.*.<guid>` | `Wingman.*.<same guid>`; the GUID suffixes stay so old and new can coexist during migration |
| `config.rs` path | `%APPDATA%\copilot-ask\config.toml` | `%APPDATA%\Wingman\config.toml`; first run copies the old file if the new one is absent and leaves the old alone |
| `autostart.rs` `Run` value | `copilot-ask` | `Wingman`; old value removed on migration |
| `install.ps1` | install dir, cert subject `O=`, package identity, `Run` value | `%LOCALAPPDATA%\Programs\Wingman`, `O=Wingman`, `RaaifYousuf.Wingman`; the old package is removed first with `uninstall.ps1 -KeepCertificate` |
| `packaging/AppxManifest.xml.in` | identity, display names, `Application Id`, AppExtension `Id`/`DisplayName` | all `Wingman`; the picker entry shows the new name |
| tray tooltip, titles, `main.rs` prefix, README, specs | `copilot-ask` | `Wingman` |

The Settings ▸ Keyboard picker choice must be re-made by hand once; no script
can do it (packaging spec).

---

## 3. Does this already exist? An honest landscape check

The owner asked for push-back if a close-enough tool exists. Checked against
what is known as of 2026-09-16:

| tool | what it covers | why Wingman is still worth building |
|---|---|---|
| **Microsoft Copilot on Windows** (Copilot Vision, Copilot Actions, the Copilot key's default target) | The closest thing: sees the screen, can act in apps, ships with Windows | Proprietary, Microsoft's models only, requires a Microsoft account, no local or offline path, no provider choice, no control over what is captured or sent. Wingman is the open, bring-your-own-model, local-capable version of the same key |
| **Click to Do** (Copilot+ PCs) | Select on-screen text or images and get actions | Requires a 40+ TOPS NPU; Core Ultra 200H chips are not Copilot+ class, so it is unavailable on the dev machine and most laptops in use today |
| **Windows Recall** | The awareness timeline | Same NPU gate; proprietary; no app-level control |
| **PowerToys Advanced Paste** (MIT) | Clipboard transforms with cloud or local models | Clipboard only, no screen, no actions on the UI. Wingman reuses its patterns and covers the case as one action |
| **UI-TARS Desktop** (Apache-2.0) | Open-source computer-use agent that operates the GUI from a vision model | Electron, needs a large VLM, autonomous clicking with no confirm step, no tray, no hotkey-first quick actions. A different product: an agent, not an assistant at a key |
| **Open Interpreter** (AGPL-3.0), **Browser Use** (MIT, Python), **Skyvern** (AGPL) | Agent frameworks that drive a computer or browser | Runtimes, not a native app; autonomous by design; two are copyleft |
| **Screenpipe** (proprietary since 2025), **Highlight**, **Rewind**, **Cluely** | Screen-aware assistants | All proprietary, most Mac-first |
| **Flow Launcher** (MIT), **Raycast** | Hotkey-summoned palettes | No screen understanding; Raycast is Mac and proprietary. Wingman borrows the palette shape |
| **AnythingLLM**, **Jan**, **Open WebUI**, **InnerZero** | Local chat apps | Chat windows, not screen-aware, no actions. Wingman deliberately has no chat at all: one press, one card |

**Verdict:** no open-source tool is a native Windows, hotkey-first,
confirm-before-act, bring-your-own-model assistant with local execution via UI
Automation and a connector layer. The proprietary one that comes closest is
Microsoft's own Copilot, and the reason Flow Launcher thrives next to PowerToys
Run is the reason Wingman can thrive next to Copilot: openness, model choice,
and control.

Two real risks, stated plainly: Microsoft will keep folding this into Windows,
and community projects die of breadth. The plan answers the first with
openness and local-first, and the second by making the **action framework**
the product and each action a small, self-contained contribution.

---

## 4. Architecture

### What stays exactly as is

Tray, keyboard hook and learn mode, click-anywhere dismiss, single instance,
autostart, capture, the sparse-package install. The GDI card stays but gains
buttons.

### What changes

```
 Copilot key ─┐
 2nd hotkey   ├──▶ app.rs ──▶ intent router ──▶ palette (top suggestion pre-selected)
 tray / palette ┘                │                      │ Enter
                                 ▼                      ▼
                   ┌──────────────────────┐   ┌──────────────────────┐
                   │ inputs/              │   │ actions/             │
                   │ screen · window      │   │ Action = data:       │
                   │ region · selection   │──▶│ inputs, prompt,      │
                   │ clipboard · typed    │   │ proposal schema,     │
                   │ uia tree · context   │   │ executor, confirm?   │
                   └──────────────────────┘   └──────────┬───────────┘
                                                         ▼
                                            ┌──────────────────────┐
                                            │ provider/ Chain+Mode │
                                            │ openai anthropic     │
                                            │ gemini ollama compat │
                                            └──────────┬───────────┘
                                                       ▼ typed Proposal
                                            ┌──────────────────────┐
                                            │ ui/card (result or   │
                                            │ preview, buttons,    │
                                            │ editable fields)     │
                                            └──────────┬───────────┘
                                                       ▼ confirmed
                                            ┌──────────────────────┐
                                            │ executors/           │
                                            │ replace_text         │
                                            │ fill_form (UIA)      │
                                            │ calendar_add         │
                                            │ clipboard · open_url │
                                            │ connector calls      │
                                            └──────────┬───────────┘
                                                       ▼
                       ┌──────────────┐    ┌──────────────────────┐
                       │ connectors/  │◀───│ store.rs (SQLite)    │
                       │ google       │    │ history · context ·  │
                       │ microsoft    │    │ profile · usage      │
                       │ ics · mcp    │    └──────────────────────┘
                       └──────────────┘
```

New modules and the one-line contract each keeps:

| module | owns | must not |
|---|---|---|
| `actions/` | `Action` model, built-in actions, `actions.toml` merge, the proposal schemas | know about Win32 or HTTP |
| `router.rs` | the intent router: one cheap vision call that returns `intent`, `confidence`, one-line summary | block the palette from opening |
| `inputs/` | `Input` enum and gathering: screen, window, region, selection, clipboard, text, UIA tree, context | know about providers |
| `inputs/selection.rs` | selected text via UI Automation `TextPattern`, fallback synthetic Ctrl+C with byte-exact clipboard restore | leave the clipboard changed |
| `inputs/uia.rs` | the foreground window's editable controls as a flat list: automation id, name, label, control type, current value, bounding rect | write anything |
| `inputs/ocr.rs` (built as `src/ocr.rs`, issue #30; move planned at merge) | `Windows.Media.Ocr` over raw RGBA8 pixels. MEASURED 2026-09-17: does not need package identity, see § 16's risk row; the card says "OCR unavailable" for the failure modes `recognize` does return (no OCR language installed, timeout, a WinRT call failing) | call the network |
| `executors/` | one file per executor: `replace_text`, `fill_form`, `calendar_add`, `clipboard`, `open_url`, `insert_text`; each takes a typed proposal and returns an `Outcome` with an undo closure where possible | run without a confirmed proposal (the type system enforces it: executors take `Confirmed<P>`) |
| `connectors/` | `Connector` trait: id, auth kind (OAuth PKCE loopback, API key, none), capabilities; `google.rs`, `microsoft.rs`, `ics.rs`, later `mcp.rs` | store a token anywhere but Credential Manager |
| `profile.rs` | "About me": names, emails, phones, addresses, company, preferences. Local, DPAPI-encrypted. **Never payment data** | be sent to a provider except as the fields an action needs |
| `provider/ollama.rs`, `openai_compat.rs`, `gemini.rs` | see § 5 | — |
| `mode.rs` | `Mode { Cloud, Local, Auto, Offline }`, `Paused`, the socket-layer offline guard | be bypassable from a provider or connector |
| `secrets.rs` | Credential Manager read/write/delete (`CredWriteW`, feature `Win32_Security_Credentials`); env override; one-time import from `config.toml` then blank | log or display a secret |
| `store.rs` | SQLite via `rusqlite` (bundled) at `%LOCALAPPDATA%\Wingman\wingman.db` | be read by providers directly |
| `context/` | awareness: foreground watcher, clipboard watcher, OCR-on-change, retention, exclusions | run on a timer; run while Paused |
| `knowledge/` | the watched folder, parsers, chunking, embeddings, hybrid retrieval (§ 18) | poll the folder; send a chunk anywhere without it appearing in the knowledge chip |
| `memory.rs` | `memory_items`: facts, preferences, corrections, profile fields, style; the payment-data denylist; decay (§ 18) | store a fact without a source |
| `ui/palette.rs` | Quick Ask window, Win32 + DirectWrite, pre-created and hidden | own business logic |
| `ui/settings_window.rs` | WebView2 host via `wry`, created on demand, destroyed on close: settings, actions editor, memory, connections, profile, usage, egress log. **No chat** | exist while idle; be on the path of any action |
| `usage.rs` | tokens per provider and model, local price table, per-day totals | phone home |

### Provider trait, extended

```rust
pub struct Request {
    pub system: String,
    pub user: String,                      // one turn; there is no multi-turn
    pub images: Vec<Png>,
    pub schema: Option<serde_json::Value>, // the action's proposal schema
    pub effort: Effort,
    pub max_tokens: u32,
}
pub struct Completion { pub text: String, pub usage: Option<Usage>, pub stop: StopReason }

pub trait Provider: Send + Sync {
    fn id(&self) -> &'static str;
    fn ready(&self) -> bool;
    fn capabilities(&self, model: &str) -> Caps;   // vision, json_schema, thinking
    fn complete(&self, req: &Request) -> anyhow::Result<Completion>;
}
```

No streaming. Every result is a structured proposal or a short card, so the
whole response is needed before anything can be shown, and dropping streaming
removes a thread-crossing path and a class of partial-state bugs.

`Chain` keeps its fallback semantics and gains a `Mode` filter. A model
without vision still runs screen actions: the image is replaced by OCR text
plus the UIA tree, which for form filling is the better input anyway.

`serde_json` keeps `preserve_order`; schema property order is load-bearing.

### Threading

Unchanged: main thread owns Win32, a worker per request, results by
`PostMessage`. Executors that touch the UI
(UIA `SetValue`, `SendInput`) run on the main thread after confirmation, in
small chunks with a message pump between them so the card stays responsive.
No async runtime; `ureq` streams a body as a `Read`.

---

## 5. Providers and models

| provider | transport | structured output | notes |
|---|---|---|---|
| OpenAI | existing Responses API | json_schema strict | keep |
| Anthropic | existing Messages API | `output_config.format` | keep; `effort` omitted for 4.5 models (MEASURED 400) |
| **Ollama** | `POST 127.0.0.1:11434/api/chat` | `format: <full JSON schema>` | `images: ["<plain base64>"]` on the user message; `think` always explicit (`false` for proposals, mapped from `effort` for chat); `keep_alive` top-level, default 30 m; `options.num_ctx` set explicitly (server default 4096); `message.thinking` discarded |
| **OpenAI-compatible** | `POST {base_url}/chat/completions` | `response_format: json_schema` where supported, else prompt-enforced with one repair pass | one entry per endpoint; `auth: bearer \| api-key-header \| none`; `image_url` with a `data:` URL; covers OpenRouter (`https://openrouter.ai/api/v1`), Groq, Mistral, DeepSeek, xAI, Together, LM Studio, llama.cpp, vLLM, Azure |
| **Gemini** | `POST …/v1beta/models/{model}:generateContent` | `generationConfig.responseJsonSchema` (`responseSchema` deprecated) | `inline_data` parts |

Model discovery: Ollama `/api/tags` with a vision badge from `/api/show`
`capabilities`; OpenAI-compatible `/v1/models`. Pulling a model streams
`/api/pull` progress into the settings window.

Reference dev hardware: an Intel Core Ultra 200H laptop (Arc iGPU, 32 GB RAM).
Vision-capable in Ollama today: `gemma3:4b`, `gemma3:12b`, `gemma4:12b`,
`qwen3.5:2b/4b/9b`. Text-only: `qwen3:14b`, `deepseek-r1:14b`, `llama3.1:8b`.
Defaults: router `qwen3.5:4b` (fast), actions `gemma4:12b`.

Five facts about Ollama on this hardware, MEASURED in the sibling CLAIR repo
and baked into the provider:

- **`127.0.0.1`, never `localhost`**: IPv6-first resolution stalls ~2 s per
  connection (68x latency difference).
- **`think` always explicit**: unset on a thinking model was 366 s versus 13 s
  for the same answer, with `message.thinking` empty.
- **`keep_alive` top-level**; nested in `options` it is ignored.
- **Arc 140T is dropped by Vulkan unless `OLLAMA_IGPU_ENABLE=1`**; the only
  oracle for GPU use is `size_vram > 0` on `/api/ps`. Settings shows that.
- **Stock Ollama's tray app respawns a CPU-only server on 11434** a second
  after being killed; the health check verifies the listener's process path.

The dev machine is not a representative install; local-model claims are
verified on a fresh VM before a release states them.

### Modes

| mode | providers | network |
|---|---|---|
| Cloud | configured cloud providers in order | yes |
| Local | Ollama and any loopback endpoint | loopback only |
| Auto (default) | Local first if Ollama is up with the model loaded, then Cloud | yes |
| **Offline** | Local only; the guard refuses non-loopback hosts at the socket layer; connectors disabled; update check disabled | loopback only, enforced |

### Pause / off

Tray item and hotkey: **Pause** (1 h / until tomorrow / until resumed). Hook
passes every chord through, awareness stops, no network, icon greyed. **Quit**
is the hard off.

---

## 6. The core loop: Look, Propose, Confirm, Do

### Actions are data

```toml
[[actions]]
id       = "calendar-add"
name     = "Add to calendar"
inputs   = ["screen", "selection"]
proposal = "calendar_event"          # built-in schema: title, start, end, tz, location, notes, attendees
executor = "calendar_add"            # built-in executor; picks the connector from settings
confirm  = true                      # false only for read-only actions
prompt   = "Extract the single most prominent event from the screen…"
prefer   = { mode = "auto" }
hotkey   = { vk = 0x33, ctrl = true, shift = true }
```

An action is a prompt, an input set, a **proposal schema** and an **executor**.
The model never acts; it only fills a typed proposal. The executor is
deterministic Rust. This split is what makes actions safe to accept from
contributors: a new action that reuses an existing executor is a TOML file
and a prompt, reviewable in minutes, and cannot do anything the executor does
not already do.

### The intent router

A bare key press opens the palette instantly with the action list. In
parallel, one cheap call (downscaled screenshot, `qwen3.5:4b` locally or
`claude-haiku-4-5` in the cloud, ~1 s) returns:

```json
{ "intent": "email-review", "confidence": 0.86, "summary": "Compose window to Dana, subject 'Q3 numbers'" }
```

When it lands, the palette's top row becomes "Review this email: to Dana, Q3
numbers", pre-selected. Enter runs it. If the user already pressed Enter on
something else, the router result is dropped. Below the confidence threshold
the palette stays in its default order. The router never runs an action.

`hotkeys.primary_action` still exists for people who want the key pinned to
one action.

### Confirm

The card gains a **preview state**: the proposal rendered as an editable
form or a diff, with **Do it** (Enter), **Edit** (the fields become editable
in place on the card), **Cancel** (Esc, click-away). Nothing runs until Enter or the
button. Read-only actions show a result card straight away.

### The rules every executor obeys

1. **Never the final button.** No Send, Submit, Place order, Pay, Book,
   Delete. Fill and hand back; the user finishes.
2. **Preview equals execution.** What the card shows is exactly what runs;
   the executor takes the confirmed proposal, not a re-generated one.
3. **Undo where the platform allows.** `fill_form` records prior values and
   offers Restore; `replace_text` keeps the original for Undo; a created
   calendar event card carries Delete.
4. **Never store payment data.** The profile has no card, bank or password
   fields. Payment stays with the browser's own autofill.
5. **Say what happened, not what was intended.** The result card is derived
   from the executor's return value: fields actually set, the event id
   actually created.

### The first four actions (Phase 2)

**Check my work** (exists). Inputs: screen. Proposal: `verdict` (detail,
headline, difficulty). Executor: none. The current app, as one action.

**Review this email.** Inputs: the compose body via UIA `TextPattern`
(Outlook, Gmail and Outlook Web in Chrome and Edge expose it), falling back to
selection, then screen. Proposal: `text_review` (verdict "good to go" or a
list of edits with before, after, reason; tone note; missing-attachment
warning when "attached" appears and no attachment control is present).
Executor: `replace_text` applies the edits through UIA `ValuePattern` or, for
rich editors, by selecting each `before` span with `TextPattern` and typing
the `after`. Confirm shows the diff. Read-only when the verdict is "good to
go".

**Add to calendar.** Inputs: selection, else screen. Proposal:
`calendar_event`. Executor: `calendar_add` through the configured connector:
`ics` (zero setup: writes a `.ics` and opens it with the default handler,
which is Outlook or Windows Calendar), `google` (Calendar API, OAuth PKCE
with a loopback redirect on `127.0.0.1`, token in Credential Manager), or
`microsoft` (Graph). Confirm shows the editable event. Result card links to
the created event and offers Delete.

**Fill this form.** Inputs: the UIA tree of the foreground window (editable
controls with labels and current values), plus the screenshot for layout,
plus the profile. Proposal: `form_fill` (a list of `{control_id, label, value,
source}` where `source` is the profile field used). Executor: `fill_form` sets
each value through `ValuePattern.SetValue`, falling back to focus plus typed
input for controls that reject it, skipping password fields and anything the
proposal marked `sensitive` unless the user ticks it in the preview. Never
touches buttons. Confirm shows a table: field, current, proposed, source.
Works on web forms in Chrome, Edge and Firefox (all expose UIA) and on Win32
and WinUI dialogs. Vision-guided clicking at coordinates is explicitly **not**
in this phase; UIA is deterministic and undoable, coordinates are neither.

### Action catalogue (later phases, one at a time, each its own issue)

Explain this error · Summarize this page · Translate selection · Reply to
this message (draft only, into the compose box) · Extract table to CSV ·
Receipt to expense row · Contact to address book · Set a reminder · Save as
note · Define this word · Rewrite for tone · Code: explain, fix, write tests
for selection · Meeting invite to agenda · Job posting to tailored cover
letter draft · Compare these two things on screen · What is this UI element.

Each adds a TOML action and at most one new executor or connector. The
contributor guide's worked example is "Translate selection", which needs
neither.

---

## 7. Palette, inputs, outputs

### The palette

Small, centred, Win32 + DirectWrite, pre-created so it shows in under 50 ms.

```
┌──────────────────────────────────────────────────────────────┐
│ 🔍 Ask anything, or pick an action…                           │
├──────────────────────────────────────────────────────────────┤
│ ★ Review this email        to Dana, "Q3 numbers"    ↵         │
│ ○ Fill this form           uia → confirm                      │
│ ○ Add to calendar          screen → confirm                   │
│ ○ Check my work            screen → card                      │
│ ○ Explain what's on screen screen → card                      │
│ ○ Extract text (offline)   screen → clipboard        local    │
├──────────────────────────────────────────────────────────────┤
│ [Chrome · Gmail] [selection 41 words] [clipboard] [context]   │
│ mode: Auto · qwen3.5:4b → claude-opus-5                       │
└──────────────────────────────────────────────────────────────┘
```

The starred row is the router's suggestion. The chips are what will be
attached; each is a toggle; nothing is attached that is not shown.

### Inputs

| input | how |
|---|---|
| screen | existing capture, active monitor |
| window | foreground window rect only |
| region | crosshair overlay, drag a rectangle |
| selection | UIA `TextPattern`; fallback Ctrl+C with clipboard save/restore; skipped when a password control has focus |
| clipboard | text or image |
| text | one line typed in the palette; one question, one card, no thread |
| uia | the foreground window's editable controls, flattened |
| context | recent awareness events, only when the chip is on |

### Outputs

Card (result or preview; the expanded card scrolls for longer answers),
insert at cursor, clipboard, and executor outcomes. There is no chat output.

---

## 8. Settings window: WebView2 via `wry`, and no chat

There is no main window in the usual sense, because there is no chat. What
exists is a **settings window** you open a few times a month: the
Windows-provided WebView2 runtime hosted in a Win32 window, UI as local
HTML/CSS/JS embedded in the exe, created on open and destroyed on close so
idle RSS is unaffected. WebView2 is kept because forms, tables and an editor
are cheap in HTML and expensive in raw Win32, not for rendering answers;
answers are plain text on the card. If the runtime is missing, a card links to
the installer; card and palette never need it.

Sections: **Actions** (editor for `actions.toml`, test an action against a
screenshot), **Memory** (§ 18), **Connections** (connect and revoke Google,
Microsoft; MCP servers later), **Profile** ("about me" fields, with a note
that payment data is deliberately not here), **Settings**, **Usage**, **Egress
log**. Nothing in it is on the path of any action.

The 1,800-line Win32 settings window is retired once this exists; the card's
few settings stay reachable from the tray.

---

## 9. Awareness

**On by default** (owner decision), which makes the first-run consent screen
mandatory: what is recorded, where it lives, that it never leaves the machine,
how to turn it off, with one button for each. Visible in the tray icon while
on.

| signal | mechanism | idle cost |
|---|---|---|
| foreground app and title | `SetWinEventHook(EVENT_SYSTEM_FOREGROUND)`, event-driven | none |
| clipboard text (sub-toggle) | `AddClipboardFormatListener`, event-driven | none |
| on-screen text | on foreground change, or 2 s after input stops following a visible change: capture the foreground window, hash, OCR if changed | tens of ms per change |

Refuses to: run on a timer; capture while an excluded window is foreground
(password managers, windows whose title contains "InPrivate", "Incognito" or
"Private Browsing", anything the user adds); record while Paused; keep past
retention (default 24 h, "clear now" button); send any of it to a provider
unless the context chip is on for that request.

Storage: SQLite encrypted at rest with DPAPI (user-scoped, built into
Windows). `PRIVACY.md` is written before this code, and the consent screen
quotes it.

Enables: context chips; "what was that error five minutes ago" as a one-line
palette question answered on a card;
**Watch this region** (a local vision model checks a region on each screen
change and notifies when a condition holds).

---

## 10. Connectors

| connector | auth | first used by | phase |
|---|---|---|---|
| `ics` | none | Add to calendar | 2 |
| `google` (Calendar; Gmail drafts later) | OAuth 2.0 PKCE, loopback redirect `http://127.0.0.1:<port>/`, scopes per capability, refresh token in Credential Manager | Add to calendar | 3 |
| `microsoft` (Graph: calendar, mail drafts) | same, MSAL-style PKCE against the common endpoint | Add to calendar, Reply draft | 3 |
| `mcp` | per server | any action that names an MCP tool as its executor | 5 |

OAuth client ids for Google and Microsoft are the project's own, registered
by the owner (OWNER_TODO), and are not secrets by design (PKCE public clients).
Connectors are disabled in Offline mode and shown greyed with the reason.

**MCP** is how the catalogue scales past what the core team writes: an action
can name `mcp:<server>/<tool>` as its executor, the proposal schema is the
tool's input schema, and the confirm card renders it. MCP servers are external
processes (Node, Python), so this is an "advanced" feature that never becomes
a requirement for the built-in actions. The official Rust SDK `rmcp` is
Apache-2.0, acceptable as a dependency.

---

## 11. Everyday tools

Small, each an action or a setting: **Extract text** (Windows OCR, offline,
Phase 2), **clipboard transforms** on `Ctrl+Shift+V` (PowerToys Advanced
Paste pattern), **voice input** (`whisper.cpp` via `whisper-rs`, `base.en`,
optional download), **read aloud** (Windows SAPI), **cost meter** in the tray
tooltip and on cloud-answered cards.

Not planned: browser extension, mobile app, cloud sync, autonomous multi-step
agents, vision-guided clicking.

---

## 12. Secrets and config

Verified 2026-09-16: `config.toml` is outside the repo and gitignored; `git
log -p --all` has no key-shaped strings. Safe to push.

1. Keys and OAuth tokens in Credential Manager (`CredWriteW`, generic
   credentials `Wingman/<provider>` and `Wingman/<connector>`). First run
   after upgrade imports any non-empty `api_key` from `config.toml` and blanks
   it. Env vars still override, for CI.
2. `config.example.toml` and `actions.example.toml` generated from defaults
   by a test so they cannot drift.
3. `.gitignore` adds `*.local.toml`, `.env`, `*.pfx`, `*.cer`, `packaging/out/`.
4. `gitleaks` in CI.
5. Settings shows only the last four characters of a saved key.

---

## 13. Community: repository, docs, CI

The framework is the product; actions are the contribution surface. Files
before the first public push:

| file | contents |
|---|---|
| `LICENSE` | MIT, Raaif Yousuf, 2026 |
| `README.md` | one screen: what it does, a 10-second GIF of one action, install, first run (cloud or local), the four rules executors obey, the Copilot-key picker step |
| `CONTRIBUTING.md` | prerequisites (Rust 1.80+, Windows 11, optional Ollama), `cargo test`, `cargo clippy -D warnings`, `cargo deny check`, **"Add an action in 20 minutes"** worked example (Translate selection), how to add an executor or connector, spec-first for anything architectural, DCO sign-off |
| `SECURITY.md` | reporting, scope (keys, capture, local store, executors) |
| `PRIVACY.md` | exactly what leaves the machine and when; what awareness stores; retention; how to wipe; quoted by the consent screen |
| `CODE_OF_CONDUCT.md` | Contributor Covenant |
| `CHANGELOG.md` | Keep a Changelog, starts at 0.1.0 |
| `THIRD_PARTY_NOTICES.md` | every dependency and every copied snippet with license text |
| `docs/architecture.md`, `docs/providers.md`, `docs/offline.md`, `docs/actions.md`, `docs/executors.md`, `docs/connectors.md` | kept current with the code |
| `deny.toml` | allow: MIT, Apache-2.0, BSD-2/3, ISC, Zlib, Unicode-3.0, Unlicense; advisories on |
| `.github/workflows/ci.yml` | on every push and PR: fmt, clippy, test, deny, gitleaks, release build, exe artifact |
| `.github/workflows/release.yml` | on tag `v*`: build, sign if the cert secret exists, attach `.exe`, `.msix`, `SHA256SUMS.txt` |
| `.github/ISSUE_TEMPLATE/` | bug, feature, **new action**, new provider, new connector |
| `.github/PULL_REQUEST_TEMPLATE.md` | spec link, tests red-then-green, docs, deny green, the observable checked |
| `CODEOWNERS` | the owner, for now |
| GitHub | Discussions on; roadmap as a pinned issue; labels below; `good first issue` on every catalogue action that needs no new executor |

Already in place as of 2026-09-16, ported from the sibling CLAIR repo:
`AGENTS.md`, `docs/README.md`, four skills, the `cold-diff-reviewer` agent, two
guard hooks, `NEXT_SESSION.md`, `OWNER_TODO.md`.

Labels: `area:actions`, `area:executors`, `area:connectors`, `area:providers`,
`area:ui`, `area:inputs`, `area:awareness`, `area:packaging`, `area:docs`,
`area:ci`, `area:core`, plus `P1`/`P2`/`P3` and the defaults.

---

## 14. Open-source sources, licenses verified 2026-09-16

| project | license | how |
|---|---|---|
| Ollama | MIT | use |
| Microsoft PowerToys | MIT (repo-wide) | copy/study: Text Extractor region overlay and OCR flow (from Joe Finney's MIT Text Grab), Advanced Paste actions, CmdPal palette |
| whisper.cpp / whisper-rs | MIT / Unlicense (whisper-rs now on codeberg) | use: dictation |
| wry / webview2-com | Apache-2.0 OR MIT / MIT | use: settings window only |
| rusqlite | MIT | use: store |
| keyring or direct `CredWriteW` | MIT OR Apache-2.0 / windows-rs | use: the direct route is a few dozen lines and one fewer dependency |
| KaTeX | MIT | use |
| Flow Launcher | MIT | study: palette, fuzzy ranking |
| windows-rs | MIT OR Apache-2.0 | use, already; UI Automation is `Win32_UI_Accessibility` |
| xcap | Apache-2.0 only | use, already; `windows-capture` (MIT) is the swap if ever needed |
| enigo | MIT | use or copy: Unicode `SendInput` |
| rmcp (MCP Rust SDK) | Apache-2.0 | use, Phase 5 |
| rust-genai / graniet llm | MIT OR Apache-2.0 / MIT | study only; async |
| Jan / llamafile | Apache-2.0 | study: model management UX |
| UI-TARS Desktop | Apache-2.0 | study only: what an autonomous agent UI looks like, so Wingman's confirm-first UI stays visibly different |
| **Screenpipe** | proprietary since 2025 | not used, not copied |
| **Open WebUI** | BSD-derived with branding clause | not used, not copied |
| **Foundry Local** | SDK MIT, runtime proprietary | not used |
| **Piper** | archived; successor GPL-3.0 | not used; SAPI instead |

Enforced by `cargo deny` (`[licenses] allow = [...]`).

---

## 15. Decisions still owed

1. Google Cloud and Azure app registrations for the OAuth client ids (owner
   accounts; Phase 3).
2. Whether `Fill this form` may use the profile's email and phone without a
   per-field tick (recommended: yes for name, email, phone, address; tick
   required for date of birth and anything the model marks sensitive).

---

## 16. Risks

| risk | handling |
|---|---|
| Microsoft ships the same thing in Windows | openness, model choice, local-first, control over capture; the Flow Launcher precedent |
| Community project dies of breadth | actions are TOML plus a prompt over a small fixed executor set; every catalogue item is one issue; `good first issue` on the ones needing no new executor |
| An executor does something the user did not preview | executors take `Confirmed<Proposal>`; the preview and the execution share one value; never the final button |
| UIA does not expose a form | fall back to selection and typed input per field, or report "could not reach the fields" in the card; never coordinates in Phase 2 |
| Windows OCR refuses an exe without package identity | Disproven. MEASURED 2026-09-17 (issue #30, `src/ocr.rs`'s `ocr_live_recognizes_gdi_rendered_text`): a process with no package identity (`GetCurrentPackageFullName` = `APPMODEL_ERROR_NO_PACKAGE`) still gets a working `OcrEngine::RecognizeAsync` -- cold 36 ms, warm 22 ms, `en-US`. `THEORY (unverified)`: whether the sparse package's own installed presence changes anything for a `Run`-key launch specifically; no mechanism is known by which it would, and this is not expected to be revisited |
| Ollama silently on CPU | Settings shows `size_vram`; first-run help names `OLLAMA_IGPU_ENABLE=1` |
| Ollama cold start | `keep_alive` 30 m; warm-up on detection |
| Awareness on by default alarms users | consent screen quoting `PRIVACY.md`, tray indicator, exclusions, 24 h retention, one-click off and wipe |
| Idle power | event-driven everywhere; measured with `powercfg` before each release |
| Key leakage | nothing in files; gitleaks; example config generated |
| Non-MIT code creeping in | `cargo deny`; `THIRD_PARTY_NOTICES.md` reviewed per PR |

---

## 17. Roadmap and issue map

Each phase is shippable on its own and is one GitHub milestone. Each bullet
is one issue; the bracketed tag is its `area:` label.

### Phase 0 — Rename to Wingman and go public

- [core] Rename package, identifiers, config path, with one-time migration from copilot-ask
- [core] Move API keys to Credential Manager; import from config; blank the file
- [docs] LICENSE, README rewrite, CONTRIBUTING with the "add an action" example, SECURITY, PRIVACY, CODE_OF_CONDUCT, CHANGELOG, THIRD_PARTY_NOTICES
- [ci] ci.yml on every push and PR: fmt, clippy, test, deny, gitleaks, build artifact
- [ci] release.yml: tag to exe, msix, checksums
- [ci] deny.toml with the license allowlist; config and actions example files generated by test
- [packaging] New icon set and package identity RaaifYousuf.Wingman
- [docs] Issue and PR templates; Discussions; pinned roadmap issue

### Phase 1 — Providers and modes

- [providers] Extend the Provider trait: Request, Completion, capabilities (single turn, no streaming)
- [providers] Ollama native: chat with images, schema format, explicit think, top-level keep_alive, num_ctx, 127.0.0.1
- [providers] Ollama discovery, vision detection via /api/show, pull with progress, GPU oracle from size_vram
- [providers] Ollama health check that verifies the listener's process path
- [providers] OpenAI-compatible generic endpoint with per-endpoint auth styles
- [providers] Gemini with responseJsonSchema
- [providers] Non-vision fallback: OCR text and UIA tree in place of the image
- [core] Mode enum, tray toggle, socket-layer Offline guard, icon states
- [core] Pause: 1 h, until tomorrow, resume; greyed icon; hook pass-through
- [core] Token accounting, local price table, tray tooltip spend
- [docs] docs/providers.md and docs/offline.md

### Phase 2 — The core loop: actions, palette, first four actions

- [actions] Action model, built-in set, actions.toml merge, proposal schema registry
- [actions] Intent router: one cheap call, confidence threshold, palette pre-selection, never runs an action
- [ui] Quick Ask palette: Win32 plus DirectWrite, pre-created, fuzzy filter, context chips
- [ui] Card preview state with Do it, Edit, Cancel buttons and editable fields
- [inputs] UIA tree of the foreground window: editable controls with labels and values
- [inputs] Selection via UIA TextPattern with clipboard-safe fallback and password-field skip
- [inputs] Region and window capture with crosshair overlay
- [inputs] Windows OCR: done as `src/ocr.rs` (issue #30, MEASURED 2026-09-17 -- see § 16); still owed: wiring into `App::ask`'s non-vision fallback and the "OCR unavailable" card
- [executors] Executor trait with Confirmed<Proposal> and undo closures
- [executors] replace_text via UIA ValuePattern and TextPattern
- [executors] fill_form via UIA with prior-value recording and Restore
- [executors] calendar_add through the ics connector
- [connectors] Connector trait and the zero-auth ics connector
- [core] Profile store: about-me fields, DPAPI encrypted, no payment data
- [actions] Check my work as the first built-in action, difficulty rubric preserved
- [actions] Review this email
- [actions] Add to calendar
- [actions] Fill this form
- [actions] Extract text to clipboard, offline
- [core] hotkeys.primary_action, double-press repeat, per-action hotkeys
- [docs] docs/actions.md and docs/executors.md

### Phase 3 — Settings window and real connectors

- [ui] WebView2 settings window via wry, created on demand, destroyed on close; no chat
- [core] SQLite schema and migrations: usage, profile, memory, egress log
- [ui] Actions editor with test-against-screenshot
- [ui] Connections page: connect, revoke, status
- [ui] Profile page
- [ui] Settings moved into the window; retire the Win32 settings window
- [ui] Usage page
- [connectors] OAuth PKCE loopback flow shared by Google and Microsoft, tokens in Credential Manager
- [connectors] Google Calendar
- [connectors] Microsoft Graph calendar
- [actions] Reply to this message: draft into the compose box, never send
- [docs] docs/connectors.md

### Phase 4 — Awareness

- [docs] PRIVACY.md final wording and the consent screen text
- [awareness] First-run consent screen
- [awareness] Foreground watcher, clipboard watcher, change-hash gate
- [awareness] OCR-on-change into the store; retention; exclusions incl. private-browsing titles
- [awareness] DPAPI encryption at rest; clear now
- [awareness] Context chips and a one-shot "what was that error" palette question
- [awareness] Watch this region with a local vision model

### Phase 5 — Scale the catalogue

- [connectors] MCP client (stdio and streamable HTTP) as an executor source
- [actions] Catalogue: Explain this error
- [actions] Catalogue: Summarize this page
- [actions] Catalogue: Translate selection (the CONTRIBUTING worked example)
- [actions] Catalogue: Extract table to CSV
- [actions] Catalogue: Receipt to expense row
- [actions] Catalogue: Contact to address book
- [actions] Catalogue: Rewrite for tone
- [actions] Catalogue: Code: explain, fix, write tests for selection
- [actions] Catalogue: Set a reminder
- [actions] Catalogue: Define this word
- [inputs] Voice input via whisper.cpp with optional model download
- [ui] Read aloud via SAPI
- [core] Opt-in update check against GitHub releases, off in Offline mode
- [packaging] winget manifest

---

## 18. Knowledge and memory

Added 2026-09-16 at the owner's request: a folder to drop documents into
(a resume, contracts, notes, writing samples) that Wingman reads, extracts
facts from, and keeps as a local memory that grows over time.

### The folder

Default `%USERPROFILE%\Documents\Wingman\Knowledge`, plus any extra folders
the user adds (an Obsidian vault, a project directory). Watched with
`ReadDirectoryChangesW`, event-driven; nothing polls. Supported: `.md`,
`.txt`, `.pdf`, `.docx`, `.html`, `.csv`, `.json`, and images through Windows
OCR. Parsers are permissive-license crates (`lopdf`/`pdf-extract`, `docx-rs`,
`html2text`; verified at adoption) and run locally.

### The pipeline

```
 file changed ──▶ hash ──▶ parse to text ──▶ chunk (~500 tokens, overlap)
                                                   │
                        ┌──────────────────────────┴───────────────────────┐
                        ▼                                                  ▼
               embed each chunk                                 extract facts (one model
               (Ollama nomic-embed-text locally;                 call per document, schema
                OpenAI / Gemini embeddings in cloud modes;       facts[] {text, kind, subject,
                Offline = local only)                            confidence, source span})
                        │                                                  │
                        ▼                                                  ▼
               SQLite: chunks + vectors (sqlite-vec)            memory_items, shown as a card:
               + FTS5, DPAPI-encrypted                          "Learned 14 things from
                                                                 resume.pdf — review"
```

Removing a file removes its chunks and offers to forget its facts. Editing a
file re-ingests only that file.

### Memory

One table, inspectable and editable on a **Memory** page in the settings window:

```
memory_items { id, kind: fact | preference | correction | profile_field | style,
               text, structured: json, source: doc(path, span) | action(id),
               confidence, created, last_used, uses, pinned }
```

The Phase 2 profile store becomes a view over `profile_field` items, so
"Fill this form" and "Reply to this message" read from one place. The
no-payment-data rule is enforced at write time by a denylist (card numbers,
IBANs, national id patterns): such a fact is refused with a card saying why.

How it grows, and who authorizes each write:

| source | authorization |
|---|---|
| a document in the folder | dropping it in is the consent; facts land automatically and the review card lets the user edit or delete |
| a **Remember this** button on any result card, or a palette line starting "remember that…" | explicit |
| the model proposing `remember[]` alongside an action result | shown as chips on the card; nothing is stored until clicked |
| a proposal the user edited before confirming | the diff is stored as a `correction` for that action and shown next time ("last time you changed the salutation") |
| writing samples in the folder | a `style` item summarizing tone, used by Reply and Rewrite |

Items decay: `last_used` and `uses` drive a "forget these?" suggestion for
stale items; pinned items never decay. Export to JSON or Markdown, import
back; this is the user's data and must be portable.

### Retrieval

For any action: hybrid search (FTS5 BM25 plus vector cosine)
over chunks and memory items, top-k within a token budget. The palette shows a
**knowledge chip** naming the sources about to be attached; it is a toggle
like every other chip, and the egress log records exactly which snippets left
the machine. Offline mode uses local embeddings only and offers to pull
`nomic-embed-text` if missing (it is already installed on the dev machine).

What it enables first: a job application form filled from the resume; a
cover letter drafted from the resume and the posting on screen; "answer from
my documents" as a one-line question answered on a card; replies in the
user's own voice.

## 19. Additional functionality

Filed as issues on 2026-09-16 across the existing milestones, each one
self-contained. Privacy and cost: screenshot redaction of passwords, card
numbers and tokens before upload; a "show me what you're sending" preview; an
egress log page; a daily cost budget; retry and rate-limit handling; prompt
caching; a structured-output repair pass. Inputs: whole-page text via UIA
instead of a screenshot; multi-language OCR. Ergonomics: per-app default
action and model; card position and monitor choice; hotkey conflict
detection. Platform:
accessibility (a UIA provider for the card and palette, full keyboard
navigation), themes and high contrast, a localization scaffold, import and
export of settings and actions, multiple profiles, a diagnostics bundle,
portable mode. Connectors: Gmail and Outlook drafts, Microsoft To Do and
Google Tasks. Community: an actions gallery repo browsable in-app. CI: a
fresh-VM packaged install smoke test and performance budget gates (exe size,
startup, idle RSS). Catalogue: describe image, chart to table, calculator and
unit conversion, ask about this page, daily brief, fetch and summarize a URL.

## Verification note

License and API facts were checked against upstream repositories, crates.io
and vendor docs on 2026-09-16. Corrections made from that pass: Screenpipe
removed as a source (relicensed proprietary); Piper removed (successor
GPL-3.0); Open WebUI's branding clause recorded; Foundry Local's split
licensing recorded; xcap marked Apache-2.0 only; Gemini switched to
`responseJsonSchema`; Ollama `think` levels, plain base64 images, `num_ctx`
default 4096 and Vulkan-on-by-default recorded; Windows OCR's package-identity
requirement added as a risk. The landscape table in § 3 is from general
knowledge as of the same date and should be re-checked before the README
makes any comparative claim.
