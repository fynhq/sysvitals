//! render: split out of `win` (see `win` for the module map).

use super::*;

// ------------------------------------------------------- measurement/render

/// Geometry of one frame measured with the font currently selected into
/// `hdc`.
pub(crate) struct FrameMetrics {
    pub(crate) line_h: i32,
    pub(crate) want_w: i32,
    pub(crate) right_x: Option<i32>,
    pub(crate) multi_line: bool,
    pub(crate) item_widths: Option<Vec<i32>>,
    /// Vertical layout only: height of each line slot in pixels, in line
    /// order. Blank separator lines take `item_spacing` (the same gap the
    /// horizontal single-row layout puts between metrics) instead of a
    /// full text line, so both layouts use one consistent block gap and
    /// the column stays short. Empty for the other layouts, where every
    /// line slot has the uniform `line_h`.
    pub(crate) line_heights: Vec<i32>,
}

/// Whether a blank line is the vertical layout's inter-block separator
/// rather than content. The vertical frame is the only multi-line,
/// single-column frame whose lines can be empty (the stacked builder
/// inserts one blank line between metric blocks).
pub(crate) fn is_vertical_separator(line: &sysvitals_core::Line) -> bool {
    line.left.is_empty() && line.right.is_empty()
}

pub(crate) unsafe fn measure_frame(
    hdc: windows::Win32::Graphics::Gdi::HDC,
    row: &FormattedRow,
    spacing_px: i32,
    vertical: bool,
    line_pad: i32,
) -> FrameMetrics {
    let multi_line = row.lines.len() > 1;
    let has_right_col = row.lines.iter().any(|l| !l.right.is_empty());
    let mut line_h = 0i32;
    let mut text_w = |text: &str| -> i32 {
        let mut rc = RECT::default();
        let mut buf = utf16(text);
        DrawTextW(
            hdc,
            &mut buf,
            &mut rc,
            DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX,
        );
        line_h = line_h.max(rc.bottom - rc.top);
        rc.right - rc.left
    };

    // Single-row with individual items: lay out with a pixel gap.
    if !row.items.is_empty() && !multi_line {
        let mut item_widths = Vec::with_capacity(row.items.len());
        let mut total_w = 0i32;
        for (i, item) in row.items.iter().enumerate() {
            let w = text_w(item);
            item_widths.push(w);
            total_w += w;
            if i + 1 < row.items.len() {
                total_w += spacing_px;
            }
        }
        return FrameMetrics {
            line_h,
            want_w: total_w,
            right_x: None,
            multi_line: false,
            item_widths: Some(item_widths),
            line_heights: Vec::new(),
        };
    }

    if !multi_line || !has_right_col {
        let mut want_w = 0i32;
        for line in &row.lines {
            want_w = want_w.max(text_w(&line.render()));
        }
        // Vertical layout: separator lines shrink to the configured block
        // gap so it matches the horizontal single-row metric gap; content
        // lines keep the measured text height plus a breathing margin.
        // `line_pad` is supplied by the caller so the 2x supersampled
        // measurement scales it along with the glyphs.
        let line_heights: Vec<i32> = if vertical {
            row.lines
                .iter()
                .map(|l| {
                    if is_vertical_separator(l) {
                        spacing_px.max(1)
                    } else {
                        line_h + line_pad
                    }
                })
                .collect()
        } else {
            Vec::new()
        };
        return FrameMetrics {
            line_h,
            want_w,
            right_x: None,
            multi_line,
            item_widths: None,
            line_heights,
        };
    }
    let mut left_col_w = 0i32;
    let mut right_col_w = 0i32;
    for line in &row.lines {
        left_col_w = left_col_w.max(text_w(&line.left));
        if !line.right.is_empty() {
            right_col_w = right_col_w.max(text_w(&line.right));
        }
    }
    let right_x = left_col_w + spacing_px;
    FrameMetrics {
        line_h,
        want_w: right_x + right_col_w,
        right_x: Some(right_x),
        multi_line,
        item_widths: None,
        line_heights: Vec::new(),
    }
}

