// AW-Enforcer Main Entry Point
#![windows_subsystem = "windows"]

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use slint::{ComponentHandle, Model, ModelRc, VecModel};
use tokio::sync::mpsc;
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    TrayIconBuilder,
};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
use windows_sys::Win32::System::Threading::CreateMutexW;

mod autostart;
mod aw_client;
mod config;
mod enforcer;
mod engine;
mod formatter;
mod overlay;

use aw_client::AwClient;
use config::{
    AppConfig, DurationConfig, MonitorSelection, NotificationMethod, Rule, RuleAction,
    RuleSchedule, TimeUnit, TriggerType,
};
use engine::{DaemonCommand, Engine, OverlayAction};
use formatter::MessageContext;
use overlay::OverlayManager;

slint::include_modules!();

/// Ensures only a single instance of AW-Enforcer runs at any time.
fn check_single_instance() -> bool {
    let mutex_name: Vec<u16> = "Global\\AW_Enforcer_SingleInstance_Mutex\0"
        .encode_utf16()
        .collect();
    unsafe {
        let handle = CreateMutexW(std::ptr::null(), 1, mutex_name.as_ptr());
        if handle.is_null() {
            return false;
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            CloseHandle(handle);
            return false;
        }
    }
    true
}

/// Loads the official Fluent ClockPause 32x32 RGBA system tray icon.
fn create_default_tray_icon() -> tray_icon::Icon {
    const TRAY_ICON_RGBA: &[u8] = include_bytes!("../assets/tray_icon_32.rgba");
    tray_icon::Icon::from_rgba(TRAY_ICON_RGBA.to_vec(), 32, 32)
        .expect("Failed to create Fluent ClockPause tray icon")
}

/// Converts a domain `Rule` into a Slint `RuleView` model for the Settings UI.
fn rule_to_view(rule: &Rule) -> RuleView {
    let slots_bool = rule.schedule.to_bool_array();
    let slots_model = ModelRc::from(Rc::new(VecModel::from(slots_bool.to_vec())));

    let targets: Vec<slint::SharedString> = rule
        .target_executables
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty() && *s != "*")
        .map(slint::SharedString::from)
        .collect();
    let targets_model = ModelRc::from(Rc::new(VecModel::from(targets)));

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
        period_val: rule.period.value as i32,
        period_unit: rule.period.unit.as_str().into(),
        reset_afk_val: rule.reset_after_afk.value as i32,
        reset_afk_unit: rule.reset_after_afk.unit.as_str().into(),
        limit_val: rule.limit.value as i32,
        limit_unit: rule.limit.unit.as_str().into(),
        action: rule.action.as_str().into(),
        warn_before_val: rule.warn_before.value as i32,
        warn_before_unit: rule.warn_before.unit.as_str().into(),
        grace_val: rule.grace_duration.value as i32,
        grace_unit: rule.grace_duration.unit.as_str().into(),
        break_val: rule.break_duration.value as i32,
        break_unit: rule.break_duration.unit.as_str().into(),
        click_through: rule.click_through,
        strict_mode: rule.strict_mode,
        notification_method: rule.notification_method.as_str().into(),
        notification_duration_val: rule.notification_duration.value as i32,
        notification_duration_unit: rule.notification_duration.unit.as_str().into(),
        show_remaining_time: rule.show_remaining_time,
        message: rule.message.as_str().into(),
        schedule_slots: slots_model,
    }
}

