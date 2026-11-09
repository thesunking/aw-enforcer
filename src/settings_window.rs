use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use slint::{ComponentHandle, Model, ModelRc, VecModel};
use tokio::sync::mpsc;
use tray_icon::menu::MenuItem;
use windows_sys::Win32::UI::WindowsAndMessaging::{SetForegroundWindow, ShowWindow, SW_RESTORE};

use crate::autostart;
use crate::aw_client::AwClient;
use crate::config::{
    self, AppConfig, DurationConfig, MonitorSelection, NotificationMethod, Rule, RuleAction,
    RuleSchedule, TimeUnit, TriggerType,
};
use crate::enforcer;
use crate::engine::DaemonCommand;
use crate::formatter::{self, MessageContext, TextHorizontalAlignment};
use crate::overlay::OverlayManager;
use crate::{RuleView, SettingsWindow};

/// Shared UI state and communication handles required by the `SettingsWindow` subsystem.
#[derive(Clone)]
pub struct SettingsContext {
    pub settings_slot: Rc<RefCell<Option<SettingsWindow>>>,
    pub rules_model: Rc<VecModel<RuleView>>,
    pub current_config: Rc<RefCell<AppConfig>>,
    pub ui_aw_client: Rc<RefCell<Arc<AwClient>>>,
    pub live_state: Rc<RefCell<LiveUiState>>,
    pub aw_connected_flag: Arc<std::sync::atomic::AtomicBool>,
    pub command_tx: mpsc::UnboundedSender<DaemonCommand>,
    pub rt_handle: tokio::runtime::Handle,
    pub item_pause: MenuItem,
    pub overlay_manager: Rc<RefCell<OverlayManager>>,
}

/// Cached live telemetry state so a newly created `SettingsWindow` immediately displays current daemon status.
pub struct LiveUiState {
    pub session_duration_text: String,
    pub screentime_text: String,
    pub active_app: String,
    pub strict_lock_active: bool,
    pub rule_statuses: Vec<(String, String)>,
    pub any_snoozed: bool,
    pub status_level: String,
    pub total_active_minutes: u32,
    pub target_minutes: u32,
    pub histogram_values: Vec<f32>,
    pub histogram_labels: Vec<String>,
    pub is_loading: bool,
}

impl Default for LiveUiState {
    fn default() -> Self {
        Self {
            session_duration_text: "0m".to_string(),
            screentime_text: "0h 0m".to_string(),
            active_app: "None".to_string(),
            strict_lock_active: false,
            rule_statuses: Vec::new(),
            any_snoozed: false,
            status_level: "Loading...".to_string(),
            total_active_minutes: 0,
            target_minutes: 480,
            histogram_values: Vec::new(),
            histogram_labels: Vec::new(),
            is_loading: true,
        }
    }
}

/// Lazily instantiates, populates, wires, and displays the `SettingsWindow` (or focuses it if already open).
/// When closed via the window's `'X'` button, the `SettingsWindow` instance and its GPU surface are dropped
/// if `destroy_windows_on_hide` is enabled in configuration; otherwise, it is kept hidden in memory.
pub fn open_settings_window(
    ctx: &SettingsContext,
    show: bool,
) -> Result<(), slint::PlatformError> {
    if let Some(ref existing) = *ctx.settings_slot.borrow() {
        return focus_existing_settings_window(existing, ctx, show);
    }

    let settings_win = SettingsWindow::new()?;
    initialize_settings_window(&settings_win, ctx);
    wire_all_settings_callbacks(&settings_win, ctx);
    wire_window_close_handler(&settings_win, ctx);

    if show {
        show_and_focus_window(&settings_win)?;
    }
    *ctx.settings_slot.borrow_mut() = Some(settings_win);
    Ok(())
}

/// Synchronizes telemetry and optionally shows and focuses an already-instantiated `SettingsWindow`.
fn focus_existing_settings_window(
    existing: &SettingsWindow,
    ctx: &SettingsContext,
    show: bool,
) -> Result<(), slint::PlatformError> {
    sync_live_state_to_ui(existing, &ctx.rules_model, &ctx.live_state.borrow());
    if show {
        scan_and_update_known_apps(existing.as_weak(), ctx.rt_handle.clone());
        spawn_aw_connection_check(
            existing.as_weak(),
            ctx.aw_connected_flag.clone(),
            ctx.ui_aw_client.borrow().clone(),
            &ctx.rt_handle,
        );
        show_and_focus_window(existing)?;
    }
    Ok(())
}

/// Populates initial configuration, cached live telemetry values, and triggers background connection/process checks.
fn initialize_settings_window(settings_win: &SettingsWindow, ctx: &SettingsContext) {
    let settings_weak = settings_win.as_weak();

    settings_win.set_rules(ModelRc::from(ctx.rules_model.clone()));
    apply_config_to_ui(settings_win, &ctx.current_config.borrow(), &ctx.rules_model);
    settings_win.set_aw_connected(
        ctx.aw_connected_flag
            .load(std::sync::atomic::Ordering::Relaxed),
    );
    sync_live_state_to_ui(settings_win, &ctx.rules_model, &ctx.live_state.borrow());

    scan_and_update_known_apps(settings_weak.clone(), ctx.rt_handle.clone());
    spawn_aw_connection_check(
        settings_weak,
        ctx.aw_connected_flag.clone(),
        ctx.ui_aw_client.borrow().clone(),
        &ctx.rt_handle,
    );
}

/// Wires all UI callbacks on `SettingsWindow` across general preferences, diagnostics, rule CRUD,
/// executable targets, schedule grids, and rich-text message editing.
fn wire_all_settings_callbacks(settings_win: &SettingsWindow, ctx: &SettingsContext) {
    wire_general_settings_callbacks(settings_win, ctx);
    wire_diagnostics_and_test_callbacks(settings_win, ctx);
    wire_rule_crud_callbacks(settings_win, ctx);
    wire_executable_target_callbacks(settings_win, ctx);
    wire_schedule_grid_callbacks(settings_win, ctx);
    wire_message_editor_callbacks(settings_win, ctx);
}

/// Wires the window close (`'X'`) event to either drop the window instance or reset unsaved changes on hide.
fn wire_window_close_handler(settings_win: &SettingsWindow, ctx: &SettingsContext) {
    let slot_for_close = ctx.settings_slot.clone();
    let rm_for_close = ctx.rules_model.clone();
    let cfg_for_close = ctx.current_config.clone();
    let live_state_for_close = ctx.live_state.clone();

    settings_win.window().on_close_requested(move || {
        let slot_inner = slot_for_close.clone();
        let rm_inner = rm_for_close.clone();
        if cfg_for_close.borrow().general.destroy_windows_on_hide {
            slint::Timer::single_shot(Duration::from_millis(0), move || {
                if let Some(win) = slot_inner.borrow_mut().take() {
                    win.hide().ok();
                    drop(win);
                }
                rm_inner.set_vec(Vec::new());
            });
        } else if let Some(win) = slot_inner.borrow().as_ref() {
            apply_config_to_ui(win, &cfg_for_close.borrow(), &rm_inner);
            sync_live_state_to_ui(win, &rm_inner, &live_state_for_close.borrow());
            win.set_strict_unlock_countdown(0);
            win.set_strict_unlocked(false);
        }
        slint::CloseRequestResponse::HideWindow
    });
}

/// Shows the `SettingsWindow` and restores/brings its native Win32 handle to the foreground.
fn show_and_focus_window(settings_win: &SettingsWindow) -> Result<(), slint::PlatformError> {
    settings_win.show()?;
    restore_and_focus_window(settings_win.window());
    Ok(())
}