/// Measure the formatted rows and grow one overlay to fit. Width only
/// ever grows; height follows the row count, and in the vertical layout
/// the per-line slot heights from [`measure_frame`] (separator lines
/// sized to `item_spacing`, not a full text line).
pub(crate) unsafe fn resize_overlay(hwnd: HWND, row: &FormattedRow, vertical: bool) {
    let cfg = config();
    let spacing_px = cfg.display.item_spacing as i32;
    let hdc = GetDC(Some(hwnd));
    let font = make_font_for(hwnd, vertical);
    let old = SelectObject(hdc, font.into());
    let m = measure_frame(hdc, row, spacing_px, vertical, 2);
    SelectObject(hdc, old);
    let _ = DeleteObject(font.into());
    let _ = ReleaseDC(Some(hwnd), hdc);
    if m.line_h == 0 {
        return;
    }
    let want_w = m.want_w + pad_px(vertical) * 2 + width_slack_px(vertical);
    let want_h = if m.line_heights.is_empty() {
        (m.line_h + 2) * row.lines.len() as i32 + 2
    } else {
        m.line_heights.iter().sum::<i32>() + 2
    };
    OVERLAYS.with(|slot| {
        if let Some(ov) = slot.borrow_mut().iter_mut().find(|o| o.hwnd == hwnd) {
            ov.win_w = ov.win_w.max(want_w);
            ov.win_h = want_h;
        }
    });
}

/// One sampling tick: take a single reading, store it, write it to the
/// history database and re-layout every overlay from it. All overlays
/// share this one sample so network rates are computed over the full
/// interval regardless of how many taskbars are showing.
pub(crate) unsafe fn sample_tick() {
    let cfg = config();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let snapshot = SAMPLER.with(|s| {
        let mut guard = s.borrow_mut();
        let sampler = guard.get_or_insert_with(Sampler::new);
        sampler.sample(now)
    });
    LAST_SNAPSHOT.with(|s| *s.borrow_mut() = Some(snapshot));
    HISTORY_WRITER.with(|slot| {
        sysvitals_support::record_history_sample(&mut slot.borrow_mut(), &cfg, now, &snapshot);
    });
    relayout_all();
}

/// Re-layout and redraw every overlay from the last sample.
pub(crate) fn relayout_all() {
    let hwnds: Vec<HWND> = OVERLAYS.with(|slot| slot.borrow().iter().map(|o| o.hwnd).collect());
    for hwnd in hwnds {
        unsafe { relayout_overlay(hwnd) };
    }
}

/// Rebuild one overlay's frame from the last sample with the current
/// config, its own orientation and font. Config hot-reload, DPI/text-size
/// changes and theme flips all use this: taking a fresh sample for an
/// unrelated setting change used to compute network rates over a few
/// milliseconds and paint a bogus spike. Only the periodic timer (and
/// startup) samples.
pub(crate) unsafe fn relayout_overlay(hwnd: HWND) {
    let Some(ov) = overlay_by_hwnd(hwnd) else {
        return;
    };
    // Auto-hide and fullscreen hide the overlay between sample ticks.
    // Measuring/formatting/painting a window nobody can see is pure waste;
    // the visible tick repaints from the latest snapshot.
    if !IsWindowVisible(hwnd).as_bool() {
        set_overlay_needs_render(hwnd, true);
        return;
    }
    let Some(snapshot) = LAST_SNAPSHOT.with(|s| *s.borrow()) else {
        sample_tick();
        return;
    };
    let cfg = config();
    let row = if ov.vertical {
        format_frame_vertical(&snapshot, &cfg)
    } else {
        format_frame(&snapshot, &cfg)
    };
    resize_overlay(hwnd, &row, ov.vertical);
    // Re-place with the new size before painting: a right-anchored window
    // must not grow into the tray until the next one-second move tick.
    apply_position(hwnd);
    // `apply_position` may hide the overlay if the taskbar vanished between
    // the check above and placement; do not paint a hidden window.
    if !IsWindowVisible(hwnd).as_bool() {
        set_overlay_needs_render(hwnd, true);
        return;
    }
    render_layered(hwnd, &row, ov.vertical);
    set_overlay_needs_render(hwnd, false);
}

