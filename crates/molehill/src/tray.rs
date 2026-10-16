//! Notification-area icon. It shows the brand mark in the variant for the
//! current state (docs/brand/png/logos), so one glance tells them apart.
//! Right-click opens Molehill's own menu window (see ui/traymenu.slint)
//! instead of a native Win32 menu.
//!
//! macOS: the same marks in the menu bar, with a native menu (same entries
//! as the Windows popup) on right-click; left-click opens the window.

use std::cell::RefCell;

use anyhow::Result;
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSMICON};

#[derive(Debug, Clone)]
pub enum TrayAction {
    Show,
    /// Open the menu at this cursor position (physical pixels).
    #[cfg_attr(not(windows), allow(dead_code))]
    Menu(i32, i32),
    /// macOS native menu entries.
    #[cfg(target_os = "macos")]
    Toggle(String),
    #[cfg(target_os = "macos")]
    DisconnectAll,
    #[cfg(target_os = "macos")]
    Quit,
}

/// What the tray icon shows, one brand mark each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayState {
    /// Nothing connected (tokyo blue).
    Idle,
    /// Signing in or connecting (amber).
    Connecting,
    /// One VPN up (mint).
    Connected,
    /// Several VPNs up (violet).
    ConnectedMany,
    /// A connection failed, none up (coral).
    Failed,
    /// The service isn't running (white mono).
    Offline,
}

impl TrayState {
    fn png(self) -> &'static [u8] {
        match self {
            Self::Idle => include_bytes!("../../../docs/brand/png/logos/molehill-mark-tokyo-blue.png"),
            Self::Connecting => include_bytes!("../../../docs/brand/png/logos/molehill-mark-amber.png"),
            Self::Connected => include_bytes!("../../../docs/brand/png/logos/molehill-mark-mint.png"),
            Self::ConnectedMany => include_bytes!("../../../docs/brand/png/logos/molehill-mark-violet.png"),
            Self::Failed => include_bytes!("../../../docs/brand/png/logos/molehill-mark-coral.png"),
            Self::Offline => include_bytes!("../../../docs/brand/png/logos/molehill-mark-dark-mono.png"),
        }
    }
}

/// The brand mark PNG, cropped to the drawing (x 2…62, y 30…58 of its 64
/// grid) and box-filtered down to a `size`² icon, vertically centred.
fn mark_pixels(png_bytes: &[u8], size: u32) -> Option<Vec<u8>> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width as f32, info.height as f32);
    let channels = info.color_type.samples();
    let grid = w / 64.0;
    // Source square around the mark: 60 units wide, centred on its 28 units of height.
    let (sx0, sy0, side) = (2.0 * grid, (44.0 - 30.0) * grid, 60.0 * grid);
    let step = side / size as f32;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let (x0, y0) = (sx0 + px as f32 * step, sy0 + py as f32 * step);
            let (mut acc, mut n) = ([0f32; 4], 0f32);
            let mut y = y0;
            while y < y0 + step {
                let mut x = x0;
                while x < x0 + step {
                    if x >= 0.0 && y >= 0.0 && x < w && y < h {
                        let i = (y as usize * info.width as usize + x as usize) * channels;
                        let a = buf[i + channels - 1] as f32;
                        // Premultiply so transparent edges don't darken the colours.
                        for c in 0..3 {
                            acc[c] += buf[i + c] as f32 * a;
                        }
                        acc[3] += a;
                    }
                    n += 1.0;
                    x += (step / 4.0).max(1.0);
                }
                y += (step / 4.0).max(1.0);
            }
            if acc[3] > 0.0 {
                out.extend_from_slice(&[(acc[0] / acc[3]) as u8, (acc[1] / acc[3]) as u8, (acc[2] / acc[3]) as u8, (acc[3] / n) as u8]);
            } else {
                out.extend_from_slice(&[0, 0, 0, 0]);
            }
        }
    }
    Some(out)
}

fn icon(state: TrayState) -> Option<Icon> {
    // The shell's small-icon size (16 at 100 %, 20 at 125 %, 24 at 150 %),
    // so the icon isn't rescaled again, blurrily, by Windows.
    #[cfg(windows)]
    let size = (unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16)) as u32;
    // The menu bar is 22 points tall; tray-icon scales for Retina itself.
    #[cfg(target_os = "macos")]
    let size = 36;
    Icon::from_rgba(mark_pixels(state.png(), size)?, size, size).ok()
}

pub struct Tray {
    icon: TrayIcon,
    last: RefCell<(TrayState, String)>,
    #[cfg(target_os = "macos")]
    last_menu: RefCell<Option<MenuModel>>,
}

/// What the macOS menu shows: summary, any tunnel on, and (id, name, state)
/// per tunnel, state being "on", "busy", "error" or "off".
#[cfg(target_os = "macos")]
#[derive(Clone, PartialEq)]
pub struct MenuModel {
    pub summary: String,
    pub any_on: bool,
    pub tunnels: Vec<(String, String, String)>,
}

