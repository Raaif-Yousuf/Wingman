//! Issue #328: "Check this PC for local models". Answers one question
//! honestly, on demand and without a benchmark: can this machine run a local
//! model well, and what does it cost on battery?
//!
//! Split per AGENTS.md rule 8: [`verdict`] and the formatting helpers are
//! pure and unit-tested on fixtures; [`gather`] reads the real machine
//! (RAM, DXGI adapters, the battery, Ollama's `/api/ps`) and is checked by
//! hand. Nothing here runs unless the user picks the tray item (rule 5: no
//! timers, no power-change watcher).
//!
//! No benchmark by default: the verdict is built from what was seen, and says
//! so. The timed request (#699) is a separate, opt-in step: one small request
//! with a built-in synthetic image against the installed local model, sent
//! only after the user picks the link on the card ([`TIMING_ACTION_LABEL`]),
//! to Ollama on this PC only.

use crate::provider::ollama::{DEFAULT_KEEP_ALIVE, DEFAULT_NUM_CTX};
use crate::provider::ollama_admin::{self, ListenerKind, OllamaHealth, PsEntry};
use serde_json::{json, Value};
use std::time::Duration;

const GIB: u64 = 1024 * 1024 * 1024;

/// Physical RAM reported by Windows is a little under the marketed size
/// (an "8 GB" laptop reports about 7.6 GiB), so the cut-offs sit between
/// the round numbers (RAM_LOW covers everything marketed as 8 GB or less).
const RAM_LOW: u64 = 9 * GIB;
const RAM_COMFORTABLE: u64 = 15 * GIB;
/// Dedicated video memory at or above this counts as a discrete GPU with
/// room for a small vision model. Below it the adapter is treated as an
/// integrated one that shares system RAM.
const VRAM_DISCRETE: u64 = 3 * GIB;
/// An integrated GPU's dedicated carve-out is small (128 MB on the Arc
/// iGPU); anything with a GiB or more of its own memory is a card.
const IGPU_DEDICATED_MAX: u64 = GIB;
const INTEL_VENDOR_ID: u32 = 0x8086;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuInfo {
    pub name: String,
    /// PCI vendor id from DXGI (0x8086 is Intel).
    pub vendor_id: u32,
    pub dedicated_bytes: u64,
    pub shared_bytes: u64,
}

impl GpuInfo {
    fn is_discrete(&self) -> bool {
        self.dedicated_bytes >= VRAM_DISCRETE
    }

