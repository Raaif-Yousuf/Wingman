# Modes and the Offline guard

What each of the four modes actually does, what the Offline guard blocks and
where, what it cannot guarantee, and how to verify it yourself. Checked
against `src/mode.rs` and `src/provider/common.rs` (issue #19).

## The four modes

`Mode` (`src/mode.rs`) is one of `Cloud | Local | Auto | Offline`, stored in
`config.toml`'s top-level `mode` key, defaulting to `Auto`. It is shown as a
radio-checked "Mode" submenu in the tray (`append_mode_submenu`,
`src/ui/tray.rs`).

| mode | providers selected | network |
|---|---|---|
| **Cloud** | every configured non-local provider (everything except Ollama), in `providers.order` | yes |
| **Local** | every configured local provider (Ollama only, today) | loopback only |
| **Auto** (default) | Local first, but only if Ollama is reachable and has the configured model loaded right now; otherwise Cloud only | yes, unless Ollama alone is used |
| **Offline** | same selection as Local | loopback only, **enforced in code**, not just by selection |

`mode::select_providers(mode, configured, ollama_ready)` (pure, unit-tested
with a full table of cases) is the only place this selection logic lives;
`Providers::build_chain_for_mode` (`src/config.rs`) calls it to build the
actual provider chain for a request.

### Cloud

Every provider in `providers.order` that is not Ollama, in order. Ollama is
excluded even if it is present in the order and even if it is ready.

### Local

Every provider in `providers.order` that Wingman classifies as local
(`mode::is_local_provider_name`, currently just `name == "ollama"`), in
order. If Ollama is not in `providers.order` at all, Local mode has no
providers and every request fails immediately with "no providers
configured".

### Auto

The default. If Ollama is named in `providers.order`, has a non-blank
`base_url`, and a fast reachability probe says it is ready, Local providers
run first and Cloud providers are the fallback. If any of those is false,
Auto is Cloud-only -- **Local is never attempted at all, not even as a
later fallback** after Cloud fails. This is a deliberate reading of "local
first if Ollama is up ... then Cloud", not "try cloud, then also try
local".

The reachability probe (`mode::probe_ollama_ready`) is `GET
{base_url}/api/tags`, bounded to 400ms, and is **only run at all** when
`mode::should_probe_ollama` says Ollama is actually configured (named in
`providers.order` with a non-blank `base_url`). This is the latency
guarantee issue #19 asked for: a user who never configured Ollama sees zero
added latency in Auto mode, because the probe is skipped entirely rather
than run and ignored. When it does run, `model_present_in_tags` checks that
the configured model specifically (not just "something") is listed in the
response before calling Ollama ready.

### Offline

Selects the same providers as Local. The difference from Local is
enforcement, not selection: Offline additionally makes the socket-layer
guard (below) active, which Local does not. In Offline mode the plan also
calls for connectors and the update checker to be disabled outright, not
merely restricted to loopback -- neither connectors nor an update checker
exist in this crate yet, so there is nothing to disable today; see "What it
cannot guarantee" below for what this means in practice.

## What the guard blocks, and where

Every function in `src/provider/common.rs` that actually opens a socket
(`post_json_with`, `post_json_with_connect_timeout`, `get_text_with_timeout`,
`post_json_read_body`) calls `offline_guard(url)` **first**, before any
transport call, before any DNS resolution. `offline_guard` is a single,
small function:

```
fn offline_guard(url: &str) -> Result<()> {
    if !mode::is_offline_now() { return Ok(()); }
    match mode::classify_host(url) {
        HostClass::Loopback => Ok(()),
        HostClass::Localhost => Err(/* points at 127.0.0.1 */),
        HostClass::NotLoopback => Err(/* refused */),
    }
}
```

`mode::classify_host` is pure string parsing of the URL's host -- **it never
resolves DNS**, which is what lets the guard decide before a socket opens at
all, not just before data is sent. It recognizes:

- IPv4 loopback (`127.0.0.0/8`, any address starting `127.`).
- The literal IPv6 loopback `::1` and the IPv4-mapped form `::ffff:127.x.x.x`
  (any hex case), only when bracketed (`[::1]`), matching RFC 3986. Other
  valid spellings of the same address (zero-padded groups, the
  fully-expanded `0:0:0:0:0:0:0:1`) are deliberately **not** recognized and
  classify as refused -- this under-recognizes rather than over-recognizes,
  since this codebase never produces an unusual spelling itself.
- `localhost` specifically is its own class, refused but with a message
  pointing at `127.0.0.1` instead (CLAUDE.md rule 6: `localhost`'s
  IPv6-first resolution stalls on Windows, so a config that names it is
  almost always meant to mean the literal loopback address).
- Anything else -- a real hostname, an unparseable string, an empty URL --
  is `NotLoopback` and refused. **Parsing failure fails closed** (refused),
  never open.

The guard also defeats a couple of URL tricks deliberately: userinfo
claiming to be loopback (`http://127.0.0.1@evil.com/`) resolves against the
real host after the last `@`, not the userinfo before it, so it correctly
refuses; a hostname that merely starts with the loopback address
(`http://127.0.0.1.evil.com/`) is a name, not an address, and is refused.

**Because every HTTP entry point in the crate funnels through
`provider/common.rs`**, a test in that file
(`no_provider_file_calls_ureq_directly_outside_common_rs`) fails the build
if any other file under `src/provider/` calls `ureq::` directly -- the
enforcement is structural, not just a convention to remember.

`mode::probe_ollama_ready`'s own network call also goes through the same
guarded `get_text_with_timeout`, even though Auto mode (the only caller)
never runs while Offline is active -- the guard costs nothing extra to also
cover it, and it means the probe can never become an accidental bypass if
that ever changed.

## What it cannot guarantee

- **Connectors and the update checker are not built yet.** The Modes table
  calls for both to be disabled *outright* in Offline mode (stricter than
  the loopback-only rule providers get), because a connector's own HTTP
  needs might not funnel through `provider/common.rs`'s guard the same way.
  Until they exist, this is a statement about a future obligation, not
  something Offline mode enforces today -- there is simply nothing else in
  the crate that opens a network connection outside `src/provider/`.
- **Awareness does not exist yet.** There is nothing to stop.
- **The guard is application-level, not OS-level.** It refuses inside
  Wingman's own request-building code, before a socket call. It does not
  use a Windows Filtering Platform callout, a firewall rule, or any other
  OS-enforced network isolation. A bug that reached the network some other
  way -- bypassing `provider/common.rs` entirely -- would not be caught by
  this guard; the structural test above is what keeps that from happening
  today, not a kernel-level backstop.
- **DNS is never resolved by the classifier, which is a feature, not a
  gap** -- but it also means a URL that only *becomes* non-loopback after
  DNS resolution (impossible for a literal IP, but relevant if a future
  config allowed a real hostname to be pointed at 127.0.0.1 in `/etc/hosts`
  or its Windows equivalent) is classified purely by the string in the URL,
  not by where a lookup would actually send the packet. Every loopback
  `base_url` this crate ships or documents is a literal IP, not a hostname,
  so this does not affect Wingman's own defaults.

## How to verify this yourself

The guard is a small, self-contained function; reading
`src/provider/common.rs`'s `offline_guard` and `src/mode.rs`'s
`classify_host` is the most direct check, alongside the unit tests already
listed above (`offline_guard_refuses_a_non_loopback_url_before_the_transport_runs`,
`offline_guard_allows_a_loopback_url_while_offline`,
`offline_guard_is_inert_outside_offline_mode`, and `classify_host`'s own
table of loopback/localhost/trick-URL cases).

To verify on a running build rather than by reading code:

1. Set Mode to **Offline** from the tray.
2. Configure a cloud provider (OpenAI, Anthropic or Gemini) with a real key,
   so there is something for a request to try reaching.
3. Start a capture before pressing the hotkey:
   - **pktmon** (built into Windows 11): `pktmon start --etw -p 0` in an
     elevated terminal, or use `pktmon` with a filter on the app's process;
     `pktmon stop` when done, then `pktmon format` the `.etl` output to
     inspect captured frames.
   - **Resource Monitor** (`resmon.exe`): the **Network** tab's "Processes
     with Network Activity" and "TCP Connections" panes; filter or watch for
     `wingman.exe` (or `copilot-ask.exe` pre-rename) and confirm no
     connection to anything but `127.0.0.1`/`[::1]` appears for the duration
     of the key press.
4. Press the hotkey (or **Ask now**). Expect the card to show the Offline
   guard's own error text (naming either "blocked a request to localhost"
   or "blocked a request to a non-local address"), and expect the capture to
   show **zero** packets leaving the loopback interface for the configured
   cloud provider's host.
5. Repeat with Mode set to Auto or Cloud and the same provider: this time
   the capture should show the real outbound connection, confirming the
   capture method itself is working (a capture that shows nothing in both
   cases proves nothing).

This is the same kind of check `working-an-issue`'s "done when" convention
asks for: a falsifiable, one-command-or-one-screen observation, not "the
code looks right."