/// Wires General Preferences callbacks: save, discard, export, import, and live hex color previews.
fn wire_general_settings_callbacks(settings_win: &SettingsWindow, ctx: &SettingsContext) {
    let settings_weak = settings_win.as_weak();

    // Save configuration
    {
        let sw = settings_weak.clone();
        let cmd_tx = ctx.command_tx.clone();
        let cfg_cell = ctx.current_config.clone();
        let ui_client_cell = ctx.ui_aw_client.clone();
        let item_pause = ctx.item_pause.clone();
        let overlay_mgr_cell = ctx.overlay_manager.clone();
        let rm = ctx.rules_model.clone();
        let live_state_cell = ctx.live_state.clone();

        settings_win.on_save_config(move || {
            if let Some(win) = sw.upgrade() {
                refresh_rule_regex_validation(&win, &rm);
                if !win.get_time_format_valid() || !win.get_rule_regex_valid() {
                    return;
                }
                let was_destroy = cfg_cell.borrow().general.destroy_windows_on_hide;
                let updated_config = config_from_ui(&win, &cfg_cell.borrow());

                // Ensure all rules in config with Titles or Urls monitoring have non-empty valid regex patterns
                for r in &updated_config.rules {
                    let (valid, err) = match r.monitor_selection {
                        MonitorSelection::Titles => {
                            validate_rule_pattern("Titles", &r.title_regex, "")
                        }
                        MonitorSelection::Urls => validate_rule_pattern("Urls", "", &r.url_regex),
                        MonitorSelection::Executables => (true, String::new()),
                    };
                    if !valid {
                        let name = if !r.name.trim().is_empty() {
                            r.name.as_str()
                        } else {
                            "Unnamed"
                        };
                        win.set_rule_regex_valid(false);
                        win.set_rule_regex_error(format!("Rule '{name}': {err}").into());
                        return;
                    }
                }

                let new_destroy = updated_config.general.destroy_windows_on_hide;
                if was_destroy != new_destroy {
                    overlay_mgr_cell
                        .borrow_mut()
                        .set_destroy_windows_on_hide(new_destroy);
                }
                item_pause.set_text(if updated_config.general.pause_all_rules {
                    "Unpause All Rules"
                } else {
                    "Pause All Rules"
                });

                apply_config_to_ui(&win, &updated_config, &rm);
                sync_live_state_to_ui(&win, &rm, &live_state_cell.borrow());

                // Persist config to disk
                if let Err(e) = updated_config.save() {
                    tracing::error!("Failed to save config: {e}");
                } else {
                    tracing::info!("Config saved successfully.");
                    win.set_has_unsaved_changes(false);
                }

                // Update Windows registry autostart
                if let Err(e) = autostart::set_autostart(updated_config.general.autostart) {
                    tracing::warn!("Failed to set Windows autostart: {e}");
                }

                *cfg_cell.borrow_mut() = updated_config.clone();
                if !ui_client_cell.borrow().matches_config(
                    updated_config.general.aw_port,
                    &updated_config.general.api_key,
                ) {
                    *ui_client_cell.borrow_mut() = Arc::new(AwClient::new(
                        updated_config.general.aw_port,
                        &updated_config.general.api_key,
                    ));
                }
                cmd_tx
                    .send(DaemonCommand::ReloadConfig(Box::new(updated_config)))
                    .ok();
            }
        });
    }

    // Color hex live preview
    {
        let sw = settings_weak.clone();
        settings_win.on_color_hex_edited(move |kind, val| {
            if let Some(win) = sw.upgrade() {
                set_color_preview(&win, &kind, &val);
            }
        });
    }

    // Discard configuration changes
    {
        let sw = settings_weak.clone();
        let cfg_cell = ctx.current_config.clone();
        let rm = ctx.rules_model.clone();
        let live_state_cell = ctx.live_state.clone();

        settings_win.on_discard_changes(move || {
            if let Some(win) = sw.upgrade() {
                let saved_config = cfg_cell.borrow().clone();
                apply_config_to_ui(&win, &saved_config, &rm);
                sync_live_state_to_ui(&win, &rm, &live_state_cell.borrow());
                tracing::info!("Reverted all settings to last saved configuration.");
            }
        });
    }

    // Export configuration
    {
        let sw = settings_weak.clone();
        let cfg_cell = ctx.current_config.clone();

        settings_win.on_export_config(move || {
            if let Some(win) = sw.upgrade() {
                let current_cfg = config_from_ui(&win, &cfg_cell.borrow());
                if let Some(path) = rfd::FileDialog::new()
                    .set_file_name("aw-enforcer-config.toml")
                    .add_filter("TOML Configuration", &["toml"])
                    .save_file()
                {
                    match current_cfg.save_to_path(&path) {
                        Ok(()) => tracing::info!("Exported configuration to {:?}", path),
                        Err(e) => tracing::error!("Failed to export config to {:?}: {e}", path),
                    }
                }
            }
        });
    }

    // Import configuration
    {
        let sw = settings_weak;
        let rm = ctx.rules_model.clone();
        let live_state_cell = ctx.live_state.clone();

        settings_win.on_import_config(move || {
            if let Some(win) = sw.upgrade() {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("TOML Configuration", &["toml"])
                    .pick_file()
                {
                    match AppConfig::load_from_path(&path) {
                        Ok(imported_cfg) => {
                            apply_config_to_ui(&win, &imported_cfg, &rm);
                            sync_live_state_to_ui(&win, &rm, &live_state_cell.borrow());
                            win.set_has_unsaved_changes(true);
                            tracing::info!(
                                "Imported configuration from {:?}; unsaved changes pending review.",
                                path
                            );
                        }
                        Err(e) => {
                            tracing::error!("Failed to import config from {:?}: {e}", path);
                        }
                    }
                }
            }
        });
    }
}

/// Wires diagnostics, preview, stats HUD, strftime validation, and strict-unlock callbacks.
fn wire_diagnostics_and_test_callbacks(settings_win: &SettingsWindow, ctx: &SettingsContext) {
    let settings_weak = settings_win.as_weak();

    // Test overlay
    {
        let cmd_tx = ctx.command_tx.clone();
        settings_win.on_test_overlay(move |mode| {
            cmd_tx
                .send(DaemonCommand::TriggerTestOverlay {
                    mode: mode.to_string(),
                })
                .ok();
        });
    }

    // Preview rule alert
    {
        let sw = settings_weak.clone();
        let cmd_tx = ctx.command_tx.clone();
        settings_win.on_preview_rule_alert(move || {
            if let Some(win) = sw.upgrade() {
                let cur_idx = win.get_selected_rule_idx();
                if cur_idx >= 0 {
                    let r_model = win.get_rules();
                    if let Some(v) = r_model.row_data(cur_idx as usize) {
                        let rule = view_to_rule(&v);
                        cmd_tx.send(DaemonCommand::PreviewRule(Box::new(rule))).ok();
                    }
                }
            }
        });
    }

    // Show activity stats
    {
        let cmd_tx = ctx.command_tx.clone();
        settings_win.on_show_activity_stats(move || {
            cmd_tx.send(DaemonCommand::ShowStatsOverlay).ok();
        });
    }

    // Validate time format code
    {
        let sw = settings_weak.clone();
        settings_win.on_validate_time_format(move |code| {
            if let Some(win) = sw.upgrade() {
                let (valid, preview) = config::validate_strftime_format(&code);
                win.set_time_format_valid(valid);
                win.set_time_format_preview(preview.as_str().into());
            }
        });
    }

    // Request strict unlock
    {
        let sw = settings_weak;
        settings_win.on_request_strict_unlock(move || {
            if let Some(win) = sw.upgrade() {
                win.set_strict_unlock_countdown(15);
            }
        });
    }
}

