//! Render pass for the settings window.

use super::*;

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Re-probe the taskbar shape at most once a second: the user can move
        // or auto-hide the bar while this window stays open, but a registry
        // read plus `SHAppBarMessage` on every keystroke-triggered repaint is
        // needless I/O on the UI thread.
        let probe_due = self
            .last_taskbar_probe
            .is_none_or(|last| last.elapsed() >= std::time::Duration::from_secs(1));
        if probe_due {
            self.vertical_taskbar = taskbar_is_vertical();
            self.layout_available = layout_available();
            self.last_taskbar_probe = Some(Instant::now());
        }
        // Deactivation flush (registered once): persist the current state
        // whenever the window loses focus, covering the case where a prefix
        // field still holds the last edit when the window is closed or the
        // user clicks away.
        if self._deactivation.is_none() {
            let sub = cx.observe_window_activation(window, |this: &mut Self, window, cx| {
                if window.is_window_active() {
                    // The first frame may have rendered before activation
                    // settled; retry the one-shot correction here.
                    #[cfg(windows)]
                    this.restore_window_size(window);
                } else {
                    this.save(cx);
                }
            });
            self._deactivation = Some(sub);
        }

        // Live OS appearance flip (registered once): the settings panel
        // always mirrors the system light/dark theme (the `display.theme`
        // setting is indicator-only), so a system flip repaints this window
        // immediately. Same first-render registration pattern as the
        // activation observer (never from a paint-phase API — the root
        // view's empty active-node stack would panic the unwrap inside
        // `window.on_key_event`).
        if self._appearance.is_none() {
            self._appearance = Some(observe_system_appearance(cx, window, |cx| {
                apply_theme_and_accent(cx);
            }));
        }

        // Every label comes from the core i18n table, keyed on the
        // *currently selected* language, so switching language repaints the
        // whole window immediately instead of only after saving.
        let lang = self.current_lang();

        // Theme colors are read once up front so the conditional `.when`
        // closure below never touches `cx` (keeps the borrow checker out of
        // the builder-chain argument positions).
        // Secondary text targets WCAG AA at the 12px sizes used here. The
        // previous 0.5/0.4 pair read too light on the tinted page and cards.
        let dim = cx.theme().foreground.opacity(0.6);
        let faint = cx.theme().foreground.opacity(0.55);
        // Hover color for the footer version link: the system accent is the
        // only affordance this GPUI snapshot offers for link text (there is
        // no underline styling on elements).
        let link = cx.theme().primary;

        // --- Flattened (segmented) option rows for small choice sets: every
        // choice is a visible tile, the active one carries the primary style.
        // Spacing/margin/anchor use dropdowns instead (longer option lists).
        let mut lang_row = h_flex().gap_1().flex_wrap();
        for i in 0..Language::ALL.len() {
            let active = i == self.lang_idx;
            lang_row = lang_row.child(
                Button::new(format!("lang-{i}"))
                    .small()
                    .rounded(px(CONTROL_RADIUS))
                    .when(active, |b| b.primary())
                    .label(Language::ALL[i].label().to_string())
                    .on_click(cx.listener(
                        move |this: &mut Self,
                              _: &ClickEvent,
                              window: &mut Window,
                              cx: &mut Context<Self>| {
                            this.lang_idx = i;
                            // The dropdown items and the anchor choices are
                            // translated text: rebuild them so the page
                            // repaints in the new language, then persist
                            // immediately.
                            this.refresh_localized_items(window, cx);
                            // The custom titlebar repaints from `lang`, but the
                            // platform title (taskbar / Mission Control /
                            // accessibility) needs an explicit update.
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
                Button::new(format!("interval-{i}"))
                    .small()
                    .rounded(px(CONTROL_RADIUS))
                    .when(active, |b| b.primary())
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

        // Theme choices render as icons (globe = follow system, sun = light,
        // moon = dark) with the translated name kept as the tooltip. All
        // three glyphs live in the bundled default asset set, so they paint
        // through the registered `Assets` source — a catalog name outside
        // that set (e.g. Monitor) would silently render blank.
        const THEME_KEYS: [&str; 3] = ["theme.auto", "theme.light", "theme.dark"];
        const THEME_ICONS: [IconName; 3] = [IconName::Globe, IconName::Sun, IconName::Moon];
        let mut theme_row = h_flex().gap_1();
        for i in 0..THEME_KEYS.len() {
            let active = i == self.theme_idx;
            let tip = t(lang, THEME_KEYS[i]).to_string();
            theme_row = theme_row.child(
                Button::new(format!("theme-{i}"))
                    .small()
                    .rounded(px(CONTROL_RADIUS))
                    .when(active, |b| b.primary())
                    .icon(THEME_ICONS[i].clone())
                    .tooltip(tip)
                    .on_click(cx.listener(
                        move |this: &mut Self,
                              _: &ClickEvent,
                              _: &mut Window,
                              cx: &mut Context<Self>| {
                            this.theme_idx = i;
                            // The choice is persisted for the indicator to
                            // hot-reload. It deliberately does NOT re-theme
                            // this window or the history popup: those always
                            // follow the system appearance, so changing it
                            // must not restyle the panels the user is
                            // looking at.
                            this.save(cx);
                            cx.notify();
                        },
                    )),
            );
        }

        // Style the dropdown triggers/inputs like Windows Settings: a filled
        // control brush, a visible 1px stroke and the WinUI 4px corner. The
        // per-element overrides win over the component's internal appearance
        // (its style refinement is applied last). Other hosts keep the
        // library look.
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
        #[cfg(windows)]
        let mut spacing_select = Select::new(&self.spacing_select);
        #[cfg(windows)]
        let mut margin_select = Select::new(&self.margin_select);
        #[cfg(windows)]
        let mut anchor_select = Select::new(&self.anchor_select);
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

        // macOS raises the three dropdowns as native menus. The trigger keeps
        // the same 180px slot and filled-control look, so the layout below is
        // shared with Windows.
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
        let spacing_control = blur_on_outside(div().w(px(180.))).child(spacing_select);
        #[cfg(windows)]
        let margin_control = blur_on_outside(div().w(px(180.))).child(margin_select);
        #[cfg(windows)]
        let anchor_control = blur_on_outside(div().w(px(180.))).child(anchor_select);
        #[cfg(target_os = "macos")]
        let spacing_control = div().w(px(180.)).child(mac_select_trigger(
            "spacing-trigger",
            spacing_items(lang)[self.spacing_idx].clone(),
            mac_style,
            cx.listener(|this, _, _, cx| {
                mac_select_popup(
                    cx,
                    spacing_items(this.current_lang()),
                    this.spacing_idx,
                    |view, index| view.spacing_idx = index,
                );
            }),
        ));
        #[cfg(target_os = "macos")]
        let margin_control = div().w(px(180.)).child(mac_select_trigger(
            "margin-trigger",
            margin_items(lang)[self.margin_idx].clone(),
            mac_style,
            cx.listener(|this, _, _, cx| {
                mac_select_popup(
                    cx,
                    margin_items(this.current_lang()),
                    this.margin_idx,
                    |view, index| view.margin_idx = index,
                );
            }),
        ));
        #[cfg(target_os = "macos")]
        let anchor_control = div().w(px(180.)).child(mac_select_trigger(
            "anchor-trigger",
            anchor_items(self.current_lang())[self.anchor_idx].clone(),
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

        // Native separators are hairlines: one device pixel. GPUI lays out in
        // logical pixels, so divide by the display scale to land on one
        // physical pixel on Retina instead of drawing a two-pixel line.
        let scale_factor = window.scale_factor();
        // Grouped-card fill. Windows paints an opaque WinUI card tone: the
        // platform renderer accumulates layer alpha additively, so a
        // translucent card over the translucent page tint saturates to alpha 1
        // and composites as an opaque gray — the Mica lift never reaches it.
        // #FBFBFB / #2B2B2B are the opaque equivalents of WinUI's 70% / 5%
        // white card over the light / dark page. macOS uses a near-white card
        // in light mode (the
        // old 0.55 white over the gray blur read as muddy gray next to System
        // Settings) and a slightly stronger dark lift; the separators below
        // do the row grouping that the Windows gap used to do.
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
                if dark {
                    // Opaque equivalent of WinUI's #0DFFFFFF (5% white) card
                    // over the dark settings page (~#2B2B2B).
                    hsla(0., 0., 0.17, 1.)
                } else {
                    // Opaque equivalent of WinUI's #B3FFFFFF (70% white) card
                    // over the light settings page (~#FBFBFB).
                    hsla(0., 0., 0.984, 1.)
                }
            }
        };
        // WinUI's CardStrokeColorDefault is 6% black in light and 10% black
        // in dark. The light stroke is what keeps the near-white card readable
        // on a bright Mica page; the dark stroke is the system's own edge.
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
                t(lang, "settings.language").to_string(),
                IconName::Globe,
                lang_row,
                dim,
            ))
            .child(card_divider(dark, scale_factor))
            .child(labeled_row(
                t(lang, "settings.sample_interval").to_string(),
                IconName::RefreshCw,
                interval_row,
                dim,
            ))
            .child(card_divider(dark, scale_factor))
            .child(switch_row(
                t(lang, "settings.auto_start").to_string(),
                IconName::Play,
                Switch::new("auto-start")
                    .checked(self.auto_start)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.auto_start = *checked;
                        this.auto_start_touched = true;
                        this.save(cx);
                        cx.notify();
                    })),
                dim,
            ));

        // The last visible group cannot be switched off. Disable that switch
        // instead of silently re-enabling the other one behind the user's
        // back (the old guard flipped it without telling anyone).
        let resources_last = self.show_resources && !self.show_network;
        let network_last = self.show_network && !self.show_resources;
        let mut resources_switch = Switch::new("show-res").checked(self.show_resources);
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
        let mut network_switch = Switch::new("show-net").checked(self.show_network);
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

        // Theme belongs with the other appearance controls, not General.
        // macOS hides it because AppKit picks the menu-bar title color.
        let mut display_card = settings_card(card_bg, card_border);
        if cfg!(not(target_os = "macos")) {
            display_card = display_card
                .child(labeled_row(
                    t(lang, "settings.theme").to_string(),
                    IconName::Palette,
                    theme_row,
                    dim,
                ))
                .child(card_divider(dark, scale_factor));
        }
        display_card = display_card
            .child(switch_row(
                t(lang, "settings.dual_row").to_string(),
                IconName::GalleryVerticalEnd,
                Switch::new("dual-row")
                    .checked(self.dual_row)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.dual_row = *checked;
                        this.save(cx);
                        cx.notify();
                    })),
                dim,
            ))
            .child(card_divider(dark, scale_factor))
            .child(switch_row(
                t(lang, "settings.show_resources").to_string(),
                IconName::Cpu,
                resources_switch,
                dim,
            ))
            .child(card_divider(dark, scale_factor))
            .child(switch_row(
                t(lang, "settings.show_network").to_string(),
                IconName::Network,
                network_switch,
                dim,
            ));

        // Layout always carries item spacing, so the section never vanishes;
        // the anchor row either renders its control, shows why it is
        // unavailable (Windows with left-aligned taskbar icons), or is
        // omitted where the platform has no anchor choice (macOS).
        let mut layout_card = settings_card(card_bg, card_border).child(labeled_row(
            t(lang, "settings.item_spacing").to_string(),
            IconName::Frame,
            spacing_control,
            dim,
        ));
        if self.layout_available && !self.vertical_taskbar {
            layout_card = layout_card
                .child(card_divider(dark, scale_factor))
                .child(labeled_row(
                    t(lang, "settings.anchor").to_string(),
                    IconName::PanelLeft,
                    anchor_control,
                    dim,
                ));
        } else if cfg!(windows) && !self.vertical_taskbar {
            layout_card = layout_card
                .child(card_divider(dark, scale_factor))
                .child(disabled_row(
                    t(lang, "settings.anchor").to_string(),
                    IconName::PanelLeft,
                    t(lang, "settings.anchor_requires_centered").to_string(),
                    dim,
                    faint,
                ));
        }
        let show_margin = (self.layout_available && self.anchor_idx == 1 && !self.vertical_taskbar)
            || self.vertical_taskbar;
        if show_margin {
            layout_card = layout_card
                .child(card_divider(dark, scale_factor))
                .child(labeled_row(
                    if self.vertical_taskbar {
                        t(lang, "settings.top_margin").to_string()
                    } else {
                        t(lang, "settings.left_margin").to_string()
                    },
                    if self.vertical_taskbar {
                        IconName::ArrowUp
                    } else {
                        IconName::ArrowLeft
                    },
                    margin_control,
                    dim,
                ));
        }

        let prefix_cpu_count = self.prefix_cpu.read(cx).value().chars().count();
        let prefix_mem_count = self.prefix_mem.read(cx).value().chars().count();
        let prefix_up_count = self.prefix_up.read(cx).value().chars().count();
        let prefix_down_count = self.prefix_down.read(cx).value().chars().count();
        let prefix_card = settings_card(card_bg, card_border).child(
            // The 2x2 prefix grid uses spacing rather than a divider
            // between its two rows (2026-10-26, user request); the other
            // cards keep their row separators.
            v_flex()
                .gap_3()
                .child(
                    h_flex()
                        .gap_4()
                        .child(prefix_cell(
                            t(lang, "settings.cpu").to_string(),
                            IconName::Cpu,
                            &self.prefix_cpu,
                            dim,
                            faint,
                            prefix_cpu_count,
                            control,
                        ))
                        .child(prefix_cell(
                            t(lang, "settings.mem").to_string(),
                            IconName::MemoryStick,
                            &self.prefix_mem,
                            dim,
                            faint,
                            prefix_mem_count,
                            control,
                        )),
                )
                .child(
                    h_flex()
                        .gap_4()
                        .child(prefix_cell(
                            t(lang, "settings.upload").to_string(),
                            IconName::ArrowUp,
                            &self.prefix_up,
                            dim,
                            faint,
                            prefix_up_count,
                            control,
                        ))
                        .child(prefix_cell(
                            t(lang, "settings.download").to_string(),
                            IconName::ArrowDown,
                            &self.prefix_down,
                            dim,
                            faint,
                            prefix_down_count,
                            control,
                        )),
                ),
        );

        // A failed-save banner stays outside the scroll area, pinned under the
        // title bar, so it remains visible while the user edits further down.
        let body = v_flex()
            .size_full()
            .p_4()
            .gap_3()
            .text_size(px(14.))
            .overflow_y_scrollbar()
            .child(settings_section(
                t(lang, "settings.general").to_string(),
                IconName::Settings,
                general_card,
                dim,
            ))
            .child(settings_section(
                t(lang, "settings.display").to_string(),
                IconName::LayoutDashboard,
                display_card,
                dim,
            ))
            .child(settings_section(
                t(lang, "settings.layout").to_string(),
                IconName::PanelBottom,
                layout_card,
                dim,
            ))
            .child(settings_section(
                t(lang, "settings.prefixes").to_string(),
                IconName::CaseSensitive,
                prefix_card,
                dim,
            ))
            // Version footer: pinned to the bottom of the scrollable column
            // and built from the product name plus `SV_BUILD_ID` (see the
            // const below). The whole stamp doubles as a single link to the
            // project repository; only the text itself is interactive, so the
            // surrounding footer stays inert.
            .child(
                h_flex().mt_auto().pt_2().justify_center().child(
                    div()
                        .id("version-link")
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

        // Both desktop panels draw into a transparent client area so the
        // tinted material continues behind the caption. macOS keeps its
        // centered 34px header and native traffic lights; Windows renders the
        // component title bar (caption drag + window controls) with a
        // transparent background, making the caption part of the page.
        #[cfg(target_os = "macos")]
        let mut root = v_flex().size_full().child(
            h_flex()
                .h(px(34.))
                .w_full()
                .flex_shrink_0()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .text_size(px(13.))
                        .font_semibold()
                        .text_color(cx.theme().foreground.opacity(0.9))
                        .child(t(lang, "settings.title").to_string()),
                ),
        );
        #[cfg(windows)]
        let mut root = v_flex().size_full().child(
            TitleBar::new()
                .w_full()
                .pl(px(16.))
                .bg(hsla(0., 0., 0., 0.))
                .border_color(hsla(0., 0., 0., 0.))
                .child(
                    div()
                        .text_size(px(13.))
                        .font_semibold()
                        .text_color(cx.theme().foreground.opacity(0.9))
                        .child(t(lang, "settings.title").to_string()),
                ),
        );
        #[cfg(not(any(windows, target_os = "macos")))]
        let mut root = v_flex().size_full();

        if self.save_failed.get() {
            root = root.child(
                h_flex()
                    .w_full()
                    .flex_shrink_0()
                    .px_4()
                    .pt_4()
                    .child(error_banner(
                        t(lang, "settings.save_failed").to_string(),
                        t(lang, "settings.save_failed_hint").to_string(),
                        dark,
                    )),
            );
        }

        root.child(div().flex_1().overflow_hidden().child(body))
    }
}
