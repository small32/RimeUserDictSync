#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod config;
mod dictionary;
mod platform;
mod sync;
mod ui;
mod webdav;

fn main() -> eframe::Result {
    ui::run()
}
