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
use engine::{DaemonCommand, Engine, OverlayAction, TrayIconState};
use overlay::OverlayManager;
use settings_window::{
    open_settings_window, spawn_aw_connection_check, sync_live_state_to_ui, LiveUiState,
    SettingsContext,
};
use tray::{TrayAction, TrayManager};

slint::include_modules!();

type OverlayActionHandler = Box<dyn Fn(OverlayAction)>;

thread_local! {
    static OVERLAY_ACTION_HANDLER: RefCell<Option<OverlayActionHandler>> = const { RefCell::new(None) };
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

    let Some(instance_lock) = acquire_single_instance(start_minimized) else {
        tracing::info!("Another instance of AW-Enforcer is already running. Exiting.");
        return Ok(());
    };
    let activate_event_val = instance_lock.activate_event as isize;

    // 3. Load application configuration & environment
    let config = init_config_and_environment();
    let current_config = Rc::new(RefCell::new(config.clone()));

    // 4. Initialize ActivityWatch REST Client
    let aw_client = Arc::new(AwClient::new(
        config.general.aw_port,
        &config.general.api_key,
    ));

    // 5. Establish Tokio communication channels
    let (command_tx, command_rx) = mpsc::unbounded_channel::<DaemonCommand>();
    let (overlay_action_tx, overlay_action_rx) = mpsc::unbounded_channel::<OverlayAction>();

    // 6. Build shared multi-threaded Tokio runtime and spawn background worker thread
    let tokio_rt = spawn_engine_worker(
        config.clone(),
        aw_client.clone(),
        command_rx,
        overlay_action_tx,
    );

    // 7. Initialize OverlayManager and System Tray Manager
    let overlay_manager = Rc::new(RefCell::new(OverlayManager::new(
        config.overlay.clone(),
        Some(command_tx.clone()),
        config.general.destroy_windows_on_hide,
    )));
    let tray_manager = Rc::new(RefCell::new(TrayManager::new(config.general.pause_all_rules)?));

    // 8. Shared UI context container for SettingsWindow, overlays, and tray
    let settings_ctx = SettingsContext {
        settings_slot: Rc::new(RefCell::new(None)),
        rules_model: Rc::new(VecModel::from(Vec::<RuleView>::new())),
        current_config,
        ui_aw_client: Rc::new(RefCell::new(aw_client)),
        live_state: Rc::new(RefCell::new(LiveUiState::default())),
        aw_connected_flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        command_tx,
        rt_handle: tokio_rt.handle().clone(),
        item_pause: tray_manager.borrow().item_pause(),
        overlay_manager,
    };

    // 9. Event-driven overlay action dispatch
    setup_overlay_action_dispatch(&settings_ctx, tray_manager.clone(), overlay_action_rx);

    // 10. Main Thread UI Loop Timer (processes tray events, countdowns, and connection health)
    let main_timer = slint::Timer::default();
    start_main_ui_timer(&main_timer, &settings_ctx, tray_manager, activate_event_val);

    // 11. Handle startup launch options (--minimized, --autostart, --test-break)
    handle_startup_launch_options(&settings_ctx, &args, start_minimized);

    // 12. Run Slint Win32 GUI Event Loop until explicit tray Quit
    slint::run_event_loop_until_quit()?;

    Ok(())
}

/// Loads application configuration, initializes the configured Slint renderer,
/// synchronizes the Windows startup registry key, and registers the application AUMID.
fn init_config_and_environment() -> AppConfig {
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
    config
}

