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
//! Only the FOREGROUND window is checked. A password manager visible beside
//! it is still in a full-monitor screenshot. Store/UWP builds run under
//! `ApplicationFrameHost.exe`, so their process name says nothing and they
//! are not covered. The tray menu and the palette take the foreground
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
    let image = (pid != 0)
        .then(|| crate::provider::ollama_admin::process_image_path(pid))
        .flatten();
    let mut buf = [0u16; 512];
    let len = unsafe { GetWindowTextW(hwnd, &mut buf) }.max(0) as usize;
    let title = String::from_utf16_lossy(&buf[..len.min(buf.len())]);
    is_password_manager(image.as_deref(), &title, extra)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none() -> Vec<String> {
        Vec::new()
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
        "maybe_start_router",
    ];

    /// What each guarded fn must contain before it reads. The router runs
    /// after the palette has taken the foreground, so `toggle_palette`
    /// asks first and hands the answer down.
    fn guard_marker(name: &str) -> &'static str {
        match name {
            // The parameter name alone also matches the signature, so the
            // marker is the use: the early return.
            "maybe_start_router" => "|| password_manager_in_front",
            _ => "refuse_if_password_manager(",
        }
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
        "ocr_text_for",
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
        let show = body
            .find("self.palette.show(")
            .expect("no palette.show call");
        assert!(ask < show, "the palette steals the foreground when shown");
        assert!(
            body.contains("self.maybe_start_router(candidates, password_manager_in_front)"),
            "toggle_palette must hand its answer to the router"
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
            "app.rs",                // guarded entry points, checked above
            "actions/extract_text.rs", // wrapper called only from the guarded fn
            "ui/region.rs",          // select_region, called only from copy_region
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
            let live = src.split("#[cfg(test)]\nmod tests {").next().unwrap();
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
