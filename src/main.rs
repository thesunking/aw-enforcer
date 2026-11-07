// AW-Enforcer Main Entry Point
#![windows_subsystem = "windows"]

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use slint::{ComponentHandle, VecModel};
use tokio::sync::mpsc;
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, WAIT_OBJECT_0,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, CreateMutexW, SetEvent, WaitForSingleObject,
};

mod autostart;
mod aw_client;
mod config;
mod enforcer;
mod engine;
mod formatter;
mod overlay;
mod settings_window;
mod tray;

use aw_client::AwClient;
use config::AppConfig;
use engine::{DaemonCommand, Engine, OverlayAction};
use overlay::OverlayManager;
use settings_window::{
    open_settings_window, spawn_aw_connection_check, update_rules_model_statuses, LiveUiState,
    SettingsContext,
};
use tray::{TrayAction, TrayManager};

slint::include_modules!();

thread_local! {
    static OVERLAY_ACTION_HANDLER: RefCell<Option<Box<dyn Fn(OverlayAction)>>> = const { RefCell::new(None) };
}

/// Application entry point: initializes single-instance lock, config, Tokio engine task, system tray icon, and Slint event loop.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Initialize logging
    tracing_subscriber::fmt::init();

    // 2. Parse launch arguments & enforce single-instance lock
    let args: Vec<String> = std::env::args().collect();
    let start_minimized = args
        .iter()
        .any(|a| a == "--minimized" || a == "--autostart");

    let _instance_lock = match acquire_single_instance(start_minimized) {
        Some(lock) => lock,
        None => {
            tracing::info!("Another instance of AW-Enforcer is already running. Exiting.");
            return Ok(());
        }
    };
    let activate_event_val = _instance_lock.activate_event as isize;

    // 3. Load application configuration & sync Windows autostart registry key
    let config = AppConfig::load();
    if std::env::var("SLINT_BACKEND").is_err() {
        if config.general.slint_renderer.eq_ignore_ascii_case("software") {
            unsafe {
                std::env::set_var("SLINT_BACKEND", "winit-software");
            }
            tracing::info!("Configured Slint renderer: CPU Software (renderer-software)");
        } else {
            unsafe {
                std::env::set_var("SLINT_BACKEND", "winit-femtovg");
            }
            tracing::info!("Configured Slint renderer: OpenGL (renderer-femtovg)");
        }
    }
    if let Err(e) = autostart::set_autostart(config.general.autostart) {
        tracing::warn!("Failed to synchronize Windows autostart registry key on launch: {e}");
    }
    enforcer::ensure_app_user_model_id_registered();
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
            .worker_threads(2)
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
        config.general.destroy_windows_on_hide,
    )));

    // 8. Shared UI state for lazy SettingsWindow instantiation
    let settings_slot: Rc<RefCell<Option<SettingsWindow>>> = Rc::new(RefCell::new(None));
    let live_ui_state: Rc<RefCell<LiveUiState>> = Rc::new(RefCell::new(LiveUiState::default()));
    let aw_connected_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let rules_model = Rc::new(VecModel::from(Vec::<RuleView>::new()));

    // 9. Build System Tray Manager
    let tray_manager = Rc::new(RefCell::new(TrayManager::new(config.general.pause_all_rules)?));

    // 10. Event-driven overlay action dispatch
    {
        let overlay_mgr = overlay_manager.clone();
        let settings_slot_ref = settings_slot.clone();
        let rules_model_ref = rules_model.clone();
        let live_state_ref = live_ui_state.clone();
        let tray_mgr_ref = tray_manager.clone();

        OVERLAY_ACTION_HANDLER.with(|cell| {
            *cell.borrow_mut() = Some(Box::new(move |action: OverlayAction| {
                let mut mgr = overlay_mgr.borrow_mut();
                match action {
                    OverlayAction::Show(mode) => {
                        mgr.show_mode(mode);
                        mgr.apply_pending_win32_styles();
                    }
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
                        any_snoozed,
                        tray_state,
                    } => {
                        {
                            let mut ls = live_state_ref.borrow_mut();
                            ls.session_duration_text = session_duration_text.clone();
                            ls.screentime_text = screentime_text.clone();
                            ls.active_app = active_app.clone();
                            ls.strict_lock_active = strict_lock_active;
                            ls.rule_statuses = rule_statuses.clone();
                            ls.any_snoozed = any_snoozed;
                        }

                        if let Some(ref win) = *settings_slot_ref.borrow() {
                            win.set_current_session_duration_text(session_duration_text.as_str().into());
                            win.set_today_screentime_text(screentime_text.as_str().into());
                            win.set_current_active_app(active_app.as_str().into());
                            win.set_strict_lock_active(strict_lock_active);
                            if !strict_lock_active {
                                win.set_strict_unlocked(false);
                                win.set_strict_unlock_countdown(0);
                            }
                        }

                        tray_mgr_ref.borrow_mut().sync_live_status(
                            strict_lock_active,
                            any_snoozed,
                            &tray_tooltip,
                            tray_state,
                        );

                        if settings_slot_ref
                            .borrow()
                            .as_ref()
                            .is_some_and(|w| w.window().is_visible())
                        {
                            update_rules_model_statuses(&rules_model_ref, &rule_statuses);
                        }
                    }
                    OverlayAction::Hide => mgr.hide_all(),
                }
            }));
        });
    }

    let overlay_forwarder_handle = rt_handle.clone();
    overlay_forwarder_handle.spawn(async move {
        while let Some(action) = overlay_action_rx.recv().await {
            slint::invoke_from_event_loop(move || {
                OVERLAY_ACTION_HANDLER.with(|cell| {
                    if let Some(ref handler) = *cell.borrow() {
                        handler(action);
                    }
                });
            })
            .ok();
        }
    });

    // 11. Main Thread UI Loop Timer (processes tray events, countdowns, and connection health)
    let main_timer = slint::Timer::default();
    let settings_ctx = SettingsContext {
        settings_slot: settings_slot.clone(),
        rules_model: rules_model.clone(),
        current_config: current_config.clone(),
        ui_aw_client: ui_aw_client.clone(),
        live_state: live_ui_state.clone(),
        aw_connected_flag: aw_connected_flag.clone(),
        command_tx: command_tx.clone(),
        rt_handle: rt_handle.clone(),
        item_pause: tray_manager.borrow().item_pause(),
        overlay_manager: overlay_manager.clone(),
    };
    {
        let ctx = settings_ctx.clone();
        let overlay_mgr = overlay_manager.clone();
        let settings_slot_ref = settings_slot.clone();
        let live_state_ref = live_ui_state.clone();
        let aw_connected_flag_ref = aw_connected_flag.clone();
        let cmd_tx = command_tx.clone();
        let cfg_cell = current_config.clone();
        let ui_client_cell = ui_aw_client.clone();
        let handle = rt_handle.clone();
        let tray_mgr = tray_manager.clone();

        let mut conn_check_counter = 0u32;
        let mut unlock_counter = 0u32;

        main_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(250),
            move || {
                let open_settings = |show: bool| open_settings_window(&ctx, show);

                // Strict unlock countdown tick (~1s = 4 ticks at 250ms)
                unlock_counter += 1;
                if unlock_counter >= 4 {
                    unlock_counter = 0;
                    if let Some(ref win) = *settings_slot_ref.borrow() {
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

                // Poll named activation event from a second instance
                if unsafe { WaitForSingleObject(activate_event_val as HANDLE, 0) } == WAIT_OBJECT_0 {
                    if let Err(e) = open_settings(true) {
                        tracing::error!("Failed to open SettingsWindow on activation event: {e}");
                    }
                }

                // Continuously ensure any realized overlay HWNDs receive their native Win32 styles & placement
                overlay_mgr.borrow_mut().apply_pending_win32_styles();

                // Process System Tray Events (Double-clicks and context menu actions)
                tray_mgr.borrow().process_events(|action| match action {
                    TrayAction::OpenSettings => {
                        if let Err(e) = open_settings(true) {
                            tracing::error!("Failed to open SettingsWindow on tray action: {e}");
                        }
                    }
                    TrayAction::ShowStats => {
                        cmd_tx.send(DaemonCommand::ShowStatsOverlay).ok();
                    }
                    TrayAction::DismissBreak => {
                        cmd_tx.send(DaemonCommand::DismissBreakOverlay).ok();
                    }
                    TrayAction::ToggleSnoozeAll => {
                        let is_locked = live_state_ref.borrow().strict_lock_active;
                        if !is_locked {
                            let was_snoozed = live_state_ref.borrow().any_snoozed;
                            if was_snoozed {
                                cmd_tx.send(DaemonCommand::ClearSnoozeAll).ok();
                            } else {
                                cmd_tx.send(DaemonCommand::SnoozeAll { minutes: 15 }).ok();
                            }
                        }
                    }
                    TrayAction::TogglePauseAll => {
                        let is_locked = live_state_ref.borrow().strict_lock_active;
                        if !is_locked {
                            let new_paused = {
                                let mut cfg = cfg_cell.borrow_mut();
                                cfg.general.pause_all_rules = !cfg.general.pause_all_rules;
                                if let Err(e) = cfg.save() {
                                    tracing::error!("Failed to save config on pause toggle: {e}");
                                }
                                cfg.general.pause_all_rules
                            };
                            tray_mgr.borrow().set_pause_label(new_paused);
                            if let Some(ref win) = *settings_slot_ref.borrow() {
                                win.set_pause_all_rules(new_paused);
                            }
                            cmd_tx
                                .send(DaemonCommand::SetPauseAll { paused: new_paused })
                                .ok();
                        }
                    }
                    TrayAction::Quit => {
                        slint::quit_event_loop().ok();
                    }
                });

                // Periodically check ActivityWatch connection status (~every 3 seconds when settings window is open)
                conn_check_counter += 1;
                if conn_check_counter >= 12 {
                    conn_check_counter = 0;
                    if let Some(win) = settings_slot_ref.borrow().as_ref() {
                        if win.window().is_visible() {
                            spawn_aw_connection_check(
                                win.as_weak(),
                                aw_connected_flag_ref.clone(),
                                ui_client_cell.borrow().clone(),
                                &handle,
                            );
                        }
                    }
                }
            },
        );
    }

    let open_settings = |show: bool| open_settings_window(&settings_ctx, show);

    // Show Settings Window unless launched with --minimized / --autostart
    if !start_minimized {
        if let Err(e) = open_settings(true) {
            tracing::error!("Failed to open SettingsWindow on startup: {e}");
        }
    } else if !config.general.destroy_windows_on_hide {
        if let Err(e) = open_settings(false) {
            tracing::error!("Failed to pre-instantiate SettingsWindow on startup: {e}");
        }
    }

    if args.iter().any(|a| a == "--test-break") {
        command_tx.send(DaemonCommand::TriggerTestOverlay { mode: "break".to_string() }).ok();
    }

    // 11. Run Slint Win32 GUI Event Loop until explicit tray Quit
    slint::run_event_loop_until_quit()?;

    Ok(())
}