/// Wires rule list management callbacks: add, delete, duplicate, move, toggle, snooze, reset, and select.
fn wire_rule_crud_callbacks(settings_win: &SettingsWindow, ctx: &SettingsContext) {
    let settings_weak = settings_win.as_weak();

    // Add rule
    {
        let rm = ctx.rules_model.clone();
        let sw = settings_weak.clone();
        settings_win.on_add_rule(move || {
            if let Some(win) = sw.upgrade() {
                if is_ui_locked_by_strict(&win) {
                    return;
                }
                let rule_id = next_available_rule_id(&rm);
                let num: usize = rule_id.trim_start_matches("rule-").parse().unwrap_or(1);
                let new_rule = Rule::default_template(num.saturating_sub(1));
                let rv = rule_to_view(&new_rule);
                let idx = rm.row_count();
                rm.push(rv.clone());
                win.set_selected_rule_idx(idx as i32);
                load_rule_into_editor(&win, &rv, &rm);
                win.set_has_unsaved_changes(true);
            }
        });
    }

    // Delete rule
    {
        let rm = ctx.rules_model.clone();
        let sw = settings_weak.clone();
        settings_win.on_delete_rule(move |idx| {
            if let Some(win) = sw.upgrade() {
                if is_ui_locked_by_strict(&win) {
                    return;
                }
                let u = idx as usize;
                if u < rm.row_count() {
                    rm.remove(u);
                    let cur = win.get_selected_rule_idx();
                    if cur == idx {
                        if rm.row_count() > 0 {
                            let new_idx = if u < rm.row_count() {
                                u
                            } else {
                                rm.row_count() - 1
                            };
                            win.set_selected_rule_idx(new_idx as i32);
                            if let Some(rule) = rm.row_data(new_idx) {
                                load_rule_into_editor(&win, &rule, &rm);
                            }
                        } else {
                            win.set_selected_rule_idx(-1);
                            win.set_editor_message_text("".into());
                            refresh_rule_regex_validation(&win, &rm);
                        }
                    } else if cur > idx {
                        win.set_selected_rule_idx(cur - 1);
                    }
                    refresh_rule_regex_validation(&win, &rm);
                    win.set_has_unsaved_changes(true);
                }
            }
        });
    }

    // Duplicate rule
    {
        let rm = ctx.rules_model.clone();
        let sw = settings_weak.clone();
        settings_win.on_duplicate_rule(move |idx| {
            if let Some(win) = sw.upgrade() {
                if is_ui_locked_by_strict(&win) {
                    return;
                }
                let u = idx as usize;
                if u < rm.row_count() {
                    if let Some(target) = rm.row_data(u) {
                        let rule_id = next_available_rule_id(&rm);
                        let target_rule = view_to_rule(&target);
                        let duplicated = target_rule.duplicate(rule_id);
                        let rv = rule_to_view(&duplicated);
                        let new_idx = rm.row_count();
                        rm.push(rv.clone());
                        win.set_selected_rule_idx(new_idx as i32);
                        load_rule_into_editor(&win, &rv, &rm);
                        win.set_has_unsaved_changes(true);
                    }
                }
            }
        });
    }

    // Move rule (reorder)
    {
        let rm = ctx.rules_model.clone();
        let sw = settings_weak.clone();
        settings_win.on_move_rule(move |from_idx, to_idx| {
            if let Some(win) = sw.upgrade() {
                if is_ui_locked_by_strict(&win) {
                    return;
                }
                if from_idx < 0 || to_idx < 0 {
                    return;
                }
                let from_u = from_idx as usize;
                let to_u = to_idx as usize;
                let count = rm.row_count();
                if from_u < count && to_u < count && from_u != to_u {
                    if let (Some(r1), Some(r2)) = (rm.row_data(from_u), rm.row_data(to_u)) {
                        rm.set_row_data(from_u, r2);
                        rm.set_row_data(to_u, r1);

                        let cur = win.get_selected_rule_idx();
                        if cur == from_idx {
                            win.set_selected_rule_idx(to_idx);
                        } else if cur == to_idx {
                            win.set_selected_rule_idx(from_idx);
                        }

                        refresh_rule_regex_validation(&win, &rm);
                        win.set_has_unsaved_changes(true);
                    }
                }
            }
        });
    }

    // Toggle rule enabled (from overview list)
    {
        let rm = ctx.rules_model.clone();
        let sw = settings_weak.clone();
        settings_win.on_toggle_rule_enabled(move |idx, enabled| {
            if let Some(win) = sw.upgrade() {
                if is_ui_locked_by_strict(&win) {
                    return;
                }
                let u = idx as usize;
                if u < rm.row_count() {
                    if let Some(mut rule) = rm.row_data(u) {
                        rule.enabled = enabled;
                        rm.set_row_data(u, rule);
                        if win.get_selected_rule_idx() == idx {
                            win.set_rule_enabled(enabled);
                        }
                        refresh_rule_regex_validation(&win, &rm);
                        win.set_has_unsaved_changes(true);
                    }
                }
            }
        });
    }

    // Snooze rule
    {
        let rm = ctx.rules_model.clone();
        let cmd_tx = ctx.command_tx.clone();
        settings_win.on_snooze_rule(move |idx, minutes| {
            let u = idx as usize;
            if u < rm.row_count() {
                if let Some(r) = rm.row_data(u) {
                    let rule_id = r.id.to_string();
                    let mins = if minutes > 0 { minutes as u32 } else { 15 };
                    cmd_tx
                        .send(DaemonCommand::SnoozeBreak {
                            rule_id,
                            minutes: mins,
                        })
                        .ok();
                }
            }
        });
    }

    // Reset rule session
    {
        let rm = ctx.rules_model.clone();
        let cmd_tx = ctx.command_tx.clone();
        settings_win.on_reset_rule_session(move |idx| {
            let u = idx as usize;
            if u < rm.row_count() {
                if let Some(r) = rm.row_data(u) {
                    let rule_id = r.id.to_string();
                    cmd_tx
                        .send(DaemonCommand::ResetRuleSession { rule_id })
                        .ok();
                }
            }
        });
    }

    // Select rule
    {
        let rm = ctx.rules_model.clone();
        let sw = settings_weak.clone();
        settings_win.on_select_rule(move |idx| {
            if let Some(win) = sw.upgrade() {
                let u = idx as usize;
                if idx >= 0 && u < rm.row_count() {
                    win.set_selected_rule_idx(idx);
                    if let Some(rule) = rm.row_data(u) {
                        load_rule_into_editor(&win, &rule, &rm);
                    }
                } else if idx < 0 {
                    win.set_selected_rule_idx(-1);
                    refresh_rule_regex_validation(&win, &rm);
                }
            }
        });
    }

    // Toggle pause all rules
    {
        let cfg_cell = ctx.current_config.clone();
        let cmd_tx = ctx.command_tx.clone();
        let item_pause = ctx.item_pause.clone();
        let sw = settings_weak;
        settings_win.on_toggle_pause_all(move || {
            if let Some(win) = sw.upgrade() {
                if is_ui_locked_by_strict(&win) {
                    return;
                }
                let new_paused = !win.get_pause_all_rules();
                win.set_pause_all_rules(new_paused);
                {
                    let mut cfg = cfg_cell.borrow_mut();
                    cfg.general.pause_all_rules = new_paused;
                    if let Err(e) = cfg.save() {
                        tracing::error!("Failed to save config on pause toggle: {e}");
                    }
                }
                item_pause.set_text(if new_paused {
                    "Unpause All Rules"
                } else {
                    "Pause All Rules"
                });
                cmd_tx
                    .send(DaemonCommand::SetPauseAll { paused: new_paused })
                    .ok();
            }
        });
    }
}

/// Wires active rule field edits and executable target management callbacks.
fn wire_executable_target_callbacks(settings_win: &SettingsWindow, ctx: &SettingsContext) {
    let settings_weak = settings_win.as_weak();

    // Active rule edited
    {
        let rm = ctx.rules_model.clone();
        let sw = settings_weak.clone();
        settings_win.on_active_rule_edited(move || {
            if let Some(win) = sw.upgrade() {
                if is_ui_locked_by_strict(&win) {
                    return;
                }
                let idx = win.get_selected_rule_idx();
                let u = idx as usize;
                if idx >= 0 && u < rm.row_count() {
                    if let Some(mut rule) = rm.row_data(u) {
                        sync_editor_to_rule_view(&win, &mut rule);

                        let has_specific_exe = rule.monitor_selection.as_str() == "Executables"
                            && first_specific_executable(&rule.target_executables).is_some();
                        if !has_specific_exe && rule.action.as_str() == "Kill Process" {
                            rule.action = "Close Window".into();
                            win.set_rule_action(rule.action.clone());
                        }

                        let cur_line = win.get_selected_message_line() as usize;
                        refresh_message_editor_state(&win, &rule.message, cur_line, Some(&rule));
                        win.set_warn_before_exceeds_threshold(
                            check_warn_before_exceeds_threshold(&rule),
                        );
                        rm.set_row_data(u, rule);
                        refresh_rule_regex_validation(&win, &rm);
                        win.set_has_unsaved_changes(true);
                    }
                }
            }
        });
    }

    // Native file dialog browse executable
    {
        let sw = settings_weak.clone();
        let rm = ctx.rules_model.clone();

        settings_win.on_browse_executable(move || {
            if let Some(win) = sw.upgrade() {
                if is_ui_locked_by_strict(&win) {
                    return;
                }
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("Executables", &["exe"])
                    .pick_file()
                {
                    if let Some(filename) =
                        path.file_name().map(|f| f.to_string_lossy().to_string())
                    {
                        add_executable_to_rule(&rm, &sw, win.get_selected_rule_idx(), &filename);
                    }
                }
            }
        });
    }

    // Refresh known apps using shared Tokio runtime handle
    {
        let sw = settings_weak.clone();
        let handle = ctx.rt_handle.clone();

        settings_win.on_refresh_known_apps(move || {
            scan_and_update_known_apps(sw.clone(), handle.clone());
        });
    }

    // Add known app to rule
    {
        let rm = ctx.rules_model.clone();
        let sw = settings_weak.clone();
        settings_win.on_add_known_app_to_rule(move |rule_idx, app| {
            if let Some(win) = sw.upgrade() {
                if is_ui_locked_by_strict(&win) {
                    return;
                }
                add_executable_to_rule(&rm, &sw, rule_idx, app.as_str());
            }
        });
    }

    // Remove executable from rule
    {
        let rm = ctx.rules_model.clone();
        let sw = settings_weak;
        settings_win.on_remove_executable_from_rule(move |rule_idx, exe_idx| {
            if let Some(win) = sw.upgrade() {
                if is_ui_locked_by_strict(&win) {
                    return;
                }
            }
            let e_idx = exe_idx as usize;
            update_rule_at(&rm, &sw, rule_idx, |rule_view, win, rm| {
                let mut current_targets: Vec<slint::SharedString> =
                    rule_view.target_executables.iter().collect();
                if e_idx < current_targets.len() {
                    current_targets.remove(e_idx);
                    rule_view.target_executables =
                        ModelRc::from(Rc::new(VecModel::from(current_targets)));
                    if first_specific_executable(&rule_view.target_executables).is_none()
                        && rule_view.action.as_str() == "Kill Process"
                    {
                        rule_view.action = "Close Window".into();
                    }
                    load_rule_into_editor(win, rule_view, rm);
                    return true;
                }
                false
            });
        });
    }
}

