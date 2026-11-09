//! widgets: split out of the crate root (see `main.rs` for the module map).

use super::*;

/// A section heading: WinUI group headers are 12px semibold and muted.
/// A muted 16px icon anchors the left edge of each group, matching the
/// single-color row icons below.
pub(crate) fn section_label(text: String, icon: IconName, dim: Hsla) -> impl IntoElement {
    h_flex()
        .items_center()
        .gap_2()
        .child(Icon::new(icon).text_color(dim))
        .child(div().text_xs().font_semibold().text_color(dim).child(text))
}

/// A grouped option surface. Windows keeps the WinUI card (8px radius plus
/// an optional hairline when the opaque light fill would melt into a bright
/// Mica page); macOS uses a flatter grouped-list card (10px radius). Rows
/// touch on both hosts and are separated by [`card_divider`], like a native
/// grouped list rather than a stack of floating controls.
pub(crate) fn settings_card(bg: Hsla, border: Option<Hsla>) -> Div {
    v_flex()
        .w_full()
        .rounded(px(if cfg!(target_os = "macos") { 10. } else { 8. }))
        .bg(bg)
        .when(border.is_some(), |this| {
            this.border_1()
                .border_color(border.unwrap_or_else(|| hsla(0., 0., 0., 0.)))
        })
        .p_3()
}

/// Grouped-list separator between option rows, one device pixel thick.
///
/// Windows uses WinUI's `DividerStrokeColorDefault` (`#0F000000` in light,
/// `#15FFFFFF` in dark). macOS keeps `NSColor.separatorColor` (10% black /
/// white), which is what System Settings paints. The line is deliberately a
/// hairline in physical pixels: one logical point would be two pixels on a
/// Retina display (or 1.25–1.5px at Windows fractional scaling) and read
/// heavier than either platform's grouped lists.
pub(crate) fn card_divider(dark: bool, scale_factor: f32) -> Div {
    let color = if cfg!(windows) {
        if dark {
            hsla(0., 0., 1., 0x15 as f32 / 255.)
        } else {
            hsla(0., 0., 0., 0x0F as f32 / 255.)
        }
    } else if dark {
        hsla(0., 0., 1., 0.10)
    } else {
        hsla(0., 0., 0., 0.10)
    };
    let hairline = 1.0 / scale_factor.max(1.0);
    div().h(px(hairline)).w_full().bg(color)
}

/// A section heading paired with its card at the tight 4px label gap.
pub(crate) fn settings_section(
    title: String,
    icon: IconName,
    card: impl IntoElement,
    dim: Hsla,
) -> impl IntoElement {
    v_flex()
        .gap_1()
        .child(section_label(title, icon, dim))
        .child(card)
}

/// Blur the window when a mouse-down occurs outside `element`. Used around
/// inputs/selects so their focus border disappears when the user clicks any
/// other part of the settings page (GPUI keeps focus otherwise).
pub(crate) fn blur_on_outside(element: Div) -> Div {
    element.on_mouse_down_out(|_, window, cx| window.blur(cx))
}

/// A labelled control row: a muted single-color icon and title on the left,
/// a wrapped tile group on the right. Centered vertically like a WinUI
/// settings row.
pub(crate) fn labeled_row(
    title: String,
    icon: IconName,
    control: impl IntoElement,
    dim: Hsla,
) -> impl IntoElement {
    h_flex()
        .justify_between()
        .items_center()
        .gap_4()
        .py_2()
        .child(
            h_flex()
                .items_center()
                .gap_2()
                .child(Icon::new(icon).text_color(dim))
                .child(title),
        )
        .child(control)
}

/// A labelled on/off row with the WinUI row height.
pub(crate) fn switch_row(
    title: String,
    icon: IconName,
    control: impl IntoElement,
    dim: Hsla,
) -> impl IntoElement {
    h_flex()
        .justify_between()
        .items_center()
        .py_2()
        .child(
            h_flex()
                .items_center()
                .gap_2()
                .child(Icon::new(icon).text_color(dim))
                .child(title),
        )
        .child(control)
}

