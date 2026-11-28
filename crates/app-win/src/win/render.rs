//! GDI indicator rendering and layout.

use super::*;
use sysvitals_core::format::{
    column_metrics, join_prefix, metric_prefix, visible_metrics, PERCENT_RESERVE, RATE_RESERVE,
};
use sysvitals_core::Metric;

pub(crate) struct FrameMetrics {
    pub(crate) line_h: i32,
    pub(crate) want_w: i32,
    pub(crate) right_x: Option<i32>,
    pub(crate) multi_line: bool,
    pub(crate) item_slots: Option<Vec<i32>>,
    pub(crate) line_heights: Vec<i32>,
}

// Slot widths come from the widest field, so digit-boundary changes stay inside the slot.
#[derive(Default)]
pub(crate) struct FrameReserves {
    items: Option<Vec<String>>,
    left_col: Vec<String>,
    right_col: Vec<String>,
}

impl FrameReserves {
    fn has_slots(&self) -> bool {
        self.items.is_some() || !self.left_col.is_empty() || !self.right_col.is_empty()
    }
}

fn reserve_string(metric: Metric, cfg: &Config) -> String {
    let value = match metric {
        Metric::Cpu | Metric::Mem => PERCENT_RESERVE,
        Metric::Up | Metric::Down => RATE_RESERVE,
    };
    join_prefix(metric_prefix(metric, cfg), value)
}

pub(crate) fn frame_reserves(row: &FormattedRow, cfg: &Config, vertical: bool) -> FrameReserves {
    if vertical {
        return FrameReserves::default();
    }
    if !row.items.is_empty() {
        let metrics = visible_metrics(cfg);
        if metrics.len() == row.items.len() {
            return FrameReserves {
                items: Some(metrics.iter().map(|m| reserve_string(*m, cfg)).collect()),
                ..Default::default()
            };
        }
        return FrameReserves::default();
    }
    if row.lines.len() <= 1 {
        return FrameReserves::default();
    }
    let (left_metrics, right_metrics) = column_metrics(cfg);
    FrameReserves {
        items: None,
        left_col: left_metrics
            .iter()
            .map(|m| reserve_string(*m, cfg))
            .collect(),
        right_col: right_metrics
            .iter()
            .map(|m| reserve_string(*m, cfg))
            .collect(),
    }
}

pub(crate) fn is_vertical_separator(line: &sysvitals_core::Line) -> bool {
    line.left.is_empty() && line.right.is_empty()
}