/// Builds the shared multithreaded Tokio runtime and spawns the background worker driving
/// the rule evaluation engine and command dispatch loop.
fn spawn_engine_worker(
    config: AppConfig,
    aw_client: Arc<AwClient>,
    command_rx: mpsc::UnboundedReceiver<DaemonCommand>,
    overlay_action_tx: mpsc::UnboundedSender<OverlayAction>,
) -> Arc<tokio::runtime::Runtime> {
    let tokio_rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("Failed to build Tokio runtime"),
    );

    let initial_poll_secs = config.general.poll_interval_seconds.max(2);
    let bg_rt = tokio_rt.clone();

    std::thread::spawn(move || {
        bg_rt.block_on(async move {
            let mut engine = Engine::new(config, aw_client, overlay_action_tx);
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

    tokio_rt
}

/// Sets up thread-local overlay action handler and spawns the Tokio forwarder to dispatch
/// actions from the background engine to the Slint GUI thread.
fn setup_overlay_action_dispatch(
    ctx: &SettingsContext,
    tray_manager: Rc<RefCell<TrayManager>>,
    mut overlay_action_rx: mpsc::UnboundedReceiver<OverlayAction>,
) {
    let overlay_mgr = ctx.overlay_manager.clone();
    let settings_slot_ref = ctx.settings_slot.clone();
    let rules_model_ref = ctx.rules_model.clone();
    let live_state_ref = ctx.live_state.clone();
    let tray_mgr_ref = tray_manager;

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
                    status_level,
                    total_active_minutes,
                    target_minutes,
                    histogram_values,
                    histogram_labels,
                    is_loading,
                } => {
                    apply_live_status_update(
                        &live_state_ref,
                        &settings_slot_ref,
                        &rules_model_ref,
                        &tray_mgr_ref,
                        session_duration_text,
                        screentime_text,
                        active_app,
                        strict_lock_active,
                        rule_statuses,
                        &tray_tooltip,
                        any_snoozed,
                        tray_state,
                        status_level,
                        total_active_minutes,
                        target_minutes,
                        histogram_values,
                        histogram_labels,
                        is_loading,
                    );
                }
                OverlayAction::Hide => mgr.hide_all(),
            }
        }));
    });

    let overlay_forwarder_handle = ctx.rt_handle.clone();
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
}

/// Updates the cached live UI state, applies telemetry text to the SettingsWindow if open,
/// updates rule overview statuses, and synchronizes system tray menu items and icon.
#[allow(clippy::too_many_arguments)]
fn apply_live_status_update(
    live_state_ref: &Rc<RefCell<LiveUiState>>,
    settings_slot_ref: &Rc<RefCell<Option<SettingsWindow>>>,
    rules_model_ref: &Rc<VecModel<RuleView>>,
    tray_mgr_ref: &Rc<RefCell<TrayManager>>,
    session_duration_text: String,
    screentime_text: String,
    active_app: String,
    strict_lock_active: bool,
    rule_statuses: Vec<(String, String)>,
    tray_tooltip: &str,
    any_snoozed: bool,
    tray_state: TrayIconState,
    status_level: String,
    total_active_minutes: u32,
    target_minutes: u32,
    histogram_values: Vec<f32>,
    histogram_labels: Vec<String>,
    is_loading: bool,
) {
    {
        let mut ls = live_state_ref.borrow_mut();
        ls.session_duration_text = session_duration_text.clone();
        ls.screentime_text = screentime_text.clone();
        ls.active_app = active_app.clone();
        ls.strict_lock_active = strict_lock_active;
        ls.rule_statuses = rule_statuses.clone();
        ls.any_snoozed = any_snoozed;
        ls.status_level = status_level.clone();
        ls.total_active_minutes = total_active_minutes;
        ls.target_minutes = target_minutes;
        ls.histogram_values = histogram_values.clone();
        ls.histogram_labels = histogram_labels.clone();
        ls.is_loading = is_loading;
    }

    if let Some(ref win) = *settings_slot_ref.borrow() {
        sync_live_state_to_ui(win, rules_model_ref, &live_state_ref.borrow());
    }

    tray_mgr_ref.borrow_mut().sync_live_status(
        strict_lock_active,
        any_snoozed,
        tray_tooltip,
        tray_state,
    );
}

/// Configures and starts the repeated 250ms main UI timer responsible for polling activation events,
/// ticking the strict unlock countdown, maintaining overlay Win32 styles, polling tray events,
/// and checking ActivityWatch connectivity.
fn start_main_ui_timer(
    main_timer: &slint::Timer,
    ctx: &SettingsContext,
    tray_manager: Rc<RefCell<TrayManager>>,
    activate_event_val: isize,
) {
    let ctx = ctx.clone();
    let tray_mgr = tray_manager;
    let mut conn_check_counter = 0u32;
    let mut unlock_counter = 0u32;

    main_timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(250),
        move || {
            // Strict unlock countdown tick (~1s = 4 ticks at 250ms)
            unlock_counter += 1;
            if unlock_counter >= 4 {
                unlock_counter = 0;
                tick_strict_unlock_countdown(&ctx.settings_slot);
            }

            // Poll named activation event from a second instance
            if is_activation_event_signaled(activate_event_val) {
                if let Err(e) = open_settings_window(&ctx, true) {
                    tracing::error!("Failed to open SettingsWindow on activation event: {e}");
                }
            }

            // Continuously ensure any realized overlay HWNDs receive their native Win32 styles & placement
            ctx.overlay_manager.borrow_mut().apply_pending_win32_styles();

            // Process System Tray Events (Double-clicks and context menu actions)
            tray_mgr.borrow().process_events(|action| {
                handle_tray_action(action, &ctx, &tray_mgr);
            });

            // Periodically check ActivityWatch connection status (~every 3 seconds when settings window is open)
            conn_check_counter += 1;
            if conn_check_counter >= 12 {
                conn_check_counter = 0;
                if let Some(win) = ctx.settings_slot.borrow().as_ref() {
                    if win.window().is_visible() {
                        spawn_aw_connection_check(
                            win.as_weak(),
                            ctx.aw_connected_flag.clone(),
                            ctx.ui_aw_client.borrow().clone(),
                            &ctx.rt_handle,
                        );
                    }
                }
            }
        },
    );
}