/// Wires weekly 7x24 schedule grid callbacks: slot, day, hour toggles, and one-click presets.
fn wire_schedule_grid_callbacks(settings_win: &SettingsWindow, ctx: &SettingsContext) {
    let settings_weak = settings_win.as_weak();

    // Toggle schedule slot
    {
        let rm = ctx.rules_model.clone();
        let sw = settings_weak.clone();
        settings_win.on_toggle_schedule_slot(move |rule_idx, day, hour, state| {
            if let Some(win) = sw.upgrade() {
                if is_ui_locked_by_strict(&win) {
                    return;
                }
            }
            let slot_idx = (day as usize) * 24 + (hour as usize);
            if slot_idx < 168 {
                update_rule_schedule(&rm, &sw, rule_idx, |slots| {
                    slots[slot_idx] = state;
                });
            }
        });
    }

    // Toggle schedule day
    {
        let rm = ctx.rules_model.clone();
        let sw = settings_weak.clone();
        settings_win.on_toggle_schedule_day(move |rule_idx, day| {
            if let Some(win) = sw.upgrade() {
                if is_ui_locked_by_strict(&win) {
                    return;
                }
            }
            let d = day as usize;
            if d < 7 {
                let start = d * 24;
                let end = start + 24;
                update_rule_schedule(&rm, &sw, rule_idx, |slots| {
                    let all_on = slots[start..end].iter().all(|&s| s);
                    for s in &mut slots[start..end] {
                        *s = !all_on;
                    }
                });
            }
        });
    }

    // Toggle schedule hour
    {
        let rm = ctx.rules_model.clone();
        let sw = settings_weak.clone();
        settings_win.on_toggle_schedule_hour(move |rule_idx, hour| {
            if let Some(win) = sw.upgrade() {
                if is_ui_locked_by_strict(&win) {
                    return;
                }
            }
            let h = hour as usize;
            if h < 24 {
                update_rule_schedule(&rm, &sw, rule_idx, |slots| {
                    let all_on = (0..7).all(|d| slots[d * 24 + h]);
                    for d in 0..7 {
                        slots[d * 24 + h] = !all_on;
                    }
                });
            }
        });
    }

    // Apply schedule preset
    {
        let rm = ctx.rules_model.clone();
        let sw = settings_weak;
        settings_win.on_apply_schedule_preset(move |rule_idx, preset| {
            if let Some(win) = sw.upgrade() {
                if is_ui_locked_by_strict(&win) {
                    return;
                }
            }
            update_rule_schedule(&rm, &sw, rule_idx, |slots| {
                if let Some(schedule) = RuleSchedule::from_preset(preset.as_str()) {
                    slots.copy_from_slice(&schedule.to_bool_array());
                }
            });
        });
    }
}

/// Wires rich-text message editor callbacks: line selection, text editing, style toggling,
/// placeholder insertion, and live preview rendering.
fn wire_message_editor_callbacks(settings_win: &SettingsWindow, ctx: &SettingsContext) {
    let settings_weak = settings_win.as_weak();

    // Message editor: select line
    {
        let sw = settings_weak.clone();
        let rm = ctx.rules_model.clone();
        settings_win.on_select_message_line(move |line_idx| {
            if let Some(win) = sw.upgrade() {
                let rule_idx = win.get_selected_rule_idx() as usize;
                if rule_idx < rm.row_count() {
                    if let Some(rule) = rm.row_data(rule_idx) {
                        refresh_message_editor_state(
                            &win,
                            &rule.message,
                            line_idx as usize,
                            Some(&rule),
                        );
                    }
                }
            }
        });
    }

    // Message editor: text edited
    {
        let sw = settings_weak.clone();
        let rm = ctx.rules_model.clone();
        settings_win.on_message_text_edited(move |text| {
            if let Some(win) = sw.upgrade() {
                if is_ui_locked_by_strict(&win) {
                    return;
                }
                let rule_idx = win.get_selected_rule_idx();
                let text_clone = text.clone();
                update_rule_at(&rm, &sw, rule_idx, |rule, win, _| {
                    rule.message = text_clone.clone();
                    let cur_line = win.get_selected_message_line() as usize;
                    refresh_message_editor_state(win, &text_clone, cur_line, Some(rule));
                    true
                });
            }
        });
    }

    // Message editor: apply line style (toggle)
    {
        let sw = settings_weak.clone();
        let rm = ctx.rules_model.clone();
        settings_win.on_apply_line_style(move |line_idx, style| {
            if let Some(win) = sw.upgrade() {
                if is_ui_locked_by_strict(&win) {
                    return;
                }
                let rule_idx = win.get_selected_rule_idx();
                update_rule_at(&rm, &sw, rule_idx, |rule, win, _| {
                    let updated =
                        formatter::apply_line_style(&rule.message, line_idx as usize, &style);
                    rule.message = updated.as_str().into();
                    refresh_message_editor_state(win, &updated, line_idx as usize, Some(rule));
                    true
                });
            }
        });
    }

    // Message editor: append placeholder
    {
        let sw = settings_weak.clone();
        let rm = ctx.rules_model.clone();
        settings_win.on_append_placeholder(move |line_idx, placeholder| {
            if let Some(win) = sw.upgrade() {
                if is_ui_locked_by_strict(&win) {
                    return;
                }
                let rule_idx = win.get_selected_rule_idx();
                update_rule_at(&rm, &sw, rule_idx, |rule, win, _| {
                    let updated = formatter::append_placeholder(
                        &rule.message,
                        line_idx as usize,
                        &placeholder,
                    );
                    rule.message = updated.as_str().into();
                    refresh_message_editor_state(win, &updated, line_idx as usize, Some(rule));
                    true
                });
            }
        });
    }

    // Update message preview live
    {
        let sw = settings_weak;
        let rm = ctx.rules_model.clone();
        settings_win.on_update_message_preview(move |rule_idx| {
            if let Some(win) = sw.upgrade() {
                let r_idx = rule_idx as usize;
                if r_idx < rm.row_count() {
                    if let Some(rule) = rm.row_data(r_idx) {
                        let cur_line = win.get_selected_message_line() as usize;
                        refresh_message_editor_state(&win, &rule.message, cur_line, Some(&rule));
                    }
                }
            }
        });
    }
}

/// Restores and brings a Slint window to the foreground via native Win32 APIs.
pub fn restore_and_focus_window(window: &slint::Window) {
    if let Some(hwnd) = OverlayManager::extract_window_hwnd(window) {
        unsafe {
            ShowWindow(hwnd, SW_RESTORE);
            SetForegroundWindow(hwnd);
        }
    }
}

/// Asynchronously checks the ActivityWatch connection and updates the UI property and atomic flag.
pub fn spawn_aw_connection_check(
    sw: slint::Weak<SettingsWindow>,
    conn_flag: Arc<std::sync::atomic::AtomicBool>,
    client: Arc<AwClient>,
    handle: &tokio::runtime::Handle,
) {
    handle.spawn(async move {
        let connected = client.check_connection().await;
        conn_flag.store(connected, std::sync::atomic::Ordering::Relaxed);
        slint::invoke_from_event_loop(move || {
            if let Some(win) = sw.upgrade() {
                win.set_aw_connected(connected);
            }
        })
        .ok();
    });
}

/// Asynchronously scans local running processes (<15ms) and updates known apps with only actively running executables.
pub fn scan_and_update_known_apps(
    sw: slint::Weak<SettingsWindow>,
    handle: tokio::runtime::Handle,
) {
    handle.spawn(async move {
        let mut app_set = std::collections::BTreeSet::new();

        enforcer::for_each_process(|_pid, name| {
            if name.len() > 4 && name[name.len() - 4..].eq_ignore_ascii_case(".exe") {
                app_set.insert(name.to_string());
            }
        });

        let app_vec: Vec<slint::SharedString> =
            app_set.into_iter().map(|s| s.as_str().into()).collect();
        slint::invoke_from_event_loop(move || {
            if let Some(win) = sw.upgrade() {
                win.set_known_apps(ModelRc::from(Rc::new(VecModel::from(app_vec))));
            }
        })
        .ok();
    });
}

/// Synchronizes cached live daemon telemetry and rule statuses to the `SettingsWindow` and its rules model.
pub fn sync_live_state_to_ui(
    win: &SettingsWindow,
    rules_model: &VecModel<RuleView>,
    ls: &LiveUiState,
) {
    win.set_current_session_duration_text(ls.session_duration_text.as_str().into());
    win.set_today_screentime_text(ls.screentime_text.as_str().into());
    win.set_current_active_app(ls.active_app.as_str().into());
    win.set_strict_lock_active(ls.strict_lock_active);
    win.set_status_level(ls.status_level.as_str().into());
    win.set_total_active_minutes(ls.total_active_minutes as i32);
    win.set_target_minutes(ls.target_minutes as i32);
    win.set_is_loading(ls.is_loading);
    win.set_histogram_values(ModelRc::from(Rc::new(VecModel::from(
        ls.histogram_values.clone(),
    ))));
    let labels: Vec<slint::SharedString> = ls
        .histogram_labels
        .iter()
        .map(|s| s.as_str().into())
        .collect();
    win.set_histogram_labels(ModelRc::from(Rc::new(VecModel::from(labels))));
    if !ls.strict_lock_active {
        win.set_strict_unlocked(false);
        win.set_strict_unlock_countdown(0);
    }
    update_rules_model_statuses(rules_model, &ls.rule_statuses);
}

