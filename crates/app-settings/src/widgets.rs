//! Reusable settings row and control widgets.

use super::*;

pub(crate) fn choice_button(id: String, active: bool) -> Button {
    Button::new(id)
        .small()
        .rounded(px(CONTROL_RADIUS))
        .when(active, |button| button.primary())
        .toggled(active)
}

pub(crate) fn window_heading(title: String, color: Hsla) -> impl IntoElement {
    div()
        .id("window-heading")
        .role(Role::Heading)
        .aria_level(1)
        .aria_label(title.clone())
        .text_size(px(13.))
        .font_semibold()
        .text_color(color)
        .child(title)
}

pub(crate) fn section_label(
    id: String,
    text: String,
    icon: IconName,
    dim: Hsla,
) -> impl IntoElement {
    h_flex()
        .items_center()
        .gap_2()
        .child(Icon::new(icon).text_color(dim))
        .child(
            div()
                .id(id)
                .role(Role::Heading)
                .aria_level(2)
                .aria_label(text.clone())
                .text_xs()
                .font_semibold()
                .text_color(dim)
                .child(text),
        )
}

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

pub(crate) fn settings_section(
    id: &'static str,
    title: String,
    icon: IconName,
    card: impl IntoElement,
    dim: Hsla,
) -> impl IntoElement {
    v_flex()
        .id(id)
        .role(Role::Group)
        .gap_1()
        .child(section_label(format!("{id}-heading"), title, icon, dim))
        .child(card)
}

pub(crate) fn blur_on_outside(element: Div) -> Div {
    element.on_mouse_down_out(|_, window, cx| window.blur(cx))
}

fn control_row(
    id: &'static str,
    title: String,
    icon: IconName,
    control: impl IntoElement,
    dim: Hsla,
    spaced: bool,
) -> impl IntoElement {
    h_flex()
        .id(id)
        .role(Role::Group)
        .justify_between()
        .items_center()
        .when(spaced, |this| this.gap_4())
        .py_2()
        .child(
            h_flex()
                .items_center()
                .gap_2()
                .child(Icon::new(icon).text_color(dim))
                .child(
                    div()
                        .id(format!("{id}-title"))
                        .role(Role::Label)
                        .aria_value(title.clone())
                        .child(title),
                ),
        )
        .child(control)
}

pub(crate) fn labeled_row(
    id: &'static str,
    title: String,
    icon: IconName,
    control: impl IntoElement,
    dim: Hsla,
) -> impl IntoElement {
    control_row(id, title, icon, control, dim, true)
}

pub(crate) fn switch_row(
    id: &'static str,
    title: String,
    icon: IconName,
    control: impl IntoElement,
    dim: Hsla,
) -> impl IntoElement {
    control_row(id, title, icon, control, dim, false)
}

pub(crate) fn disabled_row(
    id: &'static str,
    title: String,
    icon: IconName,
    note: String,
    dim: Hsla,
    faint: Hsla,
) -> impl IntoElement {
    v_flex()
        .id(id)
        .role(Role::Group)
        .py_2()
        .gap_1()
        .child(
            h_flex()
                .items_center()
                .gap_2()
                .child(Icon::new(icon).text_color(faint))
                .child(
                    div()
                        .id(format!("{id}-title"))
                        .role(Role::Label)
                        .aria_value(title.clone())
                        .text_color(dim)
                        .child(title),
                ),
        )
        .child(
            div()
                .id(format!("{id}-note"))
                .role(Role::Label)
                .aria_value(note.clone())
                .text_xs()
                .text_color(faint)
                .child(note),
        )
}

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
        .id("save-error")
        .role(Role::Alert)
        .aria_label(title.clone())
        .aria_description(hint.clone())
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

pub(crate) use sysvitals_panel::SV_BUILD_ID;

pub(crate) const REPO_URL: &str = env!("CARGO_PKG_REPOSITORY");

pub(crate) fn prefix_cell(
    id: &'static str,
    label: String,
    icon: IconName,
    input: &Entity<InputState>,
    (label_color, count_color): (Hsla, Hsla),
    count: usize,
    control: Option<(Hsla, Hsla, f32)>,
) -> impl IntoElement {
    let over = count > MAX_PREFIX_CHARS;
    let count_color = if over {
        hsla(0., 0.70, 0.50, 1.0)
    } else {
        count_color
    };
    let mut field = Input::new(input).w_full().aria_label(label.clone()).suffix(
        div()
            .text_xs()
            .text_color(count_color)
            .child(format!("{count}/{MAX_PREFIX_CHARS}")),
    );
    if let Some((bg, border, radius)) = control {
        field = field.bg(bg).border_color(border).rounded(px(radius));
    }
    v_flex()
        .id(id)
        .role(Role::Group)
        .flex_1()
        .gap_1()
        .child(
            h_flex()
                .items_center()
                .gap_1()
                .child(Icon::new(icon).text_color(label_color))
                .child(
                    div()
                        .id(format!("{id}-title"))
                        .role(Role::Label)
                        .aria_value(label.clone())
                        .text_xs()
                        .text_color(label_color)
                        .child(label),
                ),
        )
        .child(blur_on_outside(div().w_full()).child(field))
}
