//! SysVitals panel host: one executable for the settings window and the history popup.

#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    let mode = std::env::args()
        .skip(1)
        .find(|arg| arg == "--settings" || arg == "--history");
    match mode.as_deref() {
        Some("--settings") => app_settings::run(),
        Some("--history") => app_history::run(),
        _ => {
            sysvitals_support::log_event(
                "WARN",
                "panel: missing --settings/--history mode; exiting",
            );
            std::process::exit(2);
        }
    }
}
