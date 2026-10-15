//! Notification-area icon. Its color follows the tunnels: blue idle,
//! yellow connecting, green connected. Right-click opens Molehill's own
//! menu window (see ui/traymenu.slint) instead of a native Win32 menu.

use std::cell::RefCell;

use anyhow::Result;
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::logo;

#[derive(Debug, Clone, Copy)]
pub enum TrayAction {
    Show,
    /// Open the menu at this cursor position (physical pixels).
    Menu(i32, i32),
}

pub struct Tray {
    icon: TrayIcon,
    last: RefCell<([u8; 3], String)>,
}

fn icon(color: [u8; 3]) -> Option<Icon> {
    Icon::from_rgba(logo::render(32, color), 32, 32).ok()
}

impl Tray {
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
            .with_icon(icon(logo::ACCENT).expect("tray icon"))
            .build()?;
        Ok(Self { icon, last: RefCell::new((logo::ACCENT, "Molehill".into())) })
    }

    pub fn update(&self, color: [u8; 3], tooltip: &str) {
        let mut last = self.last.borrow_mut();
        if last.0 != color {
            let _ = self.icon.set_icon(icon(color));
        }
        if last.1 != tooltip {
            let _ = self.icon.set_tooltip(Some(tooltip));
        }
        *last = (color, tooltip.to_string());
    }
}
