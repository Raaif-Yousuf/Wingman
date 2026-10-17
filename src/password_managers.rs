//! Issue #339 (a): Wingman never captures or reads while a password manager
//! is the foreground window. A screenshot can go to a cloud model, and a
//! selection, clipboard or UI Automation read can pick up a vault item, so
//! the check runs on the main thread BEFORE any of them, for every action.
//!
//! The matching here is pure. The Win32 half (the foreground window's image
//! path and title) lives in `app.rs`'s `refuse_if_password_manager`, which
//! every input-gathering entry point must call first; the source-scanning
//! tests at the bottom keep that true.
//!
//! Matching is by process image name, because that is stable across
//! versions and window titles. Titles are matched only where they are
//! reliable: the Windows Credential Manager control panel, and the
//! browsers' own password pages (best effort: a browser can retitle them).
//!
//! # Limits
//!
//! Only the FOREGROUND window is REFUSED. A password manager visible beside
//! it (#575) is painted out of a cloud-bound screenshot instead: see
//! [`manager_window_rects`]. Store/UWP builds run under
//! `ApplicationFrameHost.exe`, so their process name says nothing: for such
//! a frame the image of the app's own `Windows.UI.Core.CoreWindow` child is
//! matched instead (#636, [`window_image_path`]). The tray menu and the palette take the foreground
//! before an action runs, so `App` remembers who was in front when they
//! opened (see `password_manager_hint` in `app.rs`).

/// Executable names (lowercase, with `.exe`) of password managers and of
/// Windows' own credential screens. Matched against the file name of the
/// foreground window's process, case-insensitively.
///
/// THEORY (unverified): the names for LastPass, Keeper, Proton Pass and
/// RoboForm are guesses from the vendors' installers, not measured on a
/// machine with each installed. `capture.extra_password_managers` covers a
/// wrong guess.
pub const KNOWN_EXECUTABLES: &[&str] = &[
    "1password.exe",
    "bitwarden.exe",
    "keepass.exe",
    "keepassxc.exe",
    "lastpass.exe",
    "dashlane.exe",
    "keeper.exe",
    "keeperpasswordmanager.exe",
    "nordpass.exe",
    "proton pass.exe",
    "protonpass.exe",
    "enpass.exe",
    "roboform.exe",
    // Windows: the Stored User Names and Passwords wizard and the sign-in
    // prompt broker.
    "credwiz.exe",
    "credentialuibroker.exe",
];

/// Window titles that are a password screen on their own.
const EXACT_TITLES: &[&str] = &["credential manager"];

/// A browser's own password page is titled "<page> - <browser>".
const BROWSER_PASSWORD_PAGES: &[&str] = &["password manager", "passwords", "logins and passwords"];
const BROWSER_NAMES: &[&str] = &[
    "google chrome",
    "microsoft edge",
    "mozilla firefox",
    "brave",
    "opera",
    "vivaldi",
];

/// The card text when a press is refused.
pub const REFUSAL_HEADLINE: &str = "Wingman does not read password managers.";
pub const REFUSAL_DETAIL: &str =
    "Nothing was captured or sent. Use your password manager's own autofill.";

/// The lowercase file name of `path` (either slash), `.exe` appended when
/// the name has no extension, so a user-typed "keepassxc" still matches.
fn normalized_exe_name(path: &str) -> String {
    let name = path
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(path)
        .trim()
        .to_lowercase();
    if name.is_empty() || name.contains('.') {
        name
    } else {
        format!("{name}.exe")
    }
}

fn title_is_password_screen(title: &str) -> bool {
    let lower = title.trim().to_lowercase();
    if EXACT_TITLES.contains(&lower.as_str()) {
        return true;
    }
    // Hyphen, en dash and em dash all appear as the browser separator, so
    // every non-word symbol is folded to a hyphen before splitting.
    let folded: String = lower
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c.is_whitespace() {
                c
            } else {
                '-'
            }
        })
        .collect();
    folded.rsplit_once(" - ").is_some_and(|(page, browser)| {
        BROWSER_PASSWORD_PAGES.contains(&page.trim()) && BROWSER_NAMES.contains(&browser.trim())
    })
}