/// Updates the status text of matching rules in `rules_model` if their text changed.
pub fn update_rules_model_statuses(
    rules_model: &VecModel<RuleView>,
    statuses: &[(String, String)],
) {
    for (rule_id, status_text) in statuses {
        for idx in 0..rules_model.row_count() {
            if let Some(mut r) = rules_model.row_data(idx) {
                if r.id == rule_id && r.status_text.as_str() != status_text.as_str() {
                    r.status_text = status_text.as_str().into();
                    rules_model.set_row_data(idx, r);
                }
            }
        }
    }
}

/// Synchronizes all `SettingsWindow` properties and the `rules_model` with the given `AppConfig`.
pub fn apply_config_to_ui(
    win: &SettingsWindow,
    cfg: &AppConfig,
    rules_model: &VecModel<RuleView>,
) {
    win.set_aw_port(cfg.general.aw_port as i32);
    win.set_poll_interval_seconds(cfg.general.poll_interval_seconds as i32);
    win.set_autostart(cfg.general.autostart);
    win.set_api_key(cfg.general.api_key.as_str().into());
    win.set_day_start_hour(cfg.general.day_start_hour as i32);
    win.set_suppress_fullscreen_d3d(cfg.general.suppress_fullscreen_d3d);
    win.set_pause_all_rules(cfg.general.pause_all_rules);
    win.set_destroy_windows_on_hide(cfg.general.destroy_windows_on_hide);
    win.set_slint_renderer(cfg.general.slint_renderer.as_str().into());
    let renderer_display = if cfg.general.slint_renderer.eq_ignore_ascii_case("software") {
        "CPU Software (renderer-software)"
    } else {
        "OpenGL (renderer-femtovg)"
    };
    win.set_slint_renderer_display(renderer_display.into());
    win.set_overlay_opacity(cfg.overlay.opacity);
    win.set_accent_color_hex(cfg.overlay.accent_color.as_str().into());
    win.set_background_color_hex(cfg.overlay.background_color.as_str().into());
    win.set_text_color_hex(cfg.overlay.text_color.as_str().into());
    set_color_preview(win, "accent", &cfg.overlay.accent_color);
    set_color_preview(win, "background", &cfg.overlay.background_color);
    set_color_preview(win, "text", &cfg.overlay.text_color);
    win.set_daily_target_minutes(cfg.stats.daily_screentime_target_minutes as i32);
    win.set_threshold_excessive_minutes(cfg.stats.threshold_excessive_minutes as i32);
    win.set_histogram_bin_size_minutes(cfg.stats.histogram_bin_size_minutes as i32);
    win.set_histogram_bin_count(cfg.stats.histogram_bin_count as i32);
    win.set_time_format_code(cfg.stats.histogram_time_format.as_str().into());

    let (valid, preview) = config::validate_strftime_format(&cfg.stats.histogram_time_format);
    win.set_time_format_valid(valid);
    win.set_time_format_preview(preview.as_str().into());

    let rules_vec: Vec<RuleView> = cfg.rules.iter().map(rule_to_view).collect();
    rules_model.set_vec(rules_vec);

    let cur_idx = win.get_selected_rule_idx();
    let safe_idx = if cur_idx >= 0 && (cur_idx as usize) < cfg.rules.len() {
        cur_idx as usize
    } else if !cfg.rules.is_empty() {
        if cur_idx >= 0 {
            win.set_selected_rule_idx(0);
        }
        0
    } else {
        win.set_selected_rule_idx(-1);
        0
    };

    if let Some(r) = cfg.rules.get(safe_idx) {
        let rv = rule_to_view(r);
        load_rule_into_editor(win, &rv, rules_model);
    } else {
        refresh_rule_regex_validation(win, rules_model);
    }
    win.set_has_unsaved_changes(false);
}

/// Extracts the current configuration from `SettingsWindow` and `rules_model`, applying safety sanitization.
pub fn config_from_ui(win: &SettingsWindow, base_cfg: &AppConfig) -> AppConfig {
    let mut updated_config = base_cfg.clone();

    updated_config.general.aw_port = win.get_aw_port().clamp(1, 65535) as u16;
    updated_config.general.poll_interval_seconds =
        win.get_poll_interval_seconds().max(2) as u64;
    updated_config.general.autostart = win.get_autostart();
    updated_config.general.api_key = win.get_api_key().to_string();
    updated_config.general.day_start_hour = win.get_day_start_hour().clamp(0, 23) as u32;
    updated_config.general.suppress_fullscreen_d3d = win.get_suppress_fullscreen_d3d();
    updated_config.general.pause_all_rules = win.get_pause_all_rules();
    updated_config.general.destroy_windows_on_hide = win.get_destroy_windows_on_hide();
    updated_config.general.slint_renderer = win.get_slint_renderer().to_string();

    updated_config.overlay.opacity = win.get_overlay_opacity().clamp(0.1, 1.0);
    updated_config.overlay.accent_color = win.get_accent_color_hex().to_string();
    updated_config.overlay.background_color = win.get_background_color_hex().to_string();
    updated_config.overlay.text_color = win.get_text_color_hex().to_string();

    updated_config.stats.daily_screentime_target_minutes =
        win.get_daily_target_minutes().max(1) as u32;
    let raw_excessive = win.get_threshold_excessive_minutes().max(1) as u32;
    let clamped_excessive =
        raw_excessive.max(updated_config.stats.daily_screentime_target_minutes);
    updated_config.stats.threshold_excessive_minutes = clamped_excessive;
    updated_config.stats.histogram_bin_size_minutes =
        win.get_histogram_bin_size_minutes().clamp(1, 1440) as u32;
    updated_config.stats.histogram_bin_count =
        win.get_histogram_bin_count().clamp(1, 96) as u32;
    updated_config.stats.histogram_time_format =
        win.get_time_format_code().to_string();

    // Convert rules
    let r_model = win.get_rules();
    let mut rules = Vec::new();
    for i in 0..r_model.row_count() {
        if let Some(v) = r_model.row_data(i) {
            rules.push(view_to_rule(&v));
        }
    }
    updated_config.rules = rules;
    updated_config.sanitize();

    updated_config
}

pub fn load_rule_into_editor(win: &SettingsWindow, rule: &RuleView, rm: &VecModel<RuleView>) {
    win.set_rule_enabled(rule.enabled);
    win.set_rule_name(rule.name.clone());
    win.set_rule_monitor_selection(rule.monitor_selection.clone());
    win.set_rule_title_regex(rule.title_regex.clone());
    win.set_rule_url_regex(rule.url_regex.clone());
    win.set_rule_trigger(rule.trigger.clone());
    win.set_rule_period_val(rule.period_val);
    win.set_rule_period_unit(rule.period_unit.clone());
    win.set_rule_reset_afk_val(rule.reset_afk_val);
    win.set_rule_reset_afk_unit(rule.reset_afk_unit.clone());
    win.set_rule_limit_val(rule.limit_val);
    win.set_rule_limit_unit(rule.limit_unit.clone());
    win.set_rule_action(rule.action.clone());
    win.set_rule_notification_method(rule.notification_method.clone());
    win.set_rule_notification_duration_val(rule.notification_duration_val);
    win.set_rule_notification_duration_unit(rule.notification_duration_unit.clone());
    win.set_rule_show_remaining_time(rule.show_remaining_time);
    win.set_rule_warn_before_val(rule.warn_before_val);
    win.set_rule_warn_before_unit(rule.warn_before_unit.clone());
    win.set_rule_grace_val(rule.grace_val);
    win.set_rule_grace_unit(rule.grace_unit.clone());
    win.set_rule_break_val(rule.break_val);
    win.set_rule_break_unit(rule.break_unit.clone());
    win.set_rule_click_through(rule.click_through);
    win.set_rule_strict_mode(rule.strict_mode);
    win.set_editor_message_text(rule.message.clone());
    refresh_message_editor_state(win, &rule.message, 0, Some(rule));
    win.set_warn_before_exceeds_threshold(check_warn_before_exceeds_threshold(rule));
    refresh_rule_regex_validation(win, rm);
}

/// Extracts all editable rule settings from the `SettingsWindow` into the provided `RuleView`.
pub fn sync_editor_to_rule_view(win: &SettingsWindow, rule: &mut RuleView) {
    rule.enabled = win.get_rule_enabled();
    rule.name = win.get_rule_name();
    rule.monitor_selection = win.get_rule_monitor_selection();
    rule.title_regex = win.get_rule_title_regex();
    rule.url_regex = win.get_rule_url_regex();
    rule.trigger = win.get_rule_trigger();
    rule.period_val = win.get_rule_period_val();
    rule.period_unit = win.get_rule_period_unit();
    rule.reset_afk_val = win.get_rule_reset_afk_val();
    rule.reset_afk_unit = win.get_rule_reset_afk_unit();
    rule.limit_val = win.get_rule_limit_val();
    rule.limit_unit = win.get_rule_limit_unit();
    rule.action = win.get_rule_action();
    rule.notification_method = win.get_rule_notification_method();
    rule.notification_duration_val = win.get_rule_notification_duration_val();
    rule.notification_duration_unit = win.get_rule_notification_duration_unit();
    rule.show_remaining_time = win.get_rule_show_remaining_time();
    rule.warn_before_val = win.get_rule_warn_before_val();
    rule.warn_before_unit = win.get_rule_warn_before_unit();
    rule.grace_val = win.get_rule_grace_val();
    rule.grace_unit = win.get_rule_grace_unit();
    rule.break_val = win.get_rule_break_val();
    rule.break_unit = win.get_rule_break_unit();
    rule.click_through = win.get_rule_click_through();
    rule.strict_mode = win.get_rule_strict_mode();
}

