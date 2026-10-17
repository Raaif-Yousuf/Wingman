//! One-click fix for Ollama running on the CPU instead of an Intel GPU (#382).
//!
//! The Ollama Vulkan backend drops an Intel Arc iGPU unless
//! `OLLAMA_IGPU_ENABLE=1` is in the server's environment, and then runs on
//! the CPU. The only oracle for "is the GPU used" is `size_vram > 0` on
//! `/api/ps` (AGENTS.md rule 6), so the fix ends by loading the model and
//! reading that.
//!
//! The steps are in [`run_fix`], which talks to the machine only through the
//! [`FixSystem`] trait. Tests drive it with a fake, so no test writes the real
//! `HKCU\Environment` or kills a real process (AGENTS.md rule 9). Pure
//! decisions ([`should_offer_fix`], [`FixOutcome::message`]) have no Win32 in
//! them.
//!
//! Outward actions: this writes one user environment variable, stops the
//! stock Ollama tray app and the current Ollama server, and starts
//! `ollama serve`. It only runs from the Settings "Fix it" button, and the
//! text next to the button says exactly that before it is pressed.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_WRITE,
    REG_OPTION_NON_VOLATILE, REG_SZ,
};
use windows::Win32::UI::WindowsAndMessaging::{
    SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE,
};

use super::ollama_admin::{self, GpuStatus, ListenerKind, OllamaHealth};

/// The variable that makes Ollama keep an integrated GPU.
pub const ENV_NAME: &str = "OLLAMA_IGPU_ENABLE";
pub const ENV_VALUE: &str = "1";

/// The stock tray app's file name (see `ollama_admin::classify`).
const TRAY_APP_EXE: &str = "ollama app.exe";

/// PCI vendor id of Intel, as DXGI reports it.
const VENDOR_INTEL: u32 = 0x8086;

/// What Settings shows next to the button, before it is pressed: what the
/// button does, including that it replaces a different value already set.
pub fn offer_text(previous: Option<&str>) -> String {
    match previous
        .map(str::trim)
        .filter(|v| !v.is_empty() && *v != ENV_VALUE)
    {
        Some(old) => {
            let old: String = old.chars().take(12).collect();
            format!(
                "Ollama is on the CPU, not your Intel GPU. Fix it replaces your \
                 {ENV_NAME}={old} with {ENV_VALUE} and restarts Ollama."
            )
        }
        None => format!(
            "Ollama is on the CPU, not your Intel GPU. Fix it sets {ENV_NAME}={ENV_VALUE} \
             for your account and restarts Ollama."
        ),
    }
}

/// What the button says while the fix runs.
pub const RUNNING_TEXT: &str = "Fixing: setting the variable and restarting Ollama...";

// ===========================================================================
// Pure decisions
// ===========================================================================

/// Whether any of the DXGI adapter vendor ids is Intel.
pub fn has_intel_gpu(vendor_ids: &[u32]) -> bool {
    vendor_ids.contains(&VENDOR_INTEL)
}

/// The last path component, for both `\\` and `/` separators.
fn file_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

/// Whether `path` names `ollama.exe` (case-insensitive). The only program the
/// fix will stop or start.
pub fn is_ollama_exe(path: &str) -> bool {
    file_name(path).eq_ignore_ascii_case("ollama.exe")
}

/// Whether `path` is inside one of `roots` (case-insensitive, whole path
/// components, `\\` or `/`).
pub fn under_roots(path: &str, roots: &[String]) -> bool {
    let norm = |p: &str| p.replace('/', "\\").trim_end_matches('\\').to_lowercase();
    let path = norm(path);
    roots.iter().any(|r| {
        let r = norm(r);
        !r.is_empty() && path.starts_with(&format!("{r}\\"))
    })
}

/// Where Ollama may be run from: its per-user install folder and Program
/// Files. Anything else (a Downloads copy) is never executed by the fix.
pub fn trusted_roots() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(l) = std::env::var("LOCALAPPDATA") {
        out.push(format!(r"{l}\Programs\Ollama"));
    }
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Ok(v) = std::env::var(var) {
            out.push(v);
        }
    }
    out
}

/// Whether `base_url` points at this machine. The probe reads whatever
/// server the URL names, but the fix stops local processes, so it must only
/// act when they are the same server.
pub fn is_loopback_base_url(base_url: &str) -> bool {
    let rest = base_url.split_once("://").map_or(base_url, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit('@').next().unwrap_or("");
    let host = if let Some(v6) = authority.strip_prefix('[') {
        v6.split(']').next().unwrap_or("")
    } else {
        authority.split(':').next().unwrap_or("")
    };
    host == "127.0.0.1" || host == "::1" || host.eq_ignore_ascii_case("localhost")
}

/// Whether Settings offers the fix.
/// - Not on a machine without an Intel GPU: the variable means nothing there.
/// - Listener is a real server and the model is loaded on the CPU: yes.
/// - Listener is a real server on the GPU, or no model is loaded: no.
/// - Listener is the stock tray app (CPU-only, and Settings does not ask it
///   about models): yes, because replacing it is the fix.
pub fn should_offer_fix(
    intel_gpu: bool,
    kind: Option<ListenerKind>,
    gpu: Option<GpuStatus>,
) -> bool {
    if !intel_gpu {
        return false;
    }
    matches!(
        (kind, gpu),
        (Some(ListenerKind::StockTrayServer), _)
            | (Some(ListenerKind::Other), Some(GpuStatus::Cpu))
    )
}

/// How the fix ended. Each variant says in plain words whether Ollama is
/// running afterwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixOutcome {
    /// Restarted with the variable and `size_vram > 0`.
    Fixed,
    /// Restarted with the variable and the model is still on the CPU.
    StillCpu,
    /// Restarted, but the GPU could not be checked (model would not load, or
    /// `/api/ps` did not answer).
    Unverified(String),
    /// Nothing was stopped; the reason says what failed.
    NotChanged(String),
    /// Ollama was stopped and could not be started again.
    LeftStopped(String),
}