    /// Only an Intel Arc integrated GPU (the Core Ultra kind, which has a
    /// tiny dedicated carve-out and shares system RAM) needs
    /// OLLAMA_IGPU_ENABLE=1. A discrete Arc A-series card has gigabytes of
    /// its own memory and does not.
    fn is_intel_arc_igpu(&self) -> bool {
        self.vendor_id == INTEL_VENDOR_ID
            && self.dedicated_bytes < IGPU_DEDICATED_MAX
            && self.name.to_ascii_lowercase().contains("arc")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OllamaState {
    NotRunning,
    /// The stock tray app owns the port (CPU only, see AGENTS.md pitfalls).
    StockTray,
    /// A real server answers. `on_gpu` comes from `/api/ps`: `Some(true)`
    /// when a loaded model has `size_vram > 0`, `Some(false)` when a loaded
    /// model has none, `None` when no model is loaded (or `/api/ps` failed).
    Running {
        on_gpu: Option<bool>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Power {
    Unknown,
    PluggedIn,
    OnBattery { percent: Option<u8> },
}

/// What the opt-in timed request found (#699).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Measured {
    /// The user has not asked for it. The default.
    NotRun,
    Timed(Timing),
    /// The request could not be made or answered. Plain words for the card.
    Failed(String),
}

/// One timed request, from Ollama's own nanosecond counters (rounded to
/// milliseconds): no streaming, no wall-clock guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timing {
    pub model: String,
    /// Loading the model into memory. Zero when it was already loaded.
    pub load_ms: u64,
    /// Reading the prompt and image: the wait for the first token once the
    /// model is loaded.
    pub prompt_ms: u64,
    pub eval_ms: u64,
    pub eval_count: u64,
    /// `/api/ps` after the request: `size_vram > 0`. None when unreadable.
    pub on_gpu: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineFacts {
    /// None when Windows would not say. An unreadable value is unknown,
    /// never a small number or an empty list: a failed read must not turn
    /// into a confident "little memory" or "no GPU".
    pub ram_bytes: Option<u64>,
    /// Hardware adapters only (the software "Basic Render Driver" is left
    /// out by [`gather`]). None when DXGI could not be read;
    /// Some(empty) means it was read and there is no hardware GPU.
    pub gpus: Option<Vec<GpuInfo>>,
    pub ollama: OllamaState,
    pub power: Power,
    pub measured: Measured,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Not enough could be read to say anything.
    Unknown,
    Good,
    Slow,
    NotRecommended,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub level: Level,
    pub headline: String,
    /// One honest sentence: what was seen and what it means.
    pub line: String,
    /// The hardware numbers behind the verdict.
    pub numbers: String,
    /// Present only on battery.
    pub battery_note: Option<String>,
}

impl Verdict {
    /// Card detail: the sentence, the numbers, then the battery line.
    pub fn detail(&self) -> String {
        let mut out = format!("{} {}", self.line, self.numbers);
        if let Some(note) = &self.battery_note {
            out.push(' ');
            out.push_str(note);
        }
        out
    }
}

/// `/api/ps` -> whether a loaded model runs on the GPU. `size_vram > 0` is
/// the only oracle (AGENTS.md rule 6).
pub fn on_gpu_from_ps(entries: &[PsEntry]) -> Option<bool> {
    if entries.is_empty() {
        None
    } else {
        Some(entries.iter().any(|e| e.size_vram > 0))
    }
}

fn gib_text(bytes: u64) -> String {
    let tenths = (bytes * 10 + GIB / 2) / GIB;
    if tenths % 10 == 0 {
        format!("{} GB", tenths / 10)
    } else {
        format!("{}.{} GB", tenths / 10, tenths % 10)
    }
}

fn best_gpu(gpus: &[GpuInfo]) -> Option<&GpuInfo> {
    gpus.iter()
        .max_by_key(|g| (g.is_discrete(), g.dedicated_bytes, g.shared_bytes))
}

fn numbers_text(f: &MachineFacts) -> String {
    let ram = match f.ram_bytes {
        Some(b) => format!("RAM: {}.", gib_text(b)),
        None => "RAM: could not read.".to_string(),
    };
    let gpu = match f.gpus.as_deref() {
        None => "GPU: could not read.".to_string(),
        Some(gpus) => match best_gpu(gpus) {
            None => "No GPU found.".to_string(),
            Some(g) if g.is_discrete() => format!(
                "GPU: {}, {} of its own memory.",
                g.name,
                gib_text(g.dedicated_bytes)
            ),
            Some(g) => format!(
                "GPU: {}, integrated, shares up to {} of system memory.",
                g.name,
                gib_text(g.shared_bytes.max(g.dedicated_bytes))
            ),
        },
    };
    format!("{ram} {gpu}")
}

fn battery_note(power: Power) -> Option<String> {
    match power {
        Power::OnBattery { percent } => {
            let at = percent.map(|p| format!(" (at {p}%)")).unwrap_or_default();
            Some(format!(
                "On battery{at}, a local model will noticeably shorten battery life."
            ))
        }
        _ => None,
    }
}

/// The verdict from hardware alone (no loaded model to look at). A value
/// that could not be read never counts against the machine: it makes the
/// verdict "unknown" instead.
fn hardware_verdict(f: &MachineFacts) -> (Level, &'static str, String) {
    let Some(gpus) = f.gpus.as_deref() else {
        return (
            Level::Unknown,
            "Local models: could not check",
            "Could not read this PC's GPU, so there is no verdict. Load a model in Ollama and try again."
                .to_string(),
        );
    };
    let gpu = best_gpu(gpus);
    if gpu.is_some_and(GpuInfo::is_discrete) {
        return (
            Level::Good,
            "Local models: should work well",
            "This GPU has room for a small model. Not measured yet: load a model in Ollama to confirm."
                .to_string(),
        );
    }
    let Some(ram) = f.ram_bytes else {
        return (
            Level::Unknown,
            "Local models: could not check",
            "Could not read this PC's memory, so there is no verdict.".to_string(),
        );
    };
    if ram < RAM_LOW {
        (
            Level::NotRecommended,
            "Local models: not recommended",
            "Little memory and no discrete GPU. Use a cloud model.".to_string(),
        )
    } else if gpu.is_some() && ram >= RAM_COMFORTABLE {
        (
            Level::Slow,
            "Local models: should work, slowly",
            "An integrated GPU shares system memory. Not measured yet: load a model in Ollama to confirm it uses the GPU."
                .to_string(),
        )
    } else {
        (
            Level::Slow,
            "Local models: slow",
            "No discrete GPU and modest memory. Single presses may work, but expect waits."
                .to_string(),
        )
    }
}

/// The honest one-line verdict for running local models on this machine.
pub fn verdict(f: &MachineFacts) -> Verdict {
    if let Measured::Timed(t) = &f.measured {
        let (level, headline, line) = timed_verdict(t);
        return Verdict {
            level,
            headline: headline.to_string(),
            line,
            numbers: numbers_text(f),
            battery_note: battery_note(f.power),
        };
    }
    let mut v = hardware_or_ps_verdict(f);
    if let Measured::Failed(why) = &f.measured {
        v.line = format!("{} Timed request failed: {why}", v.line);
    }
    v
}

fn hardware_or_ps_verdict(f: &MachineFacts) -> Verdict {
    let (level, headline, line) = match f.ollama {
        OllamaState::StockTray => (
            Level::Slow,
            "Local models: slow",
            "The stock Ollama tray app is answering, and it runs on the CPU only. \
             Quit it from the Windows tray so a real server can use the GPU."
                .to_string(),
        ),
        OllamaState::Running { on_gpu: Some(true) } => (
            Level::Good,
            "Local models: good",
            "Ollama is running a model on your GPU.".to_string(),
        ),
        OllamaState::Running {
            on_gpu: Some(false),
        } => match f.gpus.as_deref() {
            None => (
                Level::Slow,
                "Local models: slow",
                "Ollama is running a model on the CPU. Could not read this PC's GPU list to say why."
                    .to_string(),
            ),
            Some([]) => (
                Level::NotRecommended,
                "Local models: not recommended",
                "Ollama is running on the CPU and no GPU was found. Use a cloud model.".to_string(),
            ),
            Some(gpus) => {
                let hint = if gpus.iter().any(GpuInfo::is_intel_arc_igpu) {
                    " Intel Arc integrated graphics need OLLAMA_IGPU_ENABLE=1 before Ollama starts."
                } else {
                    " Check the GPU driver and that Ollama can see this adapter."
                };
                (
                    Level::Slow,
                    "Local models: slow",
                    format!(
                        "Ollama is running a model on the CPU although this PC has a GPU.{hint}"
                    ),
                )
            }
        },
        // No loaded model to look at (or Ollama is off): judge by hardware
        // and say it was not measured.
        _ => hardware_verdict(f),
    };
    Verdict {
        level,
        headline: headline.to_string(),
        line,
        numbers: numbers_text(f),
        battery_note: battery_note(f.power),
    }
}

/// Reads the real machine. Checked by hand (rule 8); every part is
/// best-effort, and a failed read becomes the most cautious fact rather than
/// an error (a missing GPU list never produces a "good" verdict).
pub fn gather(ollama_base_url: &str) -> MachineFacts {
    MachineFacts {
        ram_bytes: win::total_ram_bytes(),
        gpus: win::gpus(),
        ollama: ollama_state(ollama_base_url),
        power: win::power(),
        measured: Measured::NotRun,
    }
}

/// The link the PC check card offers (#699). Never run automatically.
pub const TIMING_ACTION_LABEL: &str = "Time a request";

/// Whether the card should offer [`TIMING_ACTION_LABEL`]: an Ollama server
/// answers (the stock tray app too: timing it shows how slow it is), and no
/// timing succeeded yet. A failed run can be retried.
pub fn can_offer_timing(f: &MachineFacts) -> bool {
    f.ollama != OllamaState::NotRunning && !matches!(f.measured, Measured::Timed(_))
}

/// First answer in under this long, and at least [`GOOD_TOKENS_PER_SEC`],
/// counts as good. THEORY (unverified): these cut-offs are a judgment of what
/// feels fine for one press that shows one card; they have not been tuned
/// against user reports.
const GOOD_FIRST_ANSWER_MS: u64 = 4_000;
const GOOD_TOKENS_PER_SEC: f64 = 15.0;
/// Slower than this on either count is not recommended.
const BAD_FIRST_ANSWER_MS: u64 = 15_000;
const BAD_TOKENS_PER_SEC: f64 = 4.0;
/// A load this long means the model was not in memory yet.
const COLD_LOAD_MS: u64 = 500;

fn tokens_per_sec(t: &Timing) -> Option<f64> {
    (t.eval_ms > 0 && t.eval_count > 0).then(|| t.eval_count as f64 * 1000.0 / t.eval_ms as f64)
}

fn secs_text(ms: u64) -> String {
    format!("{:.1} s", ms as f64 / 1000.0)
}

fn timed_verdict(t: &Timing) -> (Level, &'static str, String) {
    let Some(tps) = tokens_per_sec(t) else {
        return (
            Level::Unknown,
            "Local models: unclear",
            format!(
                "Timed one small request on {}, but Ollama reported no speed for it.",
                t.model
            ),
        );
    };
    // A request that had to load the model also pays one-time warm-up in its
    // first answer. MEASURED 2026-10-17 (gemma3:4b, Arc iGPU): 26.2 s to the
    // first answer when cold, 1.3 s the next time, at the same 21 tokens per
    // second. So a cold run is judged on speed alone.
    let cold = t.load_ms >= COLD_LOAD_MS;
    let first_ms = if cold { 0 } else { t.prompt_ms };
    let (level, headline) = if tps < BAD_TOKENS_PER_SEC || first_ms >= BAD_FIRST_ANSWER_MS {
        (Level::NotRecommended, "Local models: not recommended")
    } else if tps >= GOOD_TOKENS_PER_SEC && first_ms <= GOOD_FIRST_ANSWER_MS {
        (Level::Good, "Local models: good")
    } else {
        (Level::Slow, "Local models: slow")
    };
    let load = if cold {
        format!(" ({} to load the model first)", secs_text(t.load_ms))
    } else {
        String::new()
    };
    let rerun = if cold {
        " The first request includes loading, so time it again for the everyday wait."
    } else {
        ""
    };
    let speed = if tps >= 10.0 {
        format!("{tps:.0}")
    } else {
        format!("{tps:.1}")
    };
    let cpu = if t.on_gpu == Some(false) {
        " It ran on the CPU."
    } else {
        ""
    };
    (
        level,
        headline,
        format!(
            "Timed one small request on {}: first answer in {}{load}, {speed} tokens per second.{cpu}{rerun}",
            t.model,
            secs_text(t.prompt_ms)
        ),
    )
}

/// A 128 px gradient, built in: the timed request never sends anything from
/// the user's screen.
fn synthetic_png() -> Vec<u8> {
    let img = image::RgbImage::from_fn(128, 128, |x, y| {
        image::Rgb([(x * 2) as u8, (y * 2) as u8, ((x + y) % 256) as u8])
    });
    let mut out = std::io::Cursor::new(Vec::new());
    // Encoding to memory cannot fail for a valid buffer; an empty image would
    // simply make the timed request fail, which ends in a card.
    let _ = img.write_to(&mut out, image::ImageFormat::Png);
    out.into_inner()
}

/// The request body for one timed request. Same `num_ctx` and `keep_alive`
/// as a real request, so timing does not unload or reload the user's model.
/// AGENTS.md rule 6: `think` explicit, `keep_alive` top level.
fn timing_body(model: &str) -> Value {
    use base64::Engine as _;
    let image = base64::engine::general_purpose::STANDARD.encode(synthetic_png());
    json!({
        "model": model,
        "messages": [{
            "role": "user",
            "content": "Describe this picture in one short sentence.",
            "images": [image]
        }],
        "stream": false,
        "think": false,
        "keep_alive": DEFAULT_KEEP_ALIVE,
        "options": {"num_ctx": DEFAULT_NUM_CTX, "num_predict": 32}
    })
}

fn ms_of(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0) / 1_000_000
}

/// Reads Ollama's `/api/chat` answer. An `error` field or a body with no
/// counters is an `Err` in plain words.
fn parse_timing(body: &str, model: &str, on_gpu: Option<bool>) -> Result<Timing, String> {
    let v: Value = serde_json::from_str(body)
        .map_err(|_| "Ollama sent an answer that is not JSON.".to_string())?;
    if let Some(err) = v.get("error").and_then(Value::as_str) {
        return Err(format!("Ollama said: {err}"));
    }
    if v.get("eval_duration").is_none() || v.get("eval_count").is_none() {
        return Err("Ollama did not report any timings.".to_string());
    }
    Ok(Timing {
        model: model.to_string(),
        load_ms: ms_of(&v, "load_duration"),
        prompt_ms: ms_of(&v, "prompt_eval_duration"),
        eval_ms: ms_of(&v, "eval_duration"),
        eval_count: v.get("eval_count").and_then(Value::as_u64).unwrap_or(0),
        on_gpu,
    })
}

/// Sends the one timed request. Blocks (up to two minutes on a cold load):
/// call it from a worker thread only. Every failure is a
/// [`Measured::Failed`] with words for the card.
pub fn run_timed(base_url: &str, model: &str) -> Measured {
    match try_timed(base_url, model) {
        Ok(t) => Measured::Timed(t),
        Err(why) => Measured::Failed(why),
    }
}

/// Ollama reads a name with no tag as `name:latest`, so `gemma3` finds an
/// installed `gemma3:latest`.
fn same_model(installed: &str, wanted: &str) -> bool {
    installed == wanted || (!wanted.contains(':') && installed == format!("{wanted}:latest"))
}

fn try_timed(base_url: &str, model: &str) -> Result<Timing, String> {
    // Local Ollama only: never a remote host, and never `localhost`.
    if crate::mode::classify_host(base_url) != crate::mode::HostClass::Loopback {
        return Err(
            "Timing only runs against Ollama on this PC. Set the Ollama address to 127.0.0.1."
                .to_string(),
        );
    }
    let tags = ollama_admin::list_tags_with_timeout(base_url, Duration::from_secs(5))
        .map_err(|e| format!("Could not list Ollama's models: {e:#}"))?;
    let Some(entry) = tags.iter().find(|m| same_model(&m.name, model)) else {
        return Err(format!(
            "{model} is not installed in Ollama. Pull it from Settings, then try again."
        ));
    };
    if !ollama_admin::vision_from_tags_entry(entry) {
        return Err(format!(
            "{model} cannot read images, so it cannot be timed. Pick a vision model in Settings."
        ));
    }
    let url = format!("{}/api/chat", base_url.trim_end_matches('/'));
    let body = crate::provider::common::post_json_with_connect_timeout(
        &url,
        &[("Content-Type", "application/json")],
        timing_body(model),
        Duration::from_secs(3),
        Duration::from_secs(120),
        "ollama",
    )
    .map_err(|e| format!("{e:#}"))?;
    let on_gpu = ollama_admin::ps_with_timeout(base_url, Duration::from_secs(2))
        .ok()
        .and_then(|e| on_gpu_from_ps(&e));
    parse_timing(&body, model, on_gpu)
}

/// [`gather`], then (only when asked for) the timed request.
pub fn gather_timed(base_url: &str, model: &str) -> MachineFacts {
    let mut f = gather(base_url);
    f.measured = if f.ollama == OllamaState::NotRunning {
        Measured::Failed("Ollama is not running. Start it, then try again.".to_string())
    } else {
        run_timed(base_url, model)
    };
    f
}

fn ollama_state(base_url: &str) -> OllamaState {
    let port = ollama_admin::port_from_base_url(base_url).unwrap_or(11434);
    match ollama_admin::query_ollama_health(port) {
        OllamaHealth::NotListening => OllamaState::NotRunning,
        OllamaHealth::Listening {
            kind: ListenerKind::StockTrayServer,
            ..
        } => OllamaState::StockTray,
        OllamaHealth::Listening { .. } => {
            let on_gpu = ollama_admin::ps_with_timeout(base_url, std::time::Duration::from_secs(2))
                .ok()
                .and_then(|e| on_gpu_from_ps(&e));
            OllamaState::Running { on_gpu }
        }
    }
}

/// Hardware DXGI adapters, or None when DXGI could not be read. The one
/// adapter enumeration in the crate (`ollama_gpu_fix` reuses it).
pub fn adapters() -> Option<Vec<GpuInfo>> {
    win::gpus()
}

mod win {
    use super::{GpuInfo, Power};
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE,
    };
    use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    /// None when the call fails (or reports zero): unknown, not "no memory".
    pub fn total_ram_bytes() -> Option<u64> {
        let mut st = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        // SAFETY: `st` is a valid, correctly sized MEMORYSTATUSEX.
        match unsafe { GlobalMemoryStatusEx(&mut st) } {
            Ok(()) if st.ullTotalPhys > 0 => Some(st.ullTotalPhys),
            _ => None,
        }
    }

    /// None when DXGI could not be read at all (factory failure, or every
    /// adapter's description failed): unknown, not "no GPU".
    pub fn gpus() -> Option<Vec<GpuInfo>> {
        let mut out = Vec::new();
        let mut failed = false;
        // SAFETY: plain DXGI enumeration; every returned interface is owned
        // and released on drop.
        unsafe {
            let factory = CreateDXGIFactory1::<IDXGIFactory1>().ok()?;
            let mut i = 0;
            while let Ok(adapter) = factory.EnumAdapters1(i) {
                i += 1;
                let Ok(desc) = adapter.GetDesc1() else {
                    failed = true;
                    continue;
                };
                if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                    continue;
                }
                let end = desc
                    .Description
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(desc.Description.len());
                out.push(GpuInfo {
                    name: String::from_utf16_lossy(&desc.Description[..end]),
                    vendor_id: desc.VendorId,
                    dedicated_bytes: desc.DedicatedVideoMemory as u64,
                    shared_bytes: desc.SharedSystemMemory as u64,
                });
            }
        }
        if out.is_empty() && failed {
            None
        } else {
            Some(out)
        }
    }