/// Converts a Slint `RuleView` from the Settings UI back into a sanitized domain `Rule`.
fn view_to_rule(view: &RuleView) -> Rule {
    let mut slots = [true; 168];
    for (i, val) in view.schedule_slots.iter().enumerate() {
        if i < 168 {
            slots[i] = val;
        }
    }

    let mut targets: Vec<String> = view
        .target_executables
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "*")
        .collect();
    if targets.is_empty() {
        targets.push("*".to_string());
    }

    let monitor_selection = MonitorSelection::from_str_lossy(view.monitor_selection.as_str());
    let trigger = TriggerType::from_str_lossy(view.trigger.as_str());
    let mut action = RuleAction::from_str_lossy(view.action.as_str());
    if monitor_selection == MonitorSelection::Executables {
        let has_specific = targets.iter().any(|t| t != "*");
        if !has_specific && action == RuleAction::KillProcess {
            action = RuleAction::CloseWindow;
        }
    } else if action == RuleAction::KillProcess {
        action = RuleAction::CloseWindow;
    }
    let notification_method = NotificationMethod::from_str_lossy(view.notification_method.as_str());

    Rule {
        id: view.id.to_string(),
        enabled: view.enabled,
        name: view.name.to_string(),
        monitor_selection,
        target_executables: targets,
        title_regex: view.title_regex.to_string(),
        url_regex: view.url_regex.to_string(),
        trigger,
        period: DurationConfig::new(
            view.period_val.max(0) as u32,
            TimeUnit::from_str_lossy(view.period_unit.as_str()),
        ),
        reset_after_afk: DurationConfig::new(
            view.reset_afk_val.max(0) as u32,
            TimeUnit::from_str_lossy(view.reset_afk_unit.as_str()),
        ),
        limit: DurationConfig::new(
            view.limit_val.max(0) as u32,
            TimeUnit::from_str_lossy(view.limit_unit.as_str()),
        ),
        action,
        warn_before: DurationConfig::new(
            view.warn_before_val.max(0) as u32,
            TimeUnit::from_str_lossy(view.warn_before_unit.as_str()),
        ),
        grace_duration: DurationConfig::new(
            view.grace_val.max(0) as u32,
            TimeUnit::from_str_lossy(view.grace_unit.as_str()),
        ),
        break_duration: DurationConfig::new(
            view.break_val.max(0) as u32,
            TimeUnit::from_str_lossy(view.break_unit.as_str()),
        ),
        click_through: view.click_through,
        strict_mode: view.strict_mode,
        notification_method,
        notification_duration: DurationConfig::new(
            view.notification_duration_val.max(1) as u32,
            TimeUnit::from_str_lossy(view.notification_duration_unit.as_str()),
        ),
        show_remaining_time: view.show_remaining_time,
        message: view.message.to_string(),
        schedule: RuleSchedule::from_bool_array(&slots),
    }
}

/// Refreshes the message editor UI state (line count, active selection, toggle highlights, color/size dropdowns, and preview).
fn refresh_message_editor_state(
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
        let app_name = match r.monitor_selection.as_str() {
            "Titles" | "Urls" => {
                if r.name.is_empty() {
                    "Browser / Window".to_string()
                } else {
                    r.name.to_string()
                }
            }
            _ => {
                let mut found = None;
                for t in r.target_executables.iter() {
                    let s = t.trim().to_string();
                    if !s.is_empty() && s != "*" {
                        found = Some(s);
                        break;
                    }
                }
                found.unwrap_or_else(|| "ActiveApp.exe".to_string())
            }
        };

        let threshold_secs = if r.trigger.as_str() == "Period" {
            let p_unit = TimeUnit::from_str_lossy(r.period_unit.as_str());
            DurationConfig::new(r.period_val.max(0) as u32, p_unit).to_seconds()
        } else {
            let l_unit = TimeUnit::from_str_lossy(r.limit_unit.as_str());
            DurationConfig::new(r.limit_val.max(0) as u32, l_unit).to_seconds()
        };

        let warn_unit = TimeUnit::from_str_lossy(r.warn_before_unit.as_str());
        let warn_secs = DurationConfig::new(r.warn_before_val.max(0) as u32, warn_unit).to_seconds();

        let used_secs = threshold_secs.saturating_sub(warn_secs);
        let used = formatter::format_seconds_human(used_secs);

        MessageContext {
            rule_name: if r.name.is_empty() {
                "Break Reminder".to_string()
            } else {
                r.name.to_string()
            },
            app: app_name,
            session_duration: format!("{} {}", r.period_val, r.period_unit),
            break_time: format!("{} {}", r.break_val, r.break_unit),
            used,
            limit: format!("{} {}", r.limit_val, r.limit_unit),
            remaining: format!("{} {}", r.warn_before_val.max(5), r.warn_before_unit),
        }
    } else {
        MessageContext::sample()
    };
    let paras = formatter::parse_formatted_text_with_context(message, &real_ctx);
    win.set_preview_paragraphs(ModelRc::from(Rc::new(VecModel::from(paras))));

    if win.get_editor_message_text().as_str() != message {
        win.set_editor_message_text(message.into());
    }
}