impl FixOutcome {
    /// Whether Settings should keep offering the button.
    pub fn retryable(&self) -> bool {
        !matches!(self, FixOutcome::Fixed)
    }

    /// One user-facing line (AGENTS.md rule 11: no em dashes).
    pub fn message(&self) -> String {
        match self {
            FixOutcome::Fixed => {
                "Fixed. Ollama restarted and the model now runs on your GPU.".into()
            }
            FixOutcome::StillCpu => "Restarted, but the model is still on the CPU. The Intel \
                graphics driver may be too old, or the model too big for GPU memory."
                .into(),
            FixOutcome::Unverified(why) => {
                format!("Restarted, but the GPU could not be checked: {why}")
            }
            FixOutcome::NotChanged(why) => why.clone(),
            FixOutcome::LeftStopped(why) => format!(
                "Ollama is stopped. {why} The setting is saved. Start Ollama from the Start menu."
            ),
        }
    }
}

/// An error text cut short, so every result line fits the Settings label.
fn short(e: &str) -> String {
    const MAX: usize = 40;
    let e = e.trim();
    if e.chars().count() <= MAX {
        return e.to_string();
    }
    let mut out: String = e.chars().take(MAX - 3).collect();
    out.push_str("...");
    out
}

// ===========================================================================
// The steps, over an injectable machine
// ===========================================================================

/// Everything [`run_fix`] does to the machine. The real one is
/// [`RealSystem`]; tests use a fake.
pub trait FixSystem {
    /// Saves `OLLAMA_IGPU_ENABLE=1` for the user and tells running programs.
    fn set_user_env(&mut self) -> Result<(), String>;
    fn health(&mut self) -> OllamaHealth;
    /// The image path of `pid` right now (`None` when it is gone).
    fn image_of(&mut self, pid: u32) -> Option<String>;
    /// Quits the stock tray app that started the server `server_pid` (it
    /// respawns a CPU server if left alone). Only that exact process.
    fn stop_tray_app(&mut self, server_pid: u32) -> Result<(), String>;
    /// Ends the process and everything it started.
    fn kill_tree(&mut self, pid: u32) -> Result<(), String>;
    /// Starts `ollama serve` with the variable set. Returns right away.
    fn start_serve(&mut self, exe: &Path) -> Result<(), String>;
    /// Waits up to `limit` for the port to have a listener (`true`) or none
    /// (`false`); returns whether the wanted state was reached.
    fn wait_for_listener(&mut self, want_listening: bool, limit: Duration) -> bool;
    /// Loads the model so `/api/ps` has an entry for it.
    fn load_model(&mut self) -> Result<(), String>;
    fn gpu_status(&mut self) -> Result<GpuStatus, String>;
}

const STOP_WAIT: Duration = Duration::from_secs(10);
const START_WAIT: Duration = Duration::from_secs(20);

