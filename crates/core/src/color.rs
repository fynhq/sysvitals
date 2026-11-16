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
}
