//! Self-drawn status-item image rendering and invalidation.

use super::*;

#[derive(Clone)]
pub(crate) struct DrawItem {
    pub(crate) attr: Retained<NSAttributedString>,
    pub(crate) x: f64,
    pub(crate) y: f64,
}

pub(crate) struct StatusImage {
    pub(crate) image: Retained<NSImage>,
    pub(crate) width: f64,
}

pub(crate) struct RowRun {
    pub(crate) la: Retained<NSAttributedString>,
    pub(crate) lw: f64,
    pub(crate) lh: f64,
    pub(crate) ra: Option<Retained<NSAttributedString>>,
    pub(crate) rw: f64,
    pub(crate) rh: f64,
}

pub(crate) fn make_run(
    text: &str,
    font: &Retained<NSFont>,
) -> (Retained<NSAttributedString>, f64, f64) {
    let s = NSString::from_str(text);
    let obj: &AnyObject = font.as_super().as_super();
    let key: &NSString = unsafe { NSFontAttributeName };
    let dict = NSDictionary::from_slices(&[key], &[obj]);
    // SAFETY: the attribute dictionary holds a valid `NSFont` under the
    // documented `NSFontAttributeName` key.
    let attr = unsafe { NSAttributedString::new_with_attributes(&s, &dict) };
    let sz = attr.size();
    (attr, sz.width, sz.height)
}

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

pub(crate) fn metric_reserve(metric: Metric, cfg: &Config, font: &Retained<NSFont>) -> f64 {
    let value = match metric {
        Metric::Cpu | Metric::Mem => PERCENT_RESERVE,
        Metric::Up | Metric::Down => RATE_RESERVE,
    };
    make_run(&join_prefix(metric_prefix(metric, cfg), value), font).1
}

pub(crate) fn split_cell(text: &str, prefix: &str) -> (String, String) {
    let text = text.trim_end();
    if !prefix.is_empty() {
        if let Some(rest) = text.strip_prefix(prefix) {
            return (prefix.to_string(), rest.trim_start().to_string());
        }
    }
    (String::new(), text.to_string())
}

pub(crate) fn column_reserve(metrics: &[Metric], cfg: &Config, font: &Retained<NSFont>) -> f64 {
    metrics
        .iter()
        .map(|m| metric_reserve(*m, cfg, font))
        .fold(0f64, f64::max)
}

pub(crate) fn dual_row_font(size: f64) -> Retained<NSFont> {
    // SAFETY: `NSFontWeightSemibold` is an AppKit extern static; reading
    // any extern static is unsafe, matching the `NSFontAttributeName` use
    // below. The value itself is a plain `CGFloat` constant.
    let weight = unsafe { NSFontWeightSemibold };
    NSFont::systemFontOfSize_weight(size, weight)
}

pub(crate) fn render_indicator(row: &FormattedRow, cfg: &Config) {
    let dual = cfg.display.dual_row && row.lines.len() > 1;
    let spacing = f64::from(cfg.display.item_spacing);
    if !cfg.display.show_resources && !cfg.display.show_network {
        return;
    }
    let img_h = NSStatusBar::systemStatusBar().thickness();

    let base = NSFont::menuBarFontOfSize(0.0);
    let input_signature = format!(
        "{dual}|{spacing}|{img_h:.1}|{:.2}|{row:?}",
        base.pointSize()
    );
    if RENDER_INPUT_SIGNATURE.with(|sig| *sig.borrow() == input_signature)
        && STATUS_IMAGE.with(|image| image.borrow().is_some())
    {
        return;
    }
    let font_floor = (base.pointSize() * 0.75).max(1.0);
    let mut font = if dual {
        let scaled = (base.pointSize() * DUAL_ROW_FONT_FACTOR).max(1.0);
        dual_row_font(scaled)
    } else {
        base
    };
    let requested_size = font.pointSize();

    let mut items: Vec<DrawItem> = Vec::new();
    let mut texts: Vec<String> = Vec::new();
    let width;
    let mut line_step = 0.0_f64;
    if dual {
        let (left_metrics, right_metrics) = column_metrics(cfg);
        for line in &row.lines {
            texts.push(line.left.clone());
            texts.push(line.right.clone());
        }
        let (mut runs, mut left_w, mut right_w, mut line_h) = layout_dual(row, &font);
        let n = runs.len() as f64;
        line_step = line_h;
        let mut block_h = line_h + line_step * (n - 1.0).max(0.0);
        let mut attempts = 0;
        while block_h > img_h && attempts < 3 {
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
        RENDER_INPUT_SIGNATURE.with(|s| *s.borrow_mut() = input_signature.clone());
        return;
    }

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
    RENDER_INPUT_SIGNATURE.with(|s| *s.borrow_mut() = input_signature);
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

fn invalidate_draw_signature() {
    DRAW_SIGNATURE.with(|s| s.borrow_mut().clear());
    RENDER_INPUT_SIGNATURE.with(|s| s.borrow_mut().clear());
}

pub(crate) fn apply_status_image() {
    let mtm = mtm();
    STATUS_IMAGE.with(|slot| {
        let slot = slot.borrow();
        let Some(images) = slot.as_ref() else {
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
            button.setTitle(&NSString::from_str(""));
            button.setImage(Some(&images.image));
            button.setImagePosition(NSCellImagePosition::ImageOnly);
            button.setImageScaling(NSImageScaling::ScaleNone);
            item.setLength(images.width);
            let view: &NSView = &button;
            view.setNeedsDisplay(true);
            view.displayIfNeeded();
        });
    });
}

pub(crate) fn update_accessibility(text: String) {
    if text.is_empty() {
        return;
    }
    ACCESSIBILITY_TEXT.with(|slot| *slot.borrow_mut() = text);
    if IN_RUN_LOOP.get() {
        apply_pending_accessibility();
    }
}

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