/// Runs the fix. `fallback_exe` is the known install's `ollama.exe`; when it
/// exists it is always the one started. Otherwise the listener's own path is
/// started only if it is under `roots`. Only `ollama.exe` is ever stopped or
/// started: a listener with any other image name, or one that could only be
/// restarted from an untrusted folder, stops the fix before anything is
/// touched. `progress`
/// is called with a short line before each step. Never leaves Ollama stopped
/// without saying so, and says the setting was saved once it has been.
pub fn run_fix(
    sys: &mut dyn FixSystem,
    fallback_exe: Option<PathBuf>,
    roots: &[String],
    progress: &mut dyn FnMut(&str),
) -> FixOutcome {
    // Look before touching anything, so a missing exe is found while Ollama
    // is still running.
    let before = sys.health();
    let known = fallback_exe.filter(|p| is_ollama_exe(&p.to_string_lossy()));
    let (exe, listener_kind) = match &before {
        OllamaHealth::Listening {
            image_path, kind, ..
        } if is_ollama_exe(image_path) => {
            let exe =
                known.or_else(|| under_roots(image_path, roots).then(|| PathBuf::from(image_path)));
            match exe {
                Some(exe) => (exe, Some(*kind)),
                None => {
                    return FixOutcome::NotChanged(
                        "This ollama.exe is not in the usual install folder, so it was \
                         not restarted. Nothing was changed."
                            .to_string(),
                    );
                }
            }
        }
        OllamaHealth::Listening { .. } => {
            return FixOutcome::NotChanged(
                "Another program, not ollama.exe, is using the Ollama port. \
                 Nothing was changed."
                    .to_string(),
            );
        }
        OllamaHealth::NotListening => match known {
            Some(p) => (p, None),
            None => {
                return FixOutcome::NotChanged(
                    "Could not find ollama.exe to restart it, so nothing was changed.".to_string(),
                );
            }
        },
    };

    progress("Saving the setting...");
    if let Err(e) = sys.set_user_env() {
        return FixOutcome::NotChanged(format!(
            "Could not save the setting ({}). Ollama was not touched.",
            short(&e)
        ));
    }

    if let (
        Some(kind),
        OllamaHealth::Listening {
            pid: server_pid, ..
        },
    ) = (listener_kind, &before)
    {
        if kind == ListenerKind::StockTrayServer {
            progress("Quitting the stock Ollama app...");
            if let Err(e) = sys.stop_tray_app(*server_pid) {
                return FixOutcome::NotChanged(format!(
                    "Could not quit the stock Ollama app ({}). Setting saved. \
                     Quit it from the tray, then try again.",
                    short(&e)
                ));
            }
        }
        // Look again: with the tray app gone its server may have exited too.
        let target = match sys.health() {
            OllamaHealth::Listening { pid, .. } => Some(pid),
            OllamaHealth::NotListening => None,
        };
        if let Some(pid) = target {
            // The pid was resolved a moment ago and may have been reused, or
            // a different program may have taken the port.
            if !sys.image_of(pid).is_some_and(|i| is_ollama_exe(&i)) {
                return FixOutcome::NotChanged(
                    "The Ollama port changed hands, so nothing was stopped. Setting saved. \
                     Try again."
                        .to_string(),
                );
            }
            progress("Stopping Ollama...");
            if let Err(e) = sys.kill_tree(pid) {
                return FixOutcome::NotChanged(format!(
                    "Could not stop Ollama ({}). It is still running. Setting saved. \
                     Quit it, then try again.",
                    short(&e)
                ));
            }
        }
        if !sys.wait_for_listener(false, STOP_WAIT) {
            return FixOutcome::NotChanged(
                "Ollama did not stop, or started again at once. Setting saved. \
                 Quit it from the tray, then try again."
                    .to_string(),
            );
        }
    }

    progress("Starting Ollama...");
    if let Err(e) = sys.start_serve(&exe) {
        return FixOutcome::LeftStopped(format!("Could not restart it ({}).", short(&e)));
    }
    if !sys.wait_for_listener(true, START_WAIT) {
        return FixOutcome::LeftStopped("It started but is not answering.".to_string());
    }

    progress("Checking the GPU...");
    if let Err(e) = sys.load_model() {
        return FixOutcome::Unverified(format!("the model would not load ({}).", short(&e)));
    }
    match sys.gpu_status() {
        Ok(GpuStatus::Gpu) => FixOutcome::Fixed,
        Ok(GpuStatus::Cpu) => FixOutcome::StillCpu,
        Ok(GpuStatus::NotLoaded) => FixOutcome::Unverified("the model is not loaded.".to_string()),
        Err(e) => FixOutcome::Unverified(format!("{}.", short(&e))),
    }
}

// ===========================================================================
// One fix at a time, and a trace if Wingman dies in the middle
// ===========================================================================

use std::sync::atomic::{AtomicBool, Ordering};

/// Set while a fix runs anywhere in this process, so reopening Settings
/// cannot start a second one.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Held for the length of a fix. Dropping it clears the flag and the marker.
pub struct FixGuard {
    flag: &'static AtomicBool,
    marker: Option<PathBuf>,
}

impl Drop for FixGuard {
    fn drop(&mut self) {
        if let Some(m) = &self.marker {
            let _ = std::fs::remove_file(m);
        }
        self.flag.store(false, Ordering::SeqCst);
    }
}

/// [`try_begin`] with the flag and marker injected, for tests.
fn begin(flag: &'static AtomicBool, marker: Option<PathBuf>) -> Option<FixGuard> {
    flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .ok()?;
    if let Some(m) = &marker {
        // Best effort: a marker that cannot be written must not stop the fix.
        let _ = std::fs::write(m, "Fix it was running\n");
    }
    Some(FixGuard { flag, marker })
}

/// Starts a fix: `None` when one is already running.
pub fn try_begin() -> Option<FixGuard> {
    begin(&RUNNING, marker_path())
}

pub fn is_running() -> bool {
    RUNNING.load(Ordering::SeqCst)
}

/// Next to `config.toml`: present only while a fix runs, so finding it with
/// no fix running means Wingman exited mid-fix.
fn marker_path() -> Option<PathBuf> {
    let cfg = crate::config::Config::path().ok()?;
    Some(cfg.parent()?.join("gpu-fix-in-progress"))
}

/// Whether the last fix never finished (Wingman exited during it), so Ollama
/// may be stopped. Shown in diagnostics.
///
/// #846: once Ollama answers on its port the marker is stale (the user started
/// it again), so it is removed and this says no, instead of repeating the
/// warning on every later run.
pub fn interrupted(ollama_listening: bool) -> bool {
    interrupted_at(marker_path().as_deref(), is_running(), ollama_listening)
}

fn interrupted_at(marker: Option<&Path>, running: bool, ollama_listening: bool) -> bool {
    let Some(m) = marker.filter(|m| !running && m.exists()) else {
        return false;
    };
    if ollama_listening {
        // Best effort: if it cannot be removed it is still not reported, as
        // Ollama is up now, and the next run tries again.
        let _ = std::fs::remove_file(m);
        return false;
    }
    true
}