/// Whether the foreground window is a password manager: its process image
/// name is in [`KNOWN_EXECUTABLES`] or in `extra` (the user's
/// `capture.extra_password_managers`), or its title is a password screen.
/// `image_path` is `None` when the process could not be queried; the title
/// check still applies.
pub fn is_password_manager(image_path: Option<&str>, title: &str, extra: &[String]) -> bool {
    if let Some(path) = image_path {
        let name = normalized_exe_name(path);
        if !name.is_empty()
            && (KNOWN_EXECUTABLES.contains(&name.as_str())
                || extra.iter().any(|e| normalized_exe_name(e) == name))
        {
            return true;
        }
    }
    title_is_password_screen(title)
}

/// The window class of a Store app's content window inside its frame.
const CORE_WINDOW_CLASS: &str = "Windows.UI.Core.CoreWindow";

/// Whether `path` is `ApplicationFrameHost.exe`, which owns the top-level
/// frame of every UWP/Store window while the app runs in its own process.
fn is_frame_host(path: &str) -> bool {
    normalized_exe_name(path) == "applicationframehost.exe"
}

/// The image path to match for window `hwnd` of process `pid` (#636): the
/// process's own, except that a frame owned by `ApplicationFrameHost.exe`
/// gives the image of its core-window child (a different process), so a
/// Store build of a password manager is matched by name like any other. When
/// the child cannot be found the frame host's own path is returned, which
/// matches nothing: the same as before this existed. Bounded: one pass over
/// the frame's few children.
pub fn window_image_path(hwnd: windows::Win32::Foundation::HWND, pid: u32) -> Option<String> {
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumChildWindows, GetClassNameW, GetWindowThreadProcessId,
    };
    let own = crate::provider::ollama_admin::process_image_path(pid)?;
    if !is_frame_host(&own) {
        return Some(own);
    }
    struct Found {
        frame_pid: u32,
        pid: u32,
        left: u32,
    }
    unsafe extern "system" fn visit(child: HWND, lparam: LPARAM) -> windows::core::BOOL {
        // SAFETY: `lparam` is the `&mut Found` passed below, which outlives
        // the synchronous `EnumChildWindows` call.
        let f = unsafe { &mut *(lparam.0 as *mut Found) };
        if f.left == 0 {
            return false.into();
        }
        f.left -= 1;
        let mut class = [0u16; 64];
        let len = unsafe { GetClassNameW(child, &mut class) }.max(0) as usize;
        if String::from_utf16_lossy(&class[..len.min(class.len())]) != CORE_WINDOW_CLASS {
            return true.into();
        }
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(child, Some(&mut pid)) };
        if pid != 0 && pid != f.frame_pid {
            f.pid = pid;
            return false.into();
        }
        true.into()
    }
    let mut found = Found {
        frame_pid: pid,
        pid: 0,
        left: 64,
    };
    // SAFETY: `found` outlives the call; the callback is the only user. The
    // result only says whether the walk was stopped early.
    let _ = unsafe {
        EnumChildWindows(
            Some(hwnd),
            Some(visit),
            LPARAM(&mut found as *mut Found as isize),
        )
    };
    if found.pid == 0 {
        return Some(own);
    }
    crate::provider::ollama_admin::process_image_path(found.pid).or(Some(own))
}

/// The Win32 half: reads the foreground window's process image and title and
/// asks [`is_password_manager`]. Main thread, no capture, no network. No
/// foreground window (or an unreadable one) is not a refusal: nothing is
/// known to be a password manager, and the title check still ran.
pub fn foreground_is_password_manager(extra: &[String]) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId,
    };
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.0.is_null() {
        return false;
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    let image = (pid != 0).then(|| window_image_path(hwnd, pid)).flatten();
    let mut buf = [0u16; 512];
    let len = unsafe { GetWindowTextW(hwnd, &mut buf) }.max(0) as usize;
    let title = String::from_utf16_lossy(&buf[..len.min(buf.len())]);
    is_password_manager(image.as_deref(), &title, extra)
}

/// What the window enumeration learned about one top-level window.
#[derive(Debug, Clone)]
pub struct WindowFacts {
    pub image_path: Option<String>,
    pub title: String,
    pub rect: crate::capture::ScreenRect,
}

/// The most rectangles one capture paints for password managers (#575).
/// More than this is a failure, not a truncation: nothing is sent unpainted.
pub const MAX_MANAGER_RECTS: usize = 16;

