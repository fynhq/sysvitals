//! Settings window theme, typography, and backdrop.

use super::*;

pub(crate) fn apply_theme_and_accent(cx: &mut App) {
    sysvitals_panel::apply_theme_and_accent(cx, PanelSurface::Settings);
    UiTheme::update(cx, |theme| {
        theme.font_size = px(16.);
        theme.focus_ring = false;
        #[cfg(target_os = "macos")]
        {
            theme.colors.switch_thumb = hsla(0., 0., 1., 1.);
        }
        #[cfg(windows)]
        {
            let dark = theme.mode.is_dark();
            if dark {
                theme.colors.switch_thumb = hsla(0., 0., 1., 1.);
            }
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
