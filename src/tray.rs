use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent,
};

use crate::engine::TrayIconState;

/// High-level actions emitted by user interactions with the system tray icon and menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    OpenSettings,
    ShowStats,
    DismissBreak,
    ToggleSnoozeAll,
    TogglePauseAll,
    Quit,
}

/// Manages the system tray icon, its context menu items, dynamic labels, and event polling.
pub struct TrayManager {
    tray_icon: Option<TrayIcon>,
    current_state: TrayIconState,
    _item_settings: MenuItem,
    _item_stats: MenuItem,
    item_dismiss: MenuItem,
    item_snooze: MenuItem,
    pub item_pause: MenuItem,
    _item_quit: MenuItem,
}

impl TrayManager {
    /// Builds the system tray icon and menu with initial states reflecting configuration.
    pub fn new(pause_all_rules: bool) -> Result<Self, Box<dyn std::error::Error>> {
        let item_settings = MenuItem::with_id("open_settings", "Open Settings", true, None);
        let item_stats = MenuItem::with_id("show_stats", "Show Activity Stats", true, None);
        let item_dismiss = MenuItem::with_id("dismiss_break", "Dismiss Break Overlay", true, None);
        let item_snooze = MenuItem::with_id("snooze_15m", "Snooze All (15m)", true, None);
        let initial_pause_label = if pause_all_rules {
            "Resume All Rules"
        } else {
            "Pause All Rules"
        };
        let item_pause = MenuItem::with_id("toggle_pause_all", initial_pause_label, true, None);
        let item_quit = MenuItem::with_id("quit", "Quit", true, None);

        let tray_menu = Menu::new();
        tray_menu.append(&item_settings)?;
        tray_menu.append(&item_stats)?;
        tray_menu.append(&item_dismiss)?;
        tray_menu.append(&item_snooze)?;
        tray_menu.append(&item_pause)?;
        tray_menu.append(&PredefinedMenuItem::separator())?;
        tray_menu.append(&item_quit)?;

        let initial_tray_state = if pause_all_rules {
            TrayIconState::Paused
        } else {
            TrayIconState::Normal
        };
        let initial_icon = match initial_tray_state {
            TrayIconState::Paused => create_paused_tray_icon(),
            _ => create_default_tray_icon(),
        };

        let tray_icon = TrayIconBuilder::new()
            .with_menu(Box::new(tray_menu))
            .with_tooltip("AW-Enforcer")
            .with_icon(initial_icon)
            .build()
            .ok();

        Ok(Self {
            tray_icon,
            current_state: initial_tray_state,
            _item_settings: item_settings,
            _item_stats: item_stats,
            item_dismiss,
            item_snooze,
            item_pause,
            _item_quit: item_quit,
        })
    }

    /// Synchronizes menu item enablement, snooze/pause labels, tooltip text, and tri-state icon.
    pub fn sync_live_status(
        &mut self,
        strict_lock_active: bool,
        any_snoozed: bool,
        tooltip: &str,
        tray_state: TrayIconState,
    ) {
        self.item_dismiss.set_enabled(!strict_lock_active);
        self.item_snooze.set_enabled(!strict_lock_active);
        self.item_snooze.set_text(if any_snoozed {
            "Resume Snoozed Rules"
        } else {
            "Snooze All (15m)"
        });
        self.item_pause.set_enabled(!strict_lock_active);

        if let Some(ref ti) = self.tray_icon {
            let _ = ti.set_tooltip(Some(tooltip));
            if self.current_state != tray_state {
                self.current_state = tray_state;
                let icon = match tray_state {
                    TrayIconState::Normal => create_default_tray_icon(),
                    TrayIconState::Paused => create_paused_tray_icon(),
                    TrayIconState::Alert => create_alert_tray_icon(),
                };
                let _ = ti.set_icon(Some(icon));
            }
        }
    }

    /// Updates the label of the "Pause All Rules" / "Resume All Rules" menu item.
    pub fn set_pause_label(&self, paused: bool) {
        self.item_pause.set_text(if paused {
            "Resume All Rules"
        } else {
            "Pause All Rules"
        });
    }

    /// Clones the `item_pause` handle so external components (e.g. `SettingsContext`) can share it.
    pub fn item_pause(&self) -> MenuItem {
        self.item_pause.clone()
    }

    /// Polls pending tray icon double-click events and context menu item selections,
    /// translating them into domain `TrayAction`s passed to the callback closure.
    pub fn process_events<F>(&self, mut on_action: F)
    where
        F: FnMut(TrayAction),
    {
        // 1. Process System Tray Icon Double-Click Events
        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                ..
            } = event
            {
                on_action(TrayAction::OpenSettings);
            }
        }

        // 2. Process System Tray Menu Events
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            tracing::info!("Received system tray menu event: {}", event.id.0);
            match event.id.0.as_str() {
                "open_settings" => on_action(TrayAction::OpenSettings),
                "show_stats" => on_action(TrayAction::ShowStats),
                "dismiss_break" => on_action(TrayAction::DismissBreak),
                "snooze_15m" => on_action(TrayAction::ToggleSnoozeAll),
                "toggle_pause_all" => on_action(TrayAction::TogglePauseAll),
                "quit" => on_action(TrayAction::Quit),
                _ => {}
            }
        }
    }
}

/// Loads the official Fluent ClockPause 32x32 RGBA system tray icon (Normal / Blue).
fn create_default_tray_icon() -> tray_icon::Icon {
    const TRAY_ICON_RGBA: &[u8] = include_bytes!("../assets/tray_icon_32.rgba");
    tray_icon::Icon::from_rgba(TRAY_ICON_RGBA.to_vec(), 32, 32)
        .expect("Failed to create Fluent ClockPause tray icon")
}

/// Loads the paused Fluent ClockPause 32x32 RGBA system tray icon (Paused / Gray).
fn create_paused_tray_icon() -> tray_icon::Icon {
    const TRAY_ICON_PAUSED_RGBA: &[u8] = include_bytes!("../assets/tray_icon_paused_32.rgba");
    tray_icon::Icon::from_rgba(TRAY_ICON_PAUSED_RGBA.to_vec(), 32, 32)
        .expect("Failed to create Fluent ClockPause paused tray icon")
}

/// Loads the alert Fluent ClockPause 32x32 RGBA system tray icon (Alert / Red).
fn create_alert_tray_icon() -> tray_icon::Icon {
    const TRAY_ICON_ALERT_RGBA: &[u8] = include_bytes!("../assets/tray_icon_alert_32.rgba");
    tray_icon::Icon::from_rgba(TRAY_ICON_ALERT_RGBA.to_vec(), 32, 32)
        .expect("Failed to create Fluent ClockPause alert tray icon")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tray_icon_creators() {
        // Verify all 3 RGBA icons load valid 32x32 tray_icon::Icon instances without panic
        let _default_icon = create_default_tray_icon();
        let _paused_icon = create_paused_tray_icon();
        let _alert_icon = create_alert_tray_icon();
    }

    #[test]
    fn test_tray_action_variants() {
        assert_eq!(TrayAction::OpenSettings, TrayAction::OpenSettings);
        assert_ne!(TrayAction::OpenSettings, TrayAction::Quit);
    }
}

