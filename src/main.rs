#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod autostart;
mod capture;
mod config;
#[cfg(test)]
mod config_example;
mod dismiss;
mod hotkey;
mod known_folder;
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