/// Rasterize one formatted frame into premultiplied BGRA pixels at window
/// scale. The whole pipeline — 2x supersampled glyph coverage, box-filtered
/// downsample and themed composition — is window-free, so the offscreen tests
/// can run it on a CI runner without a taskbar. `font_height` is the physical
/// glyph height (negative for character height), matching what
/// `effective_font_height_for` returns for the overlay.
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn draw_frame(
    width: i32,
    height: i32,
    row: &FormattedRow,
    vertical: bool,
    spacing_px: i32,
    pad: i32,
    text_rgb: (u32, u32, u32),
    font_height: i32,
    font_weight: i32,
    face: &[u16],
) -> Option<Vec<u32>> {
    if width <= 0 || height <= 0 {
        return None;
    }

    let screen_dc = GetDC(None);
    let mem_dc = CreateCompatibleDC(Some(screen_dc));

    // --- Scratch surface at 2x supersampled scale ---------------------
    const SS: i32 = 2;
    let sw = width * SS;
    let sh = height * SS;
    let mut ss_bmi = BITMAPINFO::default();
    ss_bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    ss_bmi.bmiHeader.biWidth = sw;
    ss_bmi.bmiHeader.biHeight = -sh;
    ss_bmi.bmiHeader.biPlanes = 1;
    ss_bmi.bmiHeader.biBitCount = 32;
    ss_bmi.bmiHeader.biCompression = BI_RGB.0;
    let mut ss_bits: *mut std::ffi::c_void = std::ptr::null_mut();
    let Ok(ss_bmp) = CreateDIBSection(Some(mem_dc), &ss_bmi, DIB_RGB_COLORS, &mut ss_bits, None, 0)
    else {
        let _ = DeleteDC(mem_dc);
        let _ = ReleaseDC(None, screen_dc);
        return None;
    };
    let old_ss_bmp = SelectObject(mem_dc, ss_bmp.into());
    let ss_font = create_font(font_height * SS, font_weight, face);
    let old_ss_font = SelectObject(mem_dc, ss_font.into());
    SetBkMode(mem_dc, TRANSPARENT);
    SetTextColor(mem_dc, COLORREF(0x00FF_FFFF));

    // Clear the scratch surface to black (coverage 0).
    let ss_px = ss_bits as *mut u32;
    let ss_n = (sw * sh) as usize;
    for i in 0..ss_n {
        *ss_px.add(i) = 0;
    }

    let fm = measure_frame(mem_dc, row, spacing_px * SS, vertical, 2 * SS);
    // Line slot tops in supersampled pixels. Uniform division works for
    // the horizontal layouts; the vertical layout has per-line heights
    // (separator lines sized to `item_spacing`, content lines sized to
    // the text height + breathing), so tops accumulate from that table.
    let ss_line_h = sh / row.lines.len().max(1) as i32;
    let mut tops: Vec<(i32, i32)> = Vec::with_capacity(row.lines.len());
    if fm.line_heights.is_empty() {
        for i in 0..row.lines.len() {
            tops.push((i as i32 * ss_line_h, (i as i32 + 1) * ss_line_h));
        }
    } else {
        let mut acc = 0i32;
        for h in &fm.line_heights {
            tops.push((acc, acc + h));
            acc += h;
        }
    }
    let pad = pad * SS;
    for (i, line) in row.lines.iter().enumerate() {
        let (top, bottom) = tops.get(i).copied().unwrap_or((0, sh));
        if let Some(ref item_widths) = fm.item_widths {
            // Single-row pixel-spacing layout.
            let gap = spacing_px * SS;
            let total_w: i32 =
                item_widths.iter().sum::<i32>() + gap * (row.items.len().saturating_sub(1) as i32);
            let mut x = (sw - total_w) / 2;
            for (j, item) in row.items.iter().enumerate() {
                let mut rc = RECT {
                    left: x,
                    top,
                    right: x + item_widths[j],
                    bottom,
                };
                let mut buf = utf16(item);
                DrawTextW(
                    mem_dc,
                    &mut buf,
                    &mut rc,
                    DT_SINGLELINE | DT_LEFT | DT_VCENTER | DT_NOPREFIX,
                );
                x += item_widths[j] + gap;
            }
        } else if let Some(right_x) = fm.right_x {
            // Dual-row: left column left-aligned, right column at offset.
            let mut left_rc = RECT {
                left: pad,
                top,
                right: pad + right_x,
                bottom,
            };
            let mut lbuf = utf16(&line.left);
            DrawTextW(
                mem_dc,
                &mut lbuf,
                &mut left_rc,
                DT_SINGLELINE | DT_LEFT | DT_VCENTER | DT_NOPREFIX,
            );
            if !line.right.is_empty() {
                let mut right_rc = RECT {
                    left: pad + right_x,
                    top,
                    right: sw - pad,
                    bottom,
                };
                let mut rbuf = utf16(&line.right);
                DrawTextW(
                    mem_dc,
                    &mut rbuf,
                    &mut right_rc,
                    DT_SINGLELINE | DT_LEFT | DT_VCENTER | DT_NOPREFIX,
                );
            }
        } else if fm.multi_line {
            // Multi-line single column: left-aligned.
            let text = line.render();
            let mut line_rc = RECT {
                left: pad,
                top,
                right: sw - pad,
                bottom,
            };
            let mut buf = utf16(&text);
            DrawTextW(
                mem_dc,
                &mut buf,
                &mut line_rc,
                DT_SINGLELINE | DT_LEFT | DT_VCENTER | DT_NOPREFIX,
            );
        } else {
            // Single row: centered.
            let text = line.render();
            let mut line_rc = RECT {
                left: 0,
                top,
                right: sw,
                bottom,
            };
            let mut buf = utf16(&text);
            DrawTextW(
                mem_dc,
                &mut buf,
                &mut line_rc,
                DT_SINGLELINE | DT_CENTER | DT_VCENTER | DT_NOPREFIX,
            );
        }
    }

    SelectObject(mem_dc, old_ss_font);
    let _ = DeleteObject(ss_font.into());

    // --- Final composition at window scale, downsampled 2x2 -------------
    let (tr, tg, tb) = text_rgb;
    let mut pixels = vec![0u32; (width * height) as usize];
    // Glyph-only composition. The indicator draws no hover/pressed/open
    // background at all: the whole rectangle keeps a minimal (but
    // non-zero) alpha so it stays hit-testable over the taskbar, and the
    // supersampled glyph coverage is written premultiplied for
    // `AC_SRC_ALPHA`.
    for y in 0..height {
        for x in 0..width {
            let mut acc = 0u32;
            for dy in 0..SS {
                for dx in 0..SS {
                    let sx = x * SS + dx;
                    let sy = y * SS + dy;
                    acc += *ss_px.add((sy * sw + sx) as usize) & 0xFF;
                }
            }
            let cov = acc / (SS * SS) as u32;
            let index = (y * width + x) as usize;
            if cov == 0 {
                pixels[index] = 0x0100_0000;
            } else {
                let mix = |glyph: u32| glyph * cov / 255;
                pixels[index] = (cov << 24) | (mix(tr) << 16) | (mix(tg) << 8) | mix(tb);
            }
        }
    }

    SelectObject(mem_dc, old_ss_bmp);
    let _ = DeleteObject(ss_bmp.into());
    let _ = DeleteDC(mem_dc);
    let _ = ReleaseDC(None, screen_dc);
    Some(pixels)
}