    pub fn power() -> Power {
        let mut st = SYSTEM_POWER_STATUS::default();
        // SAFETY: `st` is a valid out-pointer.
        if unsafe { GetSystemPowerStatus(&mut st) }.is_err() {
            return Power::Unknown;
        }
        match st.ACLineStatus {
            0 => Power::OnBattery {
                percent: (st.BatteryLifePercent <= 100).then_some(st.BatteryLifePercent),
            },
            1 => Power::PluggedIn,
            _ => Power::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    fn gpu(name: &str, dedicated_gb: u64, shared_gb: u64) -> GpuInfo {
        GpuInfo {
            name: name.to_string(),
            vendor_id: 0x10de,
            dedicated_bytes: dedicated_gb * GIB,
            shared_bytes: shared_gb * GIB,
        }
    }

    fn facts(ram_gb: u64, gpus: Vec<GpuInfo>, ollama: OllamaState, power: Power) -> MachineFacts {
        MachineFacts {
            ram_bytes: Some(ram_gb * GIB),
            gpus: Some(gpus),
            ollama,
            power,
            measured: Measured::NotRun,
        }
    }

    fn arc() -> GpuInfo {
        GpuInfo {
            name: "Intel(R) Arc(TM) Graphics".to_string(),
            vendor_id: INTEL_VENDOR_ID,
            dedicated_bytes: 128 * MIB,
            shared_bytes: 8 * GIB,
        }
    }

    fn running(on_gpu: Option<bool>) -> OllamaState {
        OllamaState::Running { on_gpu }
    }

    #[test]
    fn discrete_gpu_with_ollama_on_gpu_is_good() {
        let v = verdict(&facts(
            16,
            vec![gpu("NVIDIA GeForce RTX 4060", 8, 8)],
            running(Some(true)),
            Power::PluggedIn,
        ));
        assert_eq!(v.level, Level::Good);
        assert!(v.battery_note.is_none());
    }

    #[test]
    fn small_machine_without_a_gpu_is_not_recommended() {
        let v = verdict(&facts(8, vec![], OllamaState::NotRunning, Power::PluggedIn));
        assert_eq!(v.level, Level::NotRecommended);
        assert!(v.line.contains("cloud"), "{}", v.line);
    }

    #[test]
    fn eight_gb_with_only_an_igpu_is_not_recommended() {
        let v = verdict(&facts(
            8,
            vec![arc()],
            OllamaState::NotRunning,
            Power::PluggedIn,
        ));
        assert_eq!(v.level, Level::NotRecommended);
    }

    #[test]
    fn arc_igpu_running_on_cpu_names_the_env_var() {
        let v = verdict(&facts(
            16,
            vec![arc()],
            running(Some(false)),
            Power::PluggedIn,
        ));
        assert_eq!(v.level, Level::Slow);
        assert!(v.line.contains("OLLAMA_IGPU_ENABLE=1"), "{}", v.line);
    }

    #[test]
    fn gpu_machine_with_ollama_on_cpu_is_slow_and_does_not_claim_arc() {
        let v = verdict(&facts(
            32,
            vec![gpu("NVIDIA GeForce RTX 4060", 8, 8)],
            running(Some(false)),
            Power::PluggedIn,
        ));
        assert_eq!(v.level, Level::Slow);
        assert!(!v.line.contains("OLLAMA_IGPU_ENABLE"), "{}", v.line);
    }

    #[test]
    fn cpu_only_run_with_no_gpu_is_not_recommended() {
        let v = verdict(&facts(32, vec![], running(Some(false)), Power::PluggedIn));
        assert_eq!(v.level, Level::NotRecommended);
    }

    #[test]
    fn stock_tray_app_is_slow_and_says_to_quit_it() {
        let v = verdict(&facts(
            32,
            vec![gpu("NVIDIA GeForce RTX 4060", 8, 8)],
            OllamaState::StockTray,
            Power::PluggedIn,
        ));
        assert_eq!(v.level, Level::Slow);
        assert!(v.line.contains("Quit"), "{}", v.line);
    }

    #[test]
    fn igpu_with_enough_ram_and_nothing_loaded_is_slow_and_says_unmeasured() {
        let v = verdict(&facts(16, vec![arc()], running(None), Power::PluggedIn));
        assert_eq!(v.level, Level::Slow);
        assert!(v.line.contains("Not measured"), "{}", v.line);
    }

    #[test]
    fn discrete_gpu_with_nothing_loaded_is_good_but_unmeasured() {
        let v = verdict(&facts(
            16,
            vec![gpu("NVIDIA GeForce RTX 4060", 8, 8)],
            OllamaState::NotRunning,
            Power::PluggedIn,
        ));
        assert_eq!(v.level, Level::Good);
        assert!(v.line.contains("Not measured"), "{}", v.line);
    }

    #[test]
    fn discrete_arc_card_does_not_get_the_igpu_hint() {
        let card = GpuInfo {
            name: "Intel(R) Arc(TM) A770 Graphics".to_string(),
            vendor_id: INTEL_VENDOR_ID,
            dedicated_bytes: 16 * GIB,
            shared_bytes: 8 * GIB,
        };
        let v = verdict(&facts(
            32,
            vec![card],
            running(Some(false)),
            Power::PluggedIn,
        ));
        assert_eq!(v.level, Level::Slow);
        assert!(!v.line.contains("OLLAMA_IGPU_ENABLE"), "{}", v.line);
    }

    #[test]
    fn a_non_intel_adapter_named_arc_does_not_get_the_igpu_hint() {
        let mut other = arc();
        other.vendor_id = 0x1002;
        let v = verdict(&facts(
            16,
            vec![other],
            running(Some(false)),
            Power::PluggedIn,
        ));
        assert!(!v.line.contains("OLLAMA_IGPU_ENABLE"), "{}", v.line);
    }

    #[test]
    fn unreadable_ram_is_unknown_not_little_memory() {
        let f = MachineFacts {
            ram_bytes: None,
            gpus: Some(vec![arc()]),
            ollama: OllamaState::NotRunning,
            power: Power::PluggedIn,
            measured: Measured::NotRun,
        };
        let v = verdict(&f);
        assert_eq!(v.level, Level::Unknown);
        assert!(v.line.contains("Could not read"), "{}", v.line);
        assert!(v.numbers.contains("RAM: could not read"), "{}", v.numbers);
        assert!(!v.line.contains("Little memory"), "{}", v.line);
    }

    #[test]
    fn unreadable_gpu_list_is_unknown_not_no_gpu() {
        let f = MachineFacts {
            ram_bytes: Some(8 * GIB),
            gpus: None,
            ollama: OllamaState::NotRunning,
            power: Power::PluggedIn,
            measured: Measured::NotRun,
        };
        let v = verdict(&f);
        assert_eq!(v.level, Level::Unknown);
        assert!(v.line.contains("Could not read"), "{}", v.line);
        assert!(v.numbers.contains("GPU: could not read"), "{}", v.numbers);
        assert!(!v.numbers.contains("No GPU found"), "{}", v.numbers);
    }

    #[test]
    fn unreadable_gpu_list_with_ollama_on_cpu_is_slow_not_not_recommended() {
        let f = MachineFacts {
            ram_bytes: Some(32 * GIB),
            gpus: None,
            ollama: running(Some(false)),
            power: Power::PluggedIn,
            measured: Measured::NotRun,
        };
        let v = verdict(&f);
        assert_eq!(v.level, Level::Slow);
        assert!(v.line.contains("Could not read"), "{}", v.line);
    }

    #[test]
    fn a_discrete_gpu_still_gives_a_verdict_when_ram_is_unreadable() {
        let f = MachineFacts {
            ram_bytes: None,
            gpus: Some(vec![gpu("NVIDIA GeForce RTX 4060", 8, 8)]),
            ollama: OllamaState::NotRunning,
            power: Power::PluggedIn,
            measured: Measured::NotRun,
        };
        assert_eq!(verdict(&f).level, Level::Good);
    }

    #[test]
    fn on_battery_adds_the_battery_line_with_the_percentage() {
        let v = verdict(&facts(
            16,
            vec![arc()],
            running(Some(true)),
            Power::OnBattery { percent: Some(42) },
        ));
        let note = v.battery_note.clone().expect("battery note");
        assert!(note.contains("42%"), "{note}");
        assert!(note.contains("battery life"), "{note}");
        assert!(v.detail().ends_with(&note));
    }

    #[test]
    fn on_battery_without_a_percentage_still_warns() {
        let v = verdict(&facts(
            16,
            vec![arc()],
            running(Some(true)),
            Power::OnBattery { percent: None },
        ));
        assert!(v
            .battery_note
            .as_deref()
            .unwrap()
            .starts_with("On battery,"));
    }

    #[test]
    fn unknown_power_gives_no_battery_line() {
        let v = verdict(&facts(16, vec![arc()], running(Some(true)), Power::Unknown));
        assert!(v.battery_note.is_none());
    }

    #[test]
    fn ps_entries_map_to_gpu_use_by_size_vram_only() {
        let e = |v| PsEntry {
            name: "m".to_string(),
            size_vram: v,
        };
        assert_eq!(on_gpu_from_ps(&[]), None);
        assert_eq!(on_gpu_from_ps(&[e(0)]), Some(false));
        assert_eq!(on_gpu_from_ps(&[e(0), e(1)]), Some(true));
    }

    #[test]
    fn numbers_show_ram_and_say_an_igpu_shares_memory() {
        let v = verdict(&facts(16, vec![arc()], running(None), Power::Unknown));
        assert!(v.numbers.contains("RAM: 16 GB"), "{}", v.numbers);
        assert!(v.numbers.contains("integrated"), "{}", v.numbers);
        assert!(v.numbers.contains("8 GB of system memory"), "{}", v.numbers);
    }

    #[test]
    fn numbers_say_so_when_no_gpu_was_found() {
        let v = verdict(&facts(16, vec![], OllamaState::NotRunning, Power::Unknown));
        assert!(v.numbers.contains("No GPU found"), "{}", v.numbers);
    }

    #[test]
    fn gib_text_rounds_to_one_decimal_and_drops_a_zero() {
        assert_eq!(gib_text(16 * GIB), "16 GB");
        assert_eq!(gib_text(GIB + GIB / 2), "1.5 GB");
        assert_eq!(gib_text(7 * GIB + 600 * MIB), "7.6 GB");
    }

    fn timing(load_ms: u64, prompt_ms: u64, eval_ms: u64, eval_count: u64) -> Timing {
        Timing {
            model: "gemma3:4b".to_string(),
            load_ms,
            prompt_ms,
            eval_ms,
            eval_count,
            on_gpu: Some(true),
        }
    }

    fn timed(t: Timing) -> MachineFacts {
        let mut f = facts(
            16,
            vec![gpu("NVIDIA GeForce RTX 4060", 8, 0)],
            running(Some(true)),
            Power::PluggedIn,
        );
        f.measured = Measured::Timed(t);
        f
    }

    #[test]
    fn fast_timing_is_good_and_quotes_the_numbers() {
        let v = verdict(&timed(timing(900, 400, 1000, 30)));
        assert_eq!(v.level, Level::Good);
        assert!(v.line.contains("gemma3:4b"), "{}", v.line);
        assert!(v.line.contains("0.4 s"), "first answer: {}", v.line);
        assert!(v.line.contains("30 tokens per second"), "{}", v.line);
        assert!(!v.line.contains("Not measured"), "{}", v.line);
    }

    #[test]
    fn the_model_load_is_reported_apart_from_the_first_answer() {
        let v = verdict(&timed(timing(2500, 400, 1000, 30)));
        assert!(v.line.contains("2.5 s to load"), "{}", v.line);
        let warm = verdict(&timed(timing(0, 400, 1000, 30)));
        assert!(!warm.line.contains("to load"), "{}", warm.line);
    }

    #[test]
    fn a_cold_first_answer_is_judged_on_speed_alone_and_says_to_time_again() {
        // MEASURED 2026-10-17: 26 s to the first answer cold, 1.3 s warm.
        let v = verdict(&timed(timing(14500, 26000, 1000, 21)));
        assert_eq!(v.level, Level::Good);
        assert!(v.line.contains("time it again"), "{}", v.line);
        let slow = verdict(&timed(timing(14500, 26000, 16000, 32)));
        assert_eq!(slow.level, Level::NotRecommended, "2 tokens per second");
        let warm = verdict(&timed(timing(0, 400, 1000, 30)));
        assert!(!warm.line.contains("time it again"), "{}", warm.line);
    }

    #[test]
    fn a_model_name_without_a_tag_matches_its_latest_tag() {
        assert!(same_model("gemma3:latest", "gemma3"));
        assert!(same_model("gemma3:4b", "gemma3:4b"));
        assert!(!same_model("gemma3:4b", "gemma3"));
        assert!(!same_model("gemma3:latest", "gemma3:4b"));
        assert!(!same_model("gemma3n:latest", "gemma3"));
    }

    #[test]
    fn middling_timing_is_slow() {
        // 8 tokens per second, 2 s to the first answer.
        let v = verdict(&timed(timing(0, 2000, 4000, 32)));
        assert_eq!(v.level, Level::Slow);
    }

    #[test]
    fn very_slow_timing_is_not_recommended() {
        // 2 tokens per second.
        let v = verdict(&timed(timing(0, 9000, 16000, 32)));
        assert_eq!(v.level, Level::NotRecommended);
    }

    #[test]
    fn a_slow_first_answer_alone_is_not_good() {
        // Fast decoding but 12 s before the first token.
        let v = verdict(&timed(timing(0, 12000, 1000, 30)));
        assert_ne!(v.level, Level::Good);
    }

    #[test]
    fn a_timed_run_on_the_cpu_says_so() {
        let mut t = timing(0, 400, 1000, 30);
        t.on_gpu = Some(false);
        let v = verdict(&timed(t));
        assert!(v.line.contains("CPU"), "{}", v.line);
    }

    #[test]
    fn zero_eval_time_does_not_divide_by_zero() {
        let v = verdict(&timed(timing(0, 400, 0, 0)));
        assert!(!v.line.is_empty());
        assert_ne!(v.level, Level::Good, "no measured speed is not good");
    }

    #[test]
    fn a_failed_timed_run_keeps_the_hardware_verdict_and_names_the_failure() {
        let mut f = timed(timing(0, 1, 1, 1));
        f.measured = Measured::Failed("Ollama did not answer.".to_string());
        let v = verdict(&f);
        assert!(v.line.contains("Ollama did not answer."), "{}", v.line);
        assert!(v.line.contains("Timed request failed"), "{}", v.line);
    }

    #[test]
    fn timing_is_offered_only_when_an_ollama_server_answers_and_not_yet_timed() {
        let f = |o| facts(16, vec![], o, Power::Unknown);
        assert!(can_offer_timing(&f(running(None))));
        assert!(can_offer_timing(&f(running(Some(true)))));
        assert!(can_offer_timing(&f(OllamaState::StockTray)));
        assert!(!can_offer_timing(&f(OllamaState::NotRunning)));
        assert!(!can_offer_timing(&timed(timing(0, 1, 1, 1))));
        let mut failed = f(running(None));
        failed.measured = Measured::Failed("x".to_string());
        assert!(can_offer_timing(&failed), "a failed run can be retried");
    }

    #[test]
    fn parse_timing_reads_ollamas_own_nanosecond_counters() {
        let body = r#"{"model":"gemma3:4b","message":{"role":"assistant","content":"A gradient."},"done":true,
            "total_duration":3500000000,"load_duration":2000000000,"prompt_eval_count":270,
            "prompt_eval_duration":500000000,"eval_count":20,"eval_duration":1000000000}"#;
        let t = parse_timing(body, "gemma3:4b", Some(true)).expect("timing");
        assert_eq!(t.load_ms, 2000);
        assert_eq!(t.prompt_ms, 500);
        assert_eq!(t.eval_ms, 1000);
        assert_eq!(t.eval_count, 20);
        assert_eq!(t.on_gpu, Some(true));
    }

    #[test]
    fn parse_timing_rejects_a_body_without_counters_or_json() {
        assert!(parse_timing("not json", "m", None).is_err());
        assert!(parse_timing(r#"{"done":true}"#, "m", None).is_err());
        assert!(parse_timing(r#"{"error":"model not found"}"#, "m", None)
            .unwrap_err()
            .contains("model not found"));
    }

    #[test]
    fn timing_request_follows_the_ollama_rules() {
        let body = timing_body("gemma3:4b");
        assert_eq!(body["model"], "gemma3:4b");
        assert_eq!(body["stream"], false);
        // AGENTS.md rule 6: think explicit, keep_alive top level.
        assert_eq!(body["think"], false);
        assert!(body["keep_alive"].is_string());
        assert!(body["options"].get("keep_alive").is_none());
        // Same num_ctx as a real request, so timing does not reload the model.
        assert_eq!(body["options"]["num_ctx"], DEFAULT_NUM_CTX);
        assert!(body["options"]["num_predict"].as_u64().unwrap() <= 64);
        let images = body["messages"][0]["images"].as_array().expect("one image");
        assert_eq!(images.len(), 1);
    }

    #[test]
    fn the_synthetic_image_is_a_small_valid_png() {
        let png = synthetic_png();
        assert!(png.starts_with(&[0x89, b'P', b'N', b'G']));
        assert!(png.len() < 20_000, "{} bytes", png.len());
        let img = image::load_from_memory(&png).expect("decodes");
        assert!(img.width() >= 64 && img.width() <= 512);
    }

    #[test]
    fn a_non_loopback_url_is_refused_before_any_request() {
        match run_timed("https://example.com", "gemma3:4b") {
            Measured::Failed(msg) => assert!(msg.contains("this PC"), "{msg}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn timing_texts_have_no_em_dash() {
        let mut f = timed(timing(2500, 400, 1000, 30));
        let v = verdict(&f);
        assert!(!format!("{} {}", v.headline, v.line).contains('\u{2014}'));
        f.measured = Measured::Failed("x".to_string());
        assert!(!verdict(&f).line.contains('\u{2014}'));
        assert!(!TIMING_ACTION_LABEL.contains('\u{2014}'));
    }

    #[test]
    fn no_text_contains_an_em_dash() {
        let v = verdict(&facts(
            16,
            vec![arc()],
            running(Some(false)),
            Power::OnBattery { percent: Some(10) },
        ));
        let all = format!("{} {} {}", v.headline, v.detail(), v.numbers);
        assert!(!all.contains('\u{2014}'));
    }

    /// Hand check for [`run_timed`] (#699, rule 8). Sends ONE small request to
    /// the local model: `cargo test timed_live -- --ignored --nocapture`.
    /// Compare the printed load, first answer and tokens per second with
    /// `ollama run <model> --verbose`, and `on_gpu` with `/api/ps`.
    #[test]
    #[ignore = "live: loads the local model; run manually, see this test's doc comment"]
    fn timed_live_prints_one_timing() {
        let measured = run_timed("http://127.0.0.1:11434", "gemma3:4b");
        eprintln!("{measured:#?}");
        if let Measured::Timed(t) = &measured {
            let mut f = facts(16, vec![], running(None), Power::Unknown);
            f.measured = Measured::Timed(t.clone());
            let v = verdict(&f);
            eprintln!("{}: {}", v.headline, v.line);
        }
        assert!(matches!(measured, Measured::Timed(_)), "{measured:?}");
    }

    /// Hand check for [`gather`] (rule 8). Run manually, one filtered test:
    /// `cargo test gather_live -- --ignored --nocapture`, then compare the
    /// printed RAM, adapters, power and Ollama state with Task Manager and
    /// `/api/ps`.
    #[test]
    #[ignore = "live: reads this machine; run manually, see this test's doc comment"]
    fn gather_live_prints_this_machine() {
        let facts = gather("http://127.0.0.1:11434");
        let v = verdict(&facts);
        eprintln!(
            "{facts:#?}
{:?}: {} | {}",
            v.level,
            v.headline,
            v.detail()
        );
        assert!(
            facts.ram_bytes.is_some(),
            "GlobalMemoryStatusEx returned nothing"
        );
    }
}
