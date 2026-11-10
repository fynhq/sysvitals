//! render: split out of `macos` (see `macos/mod.rs` for the module map).

use super::*;

/// One text run baked into the current indicator image: an attributed
/// string (carrying the font) plus the point to draw it at, in the image's
/// flipped (top-left origin, y downward) space. The list is captured by the
/// image's drawing handler, so AppKit can rasterize it without touching
/// thread-local state from a possibly non-main thread.
#[derive(Clone)]
pub(crate) struct DrawItem {
    pub(crate) attr: Retained<NSAttributedString>,
    pub(crate) x: f64,
    pub(crate) y: f64,
}

/// The rendered template image for the current layout plus the width the
/// status item must reserve. A template keeps AppKit's automatic menu-bar
/// light/dark inversion; there is deliberately no hover/pressed/open
/// backdrop variant.
pub(crate) struct StatusImage {
    pub(crate) image: Retained<NSImage>,
    pub(crate) width: f64,
}

/// One laid-out dual-row line: left run, optional right run, with measured
/// widths/heights kept for the column math.
pub(crate) struct RowRun {
    pub(crate) la: Retained<NSAttributedString>,
    pub(crate) lw: f64,
    pub(crate) lh: f64,
    pub(crate) ra: Option<Retained<NSAttributedString>>,
    pub(crate) rw: f64,
    pub(crate) rh: f64,
}

/// Build one attributed run (font baked in) and measure it. `size()` reads
/// the attributed string's own attributes, so measurement and drawing can
/// never disagree on the font.
pub(crate) fn make_run(
    text: &str,
    font: &Retained<NSFont>,
) -> (Retained<NSAttributedString>, f64, f64) {
    let s = NSString::from_str(text);
    // NSFont's super chain is NSFont -> NSObject -> AnyObject; the
    // attribute-dict value type is AnyObject, so two `as_super` steps land
    // on the right reference (`as_super` comes from `objc2::ClassType`).
    let obj: &AnyObject = font.as_super().as_super();
    // `NSFontAttributeName` is an extern static (the pointer itself lives
    // in AppKit), so reading it needs unsafe; dereferencing it to the
    // `&NSString` it always holds is the only operation, which the
    // framework guarantees to be valid.
    let key: &NSString = unsafe { NSFontAttributeName };
    let dict = NSDictionary::from_slices(&[key], &[obj]);
    // SAFETY: the attribute dictionary holds a valid `NSFont` under the
    // documented `NSFontAttributeName` key.
    let attr = unsafe { NSAttributedString::new_with_attributes(&s, &dict) };
    let sz = attr.size();
    (attr, sz.width, sz.height)
}

/// Build the measured runs for the 2x2 dual-row layout. Kept separate so
/// the caller can shrink the font and retry if the block exceeds the menu
/// bar height.
pub(crate) fn layout_dual(
    row: &FormattedRow,
    font: &Retained<NSFont>,
) -> (Vec<RowRun>, f64, f64, f64) {
    let mut runs: Vec<RowRun> = Vec::with_capacity(row.lines.len());
    for line in &row.lines {
        let (la, lw, lh) = make_run(line.left.trim_end(), font);
        let (ra, rw, rh) = if line.right.is_empty() {
            (None, 0.0, 0.0)
        } else {
            let (a, w, h) = make_run(line.right.trim_end(), font);
            (Some(a), w, h)
        };
        runs.push(RowRun {
            la,
            lw,
            lh,
            ra,
            rw,
            rh,
        });
    }
    let left_w = runs.iter().map(|r| r.lw).fold(0f64, f64::max);
    let right_w = runs.iter().map(|r| r.rw).fold(0f64, f64::max);
    let line_h = runs.iter().map(|r| r.lh.max(r.rh)).fold(0f64, f64::max);
    (runs, left_w, right_w, line_h)
}

