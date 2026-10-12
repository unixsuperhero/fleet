#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if let Some(code) = fleet_lib::helper::run_if_helper() {
        std::process::exit(code);
    }
    fleet_lib::run();
}
