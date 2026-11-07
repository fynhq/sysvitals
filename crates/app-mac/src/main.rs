//! macOS display layer: NSStatusItem menu-bar indicator.
//!
//! Shows CPU / memory / network stats in the macOS menu bar using a
//! self-drawn template `NSImage` on an `NSStatusItem`. Samples periodically via
//! sysvitals-core and formats with the shared format engine; AppKit tints the
//! template from the menu-bar appearance.
//!
//! Interaction model (matches Windows):
//! - Single-click: toggle the history trend chart popup
//! - Right-click: context menu (Activity Monitor / Settings / Restart / Quit)
//!
//! Architecture: pure objc2 + AppKit for the indicator itself. The GPUI
//! settings window (app-settings) and history chart (app-history) are
//! launched as separate processes, same as on Windows.

#[cfg(target_os = "macos")]
mod macos;

#[cfg(not(target_os = "macos"))]
mod macos {
    pub fn run() {
        eprintln!("app-mac: this binary only runs on macOS.");
    }
}

fn main() {
    macos::run();
}