/// Render a frame into a 32bpp premultiplied-alpha DIB and blit it onto
/// the layered window with `UpdateLayeredWindow`.
pub(crate) unsafe fn render_layered(hwnd: HWND, row: &FormattedRow, vertical: bool) {
    let mut win_rc = RECT::default();
    if GetWindowRect(hwnd, &mut win_rc).is_err() {
        return;
    }
    let w = win_rc.right - win_rc.left;
    let h = win_rc.bottom - win_rc.top;
    if w <= 0 || h <= 0 {
        return;
    }

    let (_, face) = ui_font();
    let Some(pixels) = draw_frame(
        w,
        h,
        row,
        vertical,
        config().display.item_spacing as i32,
        pad_px(vertical),
        indicator_text_rgb(),
        effective_font_height_for(hwnd, vertical),
        TEXT_WEIGHT.load(Ordering::Relaxed),
        &face,
    ) else {
        return;
    };

    let screen_dc = GetDC(None);
    let mem_dc = CreateCompatibleDC(Some(screen_dc));

    let mut bmi = BITMAPINFO::default();
    bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = w;
    bmi.bmiHeader.biHeight = -h;
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = BI_RGB.0;
    let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
    let Ok(bmp) = CreateDIBSection(Some(mem_dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
        let _ = DeleteDC(mem_dc);
        let _ = ReleaseDC(None, screen_dc);
        return;
    };
    std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits as *mut u32, pixels.len());

    let old_bmp = SelectObject(mem_dc, bmp.into());
    let src_pt = POINT { x: 0, y: 0 };
    let size = SIZE { cx: w, cy: h };
    let dst_pt = POINT {
        x: win_rc.left,
        y: win_rc.top,
    };
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    match UpdateLayeredWindow(
        hwnd,
        None,
        Some(&dst_pt),
        Some(&size),
        Some(mem_dc),
        Some(&src_pt),
        COLORREF(0),
        Some(&blend),
        ULW_ALPHA,
    ) {
        Ok(()) => {
            with_overlay_mut(hwnd, |ov| ov.layered_fail_logged = false);
        }
        Err(e) => {
            if with_overlay_mut(hwnd, |ov| {
                !std::mem::replace(&mut ov.layered_fail_logged, true)
            })
            .unwrap_or(false)
            {
                log_event("WARN", &format!("UpdateLayeredWindow failed: {e}"));
            }
        }
    }

    SelectObject(mem_dc, old_bmp);
    let _ = DeleteObject(bmp.into());
    let _ = DeleteDC(mem_dc);
    let _ = ReleaseDC(None, screen_dc);
}