/// The metrics the current config shows, in the same order
/// `format_frame` uses.
pub(crate) fn visible_metrics(cfg: &Config) -> Vec<Metric> {
    Metric::ALL
        .into_iter()
        .filter(|m| cfg.display.metric_visible(*m))
        .collect()
}

/// Worst-case width of one metric cell for the font currently in use.
/// This is the fixed slot width: the value may gain or lose a digit
/// inside the slot, but the slot itself never moves. Percentages can only
/// reach "100%"; network rates reserve the widest four-character numeric
/// field across all unit tiers (see `RATE_RESERVE`).
pub(crate) fn metric_reserve(metric: Metric, cfg: &Config, font: &Retained<NSFont>) -> f64 {
    let value = match metric {
        Metric::Cpu | Metric::Mem => "100%",
        Metric::Up | Metric::Down => RATE_RESERVE,
    };
    make_run(&join_prefix(metric_prefix(metric, cfg), value), font).1
}

/// Configured label prefix for one metric.
pub(crate) fn metric_prefix(metric: Metric, cfg: &Config) -> &str {
    match metric {
        Metric::Cpu => &cfg.display.prefix.cpu,
        Metric::Mem => &cfg.display.prefix.mem,
        Metric::Up => &cfg.display.prefix.up,
        Metric::Down => &cfg.display.prefix.down,
    }
}

/// Split a formatted cell into its configured label and value. The
/// formatter joins them with one space; drawing the two runs separately
/// lets the label stay at the slot start while the value is pinned to the
/// slot end, so the indicator's outer edges hug its content. A cell that
/// does not start with the configured prefix (unexpected formatting) is
/// returned whole as the value.
pub(crate) fn split_cell(text: &str, prefix: &str) -> (String, String) {
    let text = text.trim_end();
    if !prefix.is_empty() {
        if let Some(rest) = text.strip_prefix(prefix) {
            return (prefix.to_string(), rest.trim_start().to_string());
        }
    }
    (String::new(), text.to_string())
}

/// Widest reserved cell in a dual-row column, measured with the final
/// (possibly height-fitted) font.
pub(crate) fn column_reserve(metrics: &[Metric], cfg: &Config, font: &Retained<NSFont>) -> f64 {
    metrics
        .iter()
        .map(|m| metric_reserve(*m, cfg, font))
        .fold(0f64, f64::max)
}

/// Font used for the two-line layout: the regular menu-bar face scaled
/// down to fit the fixed bar height would lose too much stroke weight, so
/// dual-row uses a semibold system face at the same reduced size. The
/// single-row layout keeps `menuBarFontOfSize(0.0)` untouched.
pub(crate) fn dual_row_font(size: f64) -> Retained<NSFont> {
    // SAFETY: `NSFontWeightSemibold` is an AppKit extern static; reading
    // any extern static is unsafe, matching the `NSFontAttributeName` use
    // below. The value itself is a plain `CGFloat` constant.
    let weight = unsafe { NSFontWeightSemibold };
    NSFont::systemFontOfSize_weight(size, weight)
}