/// Refreshes the message editor UI state (line count, active selection, toggle highlights, color/size dropdowns, and preview).
pub fn refresh_message_editor_state(
    win: &SettingsWindow,
    message: &str,
    line_idx: usize,
    rule: Option<&RuleView>,
) {
    let line_count = message.split('\n').count().max(1);
    let line_indices: Vec<i32> = (1..=(line_count as i32)).collect();
    win.set_message_lines(ModelRc::from(Rc::new(VecModel::from(line_indices))));

    let active_line = line_idx.min(line_count - 1);
    win.set_selected_message_line(active_line as i32);

    let status = formatter::detect_line_formatting(message, active_line);
    win.set_line_is_h1(status.is_h1);
    win.set_line_is_h2(status.is_h2);
    win.set_line_is_bold(status.is_bold);
    win.set_line_is_italic(status.is_italic);
    win.set_line_is_underline(status.is_underline);
    win.set_line_color(status.color_code.as_str().into());
    win.set_line_size(status.size_val as i32);

    win.set_line_color_display(formatter::color_code_to_display(&status.color_code).into());
    let size_disp = if status.size_val > 0 {
        format!("Size: {}px", status.size_val)
    } else {
        "Size: Default".to_string()
    };
    win.set_line_size_display(size_disp.into());

    // Context for live preview interpolation
    let real_ctx = if let Some(r) = rule {
        let domain_rule = view_to_rule(r);
        formatter::build_preview_context(&domain_rule)
    } else {
        MessageContext::sample()
    };
    let alignment = if rule.is_some_and(|r| r.action.as_str() == "Show Notification") {
        TextHorizontalAlignment::Left
    } else {
        TextHorizontalAlignment::Center
    };
    let paras = formatter::parse_formatted_text_with_context_and_alignment(
        message,
        &real_ctx,
        alignment,
    );
    win.set_preview_paragraphs(ModelRc::from(Rc::new(VecModel::from(paras))));

    if win.get_editor_message_text().as_str() != message {
        win.set_editor_message_text(message.into());
    }
}

/// Updates the preview color box for accent, background, or text colors if valid hex.
pub fn set_color_preview(win: &SettingsWindow, kind: &str, hex: &str) {
    if let Some(c) = formatter::parse_hex_color(hex) {
        match kind {
            "accent" => win.set_accent_color_preview(c),
            "background" => win.set_background_color_preview(c),
            "text" => win.set_text_color_preview(c),
            _ => {}
        }
    }
}

/// Validates regular expressions for the currently selected rule (and any other rule in `rm`) if Title or URL monitoring is selected.
/// When all rules have valid regex patterns, sets `rule-regex-valid = true` and `rule-regex-error = ""`.
pub fn refresh_rule_regex_validation(win: &SettingsWindow, rm: &VecModel<RuleView>) {
    let cur_idx = win.get_selected_rule_idx();
    let (mut valid, mut err) = if cur_idx >= 0 {
        let mode = win.get_rule_monitor_selection();
        match mode.as_str() {
            "Titles" => validate_rule_pattern("Titles", &win.get_rule_title_regex(), ""),
            "Urls" => validate_rule_pattern("Urls", "", &win.get_rule_url_regex()),
            _ => (true, String::new()),
        }
    } else {
        (true, String::new())
    };

    if valid {
        for i in 0..rm.row_count() {
            if cur_idx >= 0 && i == cur_idx as usize {
                continue;
            }
            if let Some(r) = rm.row_data(i) {
                let (r_valid, r_err) = validate_rule_pattern(
                    r.monitor_selection.as_str(),
                    r.title_regex.as_str(),
                    r.url_regex.as_str(),
                );
                if !r_valid {
                    let name = if !r.name.trim().is_empty() {
                        r.name.to_string()
                    } else {
                        format!("Rule {}", i + 1)
                    };
                    valid = false;
                    err = format!("Rule '{name}': {r_err}");
                    break;
                }
            }
        }
    }

    win.set_rule_regex_valid(valid);
    win.set_rule_regex_error(if valid { "".into() } else { err.into() });
}

/// Validates regex pattern for a given monitor selection mode, rejecting empty patterns for Titles and Urls.
pub fn validate_rule_pattern(mode: &str, title_re: &str, url_re: &str) -> (bool, String) {
    match mode {
        "Titles" => {
            if title_re.trim().is_empty() {
                (false, "Title regex pattern cannot be empty".to_string())
            } else {
                config::validate_regex_pattern(title_re)
            }
        }
        "Urls" => {
            if url_re.trim().is_empty() {
                (false, "URL regex pattern cannot be empty".to_string())
            } else {
                config::validate_regex_pattern(url_re)
            }
        }
        _ => (true, String::new()),
    }
}

/// Loads all editable fields of `rule` into the top-level two-way bound properties of `SettingsWindow`.
/// Returns `true` if `warn_before` is configured (> 0) and is greater than or equal to the rule's trigger threshold.
pub fn check_warn_before_exceeds_threshold(rule_view: &RuleView) -> bool {
    let domain_rule = view_to_rule(rule_view);
    domain_rule.warn_before.to_seconds() > 0
        && domain_rule.warn_before.to_seconds() >= domain_rule.trigger_threshold_seconds()
}

/// Returns true if UI controls should be locked due to active strict break enforcement without an unlock countdown.
pub fn is_ui_locked_by_strict(win: &SettingsWindow) -> bool {
    win.get_strict_lock_active() && !win.get_strict_unlocked()
}

/// Finds the next available non-conflicting `rule-{n}` identifier.
pub fn next_available_rule_id(rm: &VecModel<RuleView>) -> String {
    let mut n = rm.row_count() + 1;
    while (0..rm.row_count()).any(|i| {
        rm.row_data(i)
            .map(|r| r.id.as_str() == format!("rule-{n}"))
            .unwrap_or(false)
    }) {
        n += 1;
    }
    format!("rule-{n}")
}

/// Mutates the `RuleView` at `rule_idx` inside `rm` if `mutator` returns `true`, marking the UI as having unsaved changes.
pub fn update_rule_at(
    rm: &VecModel<RuleView>,
    sw: &slint::Weak<SettingsWindow>,
    rule_idx: i32,
    mutator: impl FnOnce(&mut RuleView, &SettingsWindow, &VecModel<RuleView>) -> bool,
) {
    let idx = rule_idx as usize;
    if let (Some(win), Some(mut rule)) = (
        sw.upgrade(),
        (idx < rm.row_count()).then(|| rm.row_data(idx)).flatten(),
    ) {
        if mutator(&mut rule, &win, rm) {
            rm.set_row_data(idx, rule);
            win.set_has_unsaved_changes(true);
        }
    }
}

/// Appends a case-insensitively deduplicated executable name to the rule at `rule_idx`,
/// normalizing executable names without `.exe` (e.g. `"steam"` -> `"steam.exe"`).
pub fn add_executable_to_rule(
    rm: &VecModel<RuleView>,
    sw: &slint::Weak<SettingsWindow>,
    rule_idx: i32,
    exe_name: &str,
) {
    let Some(normalized) = normalize_executable_name(exe_name) else {
        return;
    };
    let bare_normalized = normalized.strip_prefix('!').unwrap_or(&normalized);
    update_rule_at(rm, sw, rule_idx, |rule_view, win, rm| {
        let mut targets: Vec<slint::SharedString> = rule_view
            .target_executables
            .iter()
            .filter(|t| is_ui_target_entry(t.as_str()))
            .collect();
        if targets
            .iter()
            .any(|t| t.as_str().eq_ignore_ascii_case(&normalized))
        {
            return false;
        }
        // Remove any opposite-polarity entry for the same base executable
        targets.retain(|t| {
            let s = t.as_str().trim();
            let bare_existing = s.strip_prefix('!').unwrap_or(s).trim();
            !bare_existing.eq_ignore_ascii_case(bare_normalized)
        });
        targets.push(normalized.as_str().into());
        if first_specific_executable_slice(&targets).is_none()
            && rule_view.action.as_str() == "Kill Process"
        {
            rule_view.action = "Close Window".into();
        }
        rule_view.target_executables = ModelRc::from(Rc::new(VecModel::from(targets)));
        load_rule_into_editor(win, rule_view, rm);
        true
    });
}