#[cfg(test)]
mod tests {
    use super::*;
    use sysvitals_core::format::Line;

    fn test_font() -> (i32, i32, Vec<u16>) {
        (-24, 400, utf16("Segoe UI"))
    }

    fn formatted(lines: &[(&str, &str)]) -> FormattedRow {
        FormattedRow {
            lines: lines
                .iter()
                .map(|(left, right)| Line {
                    left: (*left).to_string(),
                    right: (*right).to_string(),
                })
                .collect(),
            items: Vec::new(),
        }
    }

    fn render(row: &FormattedRow, width: i32, height: i32, vertical: bool) -> Vec<u32> {
        let (font_height, font_weight, face) = test_font();
        unsafe {
            draw_frame(
                width,
                height,
                row,
                vertical,
                8,
                6,
                (255, 255, 255),
                font_height,
                font_weight,
                &face,
            )
            .expect("the offscreen pipeline must produce a frame")
        }
    }

    fn glyph_pixels(pixels: &[u32], width: usize) -> impl Iterator<Item = (usize, u32)> + '_ {
        pixels
            .iter()
            .copied()
            .enumerate()
            .filter(|(_, px)| (px >> 24) > 1)
            .map(move |(index, px)| (index % width, px))
    }

    #[test]
    fn a_single_row_is_painted_centered_and_visible() {
        let row = formatted(&[("CPU 12%", "")]);
        let pixels = render(&row, 200, 32, false);
        assert_eq!(pixels.len(), 200 * 32);
        assert!(
            pixels.iter().all(|&px| (px >> 24) >= 1),
            "every pixel keeps the hit-test alpha"
        );
        assert!(
            pixels.iter().all(|&px| {
                let alpha = (px >> 24) & 0xFF;
                ((px >> 16) & 0xFF) <= alpha && ((px >> 8) & 0xFF) <= alpha && (px & 0xFF) <= alpha
            }),
            "premultiplied channels must not exceed alpha"
        );

        let xs: Vec<usize> = glyph_pixels(&pixels, 200).map(|(x, _)| x).collect();
        assert!(!xs.is_empty(), "the text must paint glyph pixels");
        assert!(
            *xs.iter().min().unwrap() > 10 && *xs.iter().max().unwrap() < 190,
            "centered text must keep a margin on both sides"
        );
        assert!(
            *xs.iter().min().unwrap() < 100 && *xs.iter().max().unwrap() > 100,
            "the row must straddle the horizontal center"
        );
    }

    #[test]
    fn a_dual_column_frame_paints_both_columns() {
        let row = formatted(&[("CPU 100%", "12%"), ("MEM 100%", "48%")]);
        let pixels = render(&row, 240, 48, false);
        let columns: Vec<usize> = glyph_pixels(&pixels, 240).map(|(x, _)| x).collect();
        assert!(
            columns.iter().any(|&x| x < 40),
            "the left column must paint near the left padding"
        );
        assert!(
            columns.iter().any(|&x| x > 60),
            "the right column must paint past the left column"
        );
    }

    #[test]
    fn a_frame_with_items_lays_out_the_single_row_gap() {
        let row = FormattedRow {
            lines: vec![Line::single("CPU 12%  MEM 48%")],
            items: vec!["CPU 12%".to_string(), "MEM 48%".to_string()],
        };
        let pixels = render(&row, 240, 32, false);
        assert!(glyph_pixels(&pixels, 240).count() > 0);
    }

    #[test]
    fn an_empty_canvas_is_rejected_instead_of_panicking() {
        let row = formatted(&[("CPU 12%", "")]);
        let (font_height, font_weight, face) = test_font();
        let rendered = unsafe {
            draw_frame(
                0,
                32,
                &row,
                false,
                8,
                6,
                (255, 255, 255),
                font_height,
                font_weight,
                &face,
            )
        };
        assert!(rendered.is_none());
    }
}