/// Render the formatted frame into the status item as a self-drawn
/// template image — the macOS counterpart of the Windows overlay's GDI
/// painting, giving pixel control over dual-row alignment, item spacing
/// and vertical centering that AppKit's string title does not offer.
///
/// Slot widths come from the format's upper bounds rather than the
/// current sample: a value gaining or losing a digit only moves inside
/// its own slot, so a dual-row column origin and the status item's length
/// stay fixed between samples. There is no baked outer inset and no
/// custom hover/press background: content meets the item's outer edges so
/// NSStatusBar can apply its normal inter-item spacing, as native extras
/// do.
///
/// Skips every action when the render signature (layout parameters plus
/// all texts) is unchanged: the mounted image simply stays, so an
/// idle-second tick costs nothing (no rebuild, no bitmap churn). On change
/// a fresh block-image is built and mounted; the previous image is
/// reference-counted away.
pub(crate) fn render_indicator(row: &FormattedRow, cfg: &Config) {
    let dual = cfg.display.dual_row && row.lines.len() > 1;
    // `sanitize` constrains `item_spacing` to {4, 8, 16, 32}, so no
    // extra lower bound is needed here.
    let spacing = f64::from(cfg.display.item_spacing);
    if !cfg.display.show_resources && !cfg.display.show_network {
        return;
    }
    // The menu-bar height (logical points) is the canvas height; drawing
    // inside it keeps the glyphs at the system's own vertical band.
    let img_h = NSStatusBar::systemStatusBar().thickness();

    // Single-row strictly follows the system menu-bar font (size 0 =
    // standard); dual-row starts from the 0.8x variant and the fit loop
    // below shrinks it further only when the two line boxes exceed the
    // fixed bar height.
    let base = NSFont::menuBarFontOfSize(0.0);
    // Never shrink below the previous 0.75x baseline: if 0.8x cannot be
    // made to fit by tightening the line step, fall back to the known-good
    // size instead of ending up smaller than before.
    let font_floor = (base.pointSize() * 0.75).max(1.0);
    let mut font = if dual {
        let scaled = (base.pointSize() * DUAL_ROW_FONT_FACTOR).max(1.0);
        dual_row_font(scaled)
    } else {
        base
    };
    let requested_size = font.pointSize();

    // --- lay out the runs (flipped space: y grows downward) -----------
    let mut items: Vec<DrawItem> = Vec::new();
    let mut texts: Vec<String> = Vec::new();
    let width;
    let mut line_step = 0.0_f64;
    // `display.theme` is deliberately not consulted: the indicator is a
    // template image, which AppKit tints from the menu-bar appearance —
    // light/dark inversion is automatic and per-item color is not a thing
    // on the menu bar (see the theme-picker removal in app-settings).
    if dual {
        // 2x2 quadrant mirroring the Windows overlay's `measure_frame`:
        // left column resources, right column network. When only one
        // group is enabled, `format_frame` puts those metrics in the left
        // column; mirror that split here so the reserved slots match.
        let (left_metrics, right_metrics) =
            if cfg.display.show_resources && cfg.display.show_network {
                (
                    vec![Metric::Cpu, Metric::Mem],
                    vec![Metric::Up, Metric::Down],
                )
            } else if cfg.display.show_resources {
                (vec![Metric::Cpu, Metric::Mem], Vec::new())
            } else {
                (vec![Metric::Up, Metric::Down], Vec::new())
            };
        for line in &row.lines {
            texts.push(line.left.clone());
            texts.push(line.right.clone());
        }
        let (mut runs, mut left_w, mut right_w, mut line_h) = layout_dual(row, &font);
        let n = runs.len() as f64;
        line_step = line_h;
        let mut block_h = line_h + line_step * (n - 1.0).max(0.0);
        // The system menu-bar thickness is the hard canvas height. The
        // attributed line box includes leading, so first try tightening
        // the inter-line step (two short rows of labels can sit closer
        // without touching); only if even the minimum step does not fit
        // do we shrink the font and re-measure. On a tall enough bar this
        // preserves the requested 0.8x; on a 22pt bar the loop falls back
        // to the 0.75x floor, and the tightened step keeps that fallback
        // inside the canvas instead of clipping its bottom.
        let mut attempts = 0;
        while block_h > img_h && attempts < 3 {
            // The step may dip slightly below one font size: the rows are
            // short labels, and at DUAL_ROW_MIN_STEP_FACTOR the previous
            // row's descenders still clear the next row's capitals.
            let min_step = (font.pointSize() * DUAL_ROW_MIN_STEP_FACTOR).max(1.0);
            let candidate = ((img_h - 0.5).max(1.0) - line_h) / (n - 1.0).max(1.0);
            if candidate >= min_step {
                line_step = candidate.min(line_h);
                block_h = line_h + line_step * (n - 1.0).max(0.0);
                break;
            }
            let factor = (img_h / block_h * 0.98).clamp(0.5, 1.0);
            let target = (font.pointSize() * factor).max(font_floor);
            if target >= font.pointSize() - 0.01 {
                break;
            }
            font = dual_row_font(target);
            let laid = layout_dual(row, &font);
            runs = laid.0;
            left_w = laid.1;
            right_w = laid.2;
            line_h = laid.3;
            line_step = line_h;
            block_h = line_h + line_step * (n - 1.0).max(0.0);
            attempts += 1;
        }
        if attempts > 0 {
            let effective = font.pointSize();
            if (DUAL_SHRUNK_FONT.with(|slot| slot.get()) - effective).abs() > 0.01 {
                DUAL_SHRUNK_FONT.with(|slot| slot.set(effective));
                log_event(
                    "INFO",
                    &format!(
                        "dual-row font fitted: requested={requested_size:.2} \
                             effective={effective:.2} bar={img_h:.1}"
                    ),
                );
            }
        }
        if block_h > img_h && !DUAL_OVERFLOW_LOGGED.replace(true) {
            log_event(
                "WARN",
                &format!(
                    "dual-row block exceeds menu bar: block={block_h:.1} bar={img_h:.1} font={:.2}",
                    font.pointSize()
                ),
            );
        }
        let y0 = ((img_h - block_h) / 2.0).max(0.0);
        // Fixed slots keep every origin stable, but the image has no
        // baked outer padding: the left cell starts at x = 0 and the
        // right value ends at the image's right edge. Inter-item spacing
        // is left to NSStatusBar, matching native menu-bar extras. Within
        // the right column the label stays at the column's fixed x while
        // the value is right-aligned, so labels remain aligned and only
        // the numerals' left edge moves inside their slot.
        let left_slot = column_reserve(&left_metrics, cfg, &font).max(left_w);
        let right_slot = column_reserve(&right_metrics, cfg, &font).max(right_w);
        let has_right = runs.iter().any(|r| r.ra.is_some());
        width = left_slot + (if has_right { spacing + right_slot } else { 0.0 });
        let right_x = left_slot + spacing;
        for (i, r) in runs.iter().enumerate() {
            let y = y0 + line_step * i as f64;
            items.push(DrawItem {
                attr: r.la.clone(),
                x: 0.0,
                y,
            });
            if r.ra.is_some() {
                let line = &row.lines[i];
                let prefix = right_metrics
                    .get(i)
                    .map(|m| metric_prefix(*m, cfg))
                    .unwrap_or("");
                let (label, value) = split_cell(&line.right, prefix);
                if !label.is_empty() {
                    let (attr, _, _) = make_run(&label, &font);
                    items.push(DrawItem {
                        attr,
                        x: right_x,
                        y,
                    });
                }
                if !value.is_empty() {
                    let (attr, value_w, _) = make_run(&value, &font);
                    items.push(DrawItem {
                        attr,
                        x: width - value_w,
                        y,
                    });
                }
            }
        }
    } else {
        // Single row: each metric gets its own fixed slot, so a digit
        // change in one metric cannot push the following metrics around;
        // each slot is vertically centered on its own measured height.
        let parts: Vec<String> = if !row.items.is_empty() {
            row.items.clone()
        } else {
            let mut v = Vec::new();
            for line in &row.lines {
                v.push(line.left.clone());
                if !line.right.is_empty() {
                    v.push(line.right.clone());
                }
            }
            v
        };
        if parts.is_empty() {
            return;
        }
        let metrics = visible_metrics(cfg);
        let mut slots: Vec<f64> = metrics
            .iter()
            .map(|m| metric_reserve(*m, cfg, &font))
            .collect();
        if slots.len() != parts.len() {
            // Defensive fallback: the frame and config disagree on the
            // visible metric set, so measure the actual strings instead
            // of risking a clipped slot.
            slots = parts
                .iter()
                .map(|p| make_run(p.trim_end(), &font).1)
                .collect();
        }
        let count = parts.len();
        let mut slot_start = 0.0;
        for (i, part) in parts.into_iter().enumerate() {
            let (attr, w, h) = make_run(part.trim_end(), &font);
            texts.push(part);
            let slot = slots.get(i).copied().unwrap_or(w).max(w);
            // First cell hugs the left edge; the last cell hugs the right
            // edge so the item's visible bounds match its content and the
            // system can apply its normal inter-item spacing. Middle
            // slots stay left-aligned on their fixed origins.
            let x = if count > 1 && i + 1 == count {
                slot_start + slot - w
            } else {
                slot_start
            };
            let y = ((img_h - h) / 2.0).max(0.0);
            items.push(DrawItem { attr, x, y });
            slot_start += slot + spacing;
        }
        width = slot_start - spacing;
    }
    if items.is_empty() || width <= 0.0 {
        return;
    }

    let sig = format!(
        "{dual}|{spacing}|{img_h:.1}|{:.2}|{width:.1}|{line_step:.2}|{}",
        font.pointSize(),
        texts.join("\u{1}")
    );
    if DRAW_SIGNATURE.with(|s| *s.borrow() == sig) {
        return;
    }

    // --- build the template image --------------------------------------
    // Flipped drawing context: origin top-left, y downward — the space the
    // layout above was computed in. The handler captures the run list, so
    // rasterization never touches thread-local state. Making the image a
    // template keeps AppKit's automatic light/dark inversion. The
    // indicator paints no hover/pressed/open backdrop: only the glyphs.
    let draw_items = items.clone();
    let draw_block = block2::RcBlock::new(move |_rect: NSRect| -> Bool {
        for it in &draw_items {
            it.attr.drawAtPoint(NSPoint::new(it.x, it.y));
        }
        Bool::YES
    });
    let image =
        NSImage::imageWithSize_flipped_drawingHandler(NSSize::new(width, img_h), true, &draw_block);
    image.setTemplate(true);

    STATUS_IMAGE.with(|slot| {
        *slot.borrow_mut() = Some(StatusImage { image, width });
    });
    DRAW_SIGNATURE.with(|s| *s.borrow_mut() = sig);
    apply_status_image();
    if !FIRST_FRAME_LOGGED.replace(true) {
        log_event("INFO", "startup: first frame applied");
    }
    let accessible = texts
        .iter()
        .filter(|text| !text.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    update_accessibility(accessible);
}

/// Force the next `render_indicator` pass to rebuild and remount the image.
fn invalidate_draw_signature() {
    DRAW_SIGNATURE.with(|s| s.borrow_mut().clear());
}

/// Mount the pre-rendered template image and its reserved width. Called
/// whenever `render_indicator` rebuilds the image, and once at startup.
pub(crate) fn apply_status_image() {
    let mtm = mtm();
    STATUS_IMAGE.with(|slot| {
        let slot = slot.borrow();
        let Some(images) = slot.as_ref() else {
            // No image yet: force the next render to build one instead of
            // matching a signature whose mount never happened.
            invalidate_draw_signature();
            return;
        };
        STATUS_ITEM.with(|si| {
            let si_borrow = si.borrow();
            let Some(item) = si_borrow.as_ref() else {
                invalidate_draw_signature();
                return;
            };
            let Some(button) = item.button(mtm) else {
                invalidate_draw_signature();
                return;
            };
            // Title cleared so only the image renders; `ImageOnly` +
            // `ScaleNone` keeps AppKit from rescaling the point layout.
            button.setTitle(&NSString::from_str(""));
            button.setImage(Some(&images.image));
            button.setImagePosition(NSCellImagePosition::ImageOnly);
            button.setImageScaling(NSImageScaling::ScaleNone);
            item.setLength(images.width);
            // Call through `NSView` explicitly: `NSControl` also declares a
            // deprecated zero-argument `setNeedsDisplay()`, which wins
            // method resolution on the button and shadows the view method.
            let view: &NSView = &button;
            view.setNeedsDisplay(true);
            view.displayIfNeeded();
        });
    });
}

/// Record the accessibility text for the current frame and, once the app
/// runs on its real run loop, push it to AppKit. The startup tick happens
/// before `NSApplication::run`, so the setters are deferred until ticks
/// are dispatched from inside the run loop. The value is stored
/// unconditionally so a deferred update still lands on the first timer
/// tick even when the image itself is reused unchanged.
pub(crate) fn update_accessibility(text: String) {
    if text.is_empty() {
        return;
    }
    ACCESSIBILITY_TEXT.with(|slot| *slot.borrow_mut() = text);
    if IN_RUN_LOOP.get() {
        apply_pending_accessibility();
    }
}

/// Push the stored accessibility value to AppKit when it changed.
pub(crate) fn apply_pending_accessibility() {
    let text = ACCESSIBILITY_TEXT.with(|slot| slot.borrow().clone());
    if text.is_empty() {
        return;
    }
    if ACCESSIBILITY_APPLIED.with(|slot| *slot.borrow() == text) {
        return;
    }
    apply_accessibility(&text);
    ACCESSIBILITY_APPLIED.with(|slot| *slot.borrow_mut() = text);
}

/// Expose the live values to VoiceOver and as the status item's native
/// tooltip. The template image itself has no accessible text, so without
/// this the menu-bar item reads as an unlabeled button.
pub(crate) fn apply_accessibility(value: &str) {
    if value.is_empty() {
        return;
    }
    let mtm = mtm();
    STATUS_ITEM.with(|si| {
        let si_borrow = si.borrow();
        let Some(item) = si_borrow.as_ref() else {
            return;
        };
        let Some(button) = item.button(mtm) else {
            return;
        };
        let label = NSString::from_str("SysVitals");
        let value = NSString::from_str(value);
        unsafe {
            let _: () = msg_send![&*button, setAccessibilityLabel: &*label];
            let _: () = msg_send![&*button, setAccessibilityValue: &*value];
            let _: () = msg_send![&*button, setToolTip: &*value];
        }
    });
}

/// Suppress AppKit's own status-button highlight. The system draws a
/// pressed/selected backdrop through the button cell; clearing both cell
/// style masks (`NSNoCellMask`) stops it from entering that drawing mode,
/// so a click paints no background of its own.
pub(crate) fn disable_button_highlight(button: &NSStatusBarButton) {
    let Some(cell) = button.cell() else {
        return;
    };
    let Ok(cell) = cell.downcast::<NSButtonCell>() else {
        return;
    };
    cell.setHighlightsBy(NSCellStyleMask::NoCellMask);
    cell.setShowsStateBy(NSCellStyleMask::NoCellMask);
}

#[cfg(test)]
mod tests {
    use super::split_cell;

    #[test]
    fn split_cell_keeps_the_configured_prefix_separate() {
        assert_eq!(
            split_cell("CPU 42%", "CPU"),
            ("CPU".to_owned(), "42%".to_owned())
        );
    }

    #[test]
    fn split_cell_trims_spacing_around_the_value() {
        assert_eq!(
            split_cell("CPU   42%  ", "CPU"),
            ("CPU".to_owned(), "42%".to_owned())
        );
    }

    #[test]
    fn split_cell_without_a_prefix_returns_the_whole_value() {
        assert_eq!(split_cell("42%", ""), (String::new(), "42%".to_owned()));
    }

    #[test]
    fn split_cell_with_only_the_prefix_has_an_empty_value() {
        assert_eq!(split_cell("CPU", "CPU"), ("CPU".to_owned(), String::new()));
    }

    #[test]
    fn split_cell_prefix_mismatch_keeps_the_text_whole() {
        assert_eq!(
            split_cell("NET 1.2 MB/s", "CPU"),
            (String::new(), "NET 1.2 MB/s".to_owned())
        );
    }
}
