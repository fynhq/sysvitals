//! theme: split out of the crate root (see `main.rs` for the module map).

use super::*;

// ---------------------------------------------------------------------------
// Window backdrop material (Win11 Mica / macOS blur, with fallbacks)
// ---------------------------------------------------------------------------

/// Re-apply the shared theme/accent/material stack, then the settings-only
/// typography and token overrides.
pub(crate) fn apply_theme_and_accent(cx: &mut App) {
    sysvitals_panel::apply_theme_and_accent(cx, PanelSurface::Settings);
    // Typography: keep the library's 16px rem base so its 0.875rem controls
    // (buttons, inputs, selects, dropdown rows) land on 14px, matching the
    // option labels on the left — the render root pins body text to that
    // 14px size. The focus ring is disabled because its 2px outer ring reads
    // as a heavy border on these compact controls, and clicking outside a
    // control is handled separately by blurring the window.
    UiTheme::update(cx, |theme| {
        theme.font_size = px(16.);
        theme.focus_ring = false;
        #[cfg(target_os = "macos")]
        {
            // Keep the switch thumb white in both appearances, on both the
            // unchecked track and the accent track. Option buttons keep their
            // original library styling; only the selected tile is
            // accent-tinted by `apply_accent`.
            theme.colors.switch_thumb = hsla(0., 0., 1., 1.);
        }
        #[cfg(windows)]
        {
            if let Some(family) = panel_font_family() {
                theme.font_family = family.into();
            }
            let dark = theme.mode.is_dark();
            // The library's switch thumb falls back to the theme background:
            // white in light mode, but neutral-950 in dark mode, which nearly
            // disappears against the #404040 track. WinUI keeps the thumb
            // white in both appearances; the checked track still follows the
            // system accent through `tokens.primary` (apply_accent).
            if dark {
                theme.colors.switch_thumb = hsla(0., 0., 1., 1.);
            }
            // WinUI control strokes and flyout surfaces. The library's
            // neutral-200/neutral-950 pair is much flatter than the Windows
            // Settings controls, which draw a visible stroke around a
            // subtly-filled input and a lighter flyout than the page behind
            // it.
            theme.colors.input = if dark {
                hsla(0., 0., 1., 0.30)
            } else {
                hsla(0., 0., 0., 0.26)
            };
            theme.colors.popover = if dark {
                hsla(0., 0., 0.17, 1.)
            } else {
                hsla(0., 0., 0.976, 1.)
            };
            theme.colors.popover_foreground = theme.colors.foreground;
        }
    });
}
