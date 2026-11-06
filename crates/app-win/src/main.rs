//! Windows display layer: one topmost layered overlay per taskbar, so every
//! monitor that shows a taskbar also shows the indicator.
//!
//! Why an overlay and not a `SetParent` child of `Shell_TrayWnd`: on current
//! Windows 11 builds the taskbar's own content is composited by a XAML Islands
//! child window whose visual sits above every GDI child of the tray, so an
//! embedded child window can never render crisply (verified empirically with
//! calibration windows: an embedded opaque child is blurred and tinted by the
//! taskbar acrylic, while an identical topmost layered popup is pixel-crisp).
//! A topmost layered popup aligned to the taskbar geometry gives the same
//! visual result through a mechanism the compositor actually honors.
//!
//! Consequences handled here:
//! - One overlay exists per taskbar: `Shell_TrayWnd` (the main bar) plus every
//!   `Shell_SecondaryTrayWnd`. Each overlay is owned by its own taskbar so it
//!   follows that bar's z-band, and each measures/renders at its monitor's own
//!   DPI. A hidden sentinel window owns the shared timers (one sample per tick
//!   drives every overlay) and reconciles the overlay set whenever taskbars or
//!   monitors appear/disappear.
//! - The overlay does not share the taskbar's z-order or visibility, so each
//!   tick re-checks whether its taskbar is present (auto-hide). The indicator
//!   hides only when that taskbar leaves the screen; no foreground-window
//!   heuristics are involved.
//! - Nothing is embedded, so there is no explorer-side state to roll back on
//!   exit; destroying the window is the whole teardown.
//!
//! Settings architecture (post-refactor):
//! The settings UI lives in a separate process (`SysVitalsSettings.exe`,
//! built with GPUI). This overlay process does NOT host any settings dialog.
//! Instead it watches the shared config file for changes: a 500 ms poll timer
//! compares the file's last-modified timestamp and, on change, reloads the
//! config and re-renders. The context menu "Settings" item simply spawns the
//! settings executable from the same directory. This decoupling keeps the
//! overlay process minimal (no UI toolkit, no modal loops) and lets the
//! settings app evolve independently.
//!
//! Typography mirrors the taskbar clock: the system UI message font at the
//! system size, semibold weight, and pure white / pure black glyphs per theme
//! so the indicator reads as part of the taskbar rather than a foreign widget.
//! Rendering is GDI text; the layout strings come from the pure core crate so
//! the single-row / dual-row quadrant rules stay unit-tested there. Glyphs are
//! drawn at 2x supersampled resolution and box-filtered down for smooth edges
//! without ClearType (which corrupts per-pixel-alpha compositing).

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod win;

fn main() {
    #[cfg(windows)]
    win::run();
    #[cfg(not(windows))]
    println!("app-win builds its display layer on Windows only.");
}