// ===========================================================================
// The real machine
// ===========================================================================

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Vendor ids of the hardware DXGI adapters (one shared enumeration, in
/// `machine`). Empty when DXGI cannot be asked (then no fix is offered: the
/// safe outcome, since the button changes things).
pub fn adapter_vendor_ids() -> Vec<u32> {
    crate::machine::adapters()
        .unwrap_or_default()
        .iter()
        .map(|g| g.vendor_id)
        .collect()
}

/// Writes a `REG_SZ` under `HKCU\<key_path>`, creating the key. The key and
/// name are parameters so a test can use a scratch key (AGENTS.md rule 9).
fn write_user_value(key_path: &str, name: &str, value: &str) -> Result<(), String> {
    let sub = wide(key_path);
    let mut key = HKEY::default();
    unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(sub.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        )
        .ok()
        .map_err(|e| format!("opening the registry key: {e}"))?;
        let name_w = wide(name);
        let data: Vec<u8> = wide(value).iter().flat_map(|u| u.to_le_bytes()).collect();
        let set = RegSetValueExW(key, PCWSTR(name_w.as_ptr()), None, REG_SZ, Some(&data));
        let _ = RegCloseKey(key);
        set.ok().map_err(|e| format!("writing the value: {e}"))
    }
}

/// Tells running programs the user environment changed, so new processes
/// started from Explorer pick it up. A program that does not answer in time
/// is skipped; this is a courtesy, not a step that can fail the fix.
fn broadcast_environment_change() {
    let area = wide("Environment");
    unsafe {
        let _ = SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            windows::Win32::Foundation::WPARAM(0),
            windows::Win32::Foundation::LPARAM(area.as_ptr() as isize),
            SMTO_ABORTIFHUNG,
            2000,
            None,
        );
    }
}

/// Where Wingman looks for `ollama.exe` when the running server's path is
/// unknown: the default per-user install folder.
pub fn default_exe() -> Option<PathBuf> {
    let local = std::env::var("LOCALAPPDATA").ok()?;
    let p = Path::new(&local).join(r"Programs\Ollama\ollama.exe");
    p.is_file().then_some(p)
}

/// Reads a string value under `HKCU\<key_path>`; `None` when absent.
fn read_user_value(key_path: &str, name: &str) -> Option<String> {
    use windows::Win32::System::Registry::{RegGetValueW, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ};
    let (sub, name_w) = (wide(key_path), wide(name));
    let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ;
    // First call sizes the buffer, so a long value is still read.
    let mut len = 0u32;
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(sub.as_ptr()),
            PCWSTR(name_w.as_ptr()),
            flags,
            None,
            None,
            Some(&mut len),
        )
        .ok()
        .ok()?;
    }
    let mut buf = vec![0u16; (len as usize).div_ceil(2) + 1];
    let mut len = (buf.len() * 2) as u32;
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(sub.as_ptr()),
            PCWSTR(name_w.as_ptr()),
            flags,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut len),
        )
        .ok()
        .ok()?;
    }
    let end = buf.iter().position(|&u| u == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end]))
}

/// The current user value of the variable, for the pre-press text.
pub fn current_user_value() -> Option<String> {
    read_user_value("Environment", ENV_NAME)
}

/// `taskkill` by PID. Exit code 128 means the process is already gone, which
/// is what we wanted; any other failure is an error. Exit codes, not message
/// text, so a localized Windows behaves the same.
fn taskkill_pid(pid: u32, tree: bool) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
    let mut args = vec!["/F".to_string()];
    if tree {
        args.push("/T".to_string());
    }
    args.extend(["/PID".to_string(), pid.to_string()]);
    let out = std::process::Command::new(Path::new(&root).join(r"System32\taskkill.exe"))
        .args(&args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("could not run taskkill: {e}"))?;
    match out.status.code() {
        Some(0) | Some(128) => Ok(()),
        Some(c) => Err(format!("taskkill exit code {c}")),
        None => Err("taskkill was interrupted".to_string()),
    }
}

/// The live machine: real registry, real processes, real Ollama.
pub struct RealSystem {
    pub base_url: String,
    pub port: u16,
    pub model: String,
}

impl FixSystem for RealSystem {
    fn set_user_env(&mut self) -> Result<(), String> {
        write_user_value("Environment", ENV_NAME, ENV_VALUE)?;
        broadcast_environment_change();
        Ok(())
    }

    fn health(&mut self) -> OllamaHealth {
        ollama_admin::query_ollama_health(self.port)
    }

    fn image_of(&mut self, pid: u32) -> Option<String> {
        ollama_admin::process_image_path(pid)
    }

    fn stop_tray_app(&mut self, server_pid: u32) -> Result<(), String> {
        // Only the server's own parent, and only if it really is the tray
        // app: never a process picked by name.
        let Some(parent) = ollama_admin::parent_pid(server_pid) else {
            return Ok(());
        };
        match ollama_admin::process_image_path(parent) {
            Some(img)
                if file_name(&img).eq_ignore_ascii_case(TRAY_APP_EXE)
                    && under_roots(&img, &trusted_roots()) =>
            {
                taskkill_pid(parent, false)
            }
            _ => Ok(()),
        }
    }

    fn kill_tree(&mut self, pid: u32) -> Result<(), String> {
        taskkill_pid(pid, true)
    }