/// Synchronizes all `SettingsWindow` properties and the `rules_model` with the given `AppConfig`.
fn apply_config_to_ui(
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
    win.set_overlay_opacity(cfg.overlay.opacity);
    win.set_accent_color_hex(cfg.overlay.accent_color.as_str().into());
    win.set_daily_target_minutes(cfg.stats.daily_screentime_target_minutes as i32);
    win.set_threshold_excessive_minutes(cfg.stats.threshold_excessive_minutes as i32);
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
        refresh_message_editor_state(win, &r.message, 0, Some(&rv));
    }
    win.set_has_unsaved_changes(false);
}

/// Mutates the `RuleView` at `rule_idx` inside `rm` if `mutator` returns `true`, marking the UI as having unsaved changes.
fn update_rule_at(
    rm: &VecModel<RuleView>,
    sw: &slint::Weak<SettingsWindow>,
    rule_idx: i32,
    mutator: impl FnOnce(&mut RuleView, &SettingsWindow) -> bool,
) {
    let idx = rule_idx as usize;
    if let (Some(win), Some(mut rule)) = (
        sw.upgrade(),
        (idx < rm.row_count()).then(|| rm.row_data(idx)).flatten(),
    ) {
        if mutator(&mut rule, &win) {
            rm.set_row_data(idx, rule);
            win.set_has_unsaved_changes(true);
        }
    }
}

/// Appends a case-insensitively deduplicated executable name to the rule at `rule_idx`.
fn add_executable_to_rule(
    rm: &VecModel<RuleView>,
    sw: &slint::Weak<SettingsWindow>,
    rule_idx: i32,
    exe_name: &str,
) {
    let trimmed = exe_name.trim();
    if trimmed.is_empty() || trimmed == "*" {
        return;
    }
    update_rule_at(rm, sw, rule_idx, |rule_view, _| {
        let mut targets: Vec<slint::SharedString> = rule_view
            .target_executables
            .iter()
            .filter(|t| {
                let s = t.as_str().trim();
                !s.is_empty() && s != "*"
            })
            .collect();
        if !targets.iter().any(|t| t.as_str().eq_ignore_ascii_case(trimmed)) {
            targets.push(trimmed.into());
            rule_view.target_executables = ModelRc::from(Rc::new(VecModel::from(targets)));
            return true;
        }
        false
    });
}

/// Applies a mutation closure `f` to the 168-slot weekly schedule array of the rule at `rule_idx`.
fn update_rule_schedule(
    rm: &VecModel<RuleView>,
    sw: &slint::Weak<SettingsWindow>,
    rule_idx: i32,
    f: impl FnOnce(&mut [bool]),
) {
    update_rule_at(rm, sw, rule_idx, |rule, _| {
        let mut slots: Vec<bool> = rule.schedule_slots.iter().collect();
        if slots.len() == 168 {
            f(&mut slots);
            rule.schedule_slots = ModelRc::from(Rc::new(VecModel::from(slots)));
            return true;
        }
        false
    });
}