/// Applies a mutation closure `f` to the 168-slot weekly schedule array of the rule at `rule_idx`.
pub fn update_rule_schedule(
    rm: &VecModel<RuleView>,
    sw: &slint::Weak<SettingsWindow>,
    rule_idx: i32,
    f: impl FnOnce(&mut [bool]),
) {
    update_rule_at(rm, sw, rule_idx, |rule, _, _| {
        let mut slots: Vec<bool> = rule.schedule_slots.iter().collect();
        if slots.len() == 168 {
            f(&mut slots);
            rule.schedule_slots = ModelRc::from(Rc::new(VecModel::from(slots)));
            return true;
        }
        false
    });
}

/// Converts a domain `Rule` into a Slint `RuleView` model for the Settings UI.
pub fn rule_to_view(rule: &Rule) -> RuleView {
    let slots_bool = rule.schedule.to_bool_array();
    let slots_model = ModelRc::from(Rc::new(VecModel::from(slots_bool.to_vec())));

    let targets: Vec<slint::SharedString> = rule
        .target_executables
        .iter()
        .filter(|s| is_ui_target_entry(s))
        .map(|s| slint::SharedString::from(s.trim()))
        .collect();
    let targets_model = ModelRc::from(Rc::new(VecModel::from(targets)));

    let dur = |d: &DurationConfig| (d.value as i32, slint::SharedString::from(d.unit.as_str()));
    let (period_val, period_unit) = dur(&rule.period);
    let (reset_afk_val, reset_afk_unit) = dur(&rule.reset_after_afk);
    let (limit_val, limit_unit) = dur(&rule.limit);
    let (warn_before_val, warn_before_unit) = dur(&rule.warn_before);
    let (grace_val, grace_unit) = dur(&rule.grace_duration);
    let (break_val, break_unit) = dur(&rule.break_duration);
    let (notification_duration_val, notification_duration_unit) = dur(&rule.notification_duration);

    RuleView {
        id: rule.id.as_str().into(),
        enabled: rule.enabled,
        name: rule.name.as_str().into(),
        monitor_selection: rule.monitor_selection.as_str().into(),
        target_executables: targets_model,
        title_regex: rule.title_regex.as_str().into(),
        url_regex: rule.url_regex.as_str().into(),
        status_text: "".into(),
        trigger: rule.trigger.as_str().into(),
        period_val,
        period_unit,
        reset_afk_val,
        reset_afk_unit,
        limit_val,
        limit_unit,
        action: rule.action.as_str().into(),
        warn_before_val,
        warn_before_unit,
        grace_val,
        grace_unit,
        break_val,
        break_unit,
        click_through: rule.click_through,
        strict_mode: rule.strict_mode,
        notification_method: rule.notification_method.as_str().into(),
        notification_duration_val,
        notification_duration_unit,
        show_remaining_time: rule.show_remaining_time,
        message: rule.message.as_str().into(),
        schedule_slots: slots_model,
    }
}

/// Converts a Slint `RuleView` from the Settings UI back into a sanitized domain `Rule`.
pub fn view_to_rule(view: &RuleView) -> Rule {
    let mut slots = [false; 168];
    for (i, val) in view.schedule_slots.iter().enumerate() {
        if i < 168 {
            slots[i] = val;
        }
    }

    let mut targets: Vec<String> = view
        .target_executables
        .iter()
        .filter(|s| is_ui_target_entry(s.as_str()))
        .map(|s| s.trim().to_string())
        .collect();
    if !targets.iter().any(|s| is_specific_executable(s)) {
        targets.insert(0, "*".to_string());
    }

    let monitor_selection = MonitorSelection::from_str_lossy(view.monitor_selection.as_str());
    let trigger = TriggerType::from_str_lossy(view.trigger.as_str());
    let action = RuleAction::from_str_lossy(view.action.as_str());
    let notification_method =
        NotificationMethod::from_str_lossy(view.notification_method.as_str());

    let mut rule = Rule {
        id: view.id.to_string(),
        enabled: view.enabled,
        name: view.name.to_string(),
        monitor_selection,
        target_executables: targets,
        title_regex: view.title_regex.to_string(),
        url_regex: view.url_regex.to_string(),
        trigger,
        period: duration_from_view(view.period_val, view.period_unit.as_str(), 1),
        reset_after_afk: duration_from_view(view.reset_afk_val, view.reset_afk_unit.as_str(), 1),
        limit: duration_from_view(view.limit_val, view.limit_unit.as_str(), 0),
        action,
        warn_before: duration_from_view(view.warn_before_val, view.warn_before_unit.as_str(), 0),
        grace_duration: duration_from_view(view.grace_val, view.grace_unit.as_str(), 0),
        break_duration: duration_from_view(view.break_val, view.break_unit.as_str(), 1),
        click_through: view.click_through,
        strict_mode: view.strict_mode,
        notification_method,
        notification_duration: duration_from_view(
            view.notification_duration_val,
            view.notification_duration_unit.as_str(),
            1,
        ),
        show_remaining_time: view.show_remaining_time,
        message: view.message.to_string(),
        schedule: RuleSchedule::from_bool_array(&slots),
    };
    rule.sanitize();
    rule
}

/// Returns the first non-empty, non-wildcard, non-exclusion executable from a rule's target executables model.
pub fn first_specific_executable(targets: &slint::ModelRc<slint::SharedString>) -> Option<String> {
    targets.iter().find_map(|t| {
        let s = t.trim();
        is_specific_executable(s).then(|| s.to_string())
    })
}

fn first_specific_executable_slice(targets: &[slint::SharedString]) -> Option<String> {
    targets.iter().find_map(|t| {
        let s = t.trim();
        is_specific_executable(s).then(|| s.to_string())
    })
}

/// Normalizes an executable name by trimming whitespace, preserving an optional leading `!` exclusion prefix,
/// appending `.exe` if missing, cleanly handling internal or trailing `.exe` with surrounding spaces,
/// and returning `None` if the input is empty, wildcard `*`, or comment sentinel `#`.
pub fn normalize_executable_name(exe_name: &str) -> Option<String> {
    let trimmed = exe_name.trim();
    if trimmed.is_empty() || trimmed == "*" || trimmed == "#" {
        return None;
    }
    let (prefix, rest) = if let Some(stripped) = trimmed.strip_prefix('!') {
        ("!", stripped.trim())
    } else {
        ("", trimmed)
    };
    if rest.is_empty() || rest == "*" || rest == "#" {
        return None;
    }
    if rest.to_ascii_lowercase().ends_with(".exe") {
        let mut stem = rest[..rest.len() - 4].trim();
        while stem.to_ascii_lowercase().ends_with(".exe") {
            stem = stem[..stem.len() - 4].trim();
        }
        if stem.is_empty() || stem == "*" || stem == "#" {
            return None;
        }
        let ext = &rest[rest.len() - 4..];
        Some(format!("{prefix}{stem}{ext}"))
    } else {
        Some(format!("{prefix}{rest}.exe"))
    }
}

/// Returns true if an executable entry is a positive specific executable (non-empty, non-wildcard `*`, non-comment `#`, and not `!`-excluded).
pub fn is_specific_executable(s: &str) -> bool {
    let trimmed = s.trim();
    !trimmed.is_empty() && trimmed != "*" && trimmed != "#" && !trimmed.starts_with('!')
}

/// Returns true if a target executable string should be retained as a visible chip in the UI
/// (includes both positive specific executables and valid `!`-prefixed exclusions, while excluding `*` and `#`).
pub fn is_ui_target_entry(s: &str) -> bool {
    let trimmed = s.trim();
    if trimmed.is_empty() || trimmed == "*" || trimmed == "#" {
        return false;
    }
    if let Some(rest) = trimmed.strip_prefix('!') {
        let r = rest.trim();
        !r.is_empty() && r != "*" && r != "#"
    } else {
        true
    }
}

