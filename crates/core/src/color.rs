//! Perceptual color helpers for the GPUI panels.

pub fn abgr_to_rgb(abgr: u32) -> u32 {
    ((abgr & 0xFF) << 16) | (abgr & 0xFF00) | ((abgr >> 16) & 0xFF)
}

pub fn relative_luminance(hex: u32) -> f64 {
    fn linearize(channel: u8) -> f64 {
        let c = f64::from(channel) / 255.0;
        if c <= 0.040_45 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    let r = linearize(((hex >> 16) & 0xFF) as u8);
    let g = linearize(((hex >> 8) & 0xFF) as u8);
    let b = linearize((hex & 0xFF) as u8);
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

pub fn prefers_dark_foreground(hex: u32) -> bool {
    let l = relative_luminance(hex);
    let contrast_black = (l + 0.05) / 0.05;
    let contrast_white = 1.05 / (l + 0.05);
    contrast_black > contrast_white * 1.15
}

// Hue, saturation and lightness are each 0..=1, the range a GPUI `hsla` call expects.
pub fn rgb_to_hsl(red: u8, green: u8, blue: u8) -> (f32, f32, f32) {
    let r = f32::from(red) / 255.0;
    let g = f32::from(green) / 255.0;
    let b = f32::from(blue) / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) * 0.5;
    if (max - min).abs() < 1e-6 {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let h = if (max - r).abs() < 1e-6 {
        ((g - b) / d + if g < b { 6.0 } else { 0.0 }) / 6.0
    } else if (max - g).abs() < 1e-6 {
        ((b - r) / d + 2.0) / 6.0
    } else {
        ((r - g) / d + 4.0) / 6.0
    };
    (h, s, l)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bright_gold_gets_a_dark_label() {
        assert!(prefers_dark_foreground(0xFFB900));
    }

    #[test]
    fn default_blue_gets_a_light_label() {
        assert!(!prefers_dark_foreground(0x0078D4));
    }

    #[test]
    fn black_and_white_land_on_opposite_sides() {
        assert!(!prefers_dark_foreground(0x000000));
        assert!(prefers_dark_foreground(0xFFFFFF));
    }

    #[test]
    fn explorer_accent_abgr_is_unpacked_to_rgb() {
        assert_eq!(abgr_to_rgb(0xFF15_7286), 0x0086_7215);
        assert_eq!(abgr_to_rgb(0xFFD4_7800), 0x0000_78D4);
    }

    #[test]
    fn luminance_of_primaries_matches_reference_values() {
        assert!((relative_luminance(0x000000) - 0.0).abs() < 1e-9);
        assert!((relative_luminance(0xFFFFFF) - 1.0).abs() < 1e-9);
        assert!((relative_luminance(0xFF0000) - 0.2126).abs() < 1e-4);
        assert!((relative_luminance(0x00FF00) - 0.7152).abs() < 1e-4);
        assert!((relative_luminance(0x0000FF) - 0.0722).abs() < 1e-4);
    }

    fn assert_hsl(rgb: (u8, u8, u8), expected: (f64, f64, f64)) {
        let (h, s, l) = rgb_to_hsl(rgb.0, rgb.1, rgb.2);
        let got = [f64::from(h), f64::from(s), f64::from(l)];
        let want = [expected.0, expected.1, expected.2];
        for index in 0..3 {
            assert!(
                (got[index] - want[index]).abs() < 1e-5,
                "{rgb:?} component {index} is {}, expected {}",
                got[index],
                want[index]
            );
        }
    }

    #[test]
    fn rgb_to_hsl_matches_the_reference_sextants() {
        assert_hsl((0, 0, 0), (0.0, 0.0, 0.0));
        assert_hsl((255, 255, 255), (0.0, 0.0, 1.0));
        assert_hsl((128, 128, 128), (0.0, 0.0, 128.0 / 255.0));
        assert_hsl((255, 0, 0), (0.0, 1.0, 0.5));
        assert_hsl((0, 255, 0), (1.0 / 3.0, 1.0, 0.5));
        assert_hsl((0, 0, 255), (2.0 / 3.0, 1.0, 0.5));
        assert_hsl((0, 120, 212), (0.572_327, 1.0, 0.415_686));
        assert_hsl((255, 185, 0), (0.120_915, 1.0, 0.5));
    }

    #[test]
    fn rgb_to_hsl_stays_inside_the_unit_range() {
        for red in [0u8, 1, 64, 128, 200, 255] {
            for green in [0u8, 1, 64, 128, 200, 255] {
                for blue in [0u8, 1, 64, 128, 200, 255] {
                    let (h, s, l) = rgb_to_hsl(red, green, blue);
                    assert!(
                        (0.0..=1.0).contains(&h)
                            && (0.0..=1.0).contains(&s)
                            && (0.0..=1.0).contains(&l),
                        "{red},{green},{blue} -> {h},{s},{l}"
                    );
                }
            }
        }
    }

    #[test]
    fn greys_are_achromatic_and_primaries_are_fully_saturated() {
        for grey in [0u8, 17, 128, 200, 255] {
            let (_, s, _) = rgb_to_hsl(grey, grey, grey);
            assert_eq!(s, 0.0, "grey {grey} must have no saturation");
        }
        for primary in [(255u8, 0u8, 0u8), (0, 255, 0), (0, 0, 255)] {
            let (_, s, l) = rgb_to_hsl(primary.0, primary.1, primary.2);
            assert_eq!(s, 1.0, "{primary:?} must be fully saturated");
            assert_eq!(l, 0.5);
        }
    }
}
