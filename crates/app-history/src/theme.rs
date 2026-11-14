//! theme: split out of the crate root (see `main.rs` for the module map).

use super::*;

// ---------------------------------------------------------------------------
// Shared theme, accent and backdrop helpers
// ---------------------------------------------------------------------------

/// Re-apply the shared theme/accent/material stack, then the history-only
/// chart-hairline, button-state and font overrides.
pub(crate) fn apply_theme_and_accent(cx: &mut App) {
    sysvitals_panel::apply_theme_and_accent(cx, PanelSurface::History);
    // The library's `chart_grid` falls back to `border.opacity(0.6)`, and
    // the dark palette's border is a near-black neutral, so the grid read as
    // black lines on the dark panel. Derive it from the foreground instead:
    // light hairlines on dark, dark hairlines on light.
    //
    // The chart's axis lines read `border` itself, which needs the same
    // treatment: light mode's `neutral-200` read as a white x axis on the
    // light material (macOS), and dark mode's `neutral-800` is the same
    // near-black problem the grid patch above fixes. The popup's only other
    // `border` consumers are the tooltip frame (keeps its shadow and popover
    // background) and the hover crossline, which mixes 80% toward the
    // foreground and stays visible either way.
    UiTheme::update(cx, |theme| {
        theme.colors.chart_grid = theme.foreground.opacity(0.12);
        theme.colors.border = theme.foreground.opacity(0.12);
    });
    // The button hover/active fills are translucent neutral layers. Windows'
    // renderer accumulates alpha across nested translucent layers, so over
    // this popup's material-tinted page the hover surface renders as a mid
    // gray. Use the opaque equivalents of what the settings window shows
    // over its opaque cards, so hovering a range or copy button reads the
    // same in both panels. macOS composites alpha correctly and keeps the
    // library colors.
    #[cfg(windows)]
    UiTheme::update(cx, |theme| {
        let (hover, active) = if theme.mode.is_dark() {
            (hsla(0., 0., 0.176, 1.), hsla(0., 0., 0.173, 1.))
        } else {
            (hsla(0., 0., 0.941, 1.), hsla(0., 0., 0.957, 1.))
        };
        theme.colors.button_hover = hover;
        theme.colors.button_active = active;
    });
}
