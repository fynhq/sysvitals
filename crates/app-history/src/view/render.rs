//! Render pass for the history popup.

use super::*;

impl Render for HistoryView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lang = self.lang;
        let title = format!("SysVitals {}", t(lang, "chart.title"));
        if self.last_title.as_deref() != Some(title.as_str()) {
            window.set_window_title(&title);
            self.last_title = Some(title);
        }
        let points = self.points();
        let loading = self.data_loading;
        let empty = self.buckets.is_empty();
        let placeholder = if loading {
            t(lang, "chart.loading").to_string()
        } else {
            t(lang, "chart.no_data").to_string()
        };
        // The chart itself paints no x labels (`x_tick_count(0)` below); this
        // panel draws them at a readable size instead. Labels are generated
        // from the time window (six evenly spaced positions), not from data
        // indices, so a data gap does not move the axis text.
        let axis_labels = self.axis_labels();
        let copied = self.copied;
        let copy_failed = self.copy_failed;
        let current_range = self.range;

        // Register the blur-close observer and the live appearance observer
        // exactly once. Both are registered here (not from `new`) because
        // neither is available before the window exists; and neither uses a
        // paint-phase API, so the empty-dispatch-tree root-view trap that
        // killed the original Esc handler does not apply.
        self.observe_activation(window, cx);
        self.observe_appearance(window, cx);

        // Build range buttons individually to avoid borrow issues with cx.listener.
        let mut range_buttons = h_flex().gap_1();
        for r in TimeRange::ALL {
            let is_active = r == current_range;
            let label = r.label(lang).to_string();
            let btn = Button::new(format!("range-{:?}", r))
                .small()
                .rounded(px(CONTROL_RADIUS))
                .when(is_active, |b| b.primary())
                .label(label)
                .on_click(cx.listener(move |this: &mut Self, _: &ClickEvent, _, cx| {
                    this.range = r;
                    this.load_data(cx);
                    cx.notify();
                }));
            range_buttons = range_buttons.child(btn);
        }

        // Icon-only copy button: a green check flashes for ~1.2s after a
        // successful copy, then the button returns to the copy icon. The
        // tooltip keeps the textual hint in the current language.
        let copy_tip = if copied {
            t(lang, "chart.copied").to_string()
        } else if copy_failed {
            t(lang, "chart.copy_failed").to_string()
        } else {
            t(lang, "chart.copy_csv").to_string()
        };

        let copy_btn = Button::new("copy-csv")
            .small()
            .rounded(px(CONTROL_RADIUS))
            .tooltip(copy_tip)
            .when(copied, |b| {
                b.icon(IconName::CircleCheck)
                    .text_color(hsla(150.0 / 360.0, 0.7, 0.45, 1.0))
            })
            .when(!copied, |b| b.icon(IconName::Copy))
            .when(copy_failed, |b| b.text_color(hsla(0.0, 0.7, 0.5, 1.0)))
            .on_click(cx.listener(|this: &mut Self, _: &ClickEvent, _, cx| {
                this.copy_csv(cx);
            }));

        let header = h_flex()
            .justify_between()
            .items_center()
            .child(
                div()
                    .text_sm()
                    .font_semibold()
                    .child(t(lang, "chart.title").to_string()),
            )
            .child(h_flex().gap_2().child(range_buttons).child(copy_btn));
        // `theme.border` resolves to a near-black neutral in dark mode, so
        // on the dark tinted popup the hairline read as a black line. Derive
        // it from the foreground instead: light on dark, dark on light, like
        // the system separators.
        let header_divider = div()
            .h(px(1.))
            .w_full()
            .bg(cx.theme().foreground.opacity(0.12));

        // One merged chart for both series. AreaChart accumulates one entry
        // per `y`/`stroke`/`fill`/`name` call in order, so two of each give
        // two lines; the zero-alpha fills keep the look of plain lines.
        // Percentages share one 0..=100 axis, which is what makes merging
        // sane.
        //
        // `appear(false)`: the library's default 1s left-to-right draw-in
        // is what the user experiences as "slow to show" — the popup's
        // job is to be readable the instant it opens, so the sweep is off.
        //
        // Y axis: `y_domain(0, 100)` + `y_tick_count(6)` + `y_padding(0, 0)`.
        // The library spaces the ticks evenly over the *full* plot height but
        // maps the pinned domain only into the height shrunk by the top
        // padding (10 px default), so the labels would read the padded
        // positions (e.g. 20.8 / 41.6 / ... instead of 20 / 40 / ...). With
        // zero padding the tick grid and the domain grid coincide and the
        // six labels land exactly on 0 / 20 / 40 / 60 / 80 / 100; the series
        // clips at the domain edges as before (pinned-plot mask). Its tick
        // labels are also painted by the overlay: `y_tick_format` returns
        // empty text, which keeps the axis line and the 32 px value-axis
        // gutter (the minimum gutter is fixed) while hiding the library's
        // 10 px labels.
        //
        // X axis: `x_tick_count(0)` keeps the axis line and the space the
        // library reserves for tick labels, but suppresses the labels: it
        // paints them at a fixed 10 px with no public size knob, so the
        // labels are drawn at `AXIS_LABEL_SIZE` by the overlay added below.
        // `points()` still labels the same six evenly spread indices
        // (mirroring the library's selection rule) with clean-time text, so
        // the overlay reads exactly like the axis the chart would have drawn,
        // only larger.
        //
        // `tooltip_title` overrides the axis label with the full
        // `YYYY-MM-DD HH:MM:SS` (with seconds), because the axis labels are
        // deliberately terse and a hover should give exact timing.
        //
        // Both label sets are painted by one canvas over the chart. The
        // canvas receives the chart's final bounds at paint time, so the
        // labels reproduce the library's plot geometry (the 32 px value-axis
        // gutter and the 18 px strip under the x axis) without re-deriving
        // the layout. The canvas paints before the chart, keeping a hover
        // tooltip above the labels.
        let y_labels: Vec<String> = (0..Y_TICK_COUNT)
            .map(|k| {
                let value =
                    Y_AXIS_MAX - (Y_AXIS_MAX - Y_AXIS_MIN) * k as f64 / (Y_TICK_COUNT - 1) as f64;
                format!("{value:.0}")
            })
            .collect();
        let label_color = cx.theme().muted_foreground;
        let axis_overlay = canvas(
            |_bounds, _window, _cx| {},
            move |bounds, _, window, cx| {
                let plot_left = X_AXIS_GUTTER;
                let plot_h = (f32::from(bounds.size.height) - X_AXIS_STRIP_H).max(0.0);
                let plot_w = (f32::from(bounds.size.width) - plot_left).max(0.0);
                let mut texts = Vec::with_capacity(axis_labels.len() + y_labels.len());
                for (i, (frac, text)) in axis_labels.iter().enumerate() {
                    let first = i == 0;
                    let last = i + 1 == axis_labels.len();
                    let align = if first {
                        TextAlign::Left
                    } else if last {
                        TextAlign::Right
                    } else {
                        TextAlign::Center
                    };
                    texts.push(
                        PlotText::new(
                            text.clone(),
                            point(px(plot_left + plot_w * *frac), px(plot_h + 3.0)),
                            label_color,
                        )
                        .font_size(px(AXIS_LABEL_SIZE))
                        .align(align),
                    );
                }
                let last_tick = (y_labels.len() - 1).max(1) as f32;
                for (k, text) in y_labels.iter().enumerate() {
                    let tick_y = plot_h * k as f32 / last_tick;
                    let top = (tick_y - AXIS_LABEL_SIZE * 0.5)
                        .clamp(0.0, (plot_h - AXIS_LABEL_SIZE).max(0.0));
                    texts.push(
                        PlotText::new(
                            text.clone(),
                            point(px(plot_left - 4.0), px(top)),
                            label_color,
                        )
                        .font_size(px(AXIS_LABEL_SIZE))
                        .align(TextAlign::Right),
                    );
                }
                PlotLabel::new(texts).paint(&bounds, window, cx);
            },
        )
        .absolute()
        .size_full();
        let empty_color = cx.theme().foreground.opacity(0.6);
        let merged_chart = div()
            .flex_1()
            .relative()
            .child(axis_overlay)
            .child(
                AreaChart::new(points)
                    // The x value is only used for axis/tooltip text; the
                    // library's ScalePoint projects by index. `points()`
                    // makes that index correspond to a bucket slot.
                    .x(|p: &ChartPoint| p.ts.to_string())
                    .y(|p: &ChartPoint| p.cpu)
                    .stroke(cpu_color())
                    .fill(transparent_color())
                    .name(t(lang, "chart.cpu").to_string())
                    .y(|p: &ChartPoint| p.mem)
                    .stroke(mem_color())
                    .fill(transparent_color())
                    .name(t(lang, "chart.mem").to_string())
                    .y_domain(Y_AXIS_MIN, Y_AXIS_MAX)
                    .y_padding(0.0, 0.0)
                    .x_axis(true)
                    .x_tick_count(0)
                    .y_axis(true)
                    .y_tick_format(|_| "")
                    .y_tick_count(Y_TICK_COUNT)
                    .grid(true)
                    .appear(false)
                    .tooltip_title(|p: &ChartPoint| format_ts_full(p.ts).into())
                    .tooltip_value(|p: &ChartPoint, _series: usize, v: f64| {
                        if p.has_data {
                            format!("{v:.2}%").into()
                        } else {
                            "--".into()
                        }
                    }),
            )
            .when(loading || empty, move |this| {
                this.child(
                    h_flex()
                        .absolute()
                        .size_full()
                        .items_center()
                        .justify_center()
                        .child(div().text_sm().text_color(empty_color).child(placeholder)),
                )
            });

        // Shared legend: dot + label per series, aligned with the chart's
        // own colors.
        let legend = h_flex()
            .gap_4()
            .items_center()
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(div().w(px(8.)).h(px(8.)).rounded(px(4.)).bg(cpu_color()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().foreground.opacity(0.7))
                            .child(t(lang, "chart.cpu").to_string()),
                    ),
            )
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(div().w(px(8.)).h(px(8.)).rounded(px(4.)).bg(mem_color()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().foreground.opacity(0.7))
                            .child(t(lang, "chart.mem").to_string()),
                    ),
            );

        v_flex()
            .size_full()
            .p_4()
            .gap_2()
            .child(header)
            .child(header_divider)
            .child(legend)
            .child(merged_chart)
    }
}
