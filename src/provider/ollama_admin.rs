//! Ollama admin/discovery surface: who is actually listening on the Ollama
//! port (#15), and model discovery, vision capability, GPU/CPU status and
//! pull-with-progress (#14). Distinct from `ollama.rs`, which is the
//! `Provider` impl that sends chat completions -- nothing in this module
//! ever calls `/api/chat`.
//!
//! # #15: health check
//!
//! `query_ollama_health` resolves the process listening on a given port via
//! two **read-only** Win32 queries: `GetExtendedTcpTable` (who owns the
//! listening socket) and a Toolhelp32 snapshot (that process's parent).
//! Nothing here starts, stops, or signals any process -- see `classify`'s
//! doc comment for the MEASURED result on this machine and why the
//! parent-process signal is trusted over the install-directory one.
//!
//! # #14: model discovery, capabilities, GPU/CPU, pull
//!
//! `list_tags`/`show_capabilities`/`ps` are plain blocking HTTP calls (same
//! `ureq`, same 127.0.0.1-only rule as `ollama.rs`) and are cheap enough to
//! call synchronously when a UI surface opens (CLAUDE.md rule 5: discovery
//! happens on demand, never on a timer). `pull` is the one exception -- a
//! real download can run for minutes, so it must be called from a worker
//! thread, never the UI thread; see its own doc comment.

use std::io::BufRead;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::json;

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, MIB_TCPTABLE_OWNER_PID, TCP_TABLE_OWNER_PID_LISTENER,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};

use super::common;
use super::ollama::is_vision_model;

// ===========================================================================
// #15: health check -- who is actually listening on the Ollama port
// ===========================================================================

/// Whether the listening process looks like the stock Ollama tray app's
/// spawned server (CPU only -- CLAUDE.md's "Stock Ollama's tray app steals
/// port 11434" pitfall) or something else (a manually started server,
/// possibly with `OLLAMA_IGPU_ENABLE=1` set so the Arc iGPU is actually
/// used).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenerKind {
    StockTrayServer,
    Other,
}

/// Result of resolving whoever (if anyone) is listening on the Ollama port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OllamaHealth {
    NotListening,
    Listening {
        pid: u32,
        /// Empty when the process's image path could not be read (e.g. it
        /// exited between the TCP-table snapshot and the query, or access
        /// was denied) -- still `Listening`, just without a path to show.
        image_path: String,
        kind: ListenerKind,
    },
}

impl OllamaHealth {
    /// A single-line, user-facing message (CLAUDE.md rule 11: no em dashes
    /// in user-facing strings).
    pub fn message(&self) -> String {
        match self {
            OllamaHealth::NotListening => "Ollama is not running.".to_string(),
            OllamaHealth::Listening {
                kind: ListenerKind::StockTrayServer,
                ..
            } => "Another Ollama is listening: the stock tray app, which runs on CPU only. \
                  Quit it from the Windows tray so Wingman can use a real server instead."
                .to_string(),
            OllamaHealth::Listening {
                kind: ListenerKind::Other,
                image_path,
                ..
            } => {
                if image_path.is_empty() {
                    "Ollama is running.".to_string()
                } else {
                    format!("Ollama is running: {image_path}.")
                }
            }
        }
    }
}

/// Resolves whoever is listening on `port` (pass 11434 for the default
/// Ollama port) using only read-only Win32 queries. Never starts, kills, or
/// otherwise signals any process.
pub fn query_ollama_health(port: u16) -> OllamaHealth {
    let Some(pid) = tcp_listener_pid(port) else {
        return OllamaHealth::NotListening;
    };
    let image_path = process_image_path(pid).unwrap_or_default();
    let parent_image_path = parent_pid(pid).and_then(process_image_path);
    let kind = classify(&image_path, parent_image_path.as_deref());
    OllamaHealth::Listening {
        pid,
        image_path,
        kind,
    }
}

