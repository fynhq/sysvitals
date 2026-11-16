//! History popup theme, accent, and chart overrides.

use super::*;

pub(crate) fn apply_theme_and_accent(cx: &mut App) {
    sysvitals_panel::apply_theme_and_accent(cx, PanelSurface::History);
    UiTheme::update(cx, |theme| {
        theme.colors.chart_grid = theme.foreground.opacity(0.12);
        theme.colors.border = theme.foreground.opacity(0.12);
    });
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