/// The most on-screen top-level windows looked at (#575). Hidden, cloaked
/// and minimised windows do not count, so this is generous.
pub const MAX_WINDOWS_SCANNED: usize = 2000;

/// Why the list of password-manager windows cannot be trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanError {
    /// The window enumeration itself failed.
    EnumerationFailed,
    /// More on-screen windows than [`MAX_WINDOWS_SCANNED`]: some were not looked at.
    BudgetExhausted,
    /// More than [`MAX_MANAGER_RECTS`] manager windows.
    TooMany,
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::EnumerationFailed => "the window list could not be read",
            Self::BudgetExhausted => "too many windows to check for password managers",
            Self::TooMany => "too many password manager windows to paint",
        })
    }
}

/// Collects [`WindowFacts`] during the window enumeration. Pure, so the
/// budget is tested without windows. A window that is not on screen costs
/// nothing and builds nothing; an on-screen one spends the budget.
pub struct Scan {
    left: usize,
    pub exhausted: bool,
    pub facts: Vec<WindowFacts>,
}

impl Scan {
    pub fn new(budget: usize) -> Self {
        Self {
            left: budget,
            exhausted: false,
            facts: Vec::new(),
        }
    }

    /// Offers one window: `None` when it is not on screen, else a closure
    /// that reads its facts. Returns whether the enumeration should go on.
    pub fn offer(&mut self, on_screen: Option<impl FnOnce() -> WindowFacts>) -> bool {
        let Some(read) = on_screen else {
            return true;
        };
        if self.left == 0 {
            self.exhausted = true;
            return false;
        }
        self.left -= 1;
        self.facts.push(read());
        true
    }
}

