# Providers

What Wingman actually sends to each provider today, checked against the code
in `src/provider/`. Planned providers (OpenAI-compatible endpoints) are
labelled **Planned** and do not exist in code yet; see the
[expansion plan](superpowers/specs/2026-09-16-expansion-plan-design.md) §5.

Every provider implements the same `Provider` trait
(`src/provider/mod.rs`): `id`, `ready`, `capabilities`, `complete`. A
`Chain` (`src/provider/mod.rs`) tries each ready provider in
`providers.order` in turn and falls through to the next on any failure,
including a 200 response that fails to parse against the action's schema.

## Where a key lives

Every cloud provider's API key is stored, once saved from Settings, as a
Windows Credential Manager generic credential named `Wingman/<provider>`
(`src/secrets.rs`, `target_name`). `config.toml` never carries a live key:
`Config::save` pushes any non-empty, non-env-sourced key to Credential
Manager first and blanks the field before the file is written
(`Config::push_secrets_to_store`, `src/config.rs`). An environment variable
override always takes precedence and is never written to the store:

| provider | env var override | Credential Manager target |
|---|---|---|
| OpenAI | `OPENAI_API_KEY` | `Wingman/openai` |
| Anthropic | `ANTHROPIC_API_KEY` | `Wingman/anthropic` |
| Gemini | `GEMINI_API_KEY` | `Wingman/gemini` |
| Ollama | none (no key concept) | not applicable |

Ollama has no `api_key` field at all (`OllamaConfig` in `src/config.rs`):
a local server has nothing to authenticate against. Settings shows only the
last four characters of a saved cloud key (`src/ui/settings.rs`).

## OpenAI

- **Endpoint:** `POST https://api.openai.com/v1/responses` (`src/provider/openai.rs`).
- **Auth:** `Authorization: Bearer <key>` header.
- **Models:** configured in `providers.openai.models`; the active model is
  `providers.openai.model`. Ships with `gpt-5.5`, `gpt-5.5-pro`, `gpt-5.4`,
  `gpt-5.4-mini`, `gpt-5.4-nano`, `gpt-5.2`, `gpt-5.1`, `gpt-5`, `gpt-5-mini`,
  `gpt-4.1` (`Providers::default` in `src/config.rs`). Add a model by editing
  the list; no rebuild needed.