/// Asynchronously scans local running processes (<15ms) and updates known apps with only actively running executables.
fn scan_and_update_known_apps(
    sw: slint::Weak<SettingsWindow>,
    handle: tokio::runtime::Handle,
) {
    handle.spawn(async move {
        let mut sys = sysinfo::System::new();
        sys.refresh_processes(sysinfo::ProcessesToUpdate::All);

        let mut app_set = std::collections::BTreeSet::new();
        for proc in sys.processes().values() {
            let name = proc.name().to_string_lossy().to_string();
            if name.ends_with(".exe") {
                app_set.insert(name);
            }
        }

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

/// Application entry point: initializes single-instance lock, config, Tokio engine task, system tray icon, and Slint event loop.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Initialize logging
    tracing_subscriber::fmt::init();

    // 2. Enforce single-instance mutex
    if !check_single_instance() {
        tracing::warn!("Another instance of AW-Enforcer is already running. Exiting.");
        return Ok(());
    }

    // 3. Load application configuration & sync Windows autostart registry key
    let config = AppConfig::load();
    if let Err(e) = autostart::set_autostart(config.general.autostart) {
        tracing::warn!("Failed to synchronize Windows autostart registry key on launch: {e}");
    }
    let current_config = Rc::new(RefCell::new(config.clone()));

    // 4. Initialize ActivityWatch REST Client
    let aw_client = Arc::new(AwClient::new(
        config.general.aw_port,
        &config.general.api_key,
    ));
    let ui_aw_client = Rc::new(RefCell::new(aw_client.clone()));

    // 5. Establish Tokio communication channels
    let (command_tx, command_rx) = mpsc::unbounded_channel::<DaemonCommand>();
    let (overlay_action_tx, mut overlay_action_rx) = mpsc::unbounded_channel::<OverlayAction>();

    // 6. Build shared multi-threaded Tokio runtime and spawn background worker thread
    let tokio_rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Failed to build Tokio runtime"),
    );
    let rt_handle = tokio_rt.handle().clone();

    let worker_config = config.clone();
    let worker_client = aw_client.clone();
    let initial_poll_secs = config.general.poll_interval_seconds.max(2);
    let bg_rt = tokio_rt.clone();

    std::thread::spawn(move || {
        bg_rt.block_on(async move {
            let mut engine = Engine::new(worker_config, worker_client, overlay_action_tx);
            let mut cmd_rx = command_rx;

            let mut ticker_1s = tokio::time::interval(Duration::from_secs(1));
            let mut ticker_poll =
                tokio::time::interval(Duration::from_secs(initial_poll_secs));

            loop {
                tokio::select! {
                    Some(cmd) = cmd_rx.recv() => {
                        if let DaemonCommand::ReloadConfig(ref new_cfg) = cmd {
                            let new_secs = new_cfg.general.poll_interval_seconds.max(2);
                            ticker_poll = tokio::time::interval(Duration::from_secs(new_secs));
                        }
                        engine.handle_command(cmd).await;
                    }
                    _ = ticker_1s.tick() => {
                        engine.realtime_tick().await;
                    }
                    _ = ticker_poll.tick() => {
                        engine.poll_tick().await;
                    }
                }
            }
        });
    });

    // 7. Initialize OverlayManager with theme configuration
    let overlay_manager = Rc::new(RefCell::new(OverlayManager::new(
        config.overlay.clone(),
        Some(command_tx.clone()),
    )));

    // 8. Instantiate Settings Window
    let settings_win = SettingsWindow::new()?;
    let settings_weak = settings_win.as_weak();

    // Populate initial configuration values
    let rules_model = Rc::new(VecModel::from(Vec::<RuleView>::new()));
    settings_win.set_rules(ModelRc::from(rules_model.clone()));
    apply_config_to_ui(&settings_win, &config, &rules_model);

    // Warm known apps asynchronously on startup
    scan_and_update_known_apps(
        settings_weak.clone(),
        rt_handle.clone(),
    );

    // 9. Build System Tray Menu
    let item_settings = MenuItem::with_id("open_settings", "Open Settings", true, None);
    let item_stats = MenuItem::with_id("show_stats", "Show Activity Stats", true, None);
    let item_dismiss = MenuItem::with_id("dismiss_break", "Dismiss Break Overlay", true, None);
    let item_snooze = MenuItem::with_id("snooze_15m", "Snooze All (15m)", true, None);
    let initial_pause_label = if config.general.pause_all_rules {
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

    let _tray_icon = TrayIconBuilder::new()
        .with_menu(Box::new(tray_menu))
        .with_tooltip("AW-Enforcer")
        .with_icon(create_default_tray_icon())
        .build()
        .ok();
    let tray_icon_cell = Rc::new(RefCell::new(_tray_icon));

    // 10. Wire Slint UI Callbacks

    // Save configuration
    {
        let sw = settings_weak.clone();
        let cmd_tx = command_tx.clone();
        let cfg_cell = current_config.clone();
        let ui_client_cell = ui_aw_client.clone();
        let item_pause = item_pause.clone();

        settings_win.on_save_config(move || {
            if let Some(win) = sw.upgrade() {
                let mut updated_config = cfg_cell.borrow().clone();

                updated_config.general.aw_port = win.get_aw_port().max(1000) as u16;
                updated_config.general.poll_interval_seconds =
                    win.get_poll_interval_seconds().max(1) as u64;
                updated_config.general.autostart = win.get_autostart();
                updated_config.general.api_key = win.get_api_key().to_string();
                updated_config.general.day_start_hour = win.get_day_start_hour().clamp(0, 23) as u32;
                updated_config.general.suppress_fullscreen_d3d = win.get_suppress_fullscreen_d3d();
                updated_config.general.pause_all_rules = win.get_pause_all_rules();
                item_pause.set_text(if updated_config.general.pause_all_rules {
                    "Resume All Rules"
                } else {
                    "Pause All Rules"
                });

                updated_config.overlay.opacity = win.get_overlay_opacity().clamp(0.1, 1.0);
                updated_config.overlay.accent_color = win.get_accent_color_hex().to_string();

                updated_config.stats.daily_screentime_target_minutes =
                    win.get_daily_target_minutes().max(1) as u32;
                updated_config.stats.threshold_excessive_minutes =
                    win.get_threshold_excessive_minutes().max(1) as u32;
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
                *ui_client_cell.borrow_mut() = Arc::new(AwClient::new(
                    updated_config.general.aw_port,
                    &updated_config.general.api_key,
                ));
                cmd_tx
                    .send(DaemonCommand::ReloadConfig(Box::new(updated_config)))
                    .ok();
            }
        });
    }

    // Discard configuration changes
    {
        let sw = settings_weak.clone();
        let cfg_cell = current_config.clone();
        let rm = rules_model.clone();

        settings_win.on_discard_changes(move || {
            if let Some(win) = sw.upgrade() {
                let saved_config = cfg_cell.borrow().clone();
                apply_config_to_ui(&win, &saved_config, &rm);
                tracing::info!("Reverted all settings to last saved configuration.");
            }
        });
    }

    // Test overlay
    {
        let cmd_tx = command_tx.clone();
        settings_win.on_test_overlay(move |mode| {
            cmd_tx
                .send(DaemonCommand::TriggerTestOverlay {
                    mode: mode.to_string(),
                })
                .ok();
        });
    }

    // Show activity stats
    {
        let cmd_tx = command_tx.clone();
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

    // Add rule
    {
        let rm = rules_model.clone();
        let sw = settings_weak.clone();
        settings_win.on_add_rule(move || {
            if let Some(win) = sw.upgrade() {
                let idx = rm.row_count();
                let new_rule = Rule::default_template(idx);
                let rv = rule_to_view(&new_rule);
                rm.push(rv.clone());
                win.set_selected_rule_idx(idx as i32);
                refresh_message_editor_state(&win, &new_rule.message, 0, Some(&rv));
                win.set_has_unsaved_changes(true);
            }
        });
    }

    // Delete rule
    {
        let rm = rules_model.clone();
        let sw = settings_weak.clone();
        settings_win.on_delete_rule(move |idx| {
            if let Some(win) = sw.upgrade() {
                let u = idx as usize;
                if u < rm.row_count() {
                    rm.remove(u);
                    let cur = win.get_selected_rule_idx();
                    if cur == idx {
                        if rm.row_count() > 0 {
                            let new_idx = if u < rm.row_count() { u } else { rm.row_count() - 1 };
                            win.set_selected_rule_idx(new_idx as i32);
                            if let Some(rule) = rm.row_data(new_idx) {
                                refresh_message_editor_state(&win, &rule.message, 0, Some(&rule));
                            }
                        } else {
                            win.set_selected_rule_idx(-1);
                            win.set_editor_message_text("".into());
                        }
                    } else if cur > idx {
                        win.set_selected_rule_idx(cur - 1);
                    }
                    win.set_has_unsaved_changes(true);
                }
            }
        });
    }

    // Native file dialog browse executable
    {
        let sw = settings_weak.clone();
        let rm = rules_model.clone();

        settings_win.on_browse_executable(move || {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("Executables", &["exe"])
                .pick_file()
            {
                if let Some(filename) = path.file_name().map(|f| f.to_string_lossy().to_string()) {
                    if let Some(win) = sw.upgrade() {
                        add_executable_to_rule(&rm, &sw, win.get_selected_rule_idx(), &filename);
                    }
                }
            }
        });
    }

    // Refresh known apps using shared Tokio runtime handle
    {
        let sw = settings_weak.clone();
        let handle = rt_handle.clone();

        settings_win.on_refresh_known_apps(move || {
            scan_and_update_known_apps(sw.clone(), handle.clone());
        });
    }

    // Add known app to rule
    {
        let rm = rules_model.clone();
        let sw = settings_weak.clone();
        settings_win.on_add_known_app_to_rule(move |rule_idx, app| {
            add_executable_to_rule(&rm, &sw, rule_idx, app.as_str());
        });
    }

    // Remove executable from rule
    {
        let rm = rules_model.clone();
        let sw = settings_weak.clone();
        settings_win.on_remove_executable_from_rule(move |rule_idx, exe_idx| {
            let e_idx = exe_idx as usize;
            update_rule_at(&rm, &sw, rule_idx, |rule_view, _| {
                let mut current_targets: Vec<slint::SharedString> =
                    rule_view.target_executables.iter().collect();
                if e_idx < current_targets.len() {
                    current_targets.remove(e_idx);
                    if current_targets.is_empty() && rule_view.action.as_str() == "Kill Process" {
                        rule_view.action = "Close Window".into();
                    }
                    rule_view.target_executables =
                        ModelRc::from(Rc::new(VecModel::from(current_targets)));
                    return true;
                }
                false
            });
        });
    }

    // Toggle schedule slot
    {
        let rm = rules_model.clone();
        let sw = settings_weak.clone();
        settings_win.on_toggle_schedule_slot(move |rule_idx, day, hour, state| {
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
        let rm = rules_model.clone();
        let sw = settings_weak.clone();
        settings_win.on_toggle_schedule_day(move |rule_idx, day| {
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
        let rm = rules_model.clone();
        let sw = settings_weak.clone();
        settings_win.on_toggle_schedule_hour(move |rule_idx, hour| {
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
        let rm = rules_model.clone();
        let sw = settings_weak.clone();
        settings_win.on_apply_schedule_preset(move |rule_idx, preset| {
            update_rule_schedule(&rm, &sw, rule_idx, |slots| {
                match preset.as_str() {
                    "select_all" => {
                        slots.fill(true);
                    }
                    "work_hours" => {
                        slots.fill(false);
                        for d in 0..5 {
                            for h in 9..=17 {
                                slots[d * 24 + h] = true;
                            }
                        }
                    }
                    "evenings" => {
                        slots.fill(false);
                        for d in 0..5 {
                            for h in 18..=23 {
                                slots[d * 24 + h] = true;
                            }
                        }
                        for d in 5..7 {
                            for h in 8..=23 {
                                slots[d * 24 + h] = true;
                            }
                        }
                    }
                    "clear" => {
                        slots.fill(false);
                    }
                    _ => {}
                }
            });
        });
    }

    // Message editor: select line
    {
        let sw = settings_weak.clone();
        let rm = rules_model.clone();
        settings_win.on_select_message_line(move |line_idx| {
            if let Some(win) = sw.upgrade() {
                let rule_idx = win.get_selected_rule_idx() as usize;
                if rule_idx < rm.row_count() {
                    if let Some(rule) = rm.row_data(rule_idx) {
                        refresh_message_editor_state(&win, &rule.message, line_idx as usize, Some(&rule));
                    }
                }
            }
        });
    }

    // Message editor: text edited
    {
        let sw = settings_weak.clone();
        let rm = rules_model.clone();
        settings_win.on_message_text_edited(move |text| {
            if let Some(win) = sw.upgrade() {
                let rule_idx = win.get_selected_rule_idx();
                let text_clone = text.clone();
                update_rule_at(&rm, &sw, rule_idx, |rule, win| {
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
        let rm = rules_model.clone();
        settings_win.on_apply_line_style(move |line_idx, style| {
            if let Some(win) = sw.upgrade() {
                let rule_idx = win.get_selected_rule_idx();
                update_rule_at(&rm, &sw, rule_idx, |rule, win| {
                    let updated = formatter::apply_line_style(&rule.message, line_idx as usize, &style);
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
        let rm = rules_model.clone();
        settings_win.on_append_placeholder(move |line_idx, placeholder| {
            if let Some(win) = sw.upgrade() {
                let rule_idx = win.get_selected_rule_idx();
                update_rule_at(&rm, &sw, rule_idx, |rule, win| {
                    let updated = formatter::append_placeholder(&rule.message, line_idx as usize, &placeholder);
                    rule.message = updated.as_str().into();
                    refresh_message_editor_state(win, &updated, line_idx as usize, Some(rule));
                    true
                });
            }
        });
    }

    // Update message preview live
    {
        let sw = settings_weak.clone();
        let rm = rules_model.clone();
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

    // Request strict unlock
    {
        let sw = settings_weak.clone();
        settings_win.on_request_strict_unlock(move || {
            if let Some(win) = sw.upgrade() {
                win.set_strict_unlock_countdown(15);
            }
        });
    }

    // 11. Main Thread UI Loop Timer (processes overlay actions, tray events, and live status)
    let main_timer = slint::Timer::default();
    {
        let overlay_mgr = overlay_manager.clone();
        let sw = settings_weak.clone();
        let cmd_tx = command_tx.clone();
        let cfg_cell = current_config.clone();
        let ui_client_cell = ui_aw_client.clone();
        let handle = rt_handle.clone();
        let tray_icon_ref = tray_icon_cell.clone();
        let item_dismiss = item_dismiss.clone();
        let item_snooze = item_snooze.clone();
        let item_pause = item_pause.clone();
        let rules_model_ref = rules_model.clone();

        let mut conn_check_counter = 0u32;
        let mut unlock_counter = 0u32;

        main_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(35),
            move || {
                // Process incoming OverlayActions from Tokio engine
                while let Ok(action) = overlay_action_rx.try_recv() {
                    let mut mgr = overlay_mgr.borrow_mut();
                    match action {
                        OverlayAction::Show(mode) => mgr.show_mode(mode),
                        OverlayAction::UpdateCountdown {
                            remaining,
                            total,
                            is_idle,
                        } => {
                            mgr.update_countdown(remaining, total, is_idle);
                        }
                        OverlayAction::UpdateTheme(theme_cfg) => {
                            mgr.update_overlay_config(theme_cfg);
                        }
                        OverlayAction::UpdateLiveStatus {
                            session_duration_text,
                            screentime_text,
                            active_app,
                            strict_lock_active,
                            rule_statuses,
                            tray_tooltip,
                        } => {
                            if let Some(win) = sw.upgrade() {
                                win.set_current_session_duration_text(session_duration_text.as_str().into());
                                win.set_today_screentime_text(screentime_text.as_str().into());
                                win.set_current_active_app(active_app.as_str().into());
                                win.set_strict_lock_active(strict_lock_active);
                                if !strict_lock_active {
                                    win.set_strict_unlocked(false);
                                    win.set_strict_unlock_countdown(0);
                                }
                            }

                            item_dismiss.set_enabled(!strict_lock_active);
                            item_snooze.set_enabled(!strict_lock_active);
                            item_pause.set_enabled(!strict_lock_active);

                            for (rule_id, status_text) in rule_statuses {
                                for idx in 0..rules_model_ref.row_count() {
                                    if let Some(mut r) = rules_model_ref.row_data(idx) {
                                        if r.id == rule_id && r.status_text.as_str() != status_text.as_str() {
                                            r.status_text = status_text.as_str().into();
                                            rules_model_ref.set_row_data(idx, r);
                                        }
                                    }
                                }
                            }

                            if let Some(ref ti) = *tray_icon_ref.borrow() {
                                let _ = ti.set_tooltip(Some(&tray_tooltip));
                            }
                        }
                        OverlayAction::Hide => mgr.hide_all(),
                    }
                }

                // Strict unlock countdown tick (~1s)
                unlock_counter += 1;
                if unlock_counter >= 28 {
                    unlock_counter = 0;
                    if let Some(win) = sw.upgrade() {
                        let cd = win.get_strict_unlock_countdown();
                        if cd > 0 {
                            let next_cd = cd - 1;
                            win.set_strict_unlock_countdown(next_cd);
                            if next_cd == 0 {
                                win.set_strict_unlocked(true);
                            }
                        }
                    }
                }

                // Continuously ensure any realized overlay HWNDs receive their native Win32 styles & placement
                overlay_mgr.borrow_mut().apply_pending_win32_styles();

                // Process System Tray Menu Events
                while let Ok(event) = MenuEvent::receiver().try_recv() {
                    tracing::info!("Received system tray menu event: {}", event.id.0);
                    match event.id.0.as_str() {
                        "open_settings" => {
                            if let Some(win) = sw.upgrade() {
                                win.show().ok();
                            }
                        }
                        "show_stats" => {
                            cmd_tx.send(DaemonCommand::ShowStatsOverlay).ok();
                        }
                        "dismiss_break" => {
                            cmd_tx.send(DaemonCommand::DismissBreakOverlay).ok();
                        }
                        "snooze_15m" => {
                            cmd_tx.send(DaemonCommand::SnoozeAll { minutes: 15 }).ok();
                        }
                        "toggle_pause_all" => {
                            let is_locked = sw
                                .upgrade()
                                .map(|w| w.get_strict_lock_active())
                                .unwrap_or(false);
                            if is_locked {
                                continue;
                            }
                            let new_paused = {
                                let mut cfg = cfg_cell.borrow_mut();
                                cfg.general.pause_all_rules = !cfg.general.pause_all_rules;
                                if let Err(e) = cfg.save() {
                                    tracing::error!("Failed to save config on pause toggle: {e}");
                                }
                                cfg.general.pause_all_rules
                            };
                            item_pause.set_text(if new_paused {
                                "Resume All Rules"
                            } else {
                                "Pause All Rules"
                            });
                            if let Some(win) = sw.upgrade() {
                                win.set_pause_all_rules(new_paused);
                            }
                            cmd_tx
                                .send(DaemonCommand::SetPauseAll { paused: new_paused })
                                .ok();
                        }
                        "quit" => {
                            slint::quit_event_loop().ok();
                        }
                        _ => {}
                    }
                }

                // Periodically check ActivityWatch connection status (~every 3 seconds) using shared runtime
                conn_check_counter += 1;
                if conn_check_counter >= 85 {
                    conn_check_counter = 0;
                    let sw = sw.clone();
                    let client = ui_client_cell.borrow().clone();

                    handle.spawn(async move {
                        let connected = client.check_connection().await;

                        slint::invoke_from_event_loop(move || {
                            if let Some(win) = sw.upgrade() {
                                win.set_aw_connected(connected);
                            }
                        })
                        .ok();
                    });
                }
            },
        );
    }

    // 12. Hide Settings Window to tray on 'X' close instead of destroying it
    settings_win.window().on_close_requested(|| {
        slint::CloseRequestResponse::HideWindow
    });

    // Show Settings Window unless launched with --minimized / --autostart
    let args: Vec<String> = std::env::args().collect();
    let start_minimized = args
        .iter()
        .any(|a| a == "--minimized" || a == "--autostart");
    if !start_minimized {
        settings_win.show()?;
    }

    if args.iter().any(|a| a == "--test-break") {
        command_tx.send(DaemonCommand::TriggerTestOverlay { mode: "break".to_string() }).ok();
    }

    // 13. Run Slint Win32 GUI Event Loop until explicit tray Quit
    slint::run_event_loop_until_quit()?;

    Ok(())
}