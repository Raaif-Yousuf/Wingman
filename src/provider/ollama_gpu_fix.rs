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
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE,
};
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

/// What Settings shows next to the button, before it is pressed: the whole
/// list of things the button does.
pub const OFFER_TEXT: &str = "Ollama is using the CPU, not your Intel GPU. Fix it sets \
    OLLAMA_IGPU_ENABLE=1 for your Windows account, then stops and restarts Ollama.";

/// What the button says while the fix runs.
pub const RUNNING_TEXT: &str = "Fixing: setting the variable and restarting Ollama...";

// ===========================================================================
// Pure decisions
// ===========================================================================

/// Whether any of the DXGI adapter vendor ids is Intel.
pub fn has_intel_gpu(vendor_ids: &[u32]) -> bool {
    vendor_ids.contains(&VENDOR_INTEL)
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
    /// Quits the stock tray app (it respawns a CPU server if left alone).
    fn stop_tray_app(&mut self) -> Result<(), String>;
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

/// Runs the fix. `fallback_exe` is where `ollama.exe` is if the running
/// server's own path could not be read. `progress` is called with a short
/// line before each step. Never leaves Ollama stopped without saying so.
pub fn run_fix(
    sys: &mut dyn FixSystem,
    fallback_exe: Option<PathBuf>,
    progress: &mut dyn FnMut(&str),
) -> FixOutcome {
    // Look before touching anything, so a missing exe is found while Ollama
    // is still running.
    let before = sys.health();
    let exe = match &before {
        OllamaHealth::Listening { image_path, .. } if !image_path.is_empty() => {
            Some(PathBuf::from(image_path))
        }
        _ => fallback_exe,
    };
    let Some(exe) = exe else {
        return FixOutcome::NotChanged(
            "Could not find ollama.exe to restart it, so nothing was changed.".to_string(),
        );
    };

    progress("Saving the setting...");
    if let Err(e) = sys.set_user_env() {
        return FixOutcome::NotChanged(format!(
            "Could not save the setting ({}). Ollama was not touched.",
            short(&e)
        ));
    }

    if let OllamaHealth::Listening { kind, .. } = before {
        if kind == ListenerKind::StockTrayServer {
            progress("Quitting the stock Ollama app...");
            if let Err(e) = sys.stop_tray_app() {
                return FixOutcome::NotChanged(format!(
                    "Could not quit the stock Ollama app ({}). Quit it from the tray, then try again.",
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
            progress("Stopping Ollama...");
            if let Err(e) = sys.kill_tree(pid) {
                return FixOutcome::NotChanged(format!(
                    "Could not stop Ollama ({}). It is still running. Quit it, then try again.",
                    short(&e)
                ));
            }
        }
        if !sys.wait_for_listener(false, STOP_WAIT) {
            return FixOutcome::NotChanged(
                "Ollama did not stop, or started again at once. Quit it from the tray, \
                 then try again."
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
// The real machine
// ===========================================================================

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Intel adapters via DXGI. Empty when DXGI cannot be asked (then no fix is
/// offered: the safe outcome, since the button changes things).
pub fn adapter_vendor_ids() -> Vec<u32> {
    let mut out = Vec::new();
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else {
            return out;
        };
        let mut i = 0;
        while let Ok(adapter) = factory.EnumAdapters1(i) {
            i += 1;
            if let Ok(desc) = adapter.GetDesc1() {
                if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0 {
                    out.push(desc.VendorId);
                }
            }
        }
    }
    out
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

fn taskkill(args: &[&str]) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
    let out = std::process::Command::new(Path::new(&root).join(r"System32\taskkill.exe"))
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("could not run taskkill: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
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

    fn stop_tray_app(&mut self) -> Result<(), String> {
        match taskkill(&["/F", "/IM", TRAY_APP_EXE]) {
            Ok(()) => Ok(()),
            // Already gone is fine: nothing left to stop.
            Err(e) if e.contains("not found") => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn kill_tree(&mut self, pid: u32) -> Result<(), String> {
        taskkill(&["/F", "/T", "/PID", &pid.to_string()])
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
        assert!(!OFFER_TEXT.contains('\u{2014}') && !RUNNING_TEXT.contains('\u{2014}'));
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
        fn stop_tray_app(&mut self) -> Result<(), String> {
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

    fn run(f: &mut Fake) -> FixOutcome {
        run_fix(f, None, &mut |_| {})
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
            OFFER_TEXT.to_string(),
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
    fn progress_lines_are_reported_in_order() {
        let mut f = working();
        let mut seen = Vec::new();
        run_fix(&mut f, None, &mut |s| seen.push(s.to_string()));
        assert_eq!(
            seen.first().map(String::as_str),
            Some("Saving the setting...")
        );
        assert_eq!(seen.last().map(String::as_str), Some("Checking the GPU..."));
    }

    // -- registry write on a scratch key (never the real Environment) -------

    #[test]
    fn write_user_value_round_trips_on_a_scratch_key() {
        use windows::Win32::System::Registry::{RegDeleteTreeW, RegGetValueW, RRF_RT_REG_SZ};
        let root = r"Software\WingmanTest382GpuFix";
        let sub = format!(r"{root}\Environment");
        write_user_value(&sub, "WINGMAN_TEST_382", "1").expect("write");
        let mut buf = [0u16; 16];
        let mut len = (buf.len() * 2) as u32;
        let got = unsafe {
            let (s, n) = (wide(&sub), wide("WINGMAN_TEST_382"));
            RegGetValueW(
                HKEY_CURRENT_USER,
                PCWSTR(s.as_ptr()),
                PCWSTR(n.as_ptr()),
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut len),
            )
        };
        let read = got.is_ok().then(|| {
            String::from_utf16_lossy(&buf[..buf.iter().position(|&u| u == 0).unwrap_or(0)])
        });
        unsafe {
            let w = wide(root);
            let _ = RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(w.as_ptr()));
        }
        assert_eq!(read.as_deref(), Some("1"));
    }
}