#[cfg(target_os = "macos")]
fn build_menu(m: &MenuModel) -> tray_icon::menu::Menu {
    use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem};
    let menu = Menu::new();
    let _ = menu.append(&MenuItem::new(format!("Molehill · {}", m.summary), false, None));
    let _ = menu.append(&PredefinedMenuItem::separator());
    for (id, name, state) in &m.tunnels {
        let mark = match state.as_str() {
            "on" => "●",
            "busy" => "◐",
            "error" => "✕",
            _ => "○",
        };
        let action = if state == "on" || state == "busy" { "Disconnect" } else { "Connect" };
        let _ = menu.append(&MenuItem::with_id(format!("t:{id}"), format!("{mark}  {name} — {action}"), true, None));
    }
    if !m.tunnels.is_empty() {
        let _ = menu.append(&PredefinedMenuItem::separator());
    }
    let _ = menu.append(&MenuItem::with_id("open", "Open Molehill", true, None));
    let _ = menu.append(&MenuItem::with_id("disconnect-all", "Disconnect all", m.any_on, None));
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&MenuItem::with_id("quit", "Quit Molehill", true, None));
    menu
}

impl Tray {
    #[cfg(target_os = "macos")]
    pub fn new(on_action: impl Fn(TrayAction) + Send + Sync + 'static) -> Result<Self> {
        use std::sync::Arc;
        use tray_icon::menu::MenuEvent;
        let on_action = Arc::new(on_action);
        let on_click = on_action.clone();
        TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                on_click(TrayAction::Show);
            }
        }));
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            let id = e.id().0.as_str();
            let action = match id {
                "open" => TrayAction::Show,
                "disconnect-all" => TrayAction::DisconnectAll,
                "quit" => TrayAction::Quit,
                _ => match id.strip_prefix("t:") {
                    Some(t) => TrayAction::Toggle(t.to_string()),
                    None => return,
                },
            };
            on_action(action);
        }));
        let model = MenuModel { summary: String::new(), any_on: false, tunnels: Vec::new() };
        let icon = TrayIconBuilder::new()
            .with_tooltip("Molehill")
            .with_icon(icon(TrayState::Idle).expect("tray icon"))
            .with_menu(Box::new(build_menu(&model)))
            .with_menu_on_left_click(false)
            .build()?;
        Ok(Self { icon, last: RefCell::new((TrayState::Idle, "Molehill".into())), last_menu: RefCell::new(Some(model)) })
    }

    /// Rebuilds the macOS menu when what it shows changed.
    #[cfg(target_os = "macos")]
    pub fn set_menu(&self, model: MenuModel) {
        let mut last = self.last_menu.borrow_mut();
        if last.as_ref() != Some(&model) {
            self.icon.set_menu(Some(Box::new(build_menu(&model))));
            *last = Some(model);
        }
    }

    #[cfg(windows)]
    pub fn new(on_action: impl Fn(TrayAction) + Send + Sync + 'static) -> Result<Self> {
        TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
            if let TrayIconEvent::Click { button, button_state: MouseButtonState::Up, position, .. } = e {
                match button {
                    MouseButton::Left => on_action(TrayAction::Show),
                    MouseButton::Right => on_action(TrayAction::Menu(position.x as i32, position.y as i32)),
                    _ => {}
                }
            }
        }));
        let icon = TrayIconBuilder::new()
            .with_tooltip("Molehill")
            .with_icon(icon(TrayState::Idle).expect("tray icon"))
            .build()?;
        Ok(Self { icon, last: RefCell::new((TrayState::Idle, "Molehill".into())) })
    }

    pub fn update(&self, state: TrayState, tooltip: &str) {
        let mut last = self.last.borrow_mut();
        if last.0 != state {
            #[cfg(windows)]
            let _ = self.icon.set_icon(icon(state));
            // The "service not running" mark is monochrome: let macOS draw it
            // in the menu bar's own colour, light or dark.
            #[cfg(target_os = "macos")]
            let _ = if state == TrayState::Offline { self.icon.set_icon_templated(icon(state)) } else { self.icon.set_icon(icon(state)) };
        }
        if last.1 != tooltip {
            let _ = self.icon.set_tooltip(Some(tooltip));
        }
        *last = (state, tooltip.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_state_has_a_distinct_icon() {
        let all = [TrayState::Idle, TrayState::Connecting, TrayState::Connected, TrayState::ConnectedMany, TrayState::Failed, TrayState::Offline];
        let icons: Vec<Vec<u8>> = all.iter().map(|s| mark_pixels(s.png(), 24).expect("decodes")).collect();
        for (i, a) in icons.iter().enumerate() {
            assert_eq!(a.len(), 24 * 24 * 4);
            assert!(a.chunks(4).any(|p| p[3] > 200), "{:?} is empty", all[i]);
            for b in &icons[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }
}