/// Decrements the strict unlock countdown timer when the SettingsWindow is open.
fn tick_strict_unlock_countdown(settings_slot: &Rc<RefCell<Option<SettingsWindow>>>) {
    if let Some(ref win) = *settings_slot.borrow() {
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

/// Dispatches high-level tray actions triggered by user interactions with the system tray icon or menu.
fn handle_tray_action(
    action: TrayAction,
    ctx: &SettingsContext,
    tray_mgr: &Rc<RefCell<TrayManager>>,
) {
    match action {
        TrayAction::OpenSettings => {
            if let Err(e) = open_settings_window(ctx, true) {
                tracing::error!("Failed to open SettingsWindow on tray action: {e}");
            }
        }
        TrayAction::ShowStats => {
            ctx.command_tx.send(DaemonCommand::ShowStatsOverlay).ok();
        }
        TrayAction::DismissBreak => {
            ctx.command_tx.send(DaemonCommand::DismissBreakOverlay).ok();
        }
        TrayAction::ToggleSnoozeAll => {
            let is_locked = ctx.live_state.borrow().strict_lock_active;
            if !is_locked {
                let was_snoozed = ctx.live_state.borrow().any_snoozed;
                if was_snoozed {
                    ctx.command_tx.send(DaemonCommand::ClearSnoozeAll).ok();
                } else {
                    ctx.command_tx.send(DaemonCommand::SnoozeAll { minutes: 15 }).ok();
                }
            }
        }
        TrayAction::TogglePauseAll => {
            let is_locked = ctx.live_state.borrow().strict_lock_active;
            if !is_locked {
                let new_paused = {
                    let mut cfg = ctx.current_config.borrow_mut();
                    cfg.general.pause_all_rules = !cfg.general.pause_all_rules;
                    if let Err(e) = cfg.save() {
                        tracing::error!("Failed to save config on pause toggle: {e}");
                    }
                    cfg.general.pause_all_rules
                };
                tray_mgr.borrow().set_pause_label(new_paused);
                if let Some(ref win) = *ctx.settings_slot.borrow() {
                    win.set_pause_all_rules(new_paused);
                }
                ctx.command_tx
                    .send(DaemonCommand::SetPauseAll { paused: new_paused })
                    .ok();
            }
        }
        TrayAction::Quit => {
            slint::quit_event_loop().ok();
        }
    }
}

/// Evaluates command-line startup options to either display the SettingsWindow, pre-instantiate it,
/// or trigger a test break overlay.
fn handle_startup_launch_options(
    ctx: &SettingsContext,
    args: &[String],
    start_minimized: bool,
) {
    let open_settings = |show: bool| open_settings_window(ctx, show);

    let destroy_on_hide = ctx.current_config.borrow().general.destroy_windows_on_hide;
    if !start_minimized {
        if let Err(e) = open_settings(true) {
            tracing::error!("Failed to open SettingsWindow on startup: {e}");
        }
    } else if !destroy_on_hide {
        if let Err(e) = open_settings(false) {
            tracing::error!("Failed to pre-instantiate SettingsWindow on startup: {e}");
        }
    }

    if args.iter().any(|a| a == "--test-break") {
        ctx.command_tx
            .send(DaemonCommand::TriggerTestOverlay {
                mode: "break".to_string(),
            })
            .ok();
    }
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

/// Performs a non-blocking check (`0`ms timeout) to see if a second instance signaled the activation event.
fn is_activation_event_signaled(activate_event_val: isize) -> bool {
    activate_event_val != 0
        && unsafe { WaitForSingleObject(activate_event_val as HANDLE, 0) == WAIT_OBJECT_0 }
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