/// Pure classification: given the listening process's own image path and
/// (if resolvable) its parent's, decide whether this looks like the stock
/// tray-spawned server. No Win32 calls -- unit-tested directly with
/// synthetic inputs.
///
/// MEASURED 2026-09-17 on this machine: the real Ollama listener on
/// 127.0.0.1:11434 had image path
/// `C:\Users\raaif\AppData\Local\Programs\Ollama\ollama.exe` (command line
/// `...\ollama.exe serve`) and parent image path
/// `C:\Users\raaif\AppData\Local\Programs\Ollama\ollama app.exe` -- both
/// signals agreed. The parent-process signal is trusted first and the
/// install-directory check is only a fallback for when the parent can't be
/// resolved: a manually started `ollama.exe serve` lives under the exact
/// same default install directory as the tray-spawned one, so the
/// directory alone cannot tell them apart (THEORY (unverified): unmeasured
/// on this machine, since no manually started server was running to check
/// against).
pub fn classify(image_path: &str, parent_image_path: Option<&str>) -> ListenerKind {
    if let Some(parent) = parent_image_path {
        return if ends_with_path_component(parent, "ollama app.exe") {
            ListenerKind::StockTrayServer
        } else {
            ListenerKind::Other
        };
    }
    if image_path
        .to_ascii_lowercase()
        .contains(r"\programs\ollama\")
    {
        ListenerKind::StockTrayServer
    } else {
        ListenerKind::Other
    }
}

fn ends_with_path_component(path: &str, component: &str) -> bool {
    let needle = format!("\\{}", component.to_ascii_lowercase());
    path.to_ascii_lowercase().ends_with(&needle)
}

/// `GetExtendedTcpTable(..., TCP_TABLE_OWNER_PID_LISTENER, ...)`: finds the
/// owning PID of whichever LISTENING IPv4 socket is bound to `port`,
/// regardless of local address (0.0.0.0 or 127.0.0.1) -- Wingman only ever
/// connects via 127.0.0.1, so any listener on this port is the one that
/// would actually receive that connection.
fn tcp_listener_pid(port: u16) -> Option<u32> {
    const AF_INET: u32 = 2;

    unsafe {
        let mut size: u32 = 0;
        // First call: no buffer, just asks for the required size. The
        // return code here is expected to be an error (buffer too small);
        // only `size` is used, so it fine to ignore.
        let _ = GetExtendedTcpTable(
            None,
            &mut size,
            false,
            AF_INET,
            TCP_TABLE_OWNER_PID_LISTENER,
            0,
        );
        if size == 0 {
            return None;
        }

        let mut buf = vec![0u8; size as usize];
        let ret = GetExtendedTcpTable(
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            &mut size,
            false,
            AF_INET,
            TCP_TABLE_OWNER_PID_LISTENER,
            0,
        );
        if ret != 0 {
            // Not NO_ERROR -- e.g. the table grew between the two calls.
            // A health check is best-effort: report "can't tell" rather
            // than retrying or failing loudly.
            return None;
        }

        let table = &*(buf.as_ptr() as *const MIB_TCPTABLE_OWNER_PID);
        let count = table.dwNumEntries as usize;
        // `table.table` is declared as a 1-element array (the classic C
        // variable-length-struct trick) -- the real row data for all
        // `count` entries follows contiguously in the same allocation.
        let rows = std::slice::from_raw_parts(table.table.as_ptr(), count);

        for row in rows {
            // dwLocalPort is in network byte order, but only the low 16
            // bits are meaningful (MSDN).
            let local_port = u16::from_be((row.dwLocalPort & 0xFFFF) as u16);
            if local_port == port {
                return Some(row.dwOwningPid);
            }
        }
        None
    }
}

/// `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION) + QueryFullProcessImageNameW`.
/// `None` on any failure (process exited, access denied, ...).
fn process_image_path(pid: u32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut size = buf.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(handle);
        if result.is_err() {
            return None;
        }
        Some(String::from_utf16_lossy(&buf[..size as usize]))
    }
}

/// `CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS)` + linear scan for `pid`'s
/// `th32ParentProcessID`. `None` if the snapshot fails or `pid` isn't found
/// in it (already exited).
fn parent_pid(pid: u32) -> Option<u32> {
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut found = None;
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                if entry.th32ProcessID == pid {
                    found = Some(entry.th32ParentProcessID);
                    break;
                }
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
        found
    }
}

// ===========================================================================
// #14: model discovery, vision capability, GPU/CPU, pull progress
// ===========================================================================

/// Parses the port out of a base url like `http://127.0.0.1:11434` (with or
/// without a trailing slash). `None` if there is no explicit `:port` or it
/// doesn't parse as a `u16` -- callers should fall back to the default
/// (11434) in that case, same as an omitted port would mean for a real URL.
pub fn port_from_base_url(base_url: &str) -> Option<u16> {
    let without_scheme = base_url
        .split_once("://")
        .map_or(base_url, |(_, rest)| rest);
    let host_port = without_scheme.trim_end_matches('/');
    let (_, port_str) = host_port.rsplit_once(':')?;
    port_str.parse().ok()
}