    fn start_serve(&mut self, exe: &Path) -> Result<(), String> {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        std::process::Command::new(exe)
            .arg("serve")
            .env(ENV_NAME, ENV_VALUE)
            .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    fn wait_for_listener(&mut self, want_listening: bool, limit: Duration) -> bool {
        let start = Instant::now();
        loop {
            let listening = !matches!(
                ollama_admin::query_ollama_health(self.port),
                OllamaHealth::NotListening
            );
            if listening == want_listening {
                return true;
            }
            if start.elapsed() >= limit {
                return false;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    fn load_model(&mut self) -> Result<(), String> {
        // An empty-prompt generate loads the model and returns. `keep_alive`
        // is top-level and `think` is explicit (AGENTS.md rule 6).
        let url = format!("{}/api/generate", self.base_url.trim_end_matches('/'));
        let body = serde_json::json!({
            "model": self.model,
            "keep_alive": "5m",
            "think": false,
            "stream": false,
        });
        super::common::post_json_with_connect_timeout(
            &url,
            &[],
            body,
            Duration::from_millis(800),
            Duration::from_secs(120),
            "ollama",
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
    }

    fn gpu_status(&mut self) -> Result<GpuStatus, String> {
        let entries = ollama_admin::ps_with_timeout(&self.base_url, Duration::from_secs(5))
            .map_err(|e| e.to_string())?;
        Ok(ollama_admin::gpu_status_for(&entries, &self.model))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- decisions ---------------------------------------------------------

    #[test]
    fn intel_vendor_id_is_detected_among_adapters() {
        assert!(has_intel_gpu(&[0x10DE, 0x8086]));
        assert!(!has_intel_gpu(&[0x10DE]));
        assert!(!has_intel_gpu(&[]));
    }

    #[test]
    fn offers_the_fix_for_intel_plus_a_real_server_on_the_cpu() {
        assert!(should_offer_fix(
            true,
            Some(ListenerKind::Other),
            Some(GpuStatus::Cpu)
        ));
    }

    #[test]
    fn no_offer_when_the_model_already_runs_on_the_gpu() {
        assert!(!should_offer_fix(
            true,
            Some(ListenerKind::Other),
            Some(GpuStatus::Gpu)
        ));
    }

    #[test]
    fn no_offer_on_a_machine_without_an_intel_gpu() {
        for kind in [ListenerKind::Other, ListenerKind::StockTrayServer] {
            assert!(!should_offer_fix(false, Some(kind), Some(GpuStatus::Cpu)));
        }
    }

    #[test]
    fn no_offer_when_no_model_is_loaded_or_nothing_listens() {
        assert!(!should_offer_fix(
            true,
            Some(ListenerKind::Other),
            Some(GpuStatus::NotLoaded)
        ));
        assert!(!should_offer_fix(true, Some(ListenerKind::Other), None));
        assert!(!should_offer_fix(true, None, None));
    }

    #[test]
    fn offers_the_fix_for_the_stock_tray_app_on_an_intel_machine() {
        assert!(should_offer_fix(
            true,
            Some(ListenerKind::StockTrayServer),
            None
        ));
    }

    #[test]
    fn messages_are_plain_and_have_no_em_dash() {
        let all = [
            FixOutcome::Fixed,
            FixOutcome::StillCpu,
            FixOutcome::Unverified("why.".into()),
            FixOutcome::NotChanged("why.".into()),
            FixOutcome::LeftStopped("why.".into()),
        ];
        for o in all {
            let m = o.message();
            assert!(!m.contains('\u{2014}'), "{m}");
            assert!(!m.is_empty());
        }
        assert!(FixOutcome::LeftStopped("x".into())
            .message()
            .contains("Ollama is stopped"));
        assert!(!offer_text(Some("0")).contains('\u{2014}') && !RUNNING_TEXT.contains('\u{2014}'));
    }

    #[test]
    fn only_a_fixed_outcome_stops_offering_the_button() {
        assert!(!FixOutcome::Fixed.retryable());
        assert!(FixOutcome::StillCpu.retryable());
        assert!(FixOutcome::NotChanged(String::new()).retryable());
    }

    // -- run_fix over a fake machine -----------------------------------------

    #[derive(Default)]
    struct Fake {
        log: Vec<String>,
        health: Vec<OllamaHealth>,
        env_err: Option<String>,
        tray_err: Option<String>,
        kill_err: Option<String>,
        start_err: Option<String>,
        stop_waits_out: bool,
        image: Option<String>,
        listen_after_start: bool,
        load_err: Option<String>,
        gpu: Option<Result<GpuStatus, String>>,
    }

    fn listening(kind: ListenerKind) -> OllamaHealth {
        OllamaHealth::Listening {
            pid: 42,
            image_path: r"C:\fake\ollama.exe".to_string(),
            kind,
        }
    }

    impl FixSystem for Fake {
        fn set_user_env(&mut self) -> Result<(), String> {
            self.log.push("env".into());
            self.env_err.clone().map_or(Ok(()), Err)
        }
        fn health(&mut self) -> OllamaHealth {
            self.log.push("health".into());
            if self.health.len() > 1 {
                self.health.remove(0)
            } else {
                self.health
                    .first()
                    .cloned()
                    .unwrap_or(OllamaHealth::NotListening)
            }
        }
        fn image_of(&mut self, _pid: u32) -> Option<String> {
            self.log.push("image".into());
            self.image
                .clone()
                .or(Some(r"C:\fake\ollama.exe".to_string()))
        }
        fn stop_tray_app(&mut self, _pid: u32) -> Result<(), String> {
            self.log.push("tray".into());
            self.tray_err.clone().map_or(Ok(()), Err)
        }
        fn kill_tree(&mut self, pid: u32) -> Result<(), String> {
            self.log.push(format!("kill {pid}"));
            self.kill_err.clone().map_or(Ok(()), Err)
        }
        fn start_serve(&mut self, exe: &Path) -> Result<(), String> {
            self.log.push(format!("start {}", exe.display()));
            self.start_err.clone().map_or(Ok(()), Err)
        }
        fn wait_for_listener(&mut self, want: bool, _limit: Duration) -> bool {
            self.log.push(format!("wait {want}"));
            if want {
                self.listen_after_start
            } else {
                !self.stop_waits_out
            }
        }
        fn load_model(&mut self) -> Result<(), String> {
            self.log.push("load".into());
            self.load_err.clone().map_or(Ok(()), Err)
        }
        fn gpu_status(&mut self) -> Result<GpuStatus, String> {
            self.log.push("gpu".into());
            self.gpu.clone().unwrap_or(Ok(GpuStatus::Gpu))
        }
    }

    fn roots() -> Vec<String> {
        vec![r"C:\fake".to_string()]
    }

    fn run(f: &mut Fake) -> FixOutcome {
        run_fix(f, None, &roots(), &mut |_| {})
    }

    fn working() -> Fake {
        Fake {
            health: vec![listening(ListenerKind::Other)],
            listen_after_start: true,
            ..Default::default()
        }
    }

    #[test]
    fn a_real_server_is_killed_restarted_and_checked() {
        let mut f = working();
        assert_eq!(run(&mut f), FixOutcome::Fixed);
        assert_eq!(
            f.log,
            [
                "health",
                "env",
                "health",
                "image",
                "kill 42",
                "wait false",
                "start C:\\fake\\ollama.exe",
                "wait true",
                "load",
                "gpu"
            ]
        );
    }

    #[test]
    fn the_stock_tray_app_is_quit_before_its_server_is_killed() {
        let mut f = Fake {
            health: vec![listening(ListenerKind::StockTrayServer)],
            listen_after_start: true,
            ..Default::default()
        };
        assert_eq!(run(&mut f), FixOutcome::Fixed);
        let tray = f.log.iter().position(|s| s == "tray").unwrap();
        let kill = f.log.iter().position(|s| s == "kill 42").unwrap();
        assert!(tray < kill, "{:?}", f.log);
    }

    #[test]
    fn a_server_that_left_with_the_tray_app_is_not_killed_again() {
        let mut f = Fake {
            health: vec![
                listening(ListenerKind::StockTrayServer),
                OllamaHealth::NotListening,
            ],
            listen_after_start: true,
            ..Default::default()
        };
        assert_eq!(run(&mut f), FixOutcome::Fixed);
        assert!(!f.log.iter().any(|s| s.starts_with("kill")), "{:?}", f.log);
    }

    #[test]
    fn a_failed_setting_write_touches_nothing_else() {
        let mut f = working();
        f.env_err = Some("access denied".into());
        let out = run(&mut f);
        assert!(matches!(out, FixOutcome::NotChanged(ref m) if m.contains("access denied")));
        assert!(!f.log.iter().any(|s| s.starts_with("kill") || s == "tray"));
        assert!(!f.log.iter().any(|s| s.starts_with("start")));
    }

    #[test]
    fn a_failed_kill_leaves_ollama_running_and_says_so() {
        let mut f = working();
        f.kill_err = Some("access denied".into());
        let out = run(&mut f);
        assert!(
            matches!(out, FixOutcome::NotChanged(ref m) if m.contains("still running")),
            "{out:?}"
        );
        assert!(!f.log.iter().any(|s| s.starts_with("start")));
    }

    #[test]
    fn a_failed_tray_quit_stops_before_killing_anything() {
        let mut f = Fake {
            health: vec![listening(ListenerKind::StockTrayServer)],
            tray_err: Some("denied".into()),
            ..Default::default()
        };
        assert!(matches!(run(&mut f), FixOutcome::NotChanged(_)));
        assert!(!f.log.iter().any(|s| s.starts_with("kill")));
    }

    #[test]
    fn a_port_that_never_frees_is_reported_not_overstarted() {
        let mut f = working();
        f.stop_waits_out = true;
        assert!(matches!(run(&mut f), FixOutcome::NotChanged(_)));
        assert!(!f.log.iter().any(|s| s.starts_with("start")));
    }

    #[test]
    fn a_failed_start_says_ollama_is_stopped() {
        let mut f = working();
        f.start_err = Some("file not found".into());
        let out = run(&mut f);
        assert!(matches!(out, FixOutcome::LeftStopped(_)), "{out:?}");
        assert!(out.message().contains("Ollama is stopped"));
        assert!(out.message().contains("file not found"));
    }

    #[test]
    fn a_server_that_never_answers_says_ollama_is_stopped() {
        let mut f = working();
        f.listen_after_start = false;
        assert!(matches!(run(&mut f), FixOutcome::LeftStopped(_)));
    }

    #[test]
    fn zero_vram_after_the_restart_reports_still_on_the_cpu() {
        let mut f = working();
        f.gpu = Some(Ok(GpuStatus::Cpu));
        assert_eq!(run(&mut f), FixOutcome::StillCpu);
    }

    #[test]
    fn a_model_that_will_not_load_is_unverified_not_fixed() {
        let mut f = working();
        f.load_err = Some("model not found".into());
        let out = run(&mut f);
        assert!(matches!(out, FixOutcome::Unverified(ref m) if m.contains("model not found")));
        assert!(!f.log.iter().any(|s| s == "gpu"));
    }

    #[test]
    fn an_unreadable_ps_is_unverified() {
        let mut f = working();
        f.gpu = Some(Err("timed out".into()));
        assert!(matches!(run(&mut f), FixOutcome::Unverified(_)));
    }

    #[test]
    fn nothing_listening_still_starts_ollama_from_the_fallback_path() {
        let mut f = Fake {
            listen_after_start: true,
            ..Default::default()
        };
        let out = run_fix(
            &mut f,
            Some(PathBuf::from(r"C:\fb\ollama.exe")),
            &roots(),
            &mut |_| {},
        );
        assert_eq!(out, FixOutcome::Fixed);
        assert!(f.log.iter().any(|s| s == "start C:\\fb\\ollama.exe"));
        assert!(!f.log.iter().any(|s| s.starts_with("kill")));
    }

    #[test]
    fn no_exe_anywhere_changes_nothing() {
        let mut f = Fake::default();
        let out = run(&mut f);
        assert!(matches!(out, FixOutcome::NotChanged(_)));
        assert_eq!(f.log, ["health"], "not even the setting is written");
    }

    #[test]
    fn every_result_line_fits_the_status_label_even_with_a_long_error() {
        // The label holds `OLLAMA_TEXT_BUDGET` (140) characters in settings.rs.
        let long = "x".repeat(300);
        let mut lines = vec![
            offer_text(None),
            offer_text(Some("0")),
            offer_text(Some(&"y".repeat(100))),
            RUNNING_TEXT.to_string(),
            FixOutcome::Fixed.message(),
            FixOutcome::StillCpu.message(),
        ];
        let cases: [fn(&mut Fake, String); 5] = [
            |f, e| f.env_err = Some(e),
            |f, e| f.kill_err = Some(e),
            |f, e| f.start_err = Some(e),
            |f, e| f.load_err = Some(e),
            |f, e| f.gpu = Some(Err(e)),
        ];
        for set in cases {
            let mut f = working();
            set(&mut f, long.clone());
            lines.push(run(&mut f).message());
        }
        let mut f = Fake {
            health: vec![listening(ListenerKind::StockTrayServer)],
            tray_err: Some(long.clone()),
            ..Default::default()
        };
        lines.push(run(&mut f).message());
        for l in lines {
            assert!(l.chars().count() <= 140, "{} chars: {l}", l.chars().count());
        }
    }

    #[test]
    fn a_listener_that_is_not_ollama_exe_is_never_killed_or_started() {
        let mut f = Fake {
            health: vec![OllamaHealth::Listening {
                pid: 7,
                image_path: r"C:\x\proxy.exe".to_string(),
                kind: ListenerKind::Other,
            }],
            listen_after_start: true,
            ..Default::default()
        };
        let out = run_fix(
            &mut f,
            Some(PathBuf::from(r"C:\d\ollama.exe")),
            &roots(),
            &mut |_| {},
        );
        assert!(matches!(out, FixOutcome::NotChanged(_)), "{out:?}");
        assert_eq!(f.log, ["health"], "not even the setting is written");
    }

    #[test]
    fn an_ollama_exe_in_downloads_is_replaced_by_the_known_install() {
        let mut f = Fake {
            health: vec![OllamaHealth::Listening {
                pid: 7,
                image_path: r"C:\Users\x\Downloads\ollama.exe".to_string(),
                kind: ListenerKind::Other,
            }],
            listen_after_start: true,
            ..Default::default()
        };
        let out = run_fix(
            &mut f,
            Some(PathBuf::from(r"C:\fake\Programs\Ollama\ollama.exe")),
            &roots(),
            &mut |_| {},
        );
        assert_eq!(out, FixOutcome::Fixed);
        assert!(f
            .log
            .iter()
            .any(|s| s == r"start C:\fake\Programs\Ollama\ollama.exe"));
        assert!(!f.log.iter().any(|s| s.contains("Downloads")));
    }

    #[test]
    fn an_untrusted_listener_with_no_known_install_changes_nothing() {
        let mut f = Fake {
            health: vec![OllamaHealth::Listening {
                pid: 7,
                image_path: r"C:\Users\x\Downloads\ollama.exe".to_string(),
                kind: ListenerKind::Other,
            }],
            ..Default::default()
        };
        let out = run_fix(&mut f, None, &roots(), &mut |_| {});
        assert!(matches!(out, FixOutcome::NotChanged(_)));
        assert_eq!(f.log, ["health"]);
    }

    #[test]
    fn roots_match_whole_folders_case_insensitively() {
        let roots = vec![r"C:\Program Files".to_string()];
        assert!(under_roots(r"c:\program files\Ollama\ollama.exe", &roots));
        assert!(under_roots("C:/Program Files/x/ollama.exe", &roots));
        assert!(!under_roots(r"C:\Program Files Evil\ollama.exe", &roots));
        assert!(!under_roots(r"C:\Program Files", &roots));
        assert!(!under_roots(r"C:\x\ollama.exe", &[String::new()]));
    }

    #[test]
    fn a_non_ollama_fallback_exe_is_refused() {
        let mut f = Fake::default();
        let out = run_fix(
            &mut f,
            Some(PathBuf::from(r"C:\d\other.exe")),
            &roots(),
            &mut |_| {},
        );
        assert!(matches!(out, FixOutcome::NotChanged(_)));
        assert_eq!(f.log, ["health"]);
    }

    #[test]
    fn a_reused_pid_is_not_killed() {
        let mut f = working();
        f.image = Some(r"C:\x\notepad.exe".to_string());
        let out = run_fix(&mut f, None, &roots(), &mut |_| {});
        assert!(matches!(out, FixOutcome::NotChanged(ref m) if m.contains("Setting saved")));
        assert!(!f
            .log
            .iter()
            .any(|s| s.starts_with("kill") || s.starts_with("start")));
    }

    #[test]
    fn failures_after_the_write_say_the_setting_was_saved() {
        let mut f = working();
        f.kill_err = Some("denied".into());
        assert!(run_fix(&mut f, None, &roots(), &mut |_| {})
            .message()
            .contains("Setting saved"));
        let mut f = Fake {
            health: vec![listening(ListenerKind::StockTrayServer)],
            tray_err: Some("denied".into()),
            ..Default::default()
        };
        assert!(run_fix(&mut f, None, &roots(), &mut |_| {})
            .message()
            .contains("Setting saved"));
        let mut f = working();
        f.stop_waits_out = true;
        assert!(run_fix(&mut f, None, &roots(), &mut |_| {})
            .message()
            .contains("Setting saved"));
    }

    #[test]
    fn image_names_are_matched_case_insensitively_on_the_file_name_only() {
        assert!(is_ollama_exe(r"C:\A\OLLAMA.EXE"));
        assert!(is_ollama_exe("ollama.exe"));
        assert!(!is_ollama_exe(r"C:\ollama.exe\proxy.exe"));
        assert!(!is_ollama_exe(r"C:\x\ollama app.exe"));
        assert!(!is_ollama_exe(""));
    }

    #[test]
    fn only_loopback_urls_count_as_this_machine() {
        for ok in [
            "http://127.0.0.1:11434",
            "http://localhost:11434/",
            "http://LOCALHOST",
            "http://[::1]:11434",
        ] {
            assert!(is_loopback_base_url(ok), "{ok}");
        }
        for bad in [
            "http://192.168.1.5:11434",
            "http://example.com",
            "http://127.0.0.1.evil.com:11434",
            "http://localhost@evil.com",
            "",
        ] {
            assert!(!is_loopback_base_url(bad), "{bad}");
        }
    }

    #[test]
    fn the_offer_text_names_a_different_existing_value() {
        assert!(offer_text(None).contains("sets OLLAMA_IGPU_ENABLE=1"));
        assert!(offer_text(Some("1")).contains("sets"));
        assert!(offer_text(Some("0")).contains("replaces your OLLAMA_IGPU_ENABLE=0"));
    }

    #[test]
    fn a_second_fix_cannot_begin_and_the_marker_tracks_the_first() {
        static FLAG: AtomicBool = AtomicBool::new(false);
        let dir = std::env::temp_dir().join("wingman-test-382-marker");
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("gpu-fix-in-progress");
        let first = begin(&FLAG, Some(marker.clone())).expect("first");
        assert!(marker.exists());
        assert!(begin(&FLAG, Some(marker.clone())).is_none());
        drop(first);
        assert!(!marker.exists());
        assert!(begin(&FLAG, Some(marker)).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_left_over_marker_is_stale_once_ollama_listens() {
        let dir = std::env::temp_dir().join("wingman-test-846-marker");
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("gpu-fix-in-progress");
        std::fs::write(&marker, "x").unwrap();
        // Nothing listening: the fix really was cut short.
        assert!(interrupted_at(Some(&marker), false, false));
        assert!(marker.exists());
        // A fix running in this process is not an interrupted one.
        assert!(!interrupted_at(Some(&marker), true, true));
        assert!(marker.exists());
        // Ollama answers: the warning ends and the marker is cleared.
        assert!(!interrupted_at(Some(&marker), false, true));
        assert!(!marker.exists());
        // No marker, or nowhere to look: never interrupted.
        assert!(!interrupted_at(Some(&marker), false, false));
        assert!(!interrupted_at(None, false, false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn progress_lines_are_reported_in_order() {
        let mut f = working();
        let mut seen = Vec::new();
        run_fix(&mut f, None, &roots(), &mut |s| seen.push(s.to_string()));
        assert_eq!(
            seen.first().map(String::as_str),
            Some("Saving the setting...")
        );
        assert_eq!(seen.last().map(String::as_str), Some("Checking the GPU..."));
    }

    // -- registry write on a scratch key (never the real Environment) -------

    #[test]
    fn write_user_value_round_trips_on_a_scratch_key() {
        use windows::Win32::System::Registry::RegDeleteTreeW;
        let root = r"Software\WingmanTest382GpuFix";
        let sub = format!(r"{root}\Environment");
        let long = "v".repeat(600);
        write_user_value(&sub, "WINGMAN_TEST_382_LONG", &long).expect("write long");
        let read_long = read_user_value(&sub, "WINGMAN_TEST_382_LONG");
        write_user_value(&sub, "WINGMAN_TEST_382", "1").expect("write");
        let read = read_user_value(&sub, "WINGMAN_TEST_382");
        unsafe {
            let w = wide(root);
            let _ = RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(w.as_ptr()));
        }
        assert_eq!(read.as_deref(), Some("1"));
        assert_eq!(read_long.as_deref(), Some(long.as_str()));
    }
}
