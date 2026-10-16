#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod actions;
#[cfg(test)]
mod actions_example;
mod activation;
mod app;
mod autostart;
mod calc;
mod capture;
mod config;
#[cfg(test)]
mod config_example;
mod connectors;
mod crash_note;
mod diagnostics;
mod dismiss;
mod dpapi;
mod egress;
mod executors;
mod hotkey;
mod hotkey_conflicts;
mod inputs;
mod known_folder;
mod machine;
mod mode;
mod ocr;
mod options_keys;
mod owner_acl;
mod password_managers;
mod pause;
mod payment_denylist;
mod portable;
mod profile;
mod provider;
mod redact;
mod router;
mod secrets;
mod single_instance;
mod speech;
mod store;
mod ui;
mod ui_thread;
mod usage;

fn main() {
    // Issue #227: first, so a panic anywhere after this leaves a crash note.
    crash_note::install_hook();

    // Checked before any production init (hotkeys, tray icon, single
    // instance mutex): issue #363, debug builds only. In a release build
    // `run_gallery` does not exist, so `--ui-gallery` is a silent no-op and
    // falls through to the normal launch, documented in AGENTS.md's
    // ui-gallery notes.
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if let Some(gallery_args) = ui::gallery::parse_gallery_args(&argv) {
        #[cfg(debug_assertions)]
        {
            if let Err(e) = ui::gallery::run_gallery(gallery_args) {
                eprintln!("Wingman: --ui-gallery failed: {e:#}");
                std::process::exit(1);
            }
            return;
        }
        #[cfg(not(debug_assertions))]
        {
            let _ = gallery_args;
        }
    }

    if let Err(e) = app::run() {
        eprintln!("Wingman: {e:#}");
        std::process::exit(1);
    }
}
