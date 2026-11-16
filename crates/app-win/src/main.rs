//! Windows taskbar overlay: topmost layered windows per taskbar; Win11 XAML tray content sits above embedded GDI children.

#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

#[cfg(windows)]
mod win;

fn main() {
    #[cfg(windows)]
    win::run();
    #[cfg(not(windows))]
    println!("app-win builds its display layer on Windows only.");
}
