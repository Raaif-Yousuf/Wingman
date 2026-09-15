#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod capture;
mod config;
mod dismiss;
mod hotkey;
mod provider;
mod ui;

fn main() {
    if let Err(e) = app::run() {
        eprintln!("copilot-ask: {e:#}");
        std::process::exit(1);
    }
}