/// The rectangles of the password-manager windows among `windows` that
/// overlap `monitor` (#575). Pure: the Win32 enumeration in `capture.rs`
/// collects the facts. A rect from another process is never trusted: empty
/// or reversed ones are dropped. More than [`MAX_MANAGER_RECTS`] is an error.
pub fn manager_window_rects(
    windows: &[WindowFacts],
    monitor: crate::capture::ScreenRect,
    extra: &[String],
) -> Result<Vec<crate::capture::ScreenRect>, ScanError> {
    let found: Vec<_> = windows
        .iter()
        .filter(|w| {
            let r = w.rect;
            r.right > r.left
                && r.bottom > r.top
                && r.left < monitor.right
                && r.right > monitor.left
                && r.top < monitor.bottom
                && r.bottom > monitor.top
        })
        .filter(|w| is_password_manager(w.image_path.as_deref(), &w.title, extra))
        .map(|w| w.rect)
        .collect();
    if found.len() > MAX_MANAGER_RECTS {
        return Err(ScanError::TooMany);
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none() -> Vec<String> {
        Vec::new()
    }

    #[test]
    fn only_the_frame_host_is_a_frame_host() {
        assert!(is_frame_host(
            r"C:\Windows\System32\ApplicationFrameHost.exe"
        ));
        assert!(is_frame_host("applicationframehost"));
        assert!(!is_frame_host(r"C:\Windows\explorer.exe"));
        assert!(!is_frame_host(""));
        // The frame host itself is not a password manager, so a frame with
        // no resolvable child stays unmatched.
        assert!(!is_password_manager(
            Some(r"C:\Windows\System32\ApplicationFrameHost.exe"),
            "Settings",
            &none()
        ));
    }

    #[test]
    fn a_store_manager_is_matched_by_its_real_process_not_the_frame() {
        // What `window_image_path` hands the matcher for a hosted app.
        assert!(is_password_manager(
            Some(r"C:\Program Files\WindowsApps\x\Bitwarden.exe"),
            "Bitwarden",
            &none()
        ));
    }

    #[test]
    fn foreground_and_beside_checks_both_resolve_the_hosted_app() {
        // Wiring (#636): a resolver nobody calls leaves Store managers
        // unmatched while every pure test passes.
        let norm = |s: &str| s.replace('\r', "");
        let pm = norm(include_str!("password_managers.rs"));
        let at = pm.find("pub fn foreground_is_password_manager(").unwrap();
        let body = &pm[at..at + pm[at..].find("\n}\n").unwrap()];
        assert!(body.contains("window_image_path(hwnd, pid)"), "{body}");
        let capture = norm(include_str!("capture.rs"));
        let at = capture.find("fn manager_windows_on(").unwrap();
        let body = &capture[at..at + capture[at..].find("\n}\n").unwrap()];
        assert!(
            body.contains("crate::password_managers::window_image_path(hwnd, pid)"),
            "{body}"
        );
    }

    // -- beside the front window (#575) ------------------------------------

    fn sr(left: i32, top: i32, right: i32, bottom: i32) -> crate::capture::ScreenRect {
        crate::capture::ScreenRect {
            left,
            top,
            right,
            bottom,
        }
    }

    fn facts(image: &str, title: &str, rect: crate::capture::ScreenRect) -> WindowFacts {
        WindowFacts {
            image_path: Some(image.to_string()),
            title: title.to_string(),
            rect,
        }
    }

    #[test]
    fn a_manager_beside_the_front_window_is_listed_and_ordinary_windows_are_not() {
        let monitor = sr(0, 0, 1920, 1080);
        let windows = [
            facts(r"C:\Windows\explorer.exe", "Docs", sr(0, 0, 900, 1000)),
            facts(
                r"C:\Program Files\KeePassXC\KeePassXC.exe",
                "Vault",
                sr(900, 0, 1900, 1000),
            ),
        ];
        assert_eq!(
            manager_window_rects(&windows, monitor, &none()),
            Ok(vec![sr(900, 0, 1900, 1000)])
        );
    }

    #[test]
    fn the_users_own_list_and_title_screens_count_beside_the_front_window_too() {
        let monitor = sr(0, 0, 100, 100);
        let extra = vec!["myvault".to_string()];
        let windows = [
            facts(r"C:\x\MyVault.exe", "x", sr(0, 0, 10, 10)),
            facts(
                r"C:\Windows\explorer.exe",
                "Credential Manager",
                sr(20, 20, 40, 40),
            ),
        ];
        assert_eq!(
            manager_window_rects(&windows, monitor, &extra)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn a_manager_on_another_monitor_or_with_a_bad_rect_is_not_listed() {
        let monitor = sr(0, 0, 1920, 1080);
        let kp = r"C:\Program Files\KeePass\KeePass.exe";
        let windows = [
            facts(kp, "elsewhere", sr(1920, 0, 3000, 500)),
            facts(kp, "left of it", sr(-900, 0, 0, 500)),
            facts(kp, "empty", sr(10, 10, 10, 50)),
            facts(kp, "reversed", sr(500, 500, 100, 100)),
            facts(kp, "minimised", sr(-32000, -32000, -31840, -31972)),
        ];
        assert!(manager_window_rects(&windows, monitor, &none())
            .unwrap()
            .is_empty());
        // A window that only overlaps the monitor is still listed.
        let edge = [facts(kp, "edge", sr(1800, 900, 2500, 1500))];
        assert_eq!(
            manager_window_rects(&edge, monitor, &none()).unwrap().len(),
            1
        );
    }

    #[test]
    fn more_manager_windows_than_the_cap_is_an_error_not_a_truncation() {
        let monitor = sr(0, 0, 100, 100);
        let mk = |n: usize| -> Vec<WindowFacts> {
            (0..n)
                .map(|_| facts(r"C:\x\bitwarden.exe", "v", sr(0, 0, 50, 50)))
                .collect()
        };
        assert_eq!(
            manager_window_rects(&mk(MAX_MANAGER_RECTS), monitor, &none())
                .unwrap()
                .len(),
            MAX_MANAGER_RECTS
        );
        assert_eq!(
            manager_window_rects(&mk(MAX_MANAGER_RECTS + 1), monitor, &none()),
            Err(ScanError::TooMany)
        );
    }

    #[test]
    fn hidden_windows_do_not_spend_the_scan_budget() {
        let mut scan = Scan::new(MAX_WINDOWS_SCANNED);
        let hidden: Option<fn() -> WindowFacts> = None;
        for _ in 0..(MAX_WINDOWS_SCANNED * 3) {
            assert!(scan.offer(hidden));
        }
        let kp = || {
            facts(
                r"C:\Program Files\KeePass\KeePass.exe",
                "v",
                sr(0, 0, 50, 50),
            )
        };
        assert!(scan.offer(Some(kp)));
        assert!(!scan.exhausted);
        assert_eq!(
            manager_window_rects(&scan.facts, sr(0, 0, 100, 100), &none())
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn running_out_of_budget_stops_the_scan_and_says_so() {
        let mut scan = Scan::new(3);
        let w = || facts(r"C:\x\a.exe", "t", sr(0, 0, 5, 5));
        for _ in 0..3 {
            assert!(scan.offer(Some(w)));
        }
        assert!(!scan.exhausted);
        assert!(!scan.offer(Some(w)), "the fourth stops the walk");
        assert!(scan.exhausted);
        assert_eq!(scan.facts.len(), 3);
    }

    #[test]
    fn known_managers_match_by_exe_name_case_insensitively() {
        for exe in [
            r"C:\Program Files\1Password\app\8\1Password.exe",
            r"C:\Program Files\KeePassXC\KeePassXC.exe",
            r"c:\users\x\appdata\local\programs\bitwarden\BITWARDEN.EXE",
            "D:/tools/KeePass/KeePass.exe",
            r"C:\Program Files\Proton\Proton Pass\Proton Pass.exe",
        ] {
            assert!(is_password_manager(Some(exe), "", &none()), "{exe}");
        }
    }

    #[test]
    fn every_known_executable_is_lowercase_and_matches_itself() {
        assert!(!KNOWN_EXECUTABLES.is_empty());
        for exe in KNOWN_EXECUTABLES {
            assert_eq!(*exe, exe.to_lowercase());
            assert!(exe.ends_with(".exe"), "{exe}");
            assert!(is_password_manager(Some(exe), "", &none()), "{exe}");
        }
    }

    #[test]
    fn ordinary_apps_are_not_password_managers() {
        for exe in [
            r"C:\Windows\explorer.exe",
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files\Microsoft Office\root\Office16\WINWORD.EXE",
            r"C:\tools\1password-notes.exe",
        ] {
            assert!(
                !is_password_manager(Some(exe), "Document", &none()),
                "{exe}"
            );
        }
        assert!(!is_password_manager(None, "", &none()));
    }

    #[test]
    fn the_user_list_extends_the_built_in_one() {
        let extra = vec!["MyVault".to_string(), r"C:\apps\Secrets.EXE".to_string()];
        assert!(is_password_manager(Some(r"C:\x\myvault.exe"), "", &extra));
        assert!(is_password_manager(Some(r"C:\x\secrets.exe"), "", &extra));
        assert!(!is_password_manager(Some(r"C:\x\other.exe"), "", &extra));
        // An empty entry must not match everything.
        let blank = vec![String::new(), "  ".to_string()];
        assert!(!is_password_manager(Some(r"C:\x\other.exe"), "t", &blank));
        assert!(!is_password_manager(Some(r"C:\x\"), "t", &blank));
    }

    #[test]
    fn credential_manager_and_browser_password_pages_match_by_title() {
        for title in [
            "Credential Manager",
            "credential manager",
            "Password Manager - Google Chrome",
            "Passwords - Mozilla Firefox",
            "Passwords \u{2014} Mozilla Firefox",
            "Logins and Passwords - Mozilla Firefox",
            "Passwords - Microsoft Edge",
        ] {
            assert!(
                is_password_manager(Some(r"C:\Windows\explorer.exe"), title, &none()),
                "{title}"
            );
        }
    }

    #[test]
    fn ordinary_pages_about_passwords_are_not_blocked() {
        for title in [
            "Passwords - Wikipedia - Google Chrome",
            "How to pick a password manager - Mozilla Firefox",
            "Credential Manager API docs",
            "Password Manager",
            "",
        ] {
            assert!(
                !is_password_manager(Some(r"C:\Windows\explorer.exe"), title, &none()),
                "{title}"
            );
        }
    }

    // --- the guard is wired into every input-gathering entry point ---

    fn lf(src: &str) -> String {
        src.replace('\r', "")
    }

    /// Non-test source of `app.rs`, split into `(fn name, body)`.
    fn app_functions() -> Vec<(String, String)> {
        let src = lf(include_str!("app.rs"));
        let end = src
            .find("#[cfg(test)]\nmod tests {")
            .expect("app.rs no longer has its `mod tests` marker; this scanner is blind");
        let mut out: Vec<(String, String)> = Vec::new();
        let mut current: Option<(String, String)> = None;
        for line in src[..end].lines() {
            let trimmed = line.trim_start();
            let indent = line.len() - trimmed.len();
            let declares_fn = indent <= 4
                && ["fn ", "pub fn ", "pub(crate) fn "]
                    .iter()
                    .any(|p| trimmed.starts_with(p));
            if declares_fn {
                out.extend(current.take());
                let name: String = trimmed
                    .trim_start_matches("pub(crate) ")
                    .trim_start_matches("pub ")
                    .trim_start_matches("fn ")
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                current = Some((name, String::new()));
            }
            if let Some((_, body)) = current.as_mut() {
                if !trimmed.starts_with("//") {
                    body.push_str(line);
                    body.push('\n');
                }
            }
        }
        out.extend(current);
        out
    }

    /// Entry points that read the screen, the selection or the clipboard
    /// on the main thread. Each must refuse first.
    const GUARDED: &[&str] = &[
        "begin_action",
        "run_model_free_screen_action",
        "copy_region",
        "calculate_selection",
        // #307: captures a small image for the router before it shows the
        // options card.
        "show_options_card",
    ];

    /// What each guarded fn must contain before it reads.
    fn guard_marker(_name: &str) -> &'static str {
        "refuse_if_password_manager("
    }

    /// Calls that put pixels or a selection in Wingman's hands from a
    /// main-thread entry point.
    const READ_MARKERS: &[&str] = &[
        "grab_raw(",
        "capture_screen()",
        "select_region(",
        "grab_virtual_desktop_raw(",
        "ForegroundSelection",
    ];

    /// Calls that read the window's contents (selection, UI tree, page text,
    /// OCR). They run on worker threads spawned after a guarded entry point,
    /// so they live in free functions; those are listed in [`WORKER_SIDE`].
    const WORKER_READS: &[&str] = &[
        "get_selection_foreground",
        "snapshot_hwnd(",
        "read_page_text(",
        "ocr::recognize(",
    ];

    /// `app.rs` functions that use [`WORKER_READS`] and are reached only
    /// from a worker closure that a guarded entry point spawned. A new read
    /// function must be added here on purpose, with that reasoning.
    const WORKER_SIDE: &[&str] = &[
        "non_vision_inputs",
        "uia_fields_text",
        "page_text_of",
        "gather_generic_inputs",
        "form_fill_worker",
        "router_non_vision_inputs",
        "ocr_text_of",
    ];

    #[test]
    fn every_main_thread_entry_point_refuses_before_it_reads() {
        let fns = app_functions();
        assert!(fns.len() > 100, "scanner found only {} fns", fns.len());
        for name in GUARDED {
            let (_, body) = fns
                .iter()
                .find(|(n, _)| n == name)
                .unwrap_or_else(|| panic!("app.rs has no fn {name}; update GUARDED"));
            let guard = body
                .find(guard_marker(name))
                .unwrap_or_else(|| panic!("{name} never checks for a password manager"));
            for marker in READ_MARKERS.iter().chain(WORKER_READS) {
                if let Some(read) = body.find(marker) {
                    assert!(guard < read, "{name} reads ({marker}) before the guard");
                }
            }
        }
    }

    #[test]
    fn the_hint_from_tray_and_palette_reaches_the_guard() {
        let fns = app_functions();
        let body_of = |name: &str| {
            fns.iter()
                .find(|(n, _)| n == name)
                .unwrap_or_else(|| panic!("app.rs has no fn {name}"))
                .1
                .clone()
        };
        // The guard asks the helper, and the helper honours the hint.
        assert!(body_of("refuse_if_password_manager").contains("self.password_manager_now()"));
        let now = body_of("password_manager_now");
        assert!(now.contains("self.password_manager_hint"));
        assert!(now.contains("foreground_is_password_manager("));
        // The palette sets the hint for its dispatch, then clears it.
        let dispatch = body_of("dispatch_palette_action");
        let set = dispatch
            .find("self.password_manager_hint = self.palette_password_manager;")
            .expect("the palette dispatch never sets the hint");
        let run = dispatch
            .find("self.dispatch_palette_target(")
            .expect("no dispatch call");
        let clear = dispatch
            .find("self.password_manager_hint = false;")
            .expect("the palette dispatch never clears the hint");
        assert!(set < run && run < clear);
        // The tray reads who was in front before the menu takes the foreground.
        let proc_src = lf(include_str!("app.rs"));
        let ask = proc_src
            .find("Tray::opens_menu(lparam) && app.password_manager_now()")
            .expect("wnd_proc never asks before the tray menu opens");
        let menu = proc_src
            .find("app.tray.on_tray_message(lparam)")
            .expect("wnd_proc never calls on_tray_message");
        assert!(ask < menu, "the tray menu takes the foreground first");
        assert!(proc_src.contains("app.password_manager_hint = menu_hint;"));
    }

    #[test]
    fn the_palette_asks_before_it_takes_the_foreground() {
        let fns = app_functions();
        let (_, body) = fns
            .iter()
            .find(|(n, _)| n == "toggle_palette")
            .expect("app.rs has no fn toggle_palette");
        let ask = body
            .find("self.password_manager_now()")
            .expect("toggle_palette never asks whether a password manager is in front");
        let show = body.find("palette.show(").expect("no palette.show call");
        assert!(ask < show, "the palette steals the foreground when shown");
        // #805: opening the palette starts no router request, so the answer
        // is kept for the action the user picks.
        assert!(
            body.contains("self.palette_password_manager = self.password_manager_now();"),
            "toggle_palette must keep its answer for the action the palette runs"
        );
    }

    #[test]
    fn no_unguarded_fn_in_app_rs_captures_or_reads_the_selection() {
        let fns = app_functions();
        let candidates: Vec<&str> = fns
            .iter()
            .filter(|(_, body)| {
                READ_MARKERS
                    .iter()
                    .chain(WORKER_READS)
                    .any(|m| body.contains(m))
            })
            .map(|(n, _)| n.as_str())
            .collect();
        assert!(!candidates.is_empty(), "no read sites found; scanner blind");
        assert!(
            candidates.iter().any(|n| WORKER_SIDE.contains(n)),
            "WORKER_SIDE names no fn that reads; the list is stale"
        );
        let unguarded: Vec<&&str> = candidates
            .iter()
            .filter(|n| !GUARDED.contains(*n) && !WORKER_SIDE.contains(*n))
            .collect();
        assert!(
            unguarded.is_empty(),
            "these app.rs fns capture or read but are in neither GUARDED nor WORKER_SIDE (add the guard, then list them): {unguarded:?}"
        );
        for name in WORKER_SIDE {
            assert!(
                candidates.contains(name),
                "{name} is in WORKER_SIDE but no longer reads anything; remove it"
            );
        }
    }

    /// Every file under `src` that calls a screen-capture function, outside
    /// its own definition and its tests. A new caller elsewhere must be
    /// reviewed for the guard and added here on purpose.
    #[test]
    fn only_known_files_capture_the_screen() {
        const ALLOWED: &[&str] = &[
            "app.rs",                  // guarded entry points, checked above
            "actions/extract_text.rs", // wrapper called only from the guarded fn
            "ui/region.rs",            // select_region, called only from copy_region
        ];
        const CAPTURE_CALLS: &[&str] = &[
            "grab_raw(",
            "grab_virtual_desktop_raw(",
            "select_region(",
            "capture_screen(",
        ];
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        walk(&root, &mut files);
        assert!(files.len() > 40, "found only {} source files", files.len());
        let mut callers: Vec<String> = Vec::new();
        for path in &files {
            let src = lf(&std::fs::read_to_string(path).unwrap());
            let live = src.split("mod tests {").next().unwrap();
            let calls = live.lines().any(|l| {
                let t = l.trim_start();
                !t.starts_with("//")
                    && !t.contains("fn ")
                    && CAPTURE_CALLS.iter().any(|c| t.contains(c))
            });
            if calls {
                let rel = path.strip_prefix(&root).unwrap();
                callers.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
        assert!(callers.len() >= 2, "scanner found no callers: {callers:?}");
        let unknown: Vec<&String> = callers
            .iter()
            .filter(|c| c.as_str() != "password_managers.rs" && !ALLOWED.contains(&c.as_str()))
            .collect();
        assert!(
            unknown.is_empty(),
            "these files capture the screen and are not on the allowlist: {unknown:?}"
        );
    }
}
