use gpui::{Pixels, Rgba, px};
use std::path::PathBuf;

const fn color(hex: u32) -> Rgba {
    let [_, red, green, blue] = hex.to_be_bytes();
    Rgba {
        r: red as f32 / 255.0,
        g: green as f32 / 255.0,
        b: blue as f32 / 255.0,
        a: 1.0,
    }
}

#[derive(Clone, Copy)]
pub struct Theme {
    pub background: Rgba,
    pub foreground: Rgba,
    pub surface: Rgba,
    pub separator: Rgba,
    pub dimmed: Rgba,
    pub accent: Rgba,
    pub selection: Rgba,
    pub danger: Rgba,
    pub warning: Rgba,
    pub good: Rgba,

    pub text_m: Pixels,
    pub button_size_m: Pixels,
    pub button_radius_m: Pixels,
    pub button_padding_m: Pixels,
    pub button_gap_m: Pixels,

    pub separator_thickness: Pixels,

    pub radius_small: Pixels,
    pub radius_medium: Pixels,

    pub spacing_tiny: Pixels,
    pub spacing_extra_small: Pixels,
    pub spacing_small: Pixels,
    pub spacing_medium: Pixels,

    pub text_extra_small: Pixels,
    pub text_small: Pixels,
    pub text_medium: Pixels,

    pub bar_height: Pixels,
    pub tray_icon_image_size: Pixels,

    pub launcher_height: Pixels,
    pub app_icon_size: Pixels,
    pub input_line_height: Pixels,
    pub cursor_width: Pixels,

    pub menu_minimum_width: Pixels,
    pub menu_maximum_width: Pixels,
}

impl Theme {
    pub const DEFAULT: Self = Self {
        background: color(0x1e1e2e),
        foreground: color(0xcdd6f4),
        surface: color(0x313244),
        separator: color(0x313244),
        dimmed: color(0x6c7086),
        accent: color(0x89b4fa),
        selection: color(0x45475a),
        danger: color(0xf38ba8),
        warning: color(0xf9e2af),
        good: color(0xa6e3a1),

        bar_height: px(32.0),

        text_m: px(16.),
        button_size_m: px(24.),
        button_radius_m: px(4.),
        button_padding_m: px(4.),
        button_gap_m: px(4.),

        separator_thickness: px(1.0),

        radius_small: px(4.0),
        radius_medium: px(8.0),

        spacing_tiny: px(2.0),
        spacing_extra_small: px(4.0),
        spacing_small: px(8.0),
        spacing_medium: px(12.0),

        text_extra_small: px(11.0),
        text_small: px(12.0),
        text_medium: px(14.0),

        tray_icon_image_size: px(16.0),

        launcher_height: px(400.0),
        app_icon_size: px(32.0),
        input_line_height: px(24.0),
        cursor_width: px(2.0),

        menu_minimum_width: px(200.0),
        menu_maximum_width: px(300.0),
    };
}

const ICON_THEMES: &[&str] = &["hicolor", "Adwaita", "Papirus", "Papirus-Dark"];
const ICON_SIZES: &[&str] = &["48x48", "32x32", "scalable", "64x64", "128x128", "256x256"];
const ICON_EXTENSIONS: &[&str] = &["png", "svg"];

/// Resolve an icon name to a file path, searching standard icon theme directories.
///
/// `custom_theme_path` is searched first (used by SNI tray items that provide their own theme).
/// `categories` controls which subdirectories to search (e.g. `&["apps"]` for the launcher,
/// `&["apps", "status", "devices", "panel"]` for the tray).
pub fn resolve_icon(name: &str, custom_theme_path: &str, categories: &[&str]) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }

    let as_path = PathBuf::from(name);
    if as_path.is_absolute() && as_path.exists() {
        return Some(as_path);
    }

    if !custom_theme_path.is_empty() {
        for size in ICON_SIZES {
            for extension in ICON_EXTENSIONS {
                let path = PathBuf::from(format!(
                    "{}/{}/apps/{}.{}",
                    custom_theme_path, size, name, extension
                ));
                if path.exists() {
                    return Some(path);
                }
            }
        }
        for extension in ICON_EXTENSIONS {
            let path = PathBuf::from(format!("{}/{}.{}", custom_theme_path, name, extension));
            if path.exists() {
                return Some(path);
            }
        }
    }

    for theme in ICON_THEMES {
        for size in ICON_SIZES {
            for category in categories {
                for extension in ICON_EXTENSIONS {
                    let path = PathBuf::from(format!(
                        "/usr/share/icons/{}/{}/{}/{}.{}",
                        theme, size, category, name, extension
                    ));
                    if path.exists() {
                        return Some(path);
                    }
                }
            }
        }
    }

    let pixmaps = PathBuf::from(format!("/usr/share/pixmaps/{}.png", name));
    if pixmaps.exists() {
        return Some(pixmaps);
    }

    None
}
