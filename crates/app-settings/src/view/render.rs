//! Render pass for the settings window.

use super::*;

use gpui_kit::component::collapsible::Collapsible;
#[cfg(not(target_os = "macos"))]
use sysvitals_core::backdrop::Material;

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let probe_due = self
            .last_taskbar_probe
            .is_none_or(|last| last.elapsed() >= std::time::Duration::from_secs(1));
        if probe_due {
            self.vertical_taskbar = taskbar_is_vertical();
            self.layout_available = layout_available();
            self.last_taskbar_probe = Some(Instant::now());
        }
        if self._deactivation.is_none() {
            let sub = cx.observe_window_activation(window, |this: &mut Self, window, cx| {
                if window.is_window_active() {
                    #[cfg(windows)]
                    this.restore_window_size(window);
                } else {
                    this.save(cx);
                }
            });
            self._deactivation = Some(sub);
        }

        if self._appearance.is_none() {
            self._appearance = Some(observe_system_appearance(cx, window, |cx| {
                apply_theme_and_accent(cx);
            }));
        }

        let lang = self.current_lang();

        let dim = cx.theme().foreground.opacity(0.6);
        let faint = cx.theme().foreground.opacity(0.55);
        let link = cx.theme().primary;

        let mut lang_row = h_flex().gap_1().flex_wrap();
        for i in 0..Language::ALL.len() {
            let active = i == self.lang_idx;
            lang_row = lang_row.child(
                choice_button(format!("lang-{i}"), active, cx.theme().primary)
                    .accessibility_label(Language::ALL[i].label())
                    .label(Language::ALL[i].label().to_string())
                    .on_click(cx.listener(
                        move |this: &mut Self,
                              _: &ClickEvent,
                              window: &mut Window,
                              cx: &mut Context<Self>| {
                            this.lang_idx = i;
                            this.refresh_localized_items(window, cx);
                            window.set_window_title(t(Language::ALL[i], "settings.title"));
                            this.save(cx);
                            cx.notify();
                        },
                    )),
            );
        }

        let mut interval_row = h_flex().gap_1().flex_wrap();
        for (i, interval_ms) in ALLOWED_INTERVAL_MS.iter().enumerate() {
            let active = i == self.interval_idx;
            let secs = *interval_ms / 1000;
            let text = format!("{secs} {}", t(lang, "settings.unit_seconds"));
            interval_row = interval_row.child(
                choice_button(format!("interval-{i}"), active, cx.theme().primary)
                    .label(text)
                    .on_click(cx.listener(
                        move |this: &mut Self,
                              _: &ClickEvent,
                              _: &mut Window,
                              cx: &mut Context<Self>| {
                            this.interval_idx = i;
                            this.save(cx);
                            cx.notify();
                        },
                    )),
            );
        }

        const THEME_KEYS: [&str; 3] = ["theme.auto", "theme.light", "theme.dark"];
        const THEME_ICONS: [IconName; 3] = [IconName::Globe, IconName::Sun, IconName::Moon];
        let mut theme_row = h_flex().gap_1();
        for i in 0..THEME_KEYS.len() {
            let active = i == self.theme_idx;
            let tip = t(lang, THEME_KEYS[i]).to_string();
            theme_row = theme_row.child(
                choice_button(format!("theme-{i}"), active, cx.theme().primary)
                    .icon(THEME_ICONS[i].clone())
                    .tooltip(tip.clone())
                    .accessibility_label(tip)
                    .on_click(cx.listener(
                        move |this: &mut Self,
                              _: &ClickEvent,
                              _: &mut Window,
                              cx: &mut Context<Self>| {
                            this.theme_idx = i;
                            this.save(cx);
                            cx.notify();
                        },
                    )),
            );
        }

        let control: Option<(Hsla, Hsla, f32)> = {
            #[cfg(windows)]
            {
                let dark = cx.theme().mode.is_dark();
                let (bg, border) = if dark {
                    (hsla(0., 0., 1., 0.06), hsla(0., 0., 1., 0.30))
                } else {
                    (hsla(0., 0., 1., 0.72), hsla(0., 0., 0., 0.26))
                };
                Some((bg, border, CONTROL_RADIUS))
            }
            #[cfg(not(windows))]
            {
                None
            }
        };
        let show_margin = (self.layout_available && self.anchor_idx == 1 && !self.vertical_taskbar)
            || self.vertical_taskbar;
        let margin_title = if self.vertical_taskbar {
            t(lang, "settings.top_margin").to_string()
        } else {
            t(lang, "settings.left_margin").to_string()
        };
        #[cfg(windows)]
        let mut spacing_select =
            Select::new(&self.spacing_select).accessibility_label(t(lang, "settings.item_spacing"));
        #[cfg(windows)]
        let mut margin_select = Select::new(&self.margin_select)
            .disabled(!show_margin)
            .accessibility_label(margin_title.clone());
        #[cfg(windows)]
        let mut anchor_select =
            Select::new(&self.anchor_select).accessibility_label(t(lang, "settings.anchor"));
        #[cfg(windows)]
        if let Some((bg, border, radius)) = control {
            spacing_select = spacing_select
                .bg(bg)
                .border_color(border)
                .rounded(px(radius));
            margin_select = margin_select
                .bg(bg)
                .border_color(border)
                .rounded(px(radius));
            anchor_select = anchor_select
                .bg(bg)
                .border_color(border)
                .rounded(px(radius));
        }

        #[cfg(target_os = "macos")]
        let mac_style = {
            let theme = cx.theme();
            MacSelectStyle {
                background: theme.input_background(),
                foreground: theme.foreground,
                border: theme.input,
                caret: theme.muted_foreground,
                radius: theme.radius,
            }
        };
        #[cfg(windows)]
        let spacing_control = blur_on_outside(div().w(px(140.))).child(spacing_select);
        #[cfg(windows)]
        let margin_control = blur_on_outside(div().w(px(140.))).child(margin_select);
        #[cfg(windows)]
        let anchor_control = blur_on_outside(div().w(px(140.))).child(anchor_select);
        #[cfg(target_os = "macos")]
        let spacing_control = div().w(px(140.)).child(mac_select_trigger(
            "spacing-trigger",
            spacing_items()[self.spacing_idx].clone(),
            t(lang, "settings.item_spacing").to_string(),
            mac_style,
            cx.listener(|this, _, _, cx| {
                mac_select_popup(cx, spacing_items(), this.spacing_idx, |view, index| {
                    view.spacing_idx = index
                });
            }),
        ));
        #[cfg(target_os = "macos")]
        let margin_control = div().w(px(140.)).child(mac_select_trigger(
            "margin-trigger",
            margin_items()[self.margin_idx].clone(),
            margin_title.clone(),
            mac_style,
            cx.listener(move |this, _, _, cx| {
                if show_margin {
                    mac_select_popup(cx, margin_items(), this.margin_idx, |view, index| {
                        view.margin_idx = index
                    });
                }
            }),
        ));
        #[cfg(target_os = "macos")]
        let anchor_control = div().w(px(140.)).child(mac_select_trigger(
            "anchor-trigger",
            anchor_items(self.current_lang())[self.anchor_idx].clone(),
            t(lang, "settings.anchor").to_string(),
            mac_style,
            cx.listener(|this, _, _, cx| {
                mac_select_popup(
                    cx,
                    anchor_items(this.current_lang()),
                    this.anchor_idx,
                    |view, index| view.anchor_idx = index,
                );
            }),
        ));

        let dark = cx.theme().mode.is_dark();

        let scale_factor = window.scale_factor();
        let card_bg = {
            #[cfg(target_os = "macos")]
            {
                if dark {
                    hsla(0., 0., 1., 0.10)
                } else {
                    hsla(0., 0., 1., 0.82)
                }
            }
            #[cfg(not(target_os = "macos"))]
            {
                // WinUI CardBackgroundFillColorDefault; opaque when no backdrop material exists.
                if matches!(detect_material(), Material::Opaque) {
                    if dark {
                        hsla(0., 0., 0.17, 1.)
                    } else {
                        hsla(0., 0., 0.984, 1.)
                    }
                } else if dark {
                    hsla(0., 0., 1., 0.05)
                } else {
                    hsla(0., 0., 1., 0.70)
                }
            }
        };
        let card_border = {
            #[cfg(target_os = "macos")]
            {
                None
            }
            #[cfg(not(target_os = "macos"))]
            {
                if dark {
                    Some(hsla(0., 0., 0., 0.10))
                } else {
                    Some(hsla(0., 0., 0., 0.06))
                }
            }
        };

        let general_card = settings_card(card_bg, card_border)
            .child(labeled_row(
                "row-language",
                t(lang, "settings.language").to_string(),
                IconName::Globe,
                lang_row,
                dim,
            ))
            .child(card_divider(dark, scale_factor))
            .child(labeled_row(
                "row-interval",
                t(lang, "settings.sample_interval").to_string(),
                IconName::RefreshCw,
                interval_row,
                dim,
            ))
            .child(card_divider(dark, scale_factor))
            .child(switch_row(
                "row-autostart",
                t(lang, "settings.auto_start").to_string(),
                IconName::Play,
                Switch::new("auto-start")
                    .checked(self.cfg.startup.auto_start)
                    .accessibility_label(t(lang, "settings.auto_start").to_string())
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.cfg.startup.auto_start = *checked;
                        this.cfg.startup.auto_start_explicit = true;
                        this.save(cx);
                        cx.notify();
                    })),
                dim,
            ));

        let resources_last = self.cfg.display.show_resources && !self.cfg.display.show_network;
        let network_last = self.cfg.display.show_network && !self.cfg.display.show_resources;
        let keep_one_hint = t(lang, "settings.keep_one_hint");
        let resources_label = if resources_last {
            format!("{} — {keep_one_hint}", t(lang, "settings.show_resources"))
        } else {
            t(lang, "settings.show_resources").to_string()
        };
        let network_label = if network_last {
            format!("{} — {keep_one_hint}", t(lang, "settings.show_network"))
        } else {
            t(lang, "settings.show_network").to_string()
        };
        let mut resources_switch = Switch::new("show-res")
            .checked(self.cfg.display.show_resources)
            .accessibility_label(resources_label);
        if resources_last {
            resources_switch = resources_switch
                .disabled(true)
                .tooltip(t(lang, "settings.keep_one_hint").to_string());
        }
        let resources_switch =
            resources_switch.on_click(cx.listener(|this, checked: &bool, _, cx| {
                this.set_resources(*checked);
                this.save(cx);
                cx.notify();
            }));
        let mut network_switch = Switch::new("show-net")
            .checked(self.cfg.display.show_network)
            .accessibility_label(network_label);
        if network_last {
            network_switch = network_switch
                .disabled(true)
                .tooltip(t(lang, "settings.keep_one_hint").to_string());
        }
        let network_switch = network_switch.on_click(cx.listener(|this, checked: &bool, _, cx| {
            this.set_network(*checked);
            this.save(cx);
            cx.notify();
        }));

        let mut display_card = settings_card(card_bg, card_border);
        if cfg!(not(target_os = "macos")) {
            display_card = display_card
                .child(labeled_row(
                    "row-theme",
                    t(lang, "settings.theme").to_string(),
                    IconName::Palette,
                    theme_row,
                    dim,
                ))
                .child(card_divider(dark, scale_factor));
        }
        display_card = display_card
            .child(switch_row(
                "row-dual-row",
                t(lang, "settings.dual_row").to_string(),
                IconName::GalleryVerticalEnd,
                Switch::new("dual-row")
                    .checked(self.cfg.display.dual_row)
                    .accessibility_label(t(lang, "settings.dual_row").to_string())
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.cfg.display.dual_row = *checked;
                        this.save(cx);
                        cx.notify();
                    })),
                dim,
            ))
            .child(card_divider(dark, scale_factor))
            .child(switch_row(
                "row-resources",
                t(lang, "settings.show_resources").to_string(),
                IconName::Cpu,
                resources_switch,
                dim,
            ))
            .child(card_divider(dark, scale_factor))
            .child(switch_row(
                "row-network",
                t(lang, "settings.show_network").to_string(),
                IconName::Network,
                network_switch,
                dim,
            ));

        let mut layout_card = settings_card(card_bg, card_border).child(labeled_row(
            "row-spacing",
            t(lang, "settings.item_spacing").to_string(),
            IconName::Frame,
            spacing_control,
            dim,
        ));
        if self.layout_available && !self.vertical_taskbar {
            layout_card = layout_card
                .child(card_divider(dark, scale_factor))
                .child(labeled_row(
                    "row-anchor",
                    t(lang, "settings.anchor").to_string(),
                    IconName::PanelLeft,
                    anchor_control,
                    dim,
                ));
        } else if cfg!(windows) && !self.vertical_taskbar {
            layout_card = layout_card
                .child(card_divider(dark, scale_factor))
                .child(disabled_row(
                    "row-anchor",
                    t(lang, "settings.anchor").to_string(),
                    IconName::PanelLeft,
                    t(lang, "settings.anchor_requires_centered").to_string(),
                    dim,
                    faint,
                ));
        }
        let margin_row = labeled_row(
            "row-margin",
            margin_title,
            if self.vertical_taskbar {
                IconName::ArrowUp
            } else {
                IconName::ArrowLeft
            },
            margin_control,
            dim,
        );
        layout_card = layout_card.child(
            Collapsible::new()
                .motion_id("margin-row")
                .open(show_margin)
                .w_full()
                .content(
                    v_flex()
                        .w_full()
                        .child(card_divider(dark, scale_factor))
                        .child(margin_row),
                ),
        );

        let prefix_cpu_count = self.prefix_cpu.read(cx).value().chars().count();
        let prefix_mem_count = self.prefix_mem.read(cx).value().chars().count();
        let prefix_up_count = self.prefix_up.read(cx).value().chars().count();
        let prefix_down_count = self.prefix_down.read(cx).value().chars().count();
        let prefix_card = settings_card(card_bg, card_border).child(
            v_flex()
                .gap_3()
                .child(
                    h_flex()
                        .gap_4()
                        .child(prefix_cell(
                            "prefix-cpu",
                            t(lang, "settings.cpu").to_string(),
                            IconName::Cpu,
                            &self.prefix_cpu,
                            (dim, faint),
                            prefix_cpu_count,
                            control,
                        ))
                        .child(prefix_cell(
                            "prefix-mem",
                            t(lang, "settings.mem").to_string(),
                            IconName::MemoryStick,
                            &self.prefix_mem,
                            (dim, faint),
                            prefix_mem_count,
                            control,
                        )),
                )
                .child(
                    h_flex()
                        .gap_4()
                        .child(prefix_cell(
                            "prefix-upload",
                            t(lang, "settings.upload").to_string(),
                            IconName::ArrowUp,
                            &self.prefix_up,
                            (dim, faint),
                            prefix_up_count,
                            control,
                        ))
                        .child(prefix_cell(
                            "prefix-download",
                            t(lang, "settings.download").to_string(),
                            IconName::ArrowDown,
                            &self.prefix_down,
                            (dim, faint),
                            prefix_down_count,
                            control,
                        )),
                ),
        );

        let body = v_flex()
            .size_full()
            .p_4()
            .gap_3()
            .text_size(px(14.))
            .overflow_y_scrollbar()
            .child(settings_section(
                "section-general",
                t(lang, "settings.general").to_string(),
                IconName::Settings,
                general_card,
                dim,
            ))
            .child(settings_section(
                "section-display",
                t(lang, "settings.display").to_string(),
                IconName::LayoutDashboard,
                display_card,
                dim,
            ))
            .child(settings_section(
                "section-layout",
                t(lang, "settings.layout").to_string(),
                IconName::PanelBottom,
                layout_card,
                dim,
            ))
            .child(settings_section(
                "section-prefixes",
                t(lang, "settings.prefixes").to_string(),
                IconName::CaseSensitive,
                prefix_card,
                dim,
            ))
            .child(
                h_flex().mt_auto().pt_2().justify_center().child(
                    div()
                        .id("version-link")
                        .role(Role::Link)
                        .aria_label(format!("SysVitals {SV_BUILD_ID}"))
                        .aria_description(REPO_URL)
                        .text_xs()
                        .text_color(faint)
                        .cursor_pointer()
                        .hover(move |style| style.text_color(link))
                        .on_click(cx.listener(
                            |_: &mut Self,
                             _: &ClickEvent,
                             _: &mut Window,
                             cx: &mut Context<Self>| {
                                cx.open_url(REPO_URL);
                            },
                        ))
                        .child(format!("SysVitals {SV_BUILD_ID}")),
                ),
            );

        // Both panels use a transparent client area; the OS backdrop material supplies the visible surface.
        #[allow(unused_variables)]
        let title_text = t(lang, "settings.title").to_string();
        #[cfg(any(windows, target_os = "macos"))]
        let heading_color = cx.theme().foreground.opacity(0.9);
        #[cfg(target_os = "macos")]
        let mut root = v_flex().size_full().child(
            h_flex()
                .h(px(34.))
                .w_full()
                .flex_shrink_0()
                .items_center()
                .justify_center()
                .child(window_heading(title_text.clone(), heading_color)),
        );
        #[cfg(windows)]
        let mut root = v_flex().size_full().child(
            TitleBar::new()
                .w_full()
                .pl(px(16.))
                .bg(hsla(0., 0., 0., 0.))
                .border_color(hsla(0., 0., 0., 0.))
                .child(window_heading(title_text.clone(), heading_color)),
        );
        #[cfg(not(any(windows, target_os = "macos")))]
        let mut root = v_flex().size_full();

        let error_banner_content = if self.config_read_failed {
            Some((
                t(lang, "settings.load_failed").to_string(),
                t(lang, "settings.load_failed_hint").to_string(),
            ))
        } else if self.save_failed.get() {
            Some((
                t(lang, "settings.save_failed").to_string(),
                t(lang, "settings.save_failed_hint").to_string(),
            ))
        } else {
            None
        };
        if let Some((title, hint)) = error_banner_content {
            root = root.child(
                h_flex()
                    .w_full()
                    .flex_shrink_0()
                    .px_4()
                    .pt_4()
                    .child(error_banner(title, hint, dark)),
            );
        }

        root.child(div().flex_1().overflow_hidden().child(body))
    }
}
