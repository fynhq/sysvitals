//! widgets: split out of the crate root (see `main.rs` for the module map).

use super::*;

/// A section heading: WinUI group headers are 12px semibold and muted.
/// A muted 16px icon anchors the left edge of each group, matching the
/// single-color row icons below.
pub fn section_label(text: String, icon: IconName, dim: Hsla) -> impl IntoElement {
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
pub fn settings_card(bg: Hsla, border: Option<Hsla>) -> Div {
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
pub fn card_divider(dark: bool, scale_factor: f32) -> Div {
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
pub fn settings_section(
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
pub fn blur_on_outside(element: Div) -> Div {
    element.on_mouse_down_out(|_, window, cx| window.blur(cx))
}

/// A labelled control row: a muted single-color icon and title on the left,
/// a wrapped tile group on the right. Centered vertically like a WinUI
/// settings row.
pub fn labeled_row(
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
pub fn switch_row(
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

/// Version banner shown in the settings footer — see the footer for how it
/// is produced by the build script.
pub const SV_BUILD_ID: &str = match option_env!("SV_BUILD_ID") {
    Some(id) => id,
    None => concat!("v", env!("CARGO_PKG_VERSION")),
};

/// Where the footer version link points. Taken from the manifest's
/// `repository` field (inherited from the workspace) so the URL cannot drift
/// from the crate metadata.
pub const REPO_URL: &str = env!("CARGO_PKG_REPOSITORY");

/// One prefix cell of the settings 2x2 grid: a dim caption above a flexible
/// text input. Takes the entity by reference so the render borrow survives
/// the child call.
pub fn prefix_cell(
    label: String,
    icon: IconName,
    input: &Entity<InputState>,
    label_color: Hsla,
    control: Option<(Hsla, Hsla, f32)>,
) -> impl IntoElement {
    let mut field = Input::new(input).w_full();
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