/// Local discovery calls (`/api/tags`, `/api/show`, `/api/ps`) are meant to
/// run synchronously when a UI surface opens (CLAUDE.md rule 5), so their
/// timeouts stay short: a hung/wedged server should not visibly stall
/// Settings opening.
const DISCOVERY_CONNECT_TIMEOUT: Duration = Duration::from_millis(800);
const DISCOVERY_TOTAL_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct TagsDetails {
    #[serde(default)]
    pub family: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TagsModel {
    pub name: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub details: TagsDetails,
    /// Present on 0.34+ (MEASURED 2026-09-17: every entry in this
    /// machine's live `/api/tags` response carries one); used directly
    /// when non-empty. See [`vision_from_tags_entry`].
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct TagsResponse {
    #[serde(default)]
    models: Vec<TagsModel>,
}

/// Whether a `/api/tags` entry supports vision: the live `capabilities`
/// array when the server returned one, else the static
/// [`is_vision_model`] allowlist fallback (same fallback `/api/show`
/// uses -- see [`show_capabilities`]).
pub fn vision_from_tags_entry(model: &TagsModel) -> bool {
    if !model.capabilities.is_empty() {
        model.capabilities.iter().any(|c| c == "vision")
    } else {
        is_vision_model(&model.name)
    }
}

/// `GET /api/tags` -- every model pulled locally.
pub fn list_tags(base_url: &str) -> Result<Vec<TagsModel>> {
    let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
    let body = get_body(&url, "ollama")?;
    let parsed: TagsResponse =
        serde_json::from_str(&body).context("ollama: /api/tags response is not valid JSON")?;
    Ok(parsed.models)
}

/// Not read from any live UI path yet -- Settings' one status line gets its
/// vision counts from `/api/tags`' own `capabilities` array
/// ([`vision_from_tags_entry`]), which is enough for that surface and one
/// fewer request. `show_capabilities` exists because #14 calls for a typed
/// `/api/show` client regardless (a future per-model detail view wants more
/// than `/api/tags` carries, e.g. `families`/`parameter_size`), so it is
/// built and tested now rather than left undone -- same "reserved for a
/// consumer that doesn't exist yet" shape as `Caps`/`Provider::capabilities`
/// in `provider/mod.rs`. A follow-up issue (referencing #51's upcoming
/// WebView2 settings window) covers wiring a real caller.
#[allow(dead_code)]
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShowCapabilities {
    pub capabilities: Vec<String>,
    pub vision: bool,
}

#[derive(Debug, Deserialize)]
struct ShowResponse {
    #[serde(default)]
    capabilities: Vec<String>,
}

/// `POST /api/show` -- capabilities for one model. Falls back to the
/// static [`is_vision_model`] allowlist when the response carries no
/// `capabilities` array (an older Ollama). See [`ShowCapabilities`]'s doc
/// comment for why nothing calls this outside tests yet.
#[allow(dead_code)]
pub fn show_capabilities(base_url: &str, model: &str) -> Result<ShowCapabilities> {
    let url = format!("{}/api/show", base_url.trim_end_matches('/'));
    let body = json!({"model": model});
    let response_body = common::post_json(
        &url,
        &[("Content-Type", "application/json")],
        &body,
        DISCOVERY_TOTAL_TIMEOUT,
        "ollama",
    )?;
    parse_show_response(&response_body, model)
}

fn parse_show_response(body: &str, model: &str) -> Result<ShowCapabilities> {
    let parsed: ShowResponse =
        serde_json::from_str(body).context("ollama: /api/show response is not valid JSON")?;
    if !parsed.capabilities.is_empty() {
        let vision = parsed.capabilities.iter().any(|c| c == "vision");
        Ok(ShowCapabilities {
            capabilities: parsed.capabilities,
            vision,
        })
    } else {
        Ok(ShowCapabilities {
            capabilities: Vec::new(),
            vision: is_vision_model(model),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PsEntry {
    pub name: String,
    #[serde(default)]
    pub size_vram: u64,
}

#[derive(Debug, Deserialize)]
struct PsResponse {
    #[serde(default)]
    models: Vec<PsEntry>,
}

/// `GET /api/ps` -- currently loaded models.
pub fn ps(base_url: &str) -> Result<Vec<PsEntry>> {
    let url = format!("{}/api/ps", base_url.trim_end_matches('/'));
    let body = get_body(&url, "ollama")?;
    let parsed: PsResponse =
        serde_json::from_str(&body).context("ollama: /api/ps response is not valid JSON")?;
    Ok(parsed.models)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuStatus {
    Gpu,
    Cpu,
    NotLoaded,
}

impl GpuStatus {
    /// Short label ("GPU"/"CPU"/"not loaded"). `settings.rs`'s status line
    /// builds its own full sentence per variant instead of using this
    /// directly; kept as the terse form for a future compact surface (e.g.
    /// a tray tooltip) that wants the label without the sentence around it.
    #[allow(dead_code)]
    pub fn label(&self) -> &'static str {
        match self {
            GpuStatus::Gpu => "GPU",
            GpuStatus::Cpu => "CPU",
            GpuStatus::NotLoaded => "not loaded",
        }
    }
}

/// Classifies `model`'s current run state from a `/api/ps` listing.
/// `size_vram > 0` is the ONLY oracle for GPU use on this hardware
/// (CLAUDE.md rule 6 / the Arc iGPU pitfall: `ollama ps`'s own PROCESSOR
/// column has been MEASURED mislabelling a real GPU run as CPU) -- this
/// reads the field directly rather than going through the CLI.
pub fn gpu_status_for(entries: &[PsEntry], model: &str) -> GpuStatus {
    match entries.iter().find(|e| e.name == model) {
        None => GpuStatus::NotLoaded,
        Some(e) if e.size_vram > 0 => GpuStatus::Gpu,
        Some(_) => GpuStatus::Cpu,
    }
}

/// A blocking GET, mirroring `common::post_json`'s shape but for the
/// no-body discovery endpoints (`/api/tags`, `/api/ps`) that don't fit
/// that helper's POST-only signature.
fn get_body(url: &str, tag: &str) -> Result<String> {
    let mut response = ureq::get(url)
        .config()
        .http_status_as_error(false)
        .timeout_connect(Some(DISCOVERY_CONNECT_TIMEOUT))
        .timeout_global(Some(DISCOVERY_TOTAL_TIMEOUT))
        .build()
        .call()
        .map_err(|e| anyhow!("{tag}: transport error: {e}"))?;

    let status = response.status();
    let body_text = response
        .body_mut()
        .read_to_string()
        .with_context(|| format!("{tag}: failed to read response body"))?;

    if !status.is_success() {
        let truncated: String = body_text.chars().take(300).collect();
        return Err(anyhow!("{tag}: HTTP {status}: {truncated}"));
    }
    Ok(body_text)
}

/// Not called from any live path yet: pulling a model needs a worker-thread
/// caller and a progress UI, neither of which exists (a follow-up issue,
/// referencing #51's upcoming WebView2 settings window, covers building
/// that surface). Built and tested now per #14's "typed client functions"
/// ask -- same "reserved for a consumer that doesn't exist yet" shape as
/// `ShowCapabilities` above and `Caps`/`Provider::capabilities` in
/// `provider/mod.rs`.
///
/// One line of `/api/pull`'s newline-delimited JSON progress stream.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PullProgress {
    // `#[serde(default)]`: an error line (`{"error": "..."}`) carries no
    // `status` at all -- MEASURED (from Ollama's documented `/api/pull`
    // response shape): without this, that line fails to deserialize
    // entirely and `parse_pull_line` silently drops it as "malformed",
    // which would swallow the one line that must abort the pull. A test
    // (`stream_pull_progress_stops_on_an_inline_error_line`) caught this by
    // asserting `unwrap_err()` where the code returned `Ok`.
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub completed: u64,
    #[serde(default)]
    pub total: u64,
    #[serde(default)]
    pub error: Option<String>,
}

#[allow(dead_code)]
impl PullProgress {
    /// 0.0..=1.0, or `None` when the server hasn't reported a total yet
    /// (early statuses like "pulling manifest" carry no completed/total).
    pub fn fraction(&self) -> Option<f32> {
        if self.total == 0 {
            None
        } else {
            Some(self.completed as f32 / self.total as f32)
        }
    }
}

/// Parses one line from `/api/pull`'s streamed response. `None` for a
/// blank line (the stream can end in one) or a line that isn't valid JSON
/// for `PullProgress` -- a single malformed line (e.g. a truncated final
/// chunk from a dropped connection) degrades to "no update" rather than
/// aborting the whole pull.
#[allow(dead_code)]
pub fn parse_pull_line(line: &str) -> Option<PullProgress> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    serde_json::from_str(trimmed).ok()
}

/// Reads `/api/pull`'s newline-delimited JSON progress stream from `reader`
/// and calls `on_progress` for each parsed line. Stops with an `Err` the
/// moment a line carries `"error"` -- Ollama reports a pull failure inline
/// in the stream (e.g. an unknown model name), not as an HTTP error status.
///
/// This is a download-progress stream, not model output -- CLAUDE.md's "no
/// chat, no streaming" rule is about conversational completions, and
/// nothing here surfaces text the model wrote.
#[allow(dead_code)]
pub fn stream_pull_progress<R: BufRead>(
    reader: R,
    mut on_progress: impl FnMut(&PullProgress),
) -> Result<()> {
    for line in reader.lines() {
        let line = line.context("ollama: failed to read /api/pull response")?;
        if let Some(progress) = parse_pull_line(&line) {
            if let Some(err) = &progress.error {
                return Err(anyhow!("ollama: pull failed: {err}"));
            }
            on_progress(&progress);
        }
    }
    Ok(())
}

/// `POST /api/pull` with `stream: true`, feeding progress to `on_progress`
/// as it arrives.
///
/// **Callers must run this on a worker thread, never the UI thread**: a
/// real model pull can run for minutes and this function blocks for the
/// whole download (mirrors the existing worker-thread pattern
/// `App::ask` uses in `app.rs` for a completion call). Nothing in this
/// function spawns a thread itself.
#[allow(dead_code)]
pub fn pull(base_url: &str, model: &str, on_progress: impl FnMut(&PullProgress)) -> Result<()> {
    let url = format!("{}/api/pull", base_url.trim_end_matches('/'));
    let body = json!({"model": model, "stream": true});

    let mut response = ureq::post(&url)
        .config()
        .http_status_as_error(false)
        .timeout_connect(Some(DISCOVERY_CONNECT_TIMEOUT))
        // Deliberately no total timeout: a real pull can legitimately run
        // for minutes. `stream_pull_progress` still returns promptly on an
        // inline error line or a dropped connection.
        .build()
        .send_json(&body)
        .map_err(|e| anyhow!("ollama: transport error: {e}"))?;

    let status = response.status();
    if !status.is_success() {
        let body_text = response.body_mut().read_to_string().unwrap_or_default();
        let truncated: String = body_text.chars().take(300).collect();
        return Err(anyhow!("ollama: HTTP {status}: {truncated}"));
    }

    let reader = std::io::BufReader::new(response.body_mut().as_reader());
    stream_pull_progress(reader, on_progress)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // -- classify (#15, pure) --------------------------------------------

    #[test]
    fn classify_trusts_stock_tray_parent_regardless_of_directory() {
        let kind = classify(
            r"C:\Somewhere\Else\ollama.exe",
            Some(r"C:\Users\raaif\AppData\Local\Programs\Ollama\ollama app.exe"),
        );
        assert_eq!(kind, ListenerKind::StockTrayServer);
    }

    #[test]
    fn classify_is_case_insensitive_on_the_parent_name() {
        let kind = classify(r"C:\x\ollama.exe", Some(r"C:\X\OLLAMA APP.EXE"));
        assert_eq!(kind, ListenerKind::StockTrayServer);
    }

    #[test]
    fn classify_trusts_a_different_parent_over_the_install_directory() {
        // MEASURED 2026-09-17 setup mirrored here: same install directory
        // as the real stock server, but a different parent (e.g. a shell
        // the user launched `ollama serve` from) must NOT be classified as
        // the stock tray server.
        let kind = classify(
            r"C:\Users\raaif\AppData\Local\Programs\Ollama\ollama.exe",
            Some(r"C:\Windows\System32\cmd.exe"),
        );
        assert_eq!(kind, ListenerKind::Other);
    }

    #[test]
    fn classify_falls_back_to_directory_when_parent_is_unresolvable() {
        let kind = classify(
            r"C:\Users\raaif\AppData\Local\Programs\Ollama\ollama.exe",
            None,
        );
        assert_eq!(kind, ListenerKind::StockTrayServer);
    }

    #[test]
    fn classify_reports_other_for_an_unrelated_directory_with_no_parent_info() {
        let kind = classify(r"C:\dev\my-own-ollama-build\ollama.exe", None);
        assert_eq!(kind, ListenerKind::Other);
    }

    // -- OllamaHealth::message (#15) --------------------------------------

    #[test]
    fn message_not_listening_is_plain() {
        assert_eq!(
            OllamaHealth::NotListening.message(),
            "Ollama is not running."
        );
    }

    #[test]
    fn message_stock_tray_server_names_it() {
        let health = OllamaHealth::Listening {
            pid: 123,
            image_path: r"C:\...\ollama.exe".to_string(),
            kind: ListenerKind::StockTrayServer,
        };
        let msg = health.message();
        assert!(msg.contains("stock tray app"), "{msg}");
        assert!(msg.contains("CPU"), "{msg}");
    }

    #[test]
    fn message_other_listener_includes_image_path() {
        let health = OllamaHealth::Listening {
            pid: 123,
            image_path: r"C:\srv\ollama.exe".to_string(),
            kind: ListenerKind::Other,
        };
        assert!(health.message().contains(r"C:\srv\ollama.exe"));
    }

    #[test]
    fn no_health_message_contains_an_em_dash() {
        // CLAUDE.md rule 11: no em dashes in user-facing strings.
        let messages = [
            OllamaHealth::NotListening.message(),
            OllamaHealth::Listening {
                pid: 1,
                image_path: String::new(),
                kind: ListenerKind::StockTrayServer,
            }
            .message(),
            OllamaHealth::Listening {
                pid: 1,
                image_path: "x".to_string(),
                kind: ListenerKind::Other,
            }
            .message(),
        ];
        for msg in messages {
            assert!(!msg.contains('\u{2014}'), "em dash in {msg:?}");
        }
    }

    // -- Win32 query (#15, one smoke test, no Ollama required) -----------

    #[test]
    fn query_ollama_health_finds_our_own_listener() {
        // Deterministic, network-free (loopback only) smoke test for the
        // real Win32 query: bind a listener ourselves, ask for it back.
        // Does not assume Ollama is installed or running.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
        let port = listener.local_addr().expect("local_addr").port();

        let health = query_ollama_health(port);
        match health {
            OllamaHealth::Listening { pid, .. } => {
                assert_eq!(
                    pid,
                    std::process::id(),
                    "should resolve back to this test process"
                );
            }
            OllamaHealth::NotListening => {
                panic!("expected Listening for our own bound port {port}")
            }
        }
        drop(listener);
    }

    /// Live, opt-in check against whatever is actually running on the real
    /// Ollama port on this machine right now (not run by default, same
    /// convention as `ollama.rs`'s `ollama_live_*` check: `cargo test
    /// ollama_admin::tests::query_ollama_health_classifies_the_real_listener
    /// -- --ignored --nocapture`). MEASURED 2026-09-17 via PowerShell
    /// (`Get-NetTCPConnection`/`Get-CimInstance Win32_Process`) that the
    /// real listener was pid 13980, image
    /// `...\Programs\Ollama\ollama.exe`, parent pid 25324 image
    /// `...\Programs\Ollama\ollama app.exe` -- this test re-derives the
    /// same classification through this crate's own Win32 code path
    /// instead of trusting the PowerShell measurement alone.
    #[test]
    #[ignore]
    fn query_ollama_health_classifies_the_real_listener() {
        let health = query_ollama_health(11434);
        eprintln!("query_ollama_health_classifies_the_real_listener: {health:?}");
        eprintln!("message: {}", health.message());
        match health {
            OllamaHealth::NotListening => {
                panic!("expected something listening on 11434 on this machine")
            }
            OllamaHealth::Listening { .. } => {}
        }
    }

    #[test]
    fn query_ollama_health_reports_not_listening_after_the_socket_closes() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
        let port = listener.local_addr().expect("local_addr").port();
        drop(listener);

        // Best-effort: on some CI/sandbox configurations a just-closed port
        // can briefly still show in TIME_WAIT for a *connection*, but there
        // is no LISTENER left, which is what TCP_TABLE_OWNER_PID_LISTENER
        // reports on -- so this should be immediate and reliable.
        assert_eq!(query_ollama_health(port), OllamaHealth::NotListening);
    }

    // -- port_from_base_url (#14, pure) -----------------------------------

    #[test]
    fn port_from_base_url_parses_the_default() {
        assert_eq!(port_from_base_url("http://127.0.0.1:11434"), Some(11434));
    }

    #[test]
    fn port_from_base_url_tolerates_a_trailing_slash() {
        assert_eq!(port_from_base_url("http://127.0.0.1:11434/"), Some(11434));
    }

    #[test]
    fn port_from_base_url_is_none_without_an_explicit_port() {
        assert_eq!(port_from_base_url("http://127.0.0.1"), None);
    }

    #[test]
    fn port_from_base_url_is_none_for_garbage() {
        assert_eq!(port_from_base_url("not a url"), None);
        assert_eq!(port_from_base_url(""), None);
        assert_eq!(port_from_base_url("http://127.0.0.1:notaport"), None);
    }

    // -- /api/tags (#14, recorded fixture) --------------------------------

    #[test]
    fn list_tags_parses_a_recorded_fixture() {
        let body = fs::read_to_string("tests/fixtures/ollama_tags.json")
            .expect("fixture file should exist");
        let parsed: TagsResponse = serde_json::from_str(&body).expect("should parse");
        let names: Vec<&str> = parsed.models.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"gemma3:4b"));
        assert!(names.contains(&"llama3.1:8b"));
    }

    #[test]
    fn vision_from_tags_entry_uses_the_live_capabilities_array() {
        let body = fs::read_to_string("tests/fixtures/ollama_tags.json")
            .expect("fixture file should exist");
        let parsed: TagsResponse = serde_json::from_str(&body).expect("should parse");

        let gemma = parsed
            .models
            .iter()
            .find(|m| m.name == "gemma3:4b")
            .expect("gemma3:4b in fixture");
        assert!(vision_from_tags_entry(gemma));

        let llama = parsed
            .models
            .iter()
            .find(|m| m.name == "llama3.1:8b")
            .expect("llama3.1:8b in fixture");
        assert!(!vision_from_tags_entry(llama));

        let deepseek = parsed
            .models
            .iter()
            .find(|m| m.name == "deepseek-r1:14b")
            .expect("deepseek-r1:14b in fixture");
        assert!(!vision_from_tags_entry(deepseek));
    }

    #[test]
    fn vision_from_tags_entry_falls_back_to_the_static_list_when_capabilities_is_absent() {
        // Synthetic: an older server that doesn't send `capabilities` at
        // all (the field defaults to empty via `#[serde(default)]`).
        let model = TagsModel {
            name: "gemma3:4b".to_string(),
            size: 0,
            details: TagsDetails::default(),
            capabilities: Vec::new(),
        };
        assert!(vision_from_tags_entry(&model));

        let text_only = TagsModel {
            name: "llama3.1:8b".to_string(),
            ..model
        };
        assert!(!vision_from_tags_entry(&text_only));
    }

    // -- /api/show (#14, recorded fixture) --------------------------------

    #[test]
    fn show_capabilities_parses_a_recorded_fixture() {
        let body = fs::read_to_string("tests/fixtures/ollama_show_gemma3_4b.json")
            .expect("fixture file should exist");
        let caps = parse_show_response(&body, "gemma3:4b").expect("should parse");
        assert!(caps.vision);
        assert!(caps.capabilities.contains(&"vision".to_string()));
        assert!(caps.capabilities.contains(&"completion".to_string()));
    }

    #[test]
    fn show_capabilities_falls_back_to_static_list_when_capabilities_field_is_missing() {
        // Synthetic: an older Ollama's /api/show response, no
        // "capabilities" key at all.
        let body = r#"{"details": {"family": "gemma3"}}"#;
        let caps = parse_show_response(body, "gemma3:4b").expect("should parse");
        assert!(caps.vision, "gemma3:4b is on the static allowlist");
        assert!(caps.capabilities.is_empty());
    }

    #[test]
    fn show_capabilities_fallback_reports_no_vision_for_a_text_only_model() {
        let body = r#"{"details": {"family": "qwen3"}}"#;
        let caps = parse_show_response(body, "qwen3:14b").expect("should parse");
        assert!(!caps.vision);
    }

    #[test]
    fn show_capabilities_rejects_invalid_json() {
        let err = parse_show_response("not json", "gemma3:4b").unwrap_err();
        assert!(err.to_string().contains("not valid JSON"));
    }

    // -- /api/ps (#14, recorded fixture + synthetic GPU/CPU) -------------

    #[test]
    fn ps_parses_a_recorded_empty_fixture() {
        // MEASURED 2026-09-17: no model was loaded on this machine at
        // capture time -- a real, recorded "nothing loaded" response.
        let body = fs::read_to_string("tests/fixtures/ollama_ps_empty.json")
            .expect("fixture file should exist");
        let parsed: PsResponse = serde_json::from_str(&body).expect("should parse");
        assert!(parsed.models.is_empty());
    }

    #[test]
    fn gpu_status_for_reports_not_loaded_when_absent_from_ps() {
        let entries: Vec<PsEntry> = Vec::new();
        assert_eq!(gpu_status_for(&entries, "gemma3:4b"), GpuStatus::NotLoaded);
    }

    #[test]
    fn gpu_status_for_reports_gpu_when_size_vram_is_positive() {
        // Synthetic: size_vram > 0 is the ONLY oracle (CLAUDE.md rule 6) --
        // not a real captured response, since capturing a "loaded on GPU"
        // response would require actually running a completion.
        let entries = vec![PsEntry {
            name: "gemma3:4b".to_string(),
            size_vram: 3_000_000_000,
        }];
        assert_eq!(gpu_status_for(&entries, "gemma3:4b"), GpuStatus::Gpu);
    }

    #[test]
    fn gpu_status_for_reports_cpu_when_loaded_with_zero_vram() {
        let entries = vec![PsEntry {
            name: "gemma3:4b".to_string(),
            size_vram: 0,
        }];
        assert_eq!(gpu_status_for(&entries, "gemma3:4b"), GpuStatus::Cpu);
    }

    #[test]
    fn gpu_status_label_text() {
        assert_eq!(GpuStatus::Gpu.label(), "GPU");
        assert_eq!(GpuStatus::Cpu.label(), "CPU");
        assert_eq!(GpuStatus::NotLoaded.label(), "not loaded");
    }

    // -- /api/pull progress parsing (#14, synthetic -- no real pulls) ----

    #[test]
    fn parse_pull_line_parses_a_status_only_line() {
        let progress = parse_pull_line(r#"{"status":"pulling manifest"}"#).expect("should parse");
        assert_eq!(progress.status, "pulling manifest");
        assert_eq!(progress.completed, 0);
        assert_eq!(progress.total, 0);
        assert!(progress.error.is_none());
    }

    #[test]
    fn parse_pull_line_parses_a_progress_line_with_counts() {
        let progress = parse_pull_line(r#"{"status":"downloading","completed":512,"total":1024}"#)
            .expect("should parse");
        assert_eq!(progress.completed, 512);
        assert_eq!(progress.total, 1024);
        assert_eq!(progress.fraction(), Some(0.5));
    }

    #[test]
    fn parse_pull_line_ignores_a_blank_line() {
        assert!(parse_pull_line("").is_none());
        assert!(parse_pull_line("   \n").is_none());
    }

    #[test]
    fn parse_pull_line_degrades_malformed_json_to_none_rather_than_panicking() {
        assert!(parse_pull_line("{not json").is_none());
    }

    #[test]
    fn fraction_is_none_without_a_total() {
        let progress = PullProgress {
            status: "pulling manifest".to_string(),
            completed: 0,
            total: 0,
            error: None,
        };
        assert_eq!(progress.fraction(), None);
    }

    #[test]
    fn stream_pull_progress_calls_back_for_every_line_in_order() {
        let body = "{\"status\":\"pulling manifest\"}\n\
                     {\"status\":\"downloading\",\"completed\":100,\"total\":1000}\n\
                     {\"status\":\"downloading\",\"completed\":1000,\"total\":1000}\n\
                     {\"status\":\"success\"}\n";
        let mut seen: Vec<String> = Vec::new();
        stream_pull_progress(body.as_bytes(), |p| seen.push(p.status.clone()))
            .expect("should succeed");
        assert_eq!(
            seen,
            vec!["pulling manifest", "downloading", "downloading", "success"]
        );
    }

    #[test]
    fn stream_pull_progress_stops_on_an_inline_error_line() {
        let body = "{\"status\":\"pulling manifest\"}\n\
                     {\"error\":\"model 'nope' not found\"}\n\
                     {\"status\":\"should never be reached\"}\n";
        let mut seen: Vec<String> = Vec::new();
        let err =
            stream_pull_progress(body.as_bytes(), |p| seen.push(p.status.clone())).unwrap_err();
        assert!(err.to_string().contains("model 'nope' not found"), "{err}");
        // The line after the error must never reach the callback.
        assert_eq!(seen, vec!["pulling manifest"]);
    }

    #[test]
    fn stream_pull_progress_skips_a_blank_line_between_json_lines() {
        let body = "{\"status\":\"a\"}\n\n{\"status\":\"b\"}\n";
        let mut seen: Vec<String> = Vec::new();
        stream_pull_progress(body.as_bytes(), |p| seen.push(p.status.clone()))
            .expect("should succeed");
        assert_eq!(seen, vec!["a", "b"]);
    }
}