/// Ensures only a single instance of AW-Enforcer runs at any time.
/// If another instance is running and this instance is launched interactively (without `--autostart` or `--minimized`),
/// signals the named activation event so the existing instance opens and focuses the SettingsWindow.
fn acquire_single_instance(is_background_launch: bool) -> Option<SingleInstanceLock> {
    let mutex_name: Vec<u16> = "Local\\AW_Enforcer_SingleInstance_Mutex\0"
        .encode_utf16()
        .collect();
    let event_name: Vec<u16> = "Local\\AW_Enforcer_Activate_Event\0"
        .encode_utf16()
        .collect();

    unsafe {
        let mutex = CreateMutexW(std::ptr::null(), 1, mutex_name.as_ptr());
        if mutex.is_null() {
            return None;
        }

        if GetLastError() == ERROR_ALREADY_EXISTS {
            if !is_background_launch {
                let ev = CreateEventW(std::ptr::null(), 0, 0, event_name.as_ptr());
                if !ev.is_null() {
                    SetEvent(ev);
                    CloseHandle(ev);
                }
            }
            CloseHandle(mutex);
            return None;
        }

        let activate_event = CreateEventW(std::ptr::null(), 0, 0, event_name.as_ptr());

        Some(SingleInstanceLock {
            _mutex: mutex,
            activate_event,
        })
    }
}

