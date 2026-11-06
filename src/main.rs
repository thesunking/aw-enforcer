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
    MouseButton, TrayIconBuilder, TrayIconEvent,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, WAIT_OBJECT_0,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, CreateMutexW, SetEvent, WaitForSingleObject,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{SetForegroundWindow, ShowWindow, SW_RESTORE};

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
use formatter::{MessageContext, TextHorizontalAlignment};
use overlay::OverlayManager;

slint::include_modules!();

thread_local! {
    static OVERLAY_ACTION_HANDLER: RefCell<Option<Box<dyn Fn(OverlayAction)>>> = const { RefCell::new(None) };
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

/// Loads the official Fluent ClockPause 32x32 RGBA system tray icon.
fn create_default_tray_icon() -> tray_icon::Icon {
    const TRAY_ICON_RGBA: &[u8] = include_bytes!("../assets/tray_icon_32.rgba");
    tray_icon::Icon::from_rgba(TRAY_ICON_RGBA.to_vec(), 32, 32)
        .expect("Failed to create Fluent ClockPause tray icon")
}

/// Returns true if an executable name is non-empty and not a wildcard (`*`).
fn is_specific_executable(s: &str) -> bool {
    let trimmed = s.trim();
    !trimmed.is_empty() && trimmed != "*"
}

/// Constructs a `DurationConfig` from a Slint view integer value and unit string, clamping to `min_val`.
fn duration_from_view(val: i32, unit: &str, min_val: u32) -> DurationConfig {
    DurationConfig::new(
        val.max(min_val as i32) as u32,
        TimeUnit::from_str_lossy(unit),
    )
}

/// Converts a domain `Rule` into a Slint `RuleView` model for the Settings UI.
fn rule_to_view(rule: &Rule) -> RuleView {
    let slots_bool = rule.schedule.to_bool_array();
    let slots_model = ModelRc::from(Rc::new(VecModel::from(slots_bool.to_vec())));

    let targets: Vec<slint::SharedString> = rule
        .target_executables
        .iter()
        .filter(|s| is_specific_executable(s))
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
fn view_to_rule(view: &RuleView) -> Rule {
    let mut slots = [false; 168];
    for (i, val) in view.schedule_slots.iter().enumerate() {
        if i < 168 {
            slots[i] = val;
        }
    }

    let mut targets: Vec<String> = view
        .target_executables
        .iter()
        .filter(|s| is_specific_executable(s.as_str()))
        .map(|s| s.trim().to_string())
        .collect();
    if targets.is_empty() {
        targets.push("*".to_string());
    }

    let monitor_selection = MonitorSelection::from_str_lossy(view.monitor_selection.as_str());
    let trigger = TriggerType::from_str_lossy(view.trigger.as_str());
    let action = RuleAction::from_str_lossy(view.action.as_str());
    let notification_method = NotificationMethod::from_str_lossy(view.notification_method.as_str());

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

/// Returns the first non-empty, non-wildcard executable from a rule's target executables model.
fn first_specific_executable(targets: &slint::ModelRc<slint::SharedString>) -> Option<String> {
    targets.iter().find_map(|t| {
        let s = t.trim();
        is_specific_executable(s).then(|| s.to_string())
    })
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
        let domain_rule = view_to_rule(r);
        formatter::build_preview_context(&domain_rule)
    } else {
        MessageContext::sample()
    };
    let alignment = if rule.map_or(false, |r| r.action.as_str() == "Show Notification") {
        TextHorizontalAlignment::Left
    } else {
        TextHorizontalAlignment::Center
    };
    let paras = formatter::parse_formatted_text_with_context_and_alignment(message, &real_ctx, alignment);
    win.set_preview_paragraphs(ModelRc::from(Rc::new(VecModel::from(paras))));

    if win.get_editor_message_text().as_str() != message {
        win.set_editor_message_text(message.into());
    }
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

/// Validates regular expressions for the currently selected rule if Title or URL monitoring is selected.
/// When Executables monitoring is selected, sets `rule-regex-valid = true` and `rule-regex-error = ""`.
fn refresh_rule_regex_validation(win: &SettingsWindow, _rm: &VecModel<RuleView>) {
    let cur_idx = win.get_selected_rule_idx();
    let (valid, err) = if cur_idx >= 0 {
        let mode = win.get_rule_monitor_selection();
        match mode.as_str() {
            "Titles" => validate_rule_pattern("Titles", &win.get_rule_title_regex(), ""),
            "Urls" => validate_rule_pattern("Urls", "", &win.get_rule_url_regex()),
            _ => (true, String::new()),
        }
    } else {
        (true, String::new())
    };

    win.set_rule_regex_valid(valid);
    win.set_rule_regex_error(if valid { "".into() } else { err.into() });
}

/// Loads all editable fields of `rule` into the top-level two-way bound properties of `SettingsWindow`.
/// Returns `true` if `warn_before` is configured (> 0) and is greater than or equal to the rule's trigger threshold.
fn check_warn_before_exceeds_threshold(rule_view: &RuleView) -> bool {
    let domain_rule = view_to_rule(rule_view);
    domain_rule.warn_before.to_seconds() > 0
        && domain_rule.warn_before.to_seconds() >= domain_rule.trigger_threshold_seconds()
}

fn load_rule_into_editor(win: &SettingsWindow, rule: &RuleView, rm: &VecModel<RuleView>) {
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
fn sync_editor_to_rule_view(win: &SettingsWindow, rule: &mut RuleView) {
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

/// Finds the next available non-conflicting `rule-{n}` identifier.
fn next_available_rule_id(rm: &VecModel<RuleView>) -> String {
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

/// Extracts the current configuration from `SettingsWindow` and `rules_model`, applying safety sanitization.
fn config_from_ui(win: &SettingsWindow, base_cfg: &AppConfig) -> AppConfig {
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

/// Updates the preview color box for accent, background, or text colors if valid hex.
fn set_color_preview(win: &SettingsWindow, kind: &str, hex: &str) {
    if let Some(c) = formatter::parse_hex_color(hex) {
        match kind {
            "accent" => win.set_accent_color_preview(c),
            "background" => win.set_background_color_preview(c),
            "text" => win.set_text_color_preview(c),
            _ => {}
        }
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

/// Mutates the `RuleView` at `rule_idx` inside `rm` if `mutator` returns `true`, marking the UI as having unsaved changes.
fn update_rule_at(
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

/// Normalizes an executable name by trimming whitespace, appending `.exe` if missing,
/// cleanly handling internal or trailing `.exe` with surrounding spaces,
/// and returning `None` if the input is empty or wildcard `*`.
pub fn normalize_executable_name(exe_name: &str) -> Option<String> {
    let trimmed = exe_name.trim();
    if trimmed.is_empty() || trimmed == "*" {
        return None;
    }
    if trimmed.to_ascii_lowercase().ends_with(".exe") {
        let mut stem = trimmed[..trimmed.len() - 4].trim();
        while stem.to_ascii_lowercase().ends_with(".exe") {
            stem = stem[..stem.len() - 4].trim();
        }
        if stem.is_empty() || stem == "*" {
            return None;
        }
        let ext = &trimmed[trimmed.len() - 4..];
        Some(format!("{}{}", stem, ext))
    } else {
        Some(format!("{}.exe", trimmed))
    }
}

/// Appends a case-insensitively deduplicated executable name to the rule at `rule_idx`,
/// normalizing executable names without `.exe` (e.g. `"steam"` -> `"steam.exe"`).
fn add_executable_to_rule(
    rm: &VecModel<RuleView>,
    sw: &slint::Weak<SettingsWindow>,
    rule_idx: i32,
    exe_name: &str,
) {
    let Some(normalized) = normalize_executable_name(exe_name) else {
        return;
    };
    update_rule_at(rm, sw, rule_idx, |rule_view, win, rm| {
        let mut targets: Vec<slint::SharedString> = rule_view
            .target_executables
            .iter()
            .filter(|t| is_specific_executable(t.as_str()))
            .collect();
        if !targets.iter().any(|t| t.as_str().eq_ignore_ascii_case(&normalized)) {
            targets.push(normalized.as_str().into());
            rule_view.target_executables = ModelRc::from(Rc::new(VecModel::from(targets)));
            load_rule_into_editor(win, rule_view, rm);
            return true;
        }
        false
    });
}

/// Returns true if UI controls should be locked due to active strict break enforcement without an unlock countdown.
fn is_ui_locked_by_strict(win: &SettingsWindow) -> bool {
    win.get_strict_lock_active() && !win.get_strict_unlocked()
}

/// Applies a mutation closure `f` to the 168-slot weekly schedule array of the rule at `rule_idx`.
fn update_rule_schedule(
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

/// Asynchronously scans local running processes (<15ms) and updates known apps with only actively running executables.
fn scan_and_update_known_apps(
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

/// Cached live telemetry state so a newly created `SettingsWindow` immediately displays current daemon status.
struct LiveUiState {
    session_duration_text: String,
    screentime_text: String,
    active_app: String,
    strict_lock_active: bool,
    rule_statuses: Vec<(String, String)>,
    any_snoozed: bool,
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
        }
    }
}

/// Updates the status text of matching rules in `rules_model` if their text changed.
fn update_rules_model_statuses(rules_model: &VecModel<RuleView>, statuses: &[(String, String)]) {
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

/// Restores and brings a Slint window to the foreground via native Win32 APIs.
fn restore_and_focus_window(window: &slint::Window) {
    if let Some(hwnd) = OverlayManager::extract_window_hwnd(window) {
        unsafe {
            ShowWindow(hwnd, SW_RESTORE);
            SetForegroundWindow(hwnd);
        }
    }
}

/// Asynchronously checks the ActivityWatch connection and updates the UI property and atomic flag.
fn spawn_aw_connection_check(
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

/// Synchronizes cached live daemon telemetry and rule statuses to the `SettingsWindow` and its rules model.
fn sync_live_state_to_ui(win: &SettingsWindow, rules_model: &VecModel<RuleView>, ls: &LiveUiState) {
    win.set_current_session_duration_text(ls.session_duration_text.as_str().into());
    win.set_today_screentime_text(ls.screentime_text.as_str().into());
    win.set_current_active_app(ls.active_app.as_str().into());
    win.set_strict_lock_active(ls.strict_lock_active);
    update_rules_model_statuses(rules_model, &ls.rule_statuses);
}

/// Lazily instantiates, populates, wires, and displays the `SettingsWindow` (or focuses it if already open).
/// When closed via the window's `'X'` button, the `SettingsWindow` instance and its GPU surface are dropped
/// if `destroy_windows_on_hide` is enabled in configuration; otherwise, it is kept hidden in memory.
#[allow(clippy::too_many_arguments)]
fn open_settings_window(
    settings_slot: &Rc<RefCell<Option<SettingsWindow>>>,
    rules_model: &Rc<VecModel<RuleView>>,
    current_config: &Rc<RefCell<AppConfig>>,
    ui_aw_client: &Rc<RefCell<Arc<AwClient>>>,
    live_state: &Rc<RefCell<LiveUiState>>,
    aw_connected_flag: &Arc<std::sync::atomic::AtomicBool>,
    command_tx: &mpsc::UnboundedSender<DaemonCommand>,
    rt_handle: &tokio::runtime::Handle,
    item_pause: &MenuItem,
    overlay_manager: &Rc<RefCell<OverlayManager>>,
    show: bool,
) -> Result<(), slint::PlatformError> {
    if let Some(ref existing) = *settings_slot.borrow() {
        sync_live_state_to_ui(existing, rules_model, &live_state.borrow());
        if show {
            scan_and_update_known_apps(existing.as_weak(), rt_handle.clone());
            spawn_aw_connection_check(
                existing.as_weak(),
                aw_connected_flag.clone(),
                ui_aw_client.borrow().clone(),
                rt_handle,
            );
            existing.show()?;
            restore_and_focus_window(existing.window());
        }
        return Ok(());
    }

    let settings_win = SettingsWindow::new()?;
    let settings_weak = settings_win.as_weak();

    // Populate initial configuration and cached live telemetry values
    settings_win.set_rules(ModelRc::from(rules_model.clone()));
    apply_config_to_ui(&settings_win, &current_config.borrow(), rules_model);
    settings_win.set_aw_connected(aw_connected_flag.load(std::sync::atomic::Ordering::Relaxed));
    sync_live_state_to_ui(&settings_win, rules_model, &live_state.borrow());

    // Warm known apps and check ActivityWatch connection asynchronously on window open
    scan_and_update_known_apps(settings_weak.clone(), rt_handle.clone());
    spawn_aw_connection_check(
        settings_weak.clone(),
        aw_connected_flag.clone(),
        ui_aw_client.borrow().clone(),
        rt_handle,
    );

    // Wire Slint UI Callbacks
    // Save configuration
    {
        let sw = settings_weak.clone();
        let cmd_tx = command_tx.clone();
        let cfg_cell = current_config.clone();
        let ui_client_cell = ui_aw_client.clone();
        let item_pause = item_pause.clone();
        let overlay_mgr_cell = overlay_manager.clone();
        let rm = rules_model.clone();
        let live_state_cell = live_state.clone();

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
                        MonitorSelection::Titles => validate_rule_pattern("Titles", &r.title_regex, ""),
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
                    overlay_mgr_cell.borrow_mut().set_destroy_windows_on_hide(new_destroy);
                }
                item_pause.set_text(if updated_config.general.pause_all_rules {
                    "Resume All Rules"
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
        let cfg_cell = current_config.clone();
        let rm = rules_model.clone();
        let live_state_cell = live_state.clone();

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
        let cfg_cell = current_config.clone();

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
        let sw = settings_weak.clone();
        let rm = rules_model.clone();
        let live_state_cell = live_state.clone();

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

    // Preview rule alert
    {
        let sw = settings_weak.clone();
        let cmd_tx = command_tx.clone();
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
        let rm = rules_model.clone();
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
                            let new_idx = if u < rm.row_count() { u } else { rm.row_count() - 1 };
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
        let rm = rules_model.clone();
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
        let rm = rules_model.clone();
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
        let rm = rules_model.clone();
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
        let rm = rules_model.clone();
        let cmd_tx = command_tx.clone();
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
        let rm = rules_model.clone();
        let cmd_tx = command_tx.clone();
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
        let rm = rules_model.clone();
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

    // Active rule edited
    {
        let rm = rules_model.clone();
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
                        win.set_warn_before_exceeds_threshold(check_warn_before_exceeds_threshold(&rule));
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
        let rm = rules_model.clone();

        settings_win.on_browse_executable(move || {
            if let Some(win) = sw.upgrade() {
                if is_ui_locked_by_strict(&win) {
                    return;
                }
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("Executables", &["exe"])
                    .pick_file()
                {
                    if let Some(filename) = path.file_name().map(|f| f.to_string_lossy().to_string()) {
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
        let rm = rules_model.clone();
        let sw = settings_weak.clone();
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
                    if current_targets.is_empty() && rule_view.action.as_str() == "Kill Process" {
                        rule_view.action = "Close Window".into();
                    }
                    rule_view.target_executables =
                        ModelRc::from(Rc::new(VecModel::from(current_targets)));
                    load_rule_into_editor(win, rule_view, rm);
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
        let rm = rules_model.clone();
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
        let rm = rules_model.clone();
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
        let rm = rules_model.clone();
        let sw = settings_weak.clone();
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
        let rm = rules_model.clone();
        settings_win.on_apply_line_style(move |line_idx, style| {
            if let Some(win) = sw.upgrade() {
                if is_ui_locked_by_strict(&win) {
                    return;
                }
                let rule_idx = win.get_selected_rule_idx();
                update_rule_at(&rm, &sw, rule_idx, |rule, win, _| {
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
                if is_ui_locked_by_strict(&win) {
                    return;
                }
                let rule_idx = win.get_selected_rule_idx();
                update_rule_at(&rm, &sw, rule_idx, |rule, win, _| {
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

    // Destroy or hide SettingsWindow instance on 'X' close
    {
        let slot_for_close = settings_slot.clone();
        let rm_for_close = rules_model.clone();
        let cfg_for_close = current_config.clone();
        let live_state_for_close = live_state.clone();
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

    if show {
        settings_win.show()?;
        restore_and_focus_window(settings_win.window());
    }
    *settings_slot.borrow_mut() = Some(settings_win);
    Ok(())
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

    // 10. Event-driven overlay action dispatch
    {
        let overlay_mgr = overlay_manager.clone();
        let settings_slot_ref = settings_slot.clone();
        let rules_model_ref = rules_model.clone();
        let live_state_ref = live_ui_state.clone();
        let tray_icon_ref = tray_icon_cell.clone();
        let item_dismiss = item_dismiss.clone();
        let item_snooze = item_snooze.clone();
        let item_pause = item_pause.clone();

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

                        item_dismiss.set_enabled(!strict_lock_active);
                        item_snooze.set_enabled(!strict_lock_active);
                        item_snooze.set_text(if any_snoozed {
                            "Resume Snoozed Rules"
                        } else {
                            "Snooze All (15m)"
                        });
                        item_pause.set_enabled(!strict_lock_active);

                        if settings_slot_ref
                            .borrow()
                            .as_ref()
                            .is_some_and(|w| w.window().is_visible())
                        {
                            update_rules_model_statuses(&rules_model_ref, &rule_statuses);
                        }

                        if let Some(ref ti) = *tray_icon_ref.borrow() {
                            let _ = ti.set_tooltip(Some(&tray_tooltip));
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
    {
        let overlay_mgr = overlay_manager.clone();
        let settings_slot_ref = settings_slot.clone();
        let rules_model_ref = rules_model.clone();
        let live_state_ref = live_ui_state.clone();
        let aw_connected_flag_ref = aw_connected_flag.clone();
        let cmd_tx = command_tx.clone();
        let cfg_cell = current_config.clone();
        let ui_client_cell = ui_aw_client.clone();
        let handle = rt_handle.clone();
        let item_pause = item_pause.clone();

        let mut conn_check_counter = 0u32;
        let mut unlock_counter = 0u32;

        main_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(250),
            move || {
                let open_settings = |show: bool| {
                    open_settings_window(
                        &settings_slot_ref,
                        &rules_model_ref,
                        &cfg_cell,
                        &ui_client_cell,
                        &live_state_ref,
                        &aw_connected_flag_ref,
                        &cmd_tx,
                        &handle,
                        &item_pause,
                        &overlay_mgr,
                        show,
                    )
                };

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

                // Process System Tray Icon Double-Click Events
                while let Ok(event) = TrayIconEvent::receiver().try_recv() {
                    if let TrayIconEvent::DoubleClick {
                        button: MouseButton::Left,
                        ..
                    } = event
                    {
                        if let Err(e) = open_settings(true) {
                            tracing::error!("Failed to open SettingsWindow on tray double-click: {e}");
                        }
                    }
                }

                // Process System Tray Menu Events
                while let Ok(event) = MenuEvent::receiver().try_recv() {
                    tracing::info!("Received system tray menu event: {}", event.id.0);
                    match event.id.0.as_str() {
                        "open_settings" => {
                            if let Err(e) = open_settings(true) {
                                tracing::error!("Failed to open SettingsWindow: {e}");
                            }
                        }
                        "show_stats" => {
                            cmd_tx.send(DaemonCommand::ShowStatsOverlay).ok();
                        }
                        "dismiss_break" => {
                            cmd_tx.send(DaemonCommand::DismissBreakOverlay).ok();
                        }
                        "snooze_15m" => {
                            let is_locked = live_state_ref.borrow().strict_lock_active;
                            if is_locked {
                                continue;
                            }
                            let was_snoozed = live_state_ref.borrow().any_snoozed;
                            if was_snoozed {
                                cmd_tx.send(DaemonCommand::ClearSnoozeAll).ok();
                            } else {
                                cmd_tx.send(DaemonCommand::SnoozeAll { minutes: 15 }).ok();
                            }
                        }
                        "toggle_pause_all" => {
                            let is_locked = live_state_ref.borrow().strict_lock_active;
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
                            if let Some(ref win) = *settings_slot_ref.borrow() {
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

    let open_settings = |show: bool| {
        open_settings_window(
            &settings_slot,
            &rules_model,
            &current_config,
            &ui_aw_client,
            &live_ui_state,
            &aw_connected_flag,
            &command_tx,
            &rt_handle,
            &item_pause,
            &overlay_manager,
            show,
        )
    };

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