/// A setting whose control is unavailable on this host. The title stays
/// readable while the note explains *why* it is missing — disappearing rows
/// make a settings page feel broken.
pub(crate) fn disabled_row(
    title: String,
    icon: IconName,
    note: String,
    dim: Hsla,
    faint: Hsla,
) -> impl IntoElement {
    v_flex()
        .py_2()
        .gap_1()
        .child(
            h_flex()
                .items_center()
                .gap_2()
                .child(Icon::new(icon).text_color(faint))
                .child(div().text_color(dim).child(title)),
        )
        .child(div().text_xs().text_color(faint).child(note))
}

/// Inline error surface for a failed config write. It sits in the fixed
/// header above the scroll area (not a toast) so the message stays visible
/// while the user edits controls further down the page.
pub(crate) fn error_banner(title: String, hint: String, dark: bool) -> impl IntoElement {
    let (fg, bg, border) = if dark {
        (
            hsla(0., 0.65, 0.62, 1.),
            hsla(0., 0.65, 0.5, 0.12),
            hsla(0., 0.65, 0.6, 0.30),
        )
    } else {
        (
            hsla(0., 0.70, 0.45, 1.),
            hsla(0., 0.70, 0.5, 0.08),
            hsla(0., 0.70, 0.5, 0.25),
        )
    };
    h_flex()
        .w_full()
        .items_start()
        .gap_2()
        .p_2()
        .rounded(px(CONTROL_RADIUS))
        .bg(bg)
        .border_1()
        .border_color(border)
        .child(Icon::new(IconName::CircleAlert).text_color(fg))
        .child(
            v_flex()
                .gap_1()
                .child(div().text_sm().text_color(fg).child(title))
                .child(div().text_xs().text_color(fg.opacity(0.85)).child(hint)),
        )
}

/// Version banner shown in the settings footer — see the footer for how it
/// is produced by the build script.
pub(crate) const SV_BUILD_ID: &str = match option_env!("SV_BUILD_ID") {
    Some(id) => id,
    None => concat!("v", env!("CARGO_PKG_VERSION")),
};

/// Where the footer version link points. Taken from the manifest's
/// `repository` field (inherited from the workspace) so the URL cannot drift
/// from the crate metadata.
pub(crate) const REPO_URL: &str = env!("CARGO_PKG_REPOSITORY");

/// One prefix cell of the settings 2x2 grid: a dim caption above a flexible
/// text input. Takes the entity by reference so the render borrow survives
/// the child call. The input carries a live `n/8` counter at its right edge:
/// the stored value is truncated on commit, but the user should see the limit
/// while typing.
pub(crate) fn prefix_cell(
    label: String,
    icon: IconName,
    input: &Entity<InputState>,
    label_color: Hsla,
    count_color: Hsla,
    count: usize,
    control: Option<(Hsla, Hsla, f32)>,
) -> impl IntoElement {
    let over = count > MAX_PREFIX_CHARS;
    let count_color = if over {
        hsla(0., 0.70, 0.50, 1.0)
    } else {
        count_color
    };
    let mut field = Input::new(input).w_full().suffix(
        div()
            .text_xs()
            .text_color(count_color)
            .child(format!("{count}/{MAX_PREFIX_CHARS}")),
    );
    if let Some((bg, border, radius)) = control {
        field = field.bg(bg).border_color(border).rounded(px(radius));
    }
    v_flex()
        .flex_1()
        .gap_1()
        .child(
            h_flex()
                .items_center()
                .gap_1()
                .child(Icon::new(icon).text_color(label_color))
                .child(div().text_xs().text_color(label_color).child(label)),
        )
        .child(blur_on_outside(div().w_full()).child(field))
}