/// Constructs a `DurationConfig` from a Slint view integer value and unit string, clamping to `min_val`.
pub fn duration_from_view(val: i32, unit: &str, min_val: u32) -> DurationConfig {
    DurationConfig::new(
        val.max(min_val as i32) as u32,
        TimeUnit::from_str_lossy(unit),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_rule_pattern_empty_and_valid() {
        // Executables mode ignores regex
        let (valid, _) = validate_rule_pattern("Executables", "", "");
        assert!(valid);

        // Titles mode requires non-empty pattern and valid regex
        let (valid_blank_title, err_blank_title) = validate_rule_pattern("Titles", "", "");
        assert!(!valid_blank_title);
        assert_eq!(err_blank_title, "Title regex pattern cannot be empty");

        let (valid_empty_title, err_empty_title) = validate_rule_pattern("Titles", "   ", "");
        assert!(!valid_empty_title);
        assert_eq!(err_empty_title, "Title regex pattern cannot be empty");

        let (valid_bad_title, _) = validate_rule_pattern("Titles", "[a-z", "");
        assert!(!valid_bad_title);

        let (valid_good_title, _) = validate_rule_pattern("Titles", "(?i)youtube", "");
        assert!(valid_good_title);

        // Urls mode requires non-empty pattern and valid regex
        let (valid_blank_url, err_blank_url) = validate_rule_pattern("Urls", "", "");
        assert!(!valid_blank_url);
        assert_eq!(err_blank_url, "URL regex pattern cannot be empty");

        let (valid_empty_url, err_empty_url) = validate_rule_pattern("Urls", "", "   ");
        assert!(!valid_empty_url);
        assert_eq!(err_empty_url, "URL regex pattern cannot be empty");

        let (valid_bad_url, _) = validate_rule_pattern("Urls", "", "(?i)*.com");
        assert!(!valid_bad_url);

        let (valid_good_url, _) = validate_rule_pattern("Urls", "", "(?i)youtube\\.com");
        assert!(valid_good_url);
    }

    #[test]
    fn test_is_specific_executable() {
        assert!(!is_specific_executable(""));
        assert!(!is_specific_executable("   "));
        assert!(!is_specific_executable("*"));
        assert!(!is_specific_executable("  *  "));
        assert!(!is_specific_executable("#"));
        assert!(!is_specific_executable("!zoom.exe"));
        assert!(!is_specific_executable("  !discord.exe  "));
        assert!(is_specific_executable("steam.exe"));
        assert!(is_specific_executable("  game.exe  "));

        assert!(!is_ui_target_entry(""));
        assert!(!is_ui_target_entry("*"));
        assert!(!is_ui_target_entry("#"));
        assert!(!is_ui_target_entry("!"));
        assert!(!is_ui_target_entry("!*"));
        assert!(!is_ui_target_entry("!#"));
        assert!(is_ui_target_entry("!zoom.exe"));
        assert!(is_ui_target_entry("steam.exe"));
    }

    #[test]
    fn test_duration_from_view() {
        let d1 = duration_from_view(0, "Min", 1);
        assert_eq!(d1.value, 1);
        assert_eq!(d1.unit, TimeUnit::Min);

        let d2 = duration_from_view(10, "Sec", 1);
        assert_eq!(d2.value, 10);
        assert_eq!(d2.unit, TimeUnit::Sec);

        let d3 = duration_from_view(-5, "Hr", 0);
        assert_eq!(d3.value, 0);
        assert_eq!(d3.unit, TimeUnit::Hr);
    }

    #[test]
    fn test_update_rules_model_statuses() {
        let mut r1 = rule_to_view(&Rule::default_template(0));
        r1.id = "rule-1".into();
        r1.status_text = "Idle".into();

        let mut r2 = rule_to_view(&Rule::default_template(1));
        r2.id = "rule-2".into();
        r2.status_text = "Idle".into();

        let model = VecModel::from(vec![r1, r2]);
        let statuses = vec![
            ("rule-1".to_string(), "Enforcing".to_string()),
            ("rule-2".to_string(), "Idle".to_string()),
        ];

        update_rules_model_statuses(&model, &statuses);

        assert_eq!(model.row_data(0).unwrap().status_text.as_str(), "Enforcing");
        assert_eq!(model.row_data(1).unwrap().status_text.as_str(), "Idle");
    }

    #[test]
    fn test_normalize_executable_name() {
        assert_eq!(normalize_executable_name(""), None);
        assert_eq!(normalize_executable_name("   "), None);
        assert_eq!(normalize_executable_name("*"), None);
        assert_eq!(normalize_executable_name(".exe"), None);
        assert_eq!(normalize_executable_name("  .exe  "), None);
        assert_eq!(
            normalize_executable_name("steam"),
            Some("steam.exe".to_string())
        );
        assert_eq!(
            normalize_executable_name("  steam  "),
            Some("steam.exe".to_string())
        );
        assert_eq!(
            normalize_executable_name("steam.exe   "),
            Some("steam.exe".to_string())
        );
        assert_eq!(
            normalize_executable_name("   steam.exe"),
            Some("steam.exe".to_string())
        );
        assert_eq!(
            normalize_executable_name("  steam   .exe  "),
            Some("steam.exe".to_string())
        );
        assert_eq!(
            normalize_executable_name("steam.exe   .exe"),
            Some("steam.exe".to_string())
        );
        assert_eq!(
            normalize_executable_name("game.EXE"),
            Some("game.EXE".to_string())
        );
        assert_eq!(
            normalize_executable_name("  game.EXE  "),
            Some("game.EXE".to_string())
        );
        assert_eq!(
            normalize_executable_name("  discord.exe  "),
            Some("discord.exe".to_string())
        );
        assert_eq!(normalize_executable_name("#"), None);
        assert_eq!(normalize_executable_name("!"), None);
        assert_eq!(normalize_executable_name("!*"), None);
        assert_eq!(normalize_executable_name("!#"), None);
        assert_eq!(normalize_executable_name("!.exe"), None);
        assert_eq!(
            normalize_executable_name("!zoom"),
            Some("!zoom.exe".to_string())
        );
        assert_eq!(
            normalize_executable_name("  !  zoom.exe  "),
            Some("!zoom.exe".to_string())
        );
    }

    #[test]
    fn test_rule_to_view_and_view_to_rule_preserves_exclusions_and_wildcard() {
        let mut rule = Rule::default_template(0);
        rule.target_executables = vec!["*".to_string(), "!zoom.exe".to_string()];
        rule.action = RuleAction::KillProcess;

        let view = rule_to_view(&rule);
        let view_targets: Vec<String> = view
            .target_executables
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(view_targets, vec!["!zoom.exe"]);
        assert_eq!(first_specific_executable(&view.target_executables), None);

        let roundtripped = view_to_rule(&view);
        assert_eq!(
            roundtripped.target_executables,
            vec!["*".to_string(), "!zoom.exe".to_string()]
        );
        assert_eq!(roundtripped.action, RuleAction::CloseWindow);
    }

    #[test]
    fn test_view_to_rule_and_config_sanitize() {
        let view = RuleView {
            id: "rule-test".into(),
            enabled: false,
            name: "Test Rule".into(),
            monitor_selection: "Executables".into(),
            target_executables: ModelRc::new(VecModel::from(vec![slint::SharedString::from("*")])),
            title_regex: "".into(),
            url_regex: "".into(),
            status_text: "".into(),
            trigger: "Period".into(),
            period_val: 0,
            period_unit: "Min".into(),
            reset_afk_val: 0,
            reset_afk_unit: "Min".into(),
            limit_val: 0,
            limit_unit: "Hr".into(),
            action: "Kill Process".into(),
            warn_before_val: 0,
            warn_before_unit: "Min".into(),
            grace_val: 0,
            grace_unit: "Sec".into(),
            break_val: 0,
            break_unit: "Min".into(),
            click_through: false,
            strict_mode: false,
            notification_method: "HUD".into(),
            notification_duration_val: 0,
            notification_duration_unit: "Sec".into(),
            show_remaining_time: false,
            message: "".into(),
            schedule_slots: ModelRc::new(VecModel::from(vec![])),
        };

        let rule = view_to_rule(&view);
        assert_eq!(rule.period.value, 1);
        assert_eq!(rule.reset_after_afk.value, 1);
        assert_eq!(rule.break_duration.value, 1);
        assert_eq!(rule.notification_duration.value, 1);
        assert_eq!(rule.action, RuleAction::CloseWindow);
        assert_eq!(rule.schedule.to_bool_array(), [false; 168]);
    }

    #[test]
    fn test_check_warn_before_exceeds_threshold() {
        let mut rule = Rule::default_template(0);
        rule.trigger = TriggerType::Period;
        rule.period = DurationConfig::new(20, TimeUnit::Min); // 1200s
        rule.warn_before = DurationConfig::new(0, TimeUnit::Sec);
        let mut view = rule_to_view(&rule);
        assert!(!check_warn_before_exceeds_threshold(&view));

        // warn_before < threshold (5m < 20m)
        view.warn_before_val = 5;
        view.warn_before_unit = "Min".into();
        assert!(!check_warn_before_exceeds_threshold(&view));

        // warn_before == threshold (20m == 20m)
        view.warn_before_val = 20;
        view.warn_before_unit = "Min".into();
        assert!(check_warn_before_exceeds_threshold(&view));

        // warn_before > threshold (25m > 20m)
        view.warn_before_val = 25;
        view.warn_before_unit = "Min".into();
        assert!(check_warn_before_exceeds_threshold(&view));

        // Limit rule with 0 limit: threshold is 0s
        view.trigger = "Limit".into();
        view.limit_val = 0;
        view.limit_unit = "Min".into();
        view.warn_before_val = 1;
        view.warn_before_unit = "Min".into();
        assert!(check_warn_before_exceeds_threshold(&view));
    }

    #[test]
    fn test_live_ui_state_defaults() {
        let ls = LiveUiState::default();
        assert_eq!(ls.status_level, "Loading...");
        assert_eq!(ls.total_active_minutes, 0);
        assert_eq!(ls.target_minutes, 480);
        assert!(ls.histogram_values.is_empty());
        assert!(ls.histogram_labels.is_empty());
        assert!(ls.is_loading);
    }

    #[test]
    fn test_pause_all_rules_default_false() {
        let cfg = AppConfig::default();
        assert!(!cfg.general.pause_all_rules);
    }
}
