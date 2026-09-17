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
mod executors;
mod hotkey;
mod hotkey_conflicts;
mod inputs;
mod known_folder;
mod mode;
mod ocr;
mod pause;
mod provider;
mod secrets;
mod single_instance;
mod ui;

fn main() {
    if let Err(e) = app::run() {
        eprintln!("Wingman: {e:#}");
        std::process::exit(1);
    }
}
