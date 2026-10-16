//! App preferences in `%APPDATA%\Molehill\settings.json`: the theme choice
//! and where the window was left. Missing or broken files mean defaults.

use molehill_core::paths;
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeMode {
    /// Follow Windows' app theme.
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::System => Self::Light,
            Self::Light => Self::Dark,
            Self::Dark => Self::System,
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Settings {
    pub theme: ThemeMode,
    /// Top-left of the window (physical pixels) and its height in logical
    /// pixels, as the user last left them.
    pub window_pos: Option<(i32, i32)>,
    pub window_height: Option<f64>,
}

fn file() -> std::path::PathBuf {
    paths::user_dir().join("settings.json")
}

pub fn load() -> Settings {
    let Some(v) = std::fs::read_to_string(file()).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) else {
        return Settings::default();
    };
    let theme = match v["theme"].as_str() {
        Some("light") => ThemeMode::Light,
        Some("dark") => ThemeMode::Dark,
        _ => ThemeMode::System,
    };
    let pos = v["window"]["x"].as_i64().zip(v["window"]["y"].as_i64()).map(|(x, y)| (x as i32, y as i32));
    let height = v["window"]["height"].as_f64().filter(|h| h.is_finite() && *h > 0.0);
    Settings { theme, window_pos: pos, window_height: height }
}

pub fn save(s: &Settings) {
    let mut window = json!({});
    if let Some((x, y)) = s.window_pos {
        window["x"] = json!(x);
        window["y"] = json!(y);
    }
    if let Some(h) = s.window_height {
        window["height"] = json!(h.round());
    }
    let v = json!({ "theme": s.theme.key(), "window": window });
    let path = file();
    let _ = std::fs::create_dir_all(paths::user_dir());
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, serde_json::to_string_pretty(&v).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Windows' app theme: true when apps should be dark.
pub fn system_prefers_dark() -> bool {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
    use windows::core::w;
    let mut value: u32 = 1;
    let mut len = 4u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut _),
            Some(&mut len),
        )
    };
    // Missing value: Windows' default is light.
    rc == windows::Win32::Foundation::ERROR_SUCCESS && value == 0
}