- **Effort/thinking:** `reasoning.effort` (`low`/`medium`/`high`), from
  `providers.openai.effort` unless a request carries its own `Effort`
  override. Omitted entirely (not sent as an empty string) when effort is
  unset/blank, or when the model does not support it. `gpt-4.1` predates
  `reasoning.effort` on the Responses API and is carved out
  (`supports_reasoning`); THEORY (unverified): whether sending it anyway
  400s or is silently ignored has not been checked live (#167).
- **Structured output:** `text.format` with `type: "json_schema"`,
  `strict: true`, and the schema verbatim from `Request::schema`. Property
  order is preserved end to end (`serde_json`'s `preserve_order`, CLAUDE.md
  rule 3): `detail` before `headline` before `difficulty`. `difficulty` is
  opt-in (`ui.show_difficulty`, default `false` as of issue #197): when off,
  the property is left out of the schema entirely and nothing is appended to
  the system prompt to ask for it, so it costs zero extra tokens unless the
  user turns it on.
- **Images:** `input_image` content parts with a `data:image/png;base64,...`
  URL, `detail: "high"`.
- **Retry/429:** shared with Anthropic and Gemini via
  `provider/common.rs::post_json` (see "Retry and 429 handling" below).
  Timeout is 90 seconds total (`REQUEST_TIMEOUT`).
- **Budget exhaustion:** `status: "incomplete"` with no `message` entry in
  `output[]` is reported as "The model ran out of room before answering.
  Lower the effort setting or raise the token limit." rather than a generic
  parse failure (#155).

## Anthropic

- **Endpoint:** `POST https://api.anthropic.com/v1/messages`
  (`src/provider/anthropic.rs`).
- **Auth:** `x-api-key` header, plus `anthropic-version: 2023-06-01`.
- **Models:** `providers.anthropic.models`, active model
  `providers.anthropic.model`. Ships with `claude-opus-5`, `claude-sonnet-5`,
  `claude-opus-4-8`, `claude-haiku-4-5`, `claude-fable-5-1`.
- **Effort/thinking:** `output_config.effort`, from
  `providers.anthropic.effort` unless overridden per request. Rejected
  outright (a hard 400, MEASURED 2026-09-15) on the 4.5-generation models
  (`claude-haiku-4-5`, `claude-sonnet-4-5`, `supports_effort`'s
  `NO_EFFORT` list), so it is omitted for those, and omitted for an
  unset/blank configured effort the same way OpenAI's is.
- **Structured output:** `output_config.format`, `type: "json_schema"`,
  `schema` taken directly (no `name`/`strict`, unlike OpenAI's spelling).
  Never the deprecated `output_format` field.
- **Images:** `image` content blocks, `source.type: "base64"`,
  `media_type: "image/png"`, placed before the text block. The message list
  is always exactly one user turn; the assistant turn is never prefilled.
- **Retry/429:** shared, see below. Timeout is 90 seconds total.
- **Refusal:** `stop_reason: "refusal"` is surfaced as
  `"anthropic: model refused to answer"` rather than being treated as a
  malformed response. `stop_reason: "max_tokens"` with no text block is
  reported as the same "ran out of room" message OpenAI uses (#155).

## Gemini

- **Endpoint:**
  `POST https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent`
  (`src/provider/gemini.rs`, issue #17).
- **Auth:** `x-goog-api-key` header. The key is deliberately never put in
  the URL (a `?key=` query string would leak into a ureq transport-error
  string or any future request log, CLAUDE.md rule 1); see
  `endpoint_url_never_contains_the_api_key`.
- **Models:** `providers.gemini.models`, active model
  `providers.gemini.model`. Ships with `gemini-3.8-flash`,
  `gemini-3.1-pro-preview`, `gemini-3.5-flash`, `gemini-2.5-pro`,
  `gemini-2.5-flash`.
- **Effort/thinking:** `generationConfig.thinkingConfig.thinkingLevel`
  (`"low"`/`"medium"`/`"high"`), sent only to the Gemini 3.x line
  (`supports_thinking`). The 2.5-series models think by default under a
  differently-shaped `thinkingBudget` (an integer token budget), which this
  provider does not implement (scope cut, not a bug). THEORY (unverified):
  whether sending `thinkingLevel` to a 2.5-series model 400s or is ignored
  has not been checked live (issue #166 tracks a live check).
- **Structured output:** `generationConfig.responseMimeType:
  "application/json"` plus `responseJsonSchema` (never the deprecated
  `responseSchema`, which needs `propertyOrdering` to preserve key order on
  older SDKs). `responseJsonSchema` preserves the schema's own key order for
  Gemini 2.5+ models directly, matching rule 3's requirement without any
  extra field.
- **Images:** `inline_data` parts (`mime_type: "image/png"`, base64
  `data`), placed before the text part. The system prompt goes in
  `systemInstruction`, never folded into `contents` (Gemini has no separate
  system role in the messages array).
- **Retry/429:** shared, see below. Timeout is 90 seconds total.
- **Refusal:** `finishReason` of `SAFETY`, `RECITATION` or
  `PROHIBITED_CONTENT` is reported as "the model declined to answer
  (<reason>)". No candidates at all with `promptFeedback.blockReason` set is
  reported as "the prompt was blocked (<reason>)" -- a prompt-level block,
  distinct from a per-candidate refusal. `finishReason: "MAX_TOKENS"` with
  no text part gets the same "ran out of room" message as the other
  providers.

## Ollama

- **Endpoint:** `POST {base_url}/api/chat` (`src/provider/ollama.rs`, issue
  #13). Default `base_url` is `http://127.0.0.1:11434`
  (`OllamaConfig::default`, `DEFAULT_BASE_URL`).
- **Auth:** none. `ready()` only checks that `base_url` is non-empty; a
  local server that isn't actually listening fails as a transport error,
  which `Chain` already falls through on.
- **127.0.0.1, never `localhost`** (CLAUDE.md rule 6): IPv6-first resolution
  of `localhost` stalls about 2 seconds per connection on Windows, MEASURED
  in the sibling CLAIR repo. `DEFAULT_BASE_URL` is asserted in tests to
  never contain the string `localhost`. The Offline guard (see
  `docs/offline.md`) also refuses `localhost` specifically, with a message
  pointing at `127.0.0.1`.
- **Models:** `providers.ollama.model`, default `gemma3:4b`. There is no
  `providers.ollama.models` list (unlike the cloud providers) -- Ollama's
  own `/api/tags` is the source of what is actually pulled; Settings shows a
  read-only status line, not an editable list.
- **Effort/thinking:** `think` (boolean) is **always sent explicitly**,
  never omitted (CLAUDE.md rule 6: leaving it unset on a thinking model was
  MEASURED 28x slower, 366s vs 13s for the same answer). `Effort::Unset` and
  `Effort::Low` map to `think: false`; `Medium`/`High` map to `true`
  (`Ollama::think`).
- **`keep_alive` is a top-level request field**, not nested inside
  `options` -- CLAUDE.md rule 6: nested there it is silently ignored.
  Defaults to `"30m"`.
- **`options.num_ctx` is always sent explicitly** (`8192`): the server
  default is 4096, which is tight once a screenshot's image tokens are
  added in.
- **Structured output:** `format` carries the full JSON Schema from
  `Request::schema` verbatim (not the bare `"json"` string Ollama also
  accepts).
- **Images:** plain base64 strings in the user message's `images` array, no
  `data:` prefix (unlike OpenAI/Anthropic/Gemini's data URLs or inline-data
  wrapping).
- **Vision capability:** a static allowlist by model-name prefix
  (`VISION_FAMILIES`: `gemma3`, `gemma4`, `qwen3.5`), used by
  `Provider::capabilities`. `src/provider/ollama_admin.rs`'s
  `vision_from_tags_entry`/`show_capabilities` prefer the live
  `capabilities` array from `/api/tags` or `/api/show` when the server
  reports one (0.34+), falling back to the same static list for an older
  server or an untagged model.
- **Retry/429:** none of the shared retry policy applies -- Ollama has no
  rate-limit concept. `complete()` uses
  `post_json_with_connect_timeout`: a short connect timeout (3s, since a
  loopback connect should be near-instant) and a long total timeout (180s,
  to allow a cold model load; MEASURED 2026-09-17: ~18s for a first-load
  `gemma3:4b` call, CPU-only, on the dev machine).
- **Budget exhaustion:** `done_reason: "length"` is reported as the same
  "ran out of room" message, whether `message.content` came back empty (all
  budget spent on `message.thinking`) or truncated-but-present (a
  non-thinking model cut short) -- both MEASURED 2026-09-17.
- **`OLLAMA_IGPU_ENABLE` for Intel iGPUs:** an environment variable for the
  Ollama *server process itself*, not read or set anywhere in this crate.
  On this machine's Intel Arc 140T, Vulkan drops the iGPU unless the Ollama
  server was started with `OLLAMA_IGPU_ENABLE=1` in its environment; without
  it, Ollama silently runs CPU-only. Set it before starting `ollama serve`
  (or the equivalent for however Ollama is launched on the target machine),
  not in Wingman's config.
- **The only oracle for GPU use is `size_vram > 0` on `/api/ps`**
  (CLAUDE.md rule 6): `ollama_admin::gpu_status_for` reads this field
  directly rather than trusting `ollama ps`'s own PROCESSOR column, which
  has been MEASURED mislabelling a real GPU run as CPU on this hardware.
  Settings' Ollama status line surfaces this as "`<model>` is loaded on
  GPU." / "on CPU." / "not loaded yet."
- **Stock Ollama's tray app steals port 11434**: about one second after
  being killed, it respawns a CPU-only server on the same port. A health
  check that only asks "did something answer" reports false success.
  `ollama_admin::query_ollama_health` (issue #15) resolves the actual
  listening process via `GetExtendedTcpTable` (read-only, never starts,
  stops or signals anything) and classifies it by its parent process image
  path (`ollama app.exe` means the stock tray-spawned server); Settings'
  status line shows the result, including a message that says exactly this
  and tells the user to quit the stock tray app.

### Planned: OpenAI-compatible endpoints

Not built. The expansion plan §5 describes a generic OpenAI-compatible
provider (`POST {base_url}/chat/completions`, `response_format:
json_schema` where supported, `auth: bearer | api-key-header | none`) meant
to cover OpenRouter, Groq, Mistral, DeepSeek, xAI, Together, LM Studio,
llama.cpp, vLLM and Azure without a new provider implementation per vendor.
No code for this exists in `src/provider/` today.

## Retry and 429 handling

Shared by OpenAI, Anthropic and Gemini through
`provider/common.rs::post_json` / `post_json_with` (issue #98). Ollama uses
a different helper (`post_json_with_connect_timeout`) with no retry policy,
since a local server has no rate limit to retry against.

- A transport error (DNS, connect, timeout, dropped connection) or an HTTP
  5xx is retried up to `RetryPolicy::max_retries` (2) times with full-jitter
  exponential backoff (`base_delay` 200ms), capped at `max_total_backoff`
  (3 seconds) of cumulative sleep across all retries for one call.
- An HTTP 429 is retried **exactly once**, sleeping the server's own
  `retry-after` header verbatim (never jittered -- that delay is the
  server's instruction), and only when that delay is at most
  `max_retry_after` (5 seconds). `retry-after` is parsed as either
  delta-seconds or an RFC 7231 HTTP date. A second 429, a too-long delay, or
  a missing header all surface as an error naming the provider and the
  delay instead of retrying again.
- Any other 4xx status is never retried.
- A provider that exhausts its retries still falls through to the next
  provider in the `Chain` as usual -- the retry policy only decides whether
  *this* provider gets a second attempt.
- All of the above happens **after** the Offline guard (see
  `docs/offline.md`), which runs first and refuses the request outright
  before any transport call, retry or sleep, when Offline mode is active
  and the URL's host is not loopback.

## How to add Ollama or Gemini to `providers.order` by hand today

Settings (`src/ui/settings.rs`) only exposes OpenAI and Anthropic:
`ID_OPENAI_KEY`/`ID_ANTHROPIC_KEY` and their model/effort combos, plus a
two-way `order_from_choice` toggle (`0` -> `["openai", "anthropic"]`, `1`
-> `["anthropic", "openai"]`) that **rebuilds `providers.order` from
scratch on every Settings save**. There is no UI path to add `"ollama"` or
`"gemini"` to the order, and Gemini has no key/model/effort fields in
Settings at all (issue #195).

To use Ollama or Gemini today, edit `%APPDATA%\Wingman\config.toml` by hand
(**Open config.toml** in the tray menu) and add the provider name to
`providers.order`, e.g.:

```toml
[providers]
order = ["ollama", "openai", "anthropic"]
```

Gemini additionally needs `providers.gemini.api_key` set (or the
`GEMINI_API_KEY` environment variable) since it has no Settings field to
paste a key into. Ollama needs nothing beyond the order entry: its
`base_url`/`model`/`effort` already have working defaults in
`OllamaConfig::default`.

**The Settings limitation, precisely:** any edit made by hand to
`providers.order` beyond `openai`/`anthropic` survives a **Reload
settings** (re-reads the file) but is **silently discarded** the next time
Settings itself is opened and saved, because `order_from_choice` always
rewrites `order` to one of its two fixed two-provider lists on save
(tracked as issue #194's original bug, now understood and filed against
#195 as the reason a Gemini field group has not been added). Until #51
(retiring the fixed-layout Win32 Settings dialog for a main-window
settings surface) lands with a proper N-provider order editor, a
hand-edited order that includes Ollama or Gemini must not be re-saved from
Settings, or it reverts to `openai`/`anthropic` only.

## Model discovery

Ollama: `GET /api/tags` lists pulled models; each entry's vision badge comes
from its own `capabilities` array (0.34+) or the static allowlist fallback
above. `POST /api/show` (`show_capabilities`, issue #14) gives the same
badge for one model plus its raw capability list, but nothing in the
running app calls it yet -- it exists and is tested ahead of a settings
surface that will use it. `POST /api/pull` (`pull`, issue #14) streams
newline-delimited JSON progress and is built and tested, but nothing calls
it yet either; both are reserved for a future settings surface (see each
function's doc comment in `src/provider/ollama_admin.rs`).

OpenAI-compatible `/v1/models` discovery is **planned**, not built (there is
no OpenAI-compatible provider to discover models for yet).
