//! Render pass for the history popup.

use super::*;

use gpui_kit::base::{Sequence, Transition};
use sysvitals_core::format::format_percent;

/// The curve both series motions use. A cubic ease-out moves most of the way
/// early and still visibly settles, which keeps the 360 ms draw from reading
/// as a flash and the 280 ms refresh from reading as a step.
fn ease_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

fn legend_entry(color: Hsla, name: String, value: String, fg: Hsla) -> impl IntoElement {
    h_flex()
        .gap_1()
        .items_center()
        .child(div().w(px(8.)).h(px(8.)).rounded(px(4.)).bg(color))
        .child(div().text_xs().text_color(fg.opacity(0.7)).child(name))
        .child(div().text_xs().text_color(fg.opacity(0.9)).child(value))
}

/// A cubic path following the chart library's natural (Catmull-Rom to
/// Bezier) interpolation for one contiguous series run. The points are
/// already in window pixels.
fn natural_path(points: &[(f32, f32)]) -> Option<Path<Pixels>> {
    let (first, _) = points.split_first()?;
    let mut path = PathBuilder::stroke(px(1.));
    path.move_to(point(px(first.0), px(first.1)));
    let last = points.len() - 1;
    for i in 0..last {
        let p0 = if i == 0 { points[0] } else { points[i - 1] };
        let p1 = points[i];
        let p2 = points[i + 1];
        let p3 = points[(i + 2).min(last)];
        let c1 = point(
            px(p1.0 + (p2.0 - p0.0) / 6.0),
            px(p1.1 + (p2.1 - p0.1) / 6.0),
        );
        let c2 = point(
            px(p2.0 - (p3.0 - p1.0) / 6.0),
            px(p2.1 - (p3.1 - p1.1) / 6.0),
        );
        path.cubic_bezier_to(point(px(p2.0), px(p2.1)), c1, c2);
    }
    path.build().ok()
}

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
        let data_error = self.data_error_logged;
        let live = self.range.auto_refresh();
        let latest = self.buckets.last().copied();
        let cpu_value = latest
            .map(|bucket| format_percent(bucket.cpu))
            .unwrap_or_else(|| "--%".to_string());
        let mem_value = latest
            .map(|bucket| format_percent(bucket.mem))
            .unwrap_or_else(|| "--%".to_string());
        // Static 1 d / 1 mo views do not follow the clock after opening; label
        // the last stored point instead of pretending it is "now".
        let latest_note = if live {
            None
        } else {
            latest.map(|bucket| format!("{} {}", t(lang, "chart.latest"), fmt_date_time(bucket.ts)))
        };
        let status = if loading {
            Some((t(lang, "chart.loading").to_string(), None))
        } else if data_error && empty {
            Some((
                t(lang, "chart.error").to_string(),
                Some(t(lang, "chart.error_hint").to_string()),
            ))
        } else if empty {
            Some((
                t(lang, "chart.no_data").to_string(),
                Some(t(lang, "chart.no_data_hint").to_string()),
            ))
        } else {
            None
        };
        // Register the blur-close and appearance observers before the draw
        // gate below: the activation pass seeds `ever_active`, which tells a
        // pre-warmed window whether it is shown already. Both calls are
        // idempotent and must run from render (not `new`) because there is no
        // window before the first one; neither uses a paint-phase API, so the
        // empty-dispatch-tree root-view trap that killed the original Esc
        // handler does not apply.
        self.observe_activation(window, cx);
        self.observe_appearance(window, cx);

        // One series motion at a time. The sequence owns the wall clock and
        // reduced-motion handling; this pass only reads its eased progress.
        // `fx_now` keeps the snapshot alive for this frame after the state is
        // cleared on the last frame. Starting a draw here (rather than when
        // the data loads) is what keeps a pre-warmed hidden window from
        // playing the motion before it is actually shown.
        let reduce_motion = cx.reduce_motion();
        self.start_pending_draw(reduce_motion);
        let fx_now = self.series_fx.clone();
        let mut series_progress = 1.0_f32;
        if let Some(fx) = &fx_now {
            let sequence = Sequence::new(format!("series-fx-{}", self.series_gen), 0.0_f32)
                .with_step(1.0, Transition::new(fx.duration()).ease(ease_out_cubic));
            let sample = sequence.sample(window, cx);
            let finished = sample.is_finished();
            series_progress = sample.into_value();
            if finished {
                self.series_fx = None;
            }
        }

        // A live refresh expands the index axis from the previous slot count
        // to the current one while easing changed values from the previous
        // snapshot. New slots start outside the right edge and slide in as
        // the denominator grows, which is what makes the update read as a
        // scroll instead of a step. A draw only constrains the paint mask;
        // the geometry is already final. Both series share one progress.
        let refresh = match &fx_now {
            Some(SeriesFx::Refresh { prev_len, prev_y }) => {
                Some((*prev_len, prev_y, series_progress))
            }
            _ => None,
        };
        let draw_progress =
            matches!(fx_now.as_ref(), Some(SeriesFx::Draw)).then_some(series_progress);
        // Contiguous runs of real samples. The chart library draws one path
        // across every datum and has no per-point break, so the series are
        // painted per run by the overlay below: a padded (no-data) slot ends
        // the previous run and starts a new one, leaving the gap blank
        // instead of drawing a flat carried-forward line or a background
        // band.
        let series_segments: Vec<Vec<(f32, f32, f32)>> = if points.len() < 2 {
            Vec::new()
        } else {
            let virtual_len = match refresh {
                Some((prev_len, _, progress)) => {
                    let prev = prev_len as f32;
                    let next = points.len() as f32;
                    prev + (next - prev) * progress
                }
                None => points.len() as f32,
            };
            let denom = (virtual_len - 1.0).max(1.0);
            data_runs(&points)
                .into_iter()
                .map(|run| {
                    run.map(|index| {
                        let point = &points[index];
                        let (mut cpu, mut mem) = (point.cpu, point.mem);
                        if let Some((prev_len, prev_y, progress)) = refresh {
                            if index < prev_len {
                                if let Some((prev_cpu, prev_mem)) = prev_y.get(index).copied() {
                                    cpu = prev_cpu + (cpu - prev_cpu) * progress as f64;
                                    mem = prev_mem + (mem - prev_mem) * progress as f64;
                                }
                            }
                        }
                        let x = index as f32 / denom;
                        let cpu =
                            ((cpu - Y_AXIS_MIN) / (Y_AXIS_MAX - Y_AXIS_MIN)).clamp(0.0, 1.0) as f32;
                        let mem =
                            ((mem - Y_AXIS_MIN) / (Y_AXIS_MAX - Y_AXIS_MIN)).clamp(0.0, 1.0) as f32;
                        (x, 1.0 - cpu, 1.0 - mem)
                    })
                    .collect()
                })
                .collect()
        };
        // The chart itself paints no x labels (`x_tick_count(0)` below); this
        // panel draws them at a readable size instead. Labels are generated
        // from the time window (six evenly spaced positions), not from data
        // indices, so a data gap does not move the axis text.
        let axis_labels = self.axis_labels();
        let copied = self.copied;
        let copy_failed = self.copy_failed;
        let current_range = self.range;

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
                    this.begin_series_draw();
                    this.load_data(cx);
                    cx.notify();
                }));
            range_buttons = range_buttons.child(btn);
        }

        // Icon-only copy button: a green check flashes for ~1.2s after a
        // successful copy, then the button returns to the copy icon. The
        // tooltip keeps the textual hint in the current language. A loading
        // or empty range has nothing to export, so the command is disabled
        // rather than failing after the click.
        // An empty/loading *visible* range has nothing to export. Longer
        // hand-edited CSV scopes still have a full retention window to copy,
        // so they are not gated on the on-screen buckets.
        let copy_disabled = !self.copy_available();
        let copy_tip = if copied {
            t(lang, "chart.copied").to_string()
        } else if copy_failed {
            t(lang, "chart.copy_failed").to_string()
        } else if copy_disabled && loading {
            t(lang, "chart.loading").to_string()
        } else if copy_disabled && empty {
            t(lang, "chart.no_data").to_string()
        } else {
            t(lang, "chart.copy_csv").to_string()
        };

        let copy_btn = Button::new("copy-csv")
            .small()
            .rounded(px(CONTROL_RADIUS))
            .disabled(copy_disabled)
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
        // tooltip above the labels. Series are painted by `series_overlay`
        // after the chart, so they sit above its grid; gaps stay blank.
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
        .top_0()
        .left_0()
        .size_full();
        // Segmented series overlay: one cubic path per contiguous run. It is
        // painted after the chart so the lines sit above the grid; the
        // deferred tooltip box still paints above this overlay. The chart's
        // own series are disabled by the degenerate `y_domain` below (the
        // library cannot break a path at padded slots), while its axes,
        // hover crosshair and tooltip text keep working.
        //
        // The paint is masked to the plot. The mask is what draws the lines
        // in from the left, and it also clips the new slots a live refresh
        // holds outside the right edge until the axis finish expanding.
        let series_overlay = canvas(
            move |bounds, _window, _cx| {
                let plot_left = X_AXIS_GUTTER;
                let plot_h = (f32::from(bounds.size.height) - X_AXIS_STRIP_H).max(0.0);
                let plot_w = (f32::from(bounds.size.width) - plot_left).max(0.0);
                // `Window::paint_path` paints in window space, so the local
                // plot coordinates must be offset by the canvas origin.
                let origin_x = f32::from(bounds.origin.x);
                let origin_y = f32::from(bounds.origin.y);
                let build = |series: usize| {
                    let mut paths = Vec::new();
                    for segment in &series_segments {
                        if segment.len() < 2 {
                            continue;
                        }
                        let points: Vec<(f32, f32)> = segment
                            .iter()
                            .map(|(x, cpu, mem)| {
                                let y = if series == 0 { *cpu } else { *mem };
                                (origin_x + plot_left + plot_w * x, origin_y + plot_h * y)
                            })
                            .collect();
                        if let Some(path) = natural_path(&points) {
                            paths.push(path);
                        }
                    }
                    paths
                };
                (build(0), build(1))
            },
            move |bounds, (cpu_paths, mem_paths), window, _cx| {
                let pad = 1.0;
                let plot_left = X_AXIS_GUTTER;
                let plot_w = (f32::from(bounds.size.width) - plot_left).max(0.0);
                let plot_h = (f32::from(bounds.size.height) - X_AXIS_STRIP_H).max(0.0);
                let origin_x = f32::from(bounds.origin.x);
                let origin_y = f32::from(bounds.origin.y);
                let reveal = draw_progress.unwrap_or(1.0).clamp(0.0, 1.0);
                let bleed = if reveal >= 1.0 { pad } else { 0.0 };
                let mask = ContentMask {
                    bounds: Bounds::from_corners(
                        point(px(origin_x + plot_left - bleed), px(origin_y - pad)),
                        point(
                            px(origin_x + plot_left + plot_w * reveal + bleed),
                            px(origin_y + plot_h + pad),
                        ),
                    ),
                };
                window.with_content_mask(Some(mask), |window| {
                    for path in cpu_paths {
                        window.paint_path(path, cpu_color());
                    }
                    for path in mem_paths {
                        window.paint_path(path, mem_color());
                    }
                });
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();
        let mut merged_chart = div()
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
                    // A degenerate domain disables the library's own series
                    // (the library draws one unbroken path and cannot skip a
                    // padded slot). The chart is kept for its axes, hover
                    // crosshair and tooltip; the visible lines are painted per
                    // contiguous run by `series_overlay` below.
                    .y_domain(0.0, 0.0)
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
                            format!("{v:.1}%").into()
                        } else {
                            "--".into()
                        }
                    }),
            )
            .child(series_overlay);
        let status_color = cx.theme().foreground.opacity(0.6);
        if let Some((title, hint)) = status {
            let mut message = v_flex()
                .gap_1()
                .items_center()
                .w_full()
                .max_w(px(430.))
                .child(
                    div()
                        .text_sm()
                        .text_color(status_color)
                        .text_center()
                        .child(title),
                );
            if let Some(hint) = hint {
                message = message.child(
                    div()
                        .text_xs()
                        .text_color(status_color.opacity(0.8))
                        .text_center()
                        .child(hint),
                );
            }
            merged_chart = merged_chart.child(
                h_flex()
                    .absolute()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .child(message),
            );
        }

        // Shared legend: dot + label + latest value per series, aligned with
        // the chart's own colors. Static ranges add the timestamp of the last
        // stored point so the number is not mistaken for a live reading.
        let fg = cx.theme().foreground;
        let mut legend = h_flex()
            .w_full()
            .justify_between()
            .items_center()
            .flex_wrap()
            .child(
                h_flex()
                    .gap_4()
                    .items_center()
                    .child(legend_entry(
                        cpu_color(),
                        t(lang, "chart.cpu").to_string(),
                        cpu_value,
                        fg,
                    ))
                    .child(legend_entry(
                        mem_color(),
                        t(lang, "chart.mem").to_string(),
                        mem_value,
                        fg,
                    )),
            );
        if let Some(note) = latest_note {
            legend = legend.child(div().text_xs().text_color(fg.opacity(0.55)).child(note));
        }

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