/// RAII guard keeping the single-instance mutex and named activation event handles alive
/// for the entire lifetime of the process.
struct SingleInstanceLock {
    _mutex: HANDLE,
    activate_event: HANDLE,
}

impl Drop for SingleInstanceLock {
    fn drop(&mut self) {
        unsafe {
            if !self.activate_event.is_null() {
                CloseHandle(self.activate_event);
            }
            if !self._mutex.is_null() {
                CloseHandle(self._mutex);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DurationConfig, Rule, RuleAction, TimeUnit, TriggerType};
    use crate::settings_window::*;
    use slint::{Model, ModelRc, VecModel};

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
        assert!(is_specific_executable("steam.exe"));
        assert!(is_specific_executable("  game.exe  "));
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
        assert_eq!(normalize_executable_name("steam"), Some("steam.exe".to_string()));
        assert_eq!(normalize_executable_name("  steam  "), Some("steam.exe".to_string()));
        assert_eq!(normalize_executable_name("steam.exe   "), Some("steam.exe".to_string()));
        assert_eq!(normalize_executable_name("   steam.exe"), Some("steam.exe".to_string()));
        assert_eq!(normalize_executable_name("  steam   .exe  "), Some("steam.exe".to_string()));
        assert_eq!(normalize_executable_name("steam.exe   .exe"), Some("steam.exe".to_string()));
        assert_eq!(normalize_executable_name("game.EXE"), Some("game.EXE".to_string()));
        assert_eq!(normalize_executable_name("  game.EXE  "), Some("game.EXE".to_string()));
        assert_eq!(normalize_executable_name("  discord.exe  "), Some("discord.exe".to_string()));
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
}
