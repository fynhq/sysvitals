//! Backdrop-material fallback rules for GPUI panels.

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Material {
    Opaque,
    Mica,
    Blur,
}

pub const MICA_MIN_BUILD: u32 = 22621;
pub const ACRYLIC_MIN_BUILD: u32 = 19041;

pub fn choose_windows_material(build: Option<u32>, transparency_enabled: bool) -> Material {
    if !transparency_enabled {
        return Material::Opaque;
    }
    match build {
        Some(b) if b >= MICA_MIN_BUILD => Material::Mica,
        Some(b) if b >= ACRYLIC_MIN_BUILD => Material::Blur,
        _ => Material::Opaque,
    }
}

pub fn choose_macos_material(transparency_enabled: bool) -> Material {
    if transparency_enabled {
        Material::Blur
    } else {
        Material::Opaque
    }
}

pub const MATERIAL_TINT_ALPHA: f32 = 0.45;

pub const MATERIAL_TINT_ALPHA_SETTINGS: f32 = 0.0;

pub const MATERIAL_TINT_ALPHA_DARK: f32 = 0.22;

pub const MATERIAL_TINT_ALPHA_MACOS: f32 = 0.82;

pub const MATERIAL_TINT_ALPHA_MACOS_DARK: f32 = 0.72;

pub fn material_tint_alpha(dark: bool) -> f32 {
    if dark {
        MATERIAL_TINT_ALPHA_DARK
    } else {
        MATERIAL_TINT_ALPHA
    }
}

pub fn material_tint_alpha_settings(_dark: bool) -> f32 {
    MATERIAL_TINT_ALPHA_SETTINGS
}

pub fn material_tint_alpha_macos(dark: bool) -> f32 {
    if dark {
        MATERIAL_TINT_ALPHA_MACOS_DARK
    } else {
        MATERIAL_TINT_ALPHA_MACOS
    }
}

pub fn should_tint(material: Material) -> bool {
    !matches!(material, Material::Opaque)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transparent_off_stays_opaque_on_modern_builds() {
        assert_eq!(
            choose_windows_material(Some(26100), false),
            Material::Opaque
        );
    }

    #[test]
    fn mica_from_22h2_onward() {
        assert_eq!(
            choose_windows_material(Some(MICA_MIN_BUILD), true),
            Material::Mica
        );
        assert_eq!(choose_windows_material(Some(26100), true), Material::Mica);
    }

    #[test]
    fn blur_between_19041_and_mica() {
        assert_eq!(
            choose_windows_material(Some(ACRYLIC_MIN_BUILD), true),
            Material::Blur
        );
        assert_eq!(choose_windows_material(Some(22000), true), Material::Blur);
        assert_eq!(
            choose_windows_material(Some(MICA_MIN_BUILD - 1), true),
            Material::Blur
        );
    }

    #[test]
    fn old_builds_and_probe_failure_stay_opaque() {
        assert_eq!(choose_windows_material(Some(18362), true), Material::Opaque);
        assert_eq!(
            choose_windows_material(Some(ACRYLIC_MIN_BUILD - 1), true),
            Material::Opaque
        );
        assert_eq!(choose_windows_material(None, true), Material::Opaque);
    }

    #[test]
    fn macos_honors_transparency_flag() {
        assert_eq!(choose_macos_material(true), Material::Blur);
        assert_eq!(choose_macos_material(false), Material::Opaque);
    }

    #[test]
    fn opaque_never_tints_the_theme() {
        assert!(!should_tint(Material::Opaque));
        assert!(should_tint(Material::Mica));
        assert!(should_tint(Material::Blur));
    }

    #[test]
    fn dark_mode_uses_a_clearer_tint() {
        assert_eq!(material_tint_alpha(false), MATERIAL_TINT_ALPHA);
        assert_eq!(material_tint_alpha(true), MATERIAL_TINT_ALPHA_DARK);
    }

    #[test]
    fn settings_exposes_the_material_fully() {
        assert_eq!(MATERIAL_TINT_ALPHA_SETTINGS, 0.0);
        assert_eq!(
            material_tint_alpha_settings(false),
            MATERIAL_TINT_ALPHA_SETTINGS
        );
        assert_eq!(
            material_tint_alpha_settings(true),
            MATERIAL_TINT_ALPHA_SETTINGS
        );
        assert!(material_tint_alpha_settings(false) < material_tint_alpha(false));
        assert!(material_tint_alpha_settings(true) < material_tint_alpha(true));
    }

    #[test]
    fn macos_uses_a_stronger_tint_than_mica() {
        assert!(material_tint_alpha_macos(false) > material_tint_alpha(false));
        assert!(material_tint_alpha_macos(true) > material_tint_alpha(true));
        assert_eq!(material_tint_alpha_macos(false), MATERIAL_TINT_ALPHA_MACOS);
        assert_eq!(
            material_tint_alpha_macos(true),
            MATERIAL_TINT_ALPHA_MACOS_DARK
        );
    }
}