pub(crate) unsafe fn measure_frame(
    hdc: windows::Win32::Graphics::Gdi::HDC,
    row: &FormattedRow,
    spacing_px: i32,
    vertical: bool,
    line_pad: i32,
    reserves: Option<&FrameReserves>,
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

    if !row.items.is_empty() && !multi_line {
        let mut item_slots = Vec::with_capacity(row.items.len());
        let mut total_w = 0i32;
        for (i, item) in row.items.iter().enumerate() {
            let slot = match reserves
                .and_then(|r| r.items.as_ref())
                .and_then(|v| v.get(i))
            {
                Some(reserve) => text_w(item).max(reserved_width(hdc, reserve)),
                None => text_w(item),
            };
            item_slots.push(slot);
            total_w += slot;
            if i + 1 < row.items.len() {
                total_w += spacing_px;
            }
        }
        return FrameMetrics {
            line_h,
            want_w: total_w,
            right_x: None,
            multi_line: false,
            item_slots: Some(item_slots),
            line_heights: Vec::new(),
        };
    }

    if !multi_line || !has_right_col {
        let mut want_w = 0i32;
        for line in &row.lines {
            want_w = want_w.max(text_w(&line.render()));
        }
        if multi_line && !vertical {
            let reserved = reserves.map(|r| r.left_col.as_slice()).unwrap_or(&[]);
            want_w = want_w.max(reserved_width(hdc, reserved));
        }
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
            item_slots: None,
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
    left_col_w = left_col_w.max(reserved_width(
        hdc,
        reserves.map(|r| r.left_col.as_slice()).unwrap_or(&[]),
    ));
    right_col_w = right_col_w.max(reserved_width(
        hdc,
        reserves.map(|r| r.right_col.as_slice()).unwrap_or(&[]),
    ));
    let right_x = left_col_w + spacing_px;
    FrameMetrics {
        line_h,
        want_w: right_x + right_col_w,
        right_x: Some(right_x),
        multi_line,
        item_slots: None,
        line_heights: Vec::new(),
    }
}

unsafe fn reserved_width(hdc: windows::Win32::Graphics::Gdi::HDC, text: &str) -> i32 {
    let mut rc = RECT::default();
    let mut buf = utf16(text);
    DrawTextW(
        hdc,
        &mut buf,
        &mut rc,
        DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX,
    );
    rc.right - rc.left
}

pub(crate) unsafe fn resize_overlay(hwnd: HWND, row: &FormattedRow, vertical: bool) {
    let cfg = config();
    let dpi = dpi_for(hwnd);
    let spacing_px = cfg.display.item_spacing as i32;
    let line_pad = scale_px(FRAME_LINE_PAD_PX, dpi);
    let hdc = GetDC(Some(hwnd));
    if hdc.is_invalid() {
        log_event(
            "WARN",
            "resize_overlay: GetDC returned no DC; overlay will not be sized",
        );
        return;
    }
    let font = make_font_for(hwnd, vertical);
    let old = SelectObject(hdc, font.into());
    let reserves = frame_reserves(row, &cfg, vertical);
    let m = measure_frame(hdc, row, spacing_px, vertical, line_pad, Some(&reserves));
    SelectObject(hdc, old);
    let _ = DeleteObject(font.into());
    let _ = ReleaseDC(Some(hwnd), hdc);
    if m.line_h == 0 {
        return;
    }
    let want_w = m.want_w + pad_px(dpi, vertical) * 2 + width_slack_px(dpi, vertical);
    let want_h = if m.line_heights.is_empty() {
        (m.line_h + line_pad) * row.lines.len() as i32 + scale_px(FRAME_EDGE_PAD_PX, dpi)
    } else {
        m.line_heights.iter().sum::<i32>() + scale_px(FRAME_EDGE_PAD_PX, dpi)
    };
    let fixed_width = reserves.has_slots();
    OVERLAYS.with(|slot| {
        if let Some(ov) = slot.borrow_mut().iter_mut().find(|o| o.hwnd == hwnd) {
            ov.win_w = if fixed_width {
                want_w
            } else {
                ov.win_w.max(want_w)
            };
            ov.win_h = want_h;
        }
    });
}

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

pub(crate) fn relayout_all() {
    let hwnds: Vec<HWND> = OVERLAYS.with(|slot| slot.borrow().iter().map(|o| o.hwnd).collect());
    for hwnd in hwnds {
        unsafe { relayout_overlay(hwnd) };
    }
}

pub(crate) unsafe fn relayout_overlay(hwnd: HWND) {
    let Some(ov) = overlay_by_hwnd(hwnd) else {
        return;
    };
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
    apply_position(hwnd);
    if !IsWindowVisible(hwnd).as_bool() {
        set_overlay_needs_render(hwnd, true);
        return;
    }
    render_layered(hwnd, &row, ov.vertical);
    set_overlay_needs_render(hwnd, false);
}

// Render 2x supersampled glyph coverage, box-filter down to window scale; the pipeline is window-free for tests.
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
    line_pad: i32,
    reserves: Option<&FrameReserves>,
) -> Option<Vec<u32>> {
    if width <= 0 || height <= 0 {
        return None;
    }

    let screen_dc = GetDC(None);
    let mem_dc = CreateCompatibleDC(Some(screen_dc));
    if mem_dc.is_invalid() {
        if !screen_dc.is_invalid() {
            let _ = ReleaseDC(None, screen_dc);
        }
        log_event(
            "WARN",
            "render: no compatible DC for the supersample buffer",
        );
        return None;
    }

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
    if ss_bits.is_null() {
        let _ = DeleteObject(ss_bmp.into());
        let _ = DeleteDC(mem_dc);
        let _ = ReleaseDC(None, screen_dc);
        log_event(
            "WARN",
            "render: CreateDIBSection returned a null pixel buffer",
        );
        return None;
    }
    let old_ss_bmp = SelectObject(mem_dc, ss_bmp.into());
    let ss_font = create_font(font_height * SS, font_weight, face);
    let old_ss_font = SelectObject(mem_dc, ss_font.into());
    SetBkMode(mem_dc, TRANSPARENT);
    SetTextColor(mem_dc, COLORREF(0x00FF_FFFF));

    // SAFETY: CreateDIBSection returned a non-null buffer of `sw * sh` 32-bit
    // pixels (checked above), and every index below stays inside that count.
    let ss_px = ss_bits as *mut u32;
    let ss_n = (sw * sh) as usize;
    for i in 0..ss_n {
        *ss_px.add(i) = 0;
    }

    let fm = measure_frame(
        mem_dc,
        row,
        spacing_px * SS,
        vertical,
        line_pad * SS,
        reserves,
    );
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
        if let Some(ref item_slots) = fm.item_slots {
            let gap = spacing_px * SS;
            let total_w: i32 =
                item_slots.iter().sum::<i32>() + gap * (row.items.len().saturating_sub(1) as i32);
            let mut x = (sw - total_w) / 2;
            for (j, item) in row.items.iter().enumerate() {
                let mut rc = RECT {
                    left: x,
                    top,
                    right: x + item_slots[j],
                    bottom,
                };
                let align = if j + 1 == row.items.len() {
                    DT_RIGHT
                } else {
                    DT_LEFT
                };
                let mut buf = utf16(item);
                DrawTextW(
                    mem_dc,
                    &mut buf,
                    &mut rc,
                    DT_SINGLELINE | align | DT_VCENTER | DT_NOPREFIX,
                );
                x += item_slots[j] + gap;
            }
        } else if let Some(right_x) = fm.right_x {
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

    let (tr, tg, tb) = text_rgb;
    let mut pixels = vec![0u32; (width * height) as usize];
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
    let cfg = config();
    let dpi = dpi_for(hwnd);
    let reserves = frame_reserves(row, &cfg, vertical);
    let pad = pad_px(dpi, vertical);
    let content_offset = if !vertical && reserves.has_slots() {
        pad + width_slack_px(dpi, vertical) / 2
    } else {
        pad
    };
    let Some(pixels) = draw_frame(
        w,
        h,
        row,
        vertical,
        cfg.display.item_spacing as i32,
        content_offset,
        indicator_text_rgb(),
        effective_font_height_for(hwnd, vertical),
        TEXT_WEIGHT.load(Ordering::Relaxed),
        &face,
        scale_px(FRAME_LINE_PAD_PX, dpi),
        Some(&reserves),
    ) else {
        return;
    };

    let screen_dc = GetDC(None);
    let mem_dc = CreateCompatibleDC(Some(screen_dc));
    if mem_dc.is_invalid() {
        if !screen_dc.is_invalid() {
            let _ = ReleaseDC(None, screen_dc);
        }
        log_event("WARN", "render_layered: no compatible DC");
        return;
    }

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
    if bits.is_null() {
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem_dc);
        let _ = ReleaseDC(None, screen_dc);
        log_event(
            "WARN",
            "render_layered: CreateDIBSection returned a null pixel buffer",
        );
        return;
    }
    // SAFETY: `bits` was checked non-null and sizes `w * h` 32-bit pixels, and
    // `pixels` was allocated for exactly those dimensions by the caller above.
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
        render_with_reserves(row, width, height, vertical, None)
    }

    fn render_with_reserves(
        row: &FormattedRow,
        width: i32,
        height: i32,
        vertical: bool,
        reserves: Option<&FrameReserves>,
    ) -> Vec<u32> {
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
                2,
                reserves,
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

    fn measure(row: &FormattedRow, reserves: Option<&FrameReserves>) -> FrameMetrics {
        let (font_height, font_weight, face) = test_font();
        unsafe {
            let dc = GetDC(None);
            assert!(!dc.is_invalid(), "the test needs a screen DC");
            let font = create_font(font_height, font_weight, &face);
            let old = SelectObject(dc, font.into());
            let metrics = measure_frame(dc, row, 8, false, 2, reserves);
            SelectObject(dc, old);
            let _ = DeleteObject(font.into());
            let _ = ReleaseDC(None, dc);
            metrics
        }
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
    fn reserved_columns_keep_the_right_column_origin_fixed() {
        let reserves = FrameReserves {
            left_col: vec!["CPU 100%".to_string(), "MEM 100%".to_string()],
            right_col: vec!["U 8888 M/s".to_string(), "D 8888 M/s".to_string()],
            ..Default::default()
        };
        let narrow = formatted(&[("CPU 9%", "U 1.20 M/s"), ("MEM 45%", "D 2.30 M/s")]);
        let wide = formatted(&[("CPU 100%", "U 1.20 M/s"), ("MEM 45%", "D 2.30 M/s")]);
        let narrow_metrics = measure(&narrow, Some(&reserves));
        let wide_metrics = measure(&wide, Some(&reserves));
        assert_eq!(narrow_metrics.right_x, wide_metrics.right_x);
        assert_eq!(narrow_metrics.want_w, wide_metrics.want_w);
    }

    #[test]
    fn reserved_item_slots_keep_the_frame_width_fixed() {
        let reserves = FrameReserves {
            items: Some(vec!["CPU 100%".to_string()]),
            ..Default::default()
        };
        let narrow = FormattedRow {
            lines: vec![Line::single("CPU 9%")],
            items: vec!["CPU 9%".to_string()],
        };
        let wide = FormattedRow {
            lines: vec![Line::single("CPU 100%")],
            items: vec!["CPU 100%".to_string()],
        };
        let narrow_metrics = measure(&narrow, Some(&reserves));
        let wide_metrics = measure(&wide, Some(&reserves));
        assert_eq!(narrow_metrics.item_slots, wide_metrics.item_slots);
        assert_eq!(narrow_metrics.want_w, wide_metrics.want_w);

        let edge = |row: &FormattedRow| {
            glyph_pixels(
                &render_with_reserves(row, 200, 32, false, Some(&reserves)),
                200,
            )
            .map(|(x, _)| x)
            .max()
            .expect("the value must paint glyphs")
        };
        assert!(
            edge(&narrow).abs_diff(edge(&wide)) <= 1,
            "the last item must keep its right edge across value widths"
        );
    }

    #[test]
    fn reserves_use_the_widest_format_field_per_metric() {
        let mut cfg = Config::default();
        cfg.display.dual_row = false;
        let row = format_frame(&sample(), &cfg);
        let reserves = frame_reserves(&row, &cfg, false);
        let expected: Vec<String> = ["CPU: 100%", "MEM: 100%", "U: 8888 M/s", "D: 8888 M/s"]
            .into_iter()
            .map(str::to_string)
            .collect();
        assert_eq!(reserves.items.as_ref(), Some(&expected));
    }

    #[test]
    fn reserves_follow_the_dual_row_columns() {
        let cfg = Config::default();
        let row = format_frame(&sample(), &cfg);
        let reserves = frame_reserves(&row, &cfg, false);
        assert_eq!(
            reserves.left_col,
            vec!["CPU: 100%".to_string(), "MEM: 100%".to_string()]
        );
        assert_eq!(
            reserves.right_col,
            vec!["U: 8888 M/s".to_string(), "D: 8888 M/s".to_string()]
        );
    }

    fn sample() -> sysvitals_core::Snapshot {
        sysvitals_core::Snapshot {
            ts: 0,
            cpu_pct: 9.0,
            mem_pct: 45.0,
            up_bps: 1024.0,
            down_bps: 2048.0,
        }
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
                2,
                None,
            )
        };
        assert!(rendered.is_none());
    }
}
