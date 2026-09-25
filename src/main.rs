#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod actions;
#[cfg(test)]
mod actions_example;
mod app;
mod autostart;
mod calc;
mod capture;
mod config;
#[cfg(test)]
mod config_example;
mod connectors;
mod diagnostics;
mod dismiss;
mod dpapi;
mod egress;
mod executors;
mod hotkey;
mod hotkey_conflicts;
mod inputs;
mod known_folder;
mod mode;
mod ocr;
mod pause;
mod payment_denylist;
mod profile;
mod provider;
mod router;
mod secrets;
mod single_instance;
mod ui;

fn main() {
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
