use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, Timelike, Utc};
use regex::Regex;
use slint::Color;
use crate::formatter::TextHorizontalAlignment;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use windows_sys::Win32::System::SystemInformation::GetTickCount;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

use crate::aw_client::{AwClient, AwEvent};
use crate::config::{
    AppConfig, MonitorSelection, NotificationMethod, OverlayConfig, Rule, RuleAction,
    TriggerType,
};
use crate::enforcer;
use crate::formatter::{self, MessageContext};
use crate::overlay::OverlayMode;

/// Computes the overlap duration in seconds between an ActivityWatch event and `[start_utc, end_utc]`.
fn event_overlap_seconds(ev: &AwEvent, start_utc: DateTime<Utc>, end_utc: DateTime<Utc>) -> f64 {
    let e_start = ev.timestamp;
    let e_end = ev.timestamp + Duration::milliseconds((ev.duration * 1000.0) as i64);
    let overlap_start = e_start.max(start_utc);
    let overlap_end = e_end.min(end_utc);
    if overlap_end > overlap_start {
        (overlap_end - overlap_start).num_milliseconds() as f64 / 1000.0
    } else {
        0.0
    }
}

/// Returns physical user idle duration in milliseconds using GetLastInputInfo.
/// Uses `wrapping_sub` to properly handle 32-bit `GetTickCount()` rollover after 49.7 days.
pub fn get_idle_time_millis() -> u32 {
    let mut lii = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    unsafe {
        if GetLastInputInfo(&mut lii) != 0 {
            let tick = GetTickCount();
            tick.wrapping_sub(lii.dwTime)
        } else {
            0
        }
    }
}

/// Computes the start timestamp of the current logical day anchored at `day_start_hour` (0..=23).
pub fn logical_day_start(now: DateTime<Local>, day_start_hour: u32) -> DateTime<Local> {
    let h = day_start_hour.min(23);
    let base_date = if now.hour() < h {
        now.date_naive().pred_opt().unwrap_or_else(|| now.date_naive())
    } else {
        now.date_naive()
    };
    base_date
        .and_hms_opt(h, 0, 0)
        .and_then(|ndt| ndt.and_local_timezone(Local).single())
        .unwrap_or(now)
}

/// Commands sent from the UI/Tray thread to the background daemon engine.
#[derive(Debug, Clone)]
pub enum DaemonCommand {
    ReloadConfig(Box<AppConfig>),
    SnoozeBreak { rule_id: String, minutes: u32 },
    AcknowledgeAppWarning { rule_id: String },
    SkipBreak { rule_id: String },
    DismissBreakOverlay,
    SnoozeAll { minutes: u32 },
    SetPauseAll { paused: bool },
    ShowStatsOverlay,
    CloseOverlay,
    TriggerTestOverlay { mode: String },
}

/// Actions dispatched from the engine to the Slint UI thread.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum OverlayAction {
    Show(OverlayMode),
    UpdateCountdown {
        remaining: u32,
        total: u32,
        is_idle: bool,
    },
    UpdateTheme(OverlayConfig),
    UpdateLiveStatus {
        streak_text: String,
        screentime_text: String,
        active_app: String,
        strict_lock_active: bool,
        rule_statuses: Vec<(String, String)>,
        tray_tooltip: String,
    },
    Hide,
}

/// Ephemeral runtime state tracked per configured `Rule` across polling and countdown ticks.
#[derive(Debug, Clone)]
pub struct RuleRuntime {
    pub compiled_regex: Option<Regex>,
    pub current_streak_seconds: u32,
    pub last_completed_at: Option<DateTime<Local>>,
    pub warned_current_cycle: bool,
    pub pending_break_since: Option<DateTime<Local>>,
    pub active_break: Option<(u32, u32)>,    // (remaining_secs, total_secs)
    pub grace_countdown: Option<(u32, u32)>, // (remaining_secs, total_secs)
    pub action_executed_today: bool,
    pub snoozed_until: Option<DateTime<Local>>,
    pub live_status_badge: String,
}

impl RuleRuntime {
    /// Initializes a fresh `RuleRuntime` and pre-compiles any configured title or URL regex.
    pub fn from_rule(rule: &Rule) -> Self {
        Self {
            compiled_regex: Self::compile_regex(rule),
            current_streak_seconds: 0,
            last_completed_at: None,
            warned_current_cycle: false,
            pending_break_since: None,
            active_break: None,
            grace_countdown: None,
            action_executed_today: false,
            snoozed_until: None,
            live_status_badge: String::new(),
        }
    }

    /// Recompiles the rule's title or URL regex when configuration is reloaded without losing runtime state.
    pub fn update_from_rule(&mut self, rule: &Rule) {
        self.compiled_regex = Self::compile_regex(rule);
    }

    /// Compiles the rule's active `title_regex` or `url_regex` according to its `monitor_selection`.
    fn compile_regex(rule: &Rule) -> Option<Regex> {
        match rule.monitor_selection {
            MonitorSelection::Titles if !rule.title_regex.trim().is_empty() => {
                Regex::new(&rule.title_regex).ok()
            }
            MonitorSelection::Urls if !rule.url_regex.trim().is_empty() => {
                Regex::new(&rule.url_regex).ok()
            }
            _ => None,
        }
    }
}

/// Core background daemon engine that polls ActivityWatch, evaluates rules, and dispatches overlays/enforcements.
pub struct Engine {
    config: AppConfig,
    aw_client: Arc<AwClient>,
    runtimes: HashMap<String, RuleRuntime>,
    global_snoozed_until: Option<DateTime<Local>>,
    last_reset_date: NaiveDate,
    overlay_tx: mpsc::UnboundedSender<OverlayAction>,
    active_overlay_mode: OverlayMode,
}

impl Engine {
    /// Constructs a new `Engine` with initial rule runtimes and logical day tracking.
    pub fn new(
        config: AppConfig,
        aw_client: Arc<AwClient>,
        overlay_tx: mpsc::UnboundedSender<OverlayAction>,
    ) -> Self {
        let mut runtimes = HashMap::new();
        for rule in &config.rules {
            runtimes.insert(rule.id.clone(), RuleRuntime::from_rule(rule));
        }

        let last_reset_date =
            logical_day_start(Local::now(), config.general.day_start_hour).date_naive();

        Self {
            config,
            aw_client,
            runtimes,
            global_snoozed_until: None,
            last_reset_date,
            overlay_tx,
            active_overlay_mode: OverlayMode::Hidden,
        }
    }

    /// Parses the default overlay text color from config, falling back to slate-50 (`#f8fafc`).
    fn default_text_color(&self) -> Color {
        formatter::parse_hex_color(&self.config.overlay.text_color)
            .unwrap_or_else(|| Color::from_rgb_u8(248, 250, 252))
    }

    /// Dispatches an `OverlayMode` to the UI thread if its priority is at least as high as the currently active overlay.
    fn show_overlay(&mut self, mode: OverlayMode) {
        // Never allow a lower-priority overlay to clobber a higher-priority active overlay
        if mode.priority() < self.active_overlay_mode.priority()
            && !matches!(self.active_overlay_mode, OverlayMode::Hidden)
        {
            return;
        }
        self.active_overlay_mode = mode.clone();
        self.overlay_tx.send(OverlayAction::Show(mode)).ok();
    }

    /// Returns `true` if any rule currently has an active break or grace countdown running.
    fn any_countdown_active(&self) -> bool {
        self.runtimes
            .values()
            .any(|rt| rt.active_break.is_some() || rt.grace_countdown.is_some())
    }

    /// Returns `true` if any enabled `ShowOverlay` rule with `strict_mode` is currently in a break or grace countdown.
    pub fn is_strict_lock_active(&self, _now: DateTime<Local>) -> bool {
        for rule in &self.config.rules {
            if !rule.enabled || rule.action != RuleAction::ShowOverlay || !rule.strict_mode {
                continue;
            }
            if let Some(rt) = self.runtimes.get(&rule.id) {
                if rt.active_break.is_some() || rt.grace_countdown.is_some() {
                    return true;
                }
            }
        }
        false
    }

    /// Handles asynchronous daemon commands from tray or UI.
    pub async fn handle_command(&mut self, cmd: DaemonCommand) {
        match cmd {
            DaemonCommand::SnoozeBreak { rule_id, minutes } => {
                let now = Local::now();
                let until = now + Duration::minutes(minutes as i64);
                if let Some(rt) = self.runtimes.get_mut(&rule_id) {
                    rt.current_streak_seconds = 0;
                    rt.active_break = None;
                    rt.grace_countdown = None;
                    rt.snoozed_until = Some(until);
                    rt.action_executed_today = false;
                    rt.warned_current_cycle = false;
                    rt.pending_break_since = None;
                    if rt.last_completed_at.is_none() {
                        rt.last_completed_at = Some(now);
                    }
                }
                self.hide_overlay();
            }
            DaemonCommand::AcknowledgeAppWarning { rule_id } => {
                if let Some(rt) = self.runtimes.get_mut(&rule_id) {
                    rt.warned_current_cycle = true;
                }
                self.hide_overlay();
            }
            DaemonCommand::SkipBreak { rule_id } => {
                if let Some(rt) = self.runtimes.get_mut(&rule_id) {
                    rt.active_break = None;
                    rt.current_streak_seconds = 0;
                    rt.snoozed_until = None;
                    rt.last_completed_at = Some(Local::now());
                    rt.action_executed_today = true;
                    rt.warned_current_cycle = false;
                    rt.pending_break_since = None;
                }
                self.hide_overlay();
            }
            DaemonCommand::DismissBreakOverlay => {
                if self.is_strict_lock_active(Local::now()) {
                    tracing::warn!("DismissBreakOverlay ignored: strict enforcement active");
                    return;
                }
                let now = Local::now();
                for rt in self.runtimes.values_mut() {
                    if rt.active_break.take().is_some() {
                        rt.current_streak_seconds = 0;
                        rt.snoozed_until = None;
                        rt.last_completed_at = Some(now);
                        rt.action_executed_today = true;
                    }
                    rt.warned_current_cycle = false;
                    rt.pending_break_since = None;
                }
                self.hide_overlay();
            }
            DaemonCommand::SnoozeAll { minutes } => {
                if self.is_strict_lock_active(Local::now()) {
                    tracing::warn!("SnoozeAll ignored: strict enforcement active");
                    return;
                }
                let now = Local::now();
                let until = now + Duration::minutes(minutes as i64);
                self.global_snoozed_until = Some(until);
                for rt in self.runtimes.values_mut() {
                    rt.current_streak_seconds = 0;
                    rt.active_break = None;
                    rt.grace_countdown = None;
                    rt.snoozed_until = Some(until);
                    rt.action_executed_today = false;
                    rt.warned_current_cycle = false;
                    rt.pending_break_since = None;
                    if rt.last_completed_at.is_none() {
                        rt.last_completed_at = Some(now);
                    }
                }
                self.hide_overlay();
            }
            DaemonCommand::SetPauseAll { paused } => {
                if paused && self.is_strict_lock_active(Local::now()) {
                    tracing::warn!("SetPauseAll ignored: strict enforcement active");
                    return;
                }
                self.config.general.pause_all_rules = paused;
                if paused {
                    for rt in self.runtimes.values_mut() {
                        rt.current_streak_seconds = 0;
                        rt.active_break = None;
                        rt.grace_countdown = None;
                        rt.warned_current_cycle = false;
                        rt.pending_break_since = None;
                    }
                    self.hide_overlay();
                }
            }
            DaemonCommand::ShowStatsOverlay => {
                self.display_stats_overlay(false).await;
            }
            DaemonCommand::CloseOverlay => {
                self.hide_overlay();
            }
            DaemonCommand::ReloadConfig(new_cfg) => {
                tracing::info!("Engine: Reloading configuration ({} rules)", new_cfg.rules.len());
                let paused = new_cfg.general.pause_all_rules;
                self.config = *new_cfg;
                self.aw_client = Arc::new(AwClient::new(
                    self.config.general.aw_port,
                    &self.config.general.api_key,
                ));
                self.runtimes.retain(|id, _| self.config.rules.iter().any(|r| &r.id == id));
                for rule in &self.config.rules {
                    self.runtimes
                        .entry(rule.id.clone())
                        .and_modify(|rt| rt.update_from_rule(rule))
                        .or_insert_with(|| RuleRuntime::from_rule(rule));
                }
                if paused {
                    for rt in self.runtimes.values_mut() {
                        rt.current_streak_seconds = 0;
                        rt.active_break = None;
                        rt.grace_countdown = None;
                        rt.warned_current_cycle = false;
                        rt.pending_break_since = None;
                    }
                    self.hide_overlay();
                }
                self.overlay_tx
                    .send(OverlayAction::UpdateTheme(self.config.overlay.clone()))
                    .ok();
            }
            DaemonCommand::TriggerTestOverlay { mode } => {
                self.trigger_test_overlay(&mode).await;
            }
        }
    }

    /// Hides any active overlay and resets `active_overlay_mode` to `OverlayMode::Hidden`.
    fn hide_overlay(&mut self) {
        self.active_overlay_mode = OverlayMode::Hidden;
        self.overlay_tx.send(OverlayAction::Hide).ok();
    }

    /// Real-time 1-second countdown tick updating overlays and idle counters.
    pub async fn realtime_tick(&mut self) {
        let is_idle = get_idle_time_millis() >= 3000;
        let now = Local::now();
        let idle_ms = get_idle_time_millis();

        // 0. Check pending natural break delays
        let mut launched_breaks = Vec::new();
        for (rule_id, rt) in self.runtimes.iter_mut() {
            if let Some(since) = rt.pending_break_since {
                if idle_ms >= 1500 || (now - since).num_seconds() >= 15 {
                    rt.pending_break_since = None;
                    launched_breaks.push(rule_id.clone());
                }
            }
        }
        for rule_id in launched_breaks {
            if let Some(rule) = self.config.rules.iter().find(|r| r.id == rule_id).cloned() {
                let re = self.runtimes.get(&rule.id).and_then(|r| r.compiled_regex.clone());
                let matched_app = find_matching_window_for_rule(&rule, re.as_ref())
                    .unwrap_or_else(|| "Active Application".to_string());
                let elapsed = self.runtimes.get(&rule.id).map(|rt| rt.current_streak_seconds).unwrap_or(0);
                let threshold = match rule.trigger {
                    TriggerType::Period => rule.period.to_seconds(),
                    TriggerType::Limit => rule.limit.to_seconds(),
                };
                let break_secs = rule.break_duration.to_seconds().max(1);
                if let Some(rt) = self.runtimes.get_mut(&rule.id) {
                    rt.active_break = Some((break_secs, break_secs));
                    rt.action_executed_today = true;
                }
                let ctx = self.build_message_context(&rule, &matched_app, elapsed, threshold, 0);
                let paragraphs = formatter::parse_message(
                    &rule.message,
                    &ctx,
                    self.default_text_color(),
                    TextHorizontalAlignment::Center,
                );
                self.show_overlay(OverlayMode::BreakEnforcer {
                    rule_id: rule.id.clone(),
                    title: rule.name.clone(),
                    paragraphs,
                    remaining_seconds: break_secs,
                    allow_snooze: !rule.strict_mode,
                    strict_mode: rule.strict_mode,
                    click_through: rule.click_through,
                });
            }
        }

        // 1. Check active break countdowns
        let mut completed_breaks = Vec::new();
        for (rule_id, rt) in self.runtimes.iter_mut() {
            if let Some((ref mut rem, total)) = rt.active_break {
                if is_idle && *rem > 0 {
                    *rem -= 1;
                }
                self.overlay_tx
                    .send(OverlayAction::UpdateCountdown {
                        remaining: *rem,
                        total,
                        is_idle,
                    })
                    .ok();

                if *rem == 0 {
                    rt.active_break = None;
                    rt.current_streak_seconds = 0;
                    rt.snoozed_until = None;
                    rt.warned_current_cycle = false;
                    rt.last_completed_at = Some(Local::now());
                    rt.action_executed_today = true;
                    completed_breaks.push(rule_id.clone());
                }
            }
        }
        if !completed_breaks.is_empty() && !self.any_countdown_active() {
            self.hide_overlay();
        }

        // 2. Check active grace countdowns for limit rules
        let mut completed_enforcements = Vec::new();
        for (rule_id, rt) in self.runtimes.iter_mut() {
            if let Some((ref mut rem, total)) = rt.grace_countdown {
                if *rem > 0 {
                    *rem -= 1;
                    self.overlay_tx
                        .send(OverlayAction::UpdateCountdown {
                            remaining: *rem,
                            total,
                            is_idle,
                        })
                        .ok();
                }

                if *rem == 0 {
                    rt.grace_countdown = None;
                    completed_enforcements.push(rule_id.clone());
                }
            }
        }

        for rule_id in completed_enforcements {
            if !self.any_countdown_active() {
                self.hide_overlay();
            }
            if let Some(rule) = self.config.rules.iter().find(|r| r.id == rule_id).cloned() {
                let re = self.runtimes.get(&rule.id).and_then(|r| r.compiled_regex.clone());
                let matched_name = find_matching_window_for_rule(&rule, re.as_ref())
                    .unwrap_or_else(|| "Active Application".to_string());
                let elapsed = self.runtimes.get(&rule.id).map(|r| r.current_streak_seconds).unwrap_or(0);
                let threshold = match rule.trigger {
                    TriggerType::Period => rule.period.to_seconds(),
                    TriggerType::Limit => rule.limit.to_seconds(),
                };
                self.execute_rule_action(&rule, re.as_ref(), now, &matched_name, elapsed, threshold).await;
            }
        }

        // 3. Check active test / HUD overlays
        if let OverlayMode::HudToast {
            ref mut remaining_seconds,
            duration_seconds,
            ..
        } = self.active_overlay_mode
        {
            if *remaining_seconds > 0 {
                *remaining_seconds -= 1;
                let rem = *remaining_seconds;
                self.overlay_tx
                    .send(OverlayAction::UpdateCountdown {
                        remaining: rem,
                        total: duration_seconds,
                        is_idle,
                    })
                    .ok();
                if rem == 0 && !self.any_countdown_active() {
                    self.hide_overlay();
                }
            }
        } else if let OverlayMode::HudTimer {
            ref rule_id,
            ref mut remaining_seconds,
            duration_seconds,
            ..
        } = self.active_overlay_mode
        {
            if rule_id == "test_timer" && *remaining_seconds > 0 {
                *remaining_seconds -= 1;
                let rem = *remaining_seconds;
                self.overlay_tx
                    .send(OverlayAction::UpdateCountdown {
                        remaining: rem,
                        total: duration_seconds,
                        is_idle,
                    })
                    .ok();
                if rem == 0 && !self.any_countdown_active() {
                    self.hide_overlay();
                }
            }
        } else if let OverlayMode::BreakEnforcer {
            ref rule_id,
            ref mut remaining_seconds,
            ..
        } = self.active_overlay_mode
        {
            if rule_id == "test_break" && *remaining_seconds > 0 {
                if is_idle {
                    *remaining_seconds -= 1;
                }
                let rem = *remaining_seconds;
                self.overlay_tx
                    .send(OverlayAction::UpdateCountdown {
                        remaining: rem,
                        total: 300,
                        is_idle,
                    })
                    .ok();
                if rem == 0 && !self.any_countdown_active() {
                    self.hide_overlay();
                }
            }
        }
    }

    /// Periodic ActivityWatch polling tick (e.g. every 10s).
    pub async fn poll_tick(&mut self) {
        let now = Local::now();

        // 1. Midnight rollover check anchored to logical_day_start
        let logical_now = logical_day_start(now, self.config.general.day_start_hour);
        if logical_now.date_naive() != self.last_reset_date {
            self.last_reset_date = logical_now.date_naive();
            for rt in self.runtimes.values_mut() {
                rt.warned_current_cycle = false;
                rt.action_executed_today = false;
                rt.grace_countdown = None;
                rt.snoozed_until = None;
                rt.pending_break_since = None;
            }
        }

        // 2. Global snooze check
        if let Some(until) = self.global_snoozed_until {
            if now < until {
                return;
            } else {
                self.global_snoozed_until = None;
            }
        }

        let day_index = now.weekday().num_days_from_monday() as usize;
        let hour_index = now.hour() as usize;

        // 3. Compute earliest required lookback across all enabled active rules
        let start_of_day_utc = logical_day_start(now, self.config.general.day_start_hour).with_timezone(&Utc);
        let now_utc = now.with_timezone(&Utc);

        let mut min_start_utc = start_of_day_utc;
        let mut has_url_rules = false;

        for rule in &self.config.rules {
            if rule.enabled && rule.schedule.is_active(day_index, hour_index) {
                if rule.monitor_selection == MonitorSelection::Urls {
                    has_url_rules = true;
                }
                if rule.trigger == TriggerType::Period {
                    let reset_gap_secs = rule.reset_after_afk.to_seconds().max(1);
                    let period_secs = rule.period.to_seconds().max(1);
                    let lookback_secs = (period_secs + reset_gap_secs + 3600) as i64;
                    let rule_start_utc = (now - Duration::seconds(lookback_secs)).with_timezone(&Utc);
                    min_start_utc = min_start_utc.min(rule_start_utc);
                }
            }
        }

        let all_events = self
            .aw_client
            .query_intersected_events(min_start_utc, now_utc)
            .await
            .unwrap_or_default();

        let web_events = if has_url_rules {
            self.aw_client.query_web_events(min_start_utc, now_utc).await
        } else {
            Vec::new()
        };

        // 4. Evaluate Unified Rules (skip if all rules are paused indefinitely)
        if self.config.general.pause_all_rules {
            for rt in self.runtimes.values_mut() {
                rt.current_streak_seconds = 0;
            }
        } else {
            for rule in self.config.rules.clone() {
                if !rule.enabled || !rule.schedule.is_active(day_index, hour_index) {
                    if let Some(rt) = self.runtimes.get_mut(&rule.id) {
                        rt.current_streak_seconds = 0;
                    }
                    continue;
                }

                let events_for_rule = if rule.monitor_selection == MonitorSelection::Urls {
                    &web_events
                } else {
                    &all_events
                };

                let compiled_re = self.runtimes.get(&rule.id).and_then(|r| r.compiled_regex.clone());
                self.evaluate_rule(
                    &rule,
                    compiled_re.as_ref(),
                    now,
                    events_for_rule,
                    start_of_day_utc,
                    now_utc,
                )
                .await;
            }
        }

        // 5. Telemetry push for Tab 2 Live Status & Per-rule status badges
        let max_streak_secs = self.runtimes.values().map(|rt| rt.current_streak_seconds).max().unwrap_or(0);

        let total_today_secs: f64 = all_events
            .iter()
            .map(|ev| event_overlap_seconds(ev, start_of_day_utc, now_utc))
            .sum();
        let total_today_mins = (total_today_secs / 60.0).round() as u32;

        let active_app = all_events
            .iter()
            .max_by_key(|e| e.timestamp)
            .and_then(|e| e.data.get("app"))
            .and_then(|a| a.as_str())
            .unwrap_or("None")
            .to_string();

        let mut rule_statuses = Vec::new();
        for rule in &self.config.rules {
            let rt = match self.runtimes.get_mut(&rule.id) {
                Some(rt) => rt,
                None => continue,
            };
            let badge = if !rule.enabled {
                "Disabled".to_string()
            } else if self.config.general.pause_all_rules {
                "Paused".to_string()
            } else if !rule.schedule.is_active(day_index, hour_index) {
                "Off-schedule".to_string()
            } else if rt.snoozed_until.map(|u| u > now).unwrap_or(false) {
                "Snoozed".to_string()
            } else if rule.trigger == TriggerType::Period {
                format!(
                    "Streak: {} / {}",
                    formatter::format_seconds_human(rt.current_streak_seconds),
                    rule.period.to_short_string()
                )
            } else {
                let events_for_rule = if rule.monitor_selection == MonitorSelection::Urls {
                    &web_events
                } else {
                    &all_events
                };
                let daily_usage_seconds = calculate_usage_for_rule(
                    events_for_rule,
                    rule,
                    rt.compiled_regex.as_ref(),
                    start_of_day_utc,
                    now_utc,
                );
                format!(
                    "Used: {} / {}",
                    formatter::format_seconds_human(daily_usage_seconds),
                    rule.limit.to_short_string()
                )
            };
            rt.live_status_badge = badge.clone();
            rule_statuses.push((rule.id.clone(), badge));
        }

        let strict_lock_active = self.is_strict_lock_active(now);
        let tray_tooltip = if self.config.general.pause_all_rules {
            format!(
                "AW-Enforcer (Paused) — Today: {}h {}m",
                total_today_mins / 60,
                total_today_mins % 60
            )
        } else {
            format!(
                "AW-Enforcer — Today: {}h {}m | Streak: {}m",
                total_today_mins / 60,
                total_today_mins % 60,
                max_streak_secs / 60
            )
        };

        self.overlay_tx
            .send(OverlayAction::UpdateLiveStatus {
                streak_text: format!("{}m", max_streak_secs / 60),
                screentime_text: format!("{}h {}m", total_today_mins / 60, total_today_mins % 60),
                active_app,
                strict_lock_active,
                rule_statuses,
                tray_tooltip,
            })
            .ok();
    }

    /// Builds a `MessageContext` populated with formatted streak, usage, limit, and remaining durations for a rule.
    fn build_message_context(
        &self,
        rule: &Rule,
        matched_app_name: &str,
        elapsed_secs: u32,
        _threshold_secs: u32,
        rem_secs: u32,
    ) -> MessageContext {
        let used_str = formatter::format_seconds_human(elapsed_secs);
        let limit_str = rule.limit.to_short_string();
        let rem_str = formatter::format_seconds_human(rem_secs);
        let streak_str = formatter::format_seconds_human(elapsed_secs);
        let break_str = rule.break_duration.to_short_string();

        MessageContext {
            rule_name: rule.name.clone(),
            app: matched_app_name.to_string(),
            streak: streak_str,
            break_time: break_str,
            used: used_str,
            limit: limit_str,
            remaining: rem_str,
        }
    }

    /// Dispatches a Stage-1 pre-action warning via either a HUD toast or a native Windows notification.
    fn dispatch_pre_action_warning(&mut self, rule: &Rule, ctx: &MessageContext, rem_secs: u32) {
        let suppress_intrusive = self.config.general.suppress_fullscreen_d3d
            && enforcer::is_fullscreen_or_presentation_active();
        let use_windows = suppress_intrusive
            || (rule.action == RuleAction::ShowNotification && rule.notification_method == NotificationMethod::Windows);

        let warn_title = format!("Upcoming: {}", rule.name);
        let warn_body = format!("{} in {}.", rule.action.as_str(), formatter::format_seconds_human(rem_secs));

        if use_windows {
            enforcer::send_system_notification(&warn_title, &warn_body);
        } else {
            let default_msg = format!("[color=#f59e0b bold] {} in {}.", rule.action.as_str(), formatter::format_seconds_human(rem_secs));
            let raw_msg = if !rule.message.trim().is_empty() && matches!(rule.action, RuleAction::ShowOverlay | RuleAction::ShowNotification) {
                &rule.message
            } else {
                &default_msg
            };
            let paragraphs = formatter::parse_message(
                raw_msg,
                ctx,
                self.default_text_color(),
                TextHorizontalAlignment::Left,
            );
            let dur = rule.notification_duration.to_seconds().max(1);
            self.show_overlay(OverlayMode::HudToast {
                rule_id: rule.id.clone(),
                title: warn_title,
                paragraphs,
                snooze_options_minutes: if rule.action == RuleAction::ShowOverlay && rule.strict_mode { Vec::new() } else { vec![5, 10, 15] },
                duration_seconds: dur,
                remaining_seconds: dur,
                show_time_remaining: rule.show_remaining_time,
            });
        }
    }

    /// Evaluates a single enabled, in-schedule rule against recent ActivityWatch events and triggers warnings or actions.
    async fn evaluate_rule(
        &mut self,
        rule: &Rule,
        compiled_re: Option<&Regex>,
        now: DateTime<Local>,
        events: &[AwEvent],
        start_of_day_utc: DateTime<Utc>,
        now_utc: DateTime<Utc>,
    ) {
        // 1. Skip if currently in break, waiting for natural pause, in grace countdown, or snoozed
        {
            let Some(rt) = self.runtimes.get_mut(&rule.id) else { return };
            if rt.active_break.is_some()
                || rt.pending_break_since.is_some()
                || rt.grace_countdown.is_some()
            {
                return;
            }
            if let Some(until) = rt.snoozed_until {
                if now < until {
                    return;
                } else {
                    rt.snoozed_until = None;
                }
            }
        }

        // 2. Compute elapsed time and target threshold based on TriggerType
        let (elapsed_secs, threshold_secs) = match rule.trigger {
            TriggerType::Period => {
                let last_completed = self.runtimes.get(&rule.id).and_then(|r| r.last_completed_at);
                let reset_gap = rule.reset_after_afk.to_seconds().max(1);
                let period_secs = rule.period.to_seconds().max(1);
                let streak = calculate_continuous_streak_for_rule(
                    events,
                    now,
                    last_completed,
                    reset_gap,
                    rule,
                    compiled_re,
                );
                if let Some(rt) = self.runtimes.get_mut(&rule.id) {
                    if streak == 0 {
                        rt.warned_current_cycle = false;
                    }
                    rt.current_streak_seconds = streak;
                }
                (streak, period_secs)
            }
            TriggerType::Limit => {
                let used = calculate_usage_for_rule(events, rule, compiled_re, start_of_day_utc, now_utc);
                let limit_secs = rule.limit.to_seconds();
                if let Some(rt) = self.runtimes.get_mut(&rule.id) {
                    rt.current_streak_seconds = 0;
                }
                (used, limit_secs)
            }
        };

        let matched_window = find_matching_window_for_rule(rule, compiled_re);
        let Some(matched_app_name) = matched_window else {
            return;
        };

        let warn_secs = rule.warn_before.to_seconds();
        let grace_secs = rule.grace_duration.to_seconds();

        // 3. Stage 1: Warn Before Action (0 = disabled)
        if threshold_secs > 0
            && warn_secs > 0
            && warn_secs < threshold_secs
            && elapsed_secs >= threshold_secs.saturating_sub(warn_secs)
            && elapsed_secs < threshold_secs
        {
            let already_warned = self
                .runtimes
                .get(&rule.id)
                .map(|r| r.warned_current_cycle)
                .unwrap_or(false);
            if !already_warned {
                if let Some(rt) = self.runtimes.get_mut(&rule.id) {
                    rt.warned_current_cycle = true;
                }
                let rem_secs = threshold_secs.saturating_sub(elapsed_secs).max(1);
                let ctx = self.build_message_context(rule, &matched_app_name, elapsed_secs, threshold_secs, rem_secs);
                self.dispatch_pre_action_warning(rule, &ctx, rem_secs);
            }
            return;
        }

        // 4. Stage 2: Threshold Reached
        if elapsed_secs >= threshold_secs {
            // For Limit triggers with non-destructive actions (ShowOverlay, ShowNotification, ShowStatistics),
            // only fire once per logical day unless snoozed.
            let is_destructive = matches!(
                rule.action,
                RuleAction::LockWorkstation
                    | RuleAction::CloseTab
                    | RuleAction::CloseWindow
                    | RuleAction::KillProcess
            );
            let already_executed = self
                .runtimes
                .get(&rule.id)
                .map(|r| r.action_executed_today)
                .unwrap_or(false);

            if rule.trigger == TriggerType::Limit && !is_destructive && already_executed {
                return;
            }

            let effective_grace = if rule.trigger == TriggerType::Limit && already_executed && grace_secs > 0 {
                grace_secs.min(5)
            } else {
                grace_secs
            };

            if effective_grace > 0 {
                if let Some(rt) = self.runtimes.get_mut(&rule.id) {
                    rt.grace_countdown = Some((effective_grace, effective_grace));
                }
                let ctx = self.build_message_context(rule, &matched_app_name, elapsed_secs, threshold_secs, 0);
                let default_msg = format!(
                    "[color=#f87171 bold] {} in {}s.",
                    rule.action.as_str(),
                    effective_grace
                );
                let raw_msg = if matches!(rule.action, RuleAction::ShowOverlay | RuleAction::ShowNotification)
                    && !rule.message.trim().is_empty()
                {
                    &rule.message
                } else {
                    &default_msg
                };
                let paragraphs = formatter::parse_message(
                    raw_msg,
                    &ctx,
                    self.default_text_color(),
                    TextHorizontalAlignment::Left,
                );
                self.show_overlay(OverlayMode::HudTimer {
                    rule_id: rule.id.clone(),
                    title: format!("{}: {}", rule.action.as_str(), rule.name),
                    paragraphs,
                    duration_seconds: effective_grace,
                    remaining_seconds: effective_grace,
                });
            } else {
                self.execute_rule_action(rule, compiled_re, now, &matched_app_name, elapsed_secs, threshold_secs).await;
            }
        }
    }

    /// Executes the rule's configured `RuleAction` once its threshold (and any grace countdown) has completed.
    async fn execute_rule_action(
        &mut self,
        rule: &Rule,
        compiled_re: Option<&Regex>,
        now: DateTime<Local>,
        matched_app_name: &str,
        elapsed_secs: u32,
        threshold_secs: u32,
    ) {
        let ctx = self.build_message_context(rule, matched_app_name, elapsed_secs, threshold_secs, 0);

        match rule.action {
            RuleAction::ShowOverlay => {
                if self.config.general.suppress_fullscreen_d3d
                    && enforcer::is_fullscreen_or_presentation_active()
                {
                    if let Some(rt) = self.runtimes.get_mut(&rule.id) {
                        rt.current_streak_seconds = 0;
                        rt.warned_current_cycle = false;
                        rt.last_completed_at = Some(now);
                        rt.action_executed_today = true;
                    }
                    enforcer::send_system_notification(&rule.name, &ctx.interpolate(&rule.message));
                    return;
                }

                if get_idle_time_millis() < 1500 && rule.grace_duration.to_seconds() == 0 {
                    if let Some(rt) = self.runtimes.get_mut(&rule.id) {
                        rt.pending_break_since = Some(now);
                    }
                    return;
                }

                let break_secs = rule.break_duration.to_seconds().max(1);
                if let Some(rt) = self.runtimes.get_mut(&rule.id) {
                    rt.active_break = Some((break_secs, break_secs));
                    rt.action_executed_today = true;
                }
                let paragraphs = formatter::parse_message(
                    &rule.message,
                    &ctx,
                    self.default_text_color(),
                    TextHorizontalAlignment::Center,
                );
                self.show_overlay(OverlayMode::BreakEnforcer {
                    rule_id: rule.id.clone(),
                    title: rule.name.clone(),
                    paragraphs,
                    remaining_seconds: break_secs,
                    allow_snooze: !rule.strict_mode,
                    strict_mode: rule.strict_mode,
                    click_through: rule.click_through,
                });
            }
            RuleAction::ShowNotification => {
                if let Some(rt) = self.runtimes.get_mut(&rule.id) {
                    rt.current_streak_seconds = 0;
                    rt.warned_current_cycle = false;
                    rt.last_completed_at = Some(now);
                    rt.action_executed_today = true;
                }
                match rule.notification_method {
                    NotificationMethod::Windows => {
                        enforcer::send_system_notification(&rule.name, &ctx.interpolate(&rule.message));
                    }
                    NotificationMethod::Hud => {
                        let paragraphs = formatter::parse_message(
                            &rule.message,
                            &ctx,
                            self.default_text_color(),
                            TextHorizontalAlignment::Left,
                        );
                        let dur = rule.notification_duration.to_seconds().max(1);
                        self.show_overlay(OverlayMode::HudToast {
                            rule_id: rule.id.clone(),
                            title: rule.name.clone(),
                            paragraphs,
                            snooze_options_minutes: vec![5, 10, 15],
                            duration_seconds: dur,
                            remaining_seconds: dur,
                            show_time_remaining: rule.show_remaining_time,
                        });
                    }
                }
            }
            RuleAction::ShowStatistics => {
                if let Some(rt) = self.runtimes.get_mut(&rule.id) {
                    rt.current_streak_seconds = 0;
                    rt.warned_current_cycle = false;
                    rt.last_completed_at = Some(now);
                    rt.action_executed_today = true;
                }
                self.display_stats_overlay(false).await;
            }
            RuleAction::LockWorkstation => {
                if let Some(rt) = self.runtimes.get_mut(&rule.id) {
                    rt.current_streak_seconds = 0;
                    rt.warned_current_cycle = false;
                    rt.last_completed_at = Some(now);
                    rt.action_executed_today = true;
                }
                enforcer::lock_workstation();
            }
            RuleAction::CloseTab => {
                if let Some(rt) = self.runtimes.get_mut(&rule.id) {
                    rt.current_streak_seconds = 0;
                    rt.warned_current_cycle = false;
                    rt.last_completed_at = Some(now);
                    rt.action_executed_today = true;
                }
                let browser_exes = [
                    "chrome.exe".to_string(),
                    "msedge.exe".to_string(),
                    "firefox.exe".to_string(),
                    "brave.exe".to_string(),
                    "opera.exe".to_string(),
                    "vivaldi.exe".to_string(),
                ];
                let exes = if rule.monitor_selection == MonitorSelection::Executables {
                    &rule.target_executables
                } else {
                    &browser_exes[..]
                };
                if !enforcer::close_active_browser_tab(exes, compiled_re) {
                    enforcer::close_target_windows(exes, compiled_re);
                }
            }
            RuleAction::CloseWindow => {
                if let Some(rt) = self.runtimes.get_mut(&rule.id) {
                    rt.current_streak_seconds = 0;
                    rt.warned_current_cycle = false;
                    rt.last_completed_at = Some(now);
                    rt.action_executed_today = true;
                }
                let browser_exes = [
                    "chrome.exe".to_string(),
                    "msedge.exe".to_string(),
                    "firefox.exe".to_string(),
                    "brave.exe".to_string(),
                    "opera.exe".to_string(),
                    "vivaldi.exe".to_string(),
                ];
                if rule.monitor_selection == MonitorSelection::Titles {
                    enforcer::close_target_windows(&[], compiled_re);
                } else if rule.monitor_selection == MonitorSelection::Urls {
                    enforcer::close_target_windows(&browser_exes, None);
                } else {
                    enforcer::close_target_windows(&rule.target_executables, None);
                }
            }
            RuleAction::KillProcess => {
                if let Some(rt) = self.runtimes.get_mut(&rule.id) {
                    rt.current_streak_seconds = 0;
                    rt.warned_current_cycle = false;
                    rt.last_completed_at = Some(now);
                    rt.action_executed_today = true;
                }
                enforcer::kill_target_processes(&rule.target_executables);
            }
        }
    }

    /// Prepares and displays the 24-hour activity status overlay.
    pub async fn display_stats_overlay(&mut self, click_through: bool) {
        // Immediately pop up the HUD window in loading state so the user gets instant feedback
        let loading_mode = OverlayMode::HudStatus {
            title: "24-Hour Screen Time Summary".into(),
            status_level: "Loading...".into(),
            total_active_minutes: 0,
            target_minutes: self.config.stats.daily_screentime_target_minutes,
            bin_values: Vec::new(),
            bin_labels: Vec::new(),
            is_loading: true,
            click_through,
        };
        self.show_overlay(loading_mode);

        let now = Local::now();
        let bin_count = self.config.stats.histogram_bin_count.max(1) as usize;
        let bin_size_mins = self.config.stats.histogram_bin_size_minutes.max(1) as i64;

        // Align bins to the start of the current hour
        let current_minute = now.minute();
        let bin_size = bin_size_mins.max(1);
        let aligned_minute = if bin_size >= 60 {
            0
        } else {
            (current_minute / (bin_size as u32)) * (bin_size as u32)
        };

        let current_bin_start = now
            .with_minute(aligned_minute)
            .and_then(|t| t.with_second(0))
            .and_then(|t| t.with_nanosecond(0))
            .unwrap_or(now);

        let start_time = current_bin_start - Duration::minutes(((bin_count - 1) as i64) * bin_size);
        let start_utc = start_time.with_timezone(&Utc);
        let end_utc = (current_bin_start + Duration::minutes(bin_size)).with_timezone(&Utc);

        let events = self
            .aw_client
            .query_intersected_events(start_utc, end_utc)
            .await
            .unwrap_or_default();

        // If the overlay was dismissed/closed while waiting for ActivityWatch, do not re-open it
        if matches!(self.active_overlay_mode, OverlayMode::Hidden) {
            return;
        }

        let mut bin_seconds = vec![0.0f32; bin_count];
        let mut bin_labels = Vec::with_capacity(bin_count);
        let time_fmt = &self.config.stats.histogram_time_format;

        for i in 0..bin_count {
            let b_start = start_time + Duration::minutes((i as i64) * bin_size_mins);
            let b_end = b_start + Duration::minutes(bin_size_mins);

            let label = if i % 4 == 0 || i == bin_count - 1 {
                b_start.format(time_fmt).to_string()
            } else {
                String::new()
            };
            bin_labels.push(label);

            let b_start_utc = b_start.with_timezone(&Utc);
            let b_end_utc = b_end.with_timezone(&Utc);

            for ev in &events {
                bin_seconds[i] += event_overlap_seconds(ev, b_start_utc, b_end_utc) as f32;
            }
        }

        let max_bin_capacity = (bin_size_mins * 60) as f32;
        let bin_values: Vec<f32> = bin_seconds
            .iter()
            .map(|&s| (s / max_bin_capacity).clamp(0.0, 1.0))
            .collect();

        let total_active_seconds: f32 = bin_seconds.iter().sum();
        let total_active_minutes = (total_active_seconds / 60.0).round() as u32;

        let status_level = if total_active_minutes
            <= self.config.stats.daily_screentime_target_minutes
        {
            "Great"
        } else if total_active_minutes <= self.config.stats.threshold_excessive_minutes {
            "Okay"
        } else {
            "Excessive"
        };

        let mode = OverlayMode::HudStatus {
            title: "24-Hour Screen Time Summary".into(),
            status_level: status_level.into(),
            total_active_minutes,
            target_minutes: self.config.stats.daily_screentime_target_minutes,
            bin_values,
            bin_labels,
            is_loading: false,
            click_through,
        };

        self.show_overlay(mode);
    }

    /// Triggers a test overlay for UI verification using Option 3 structured paragraph syntax.
    pub async fn trigger_test_overlay(&mut self, mode: &str) {
        match mode {
            "break" => {
                let text = "# Take a Break\n\n[color=#38bdf8 size=18 bold] Rest your eyes and stretch for {break_time}.";
                let paragraphs = formatter::parse_formatted_text(text);
                let mode = OverlayMode::BreakEnforcer {
                    rule_id: "test_break".into(),
                    title: "Test Break Enforcer".into(),
                    paragraphs,
                    remaining_seconds: 300,
                    allow_snooze: true,
                    strict_mode: false,
                    click_through: false,
                };
                self.show_overlay(mode);
            }
            "hud_toast" => {
                let text = "## App Quota Warning\n[color=#f59e0b bold] You have {remaining} remaining for {app}.";
                let paragraphs = formatter::parse_formatted_text(text);
                let mode = OverlayMode::HudToast {
                    rule_id: "test_toast".into(),
                    title: "Quota Warning".into(),
                    paragraphs,
                    snooze_options_minutes: vec![5, 10, 15],
                    duration_seconds: 10,
                    remaining_seconds: 10,
                    show_time_remaining: true,
                };
                self.show_overlay(mode);
            }
            "hud_timer" => {
                let text = "[color=#f87171 bold] Time limit exceeded. Application closing in 30s.";
                let paragraphs = formatter::parse_formatted_text(text);
                let mode = OverlayMode::HudTimer {
                    rule_id: "test_timer".into(),
                    title: "Application Closing".into(),
                    paragraphs,
                    duration_seconds: 30,
                    remaining_seconds: 30,
                };
                self.show_overlay(mode);
            }
            "hud_status" => {
                self.display_stats_overlay(false).await;
            }
            _ => {}
        }
    }
}

/// Helper function to check if an event matches a unified rule's monitor criteria.
pub fn event_matches_rule(ev: &AwEvent, rule: &Rule, compiled_re: Option<&Regex>) -> bool {
    match rule.monitor_selection {
        MonitorSelection::Titles => {
            if let Some(re) = compiled_re {
                if let Some(title) = ev.data.get("title").and_then(|v| v.as_str()) {
                    return !title.is_empty() && re.is_match(title);
                }
            }
            false
        }
        MonitorSelection::Urls => {
            if let Some(re) = compiled_re {
                if let Some(url) = ev.data.get("url").and_then(|v| v.as_str()) {
                    return !url.is_empty() && re.is_match(url);
                }
            }
            false
        }
        MonitorSelection::Executables => {
            if rule.target_executables.is_empty()
                || rule.target_executables.iter().any(|t| t.trim() == "*")
            {
                return true;
            }
            if let Some(app) = ev.data.get("app").and_then(|v| v.as_str()) {
                for target in &rule.target_executables {
                    let trimmed = target.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let with_exe = format!("{trimmed}.exe");
                    if app.eq_ignore_ascii_case(trimmed) || app.eq_ignore_ascii_case(&with_exe) {
                        return true;
                    }
                }
            }
            false
        }
    }
}

/// Helper to check if a visible matching window exists for this rule and return the matched title/process name.
pub fn find_matching_window_for_rule(rule: &Rule, compiled_re: Option<&Regex>) -> Option<String> {
    match rule.monitor_selection {
        MonitorSelection::Titles => enforcer::find_matching_window(&[], compiled_re),
        MonitorSelection::Urls => {
            let browser_exes = [
                "chrome.exe".to_string(),
                "msedge.exe".to_string(),
                "firefox.exe".to_string(),
                "brave.exe".to_string(),
                "opera.exe".to_string(),
                "vivaldi.exe".to_string(),
            ];
            enforcer::find_matching_window(&browser_exes, None)
        }
        MonitorSelection::Executables => {
            if rule.target_executables.is_empty()
                || rule.target_executables.iter().any(|t| t.trim() == "*")
            {
                Some("Active Application".to_string())
            } else {
                enforcer::find_matching_window(&rule.target_executables, None)
            }
        }
    }
}

/// Computes continuous active streak by walking backward from now.
pub fn calculate_continuous_streak_for_rule(
    events: &[AwEvent],
    now: DateTime<Local>,
    last_break_completed_at: Option<DateTime<Local>>,
    reset_gap_secs: u32,
    rule: &Rule,
    compiled_re: Option<&Regex>,
) -> u32 {
    let now_utc = now.with_timezone(&Utc);
    let reset_gap_sec = reset_gap_secs as i64;
    let min_allowed_utc = last_break_completed_at.map(|dt| dt.with_timezone(&Utc));

    let mut relevant: Vec<&AwEvent> = events
        .iter()
        .filter(|ev| {
            if let Some(min_dt) = min_allowed_utc {
                let ev_end = ev.timestamp + Duration::milliseconds((ev.duration * 1000.0) as i64);
                if ev_end <= min_dt {
                    return false;
                }
            }

            event_matches_rule(ev, rule, compiled_re)
        })
        .collect();

    if relevant.is_empty() {
        return 0;
    }

    relevant.sort_by_key(|e| e.timestamp);

    let last_ev = relevant.last().unwrap();
    let last_ev_end =
        last_ev.timestamp + Duration::milliseconds((last_ev.duration * 1000.0) as i64);

    // If currently AFK for >= reset_gap_sec, streak has reset
    if (now_utc - last_ev_end).num_seconds() >= reset_gap_sec {
        return 0;
    }

    let mut streak_seconds = 0i64;
    let mut current_boundary = last_ev_end;

    for ev in relevant.iter().rev() {
        let ev_end = ev.timestamp + Duration::milliseconds((ev.duration * 1000.0) as i64);
        let gap = (current_boundary - ev_end).num_seconds();

        if gap >= reset_gap_sec {
            break; // Streak interrupted by AFK gap
        }

        let effective_start = if let Some(min_dt) = min_allowed_utc {
            ev.timestamp.max(min_dt)
        } else {
            ev.timestamp
        };
        let clamped_end = ev_end.min(current_boundary);

        if clamped_end > effective_start {
            streak_seconds += (clamped_end - effective_start).num_milliseconds() / 1000;
        }
        current_boundary = current_boundary.min(effective_start);
    }

    streak_seconds.max(0) as u32
}

/// Calculates total active usage seconds for a rule from a list of events.
pub fn calculate_usage_for_rule(
    events: &[AwEvent],
    rule: &Rule,
    compiled_regex: Option<&Regex>,
    start_of_day_utc: DateTime<Utc>,
    now_utc: DateTime<Utc>,
) -> u32 {
    let total_seconds: f64 = events
        .iter()
        .filter(|ev| event_matches_rule(ev, rule, compiled_regex))
        .map(|ev| event_overlap_seconds(ev, start_of_day_utc, now_utc))
        .sum();
    total_seconds.round() as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;

    /// Helper to construct a synthetic `AwEvent` with `app`, `title`, and `url` fields.
    fn make_event(start_utc: DateTime<Utc>, duration_secs: f64, app: &str, title: &str, url: &str) -> AwEvent {
        AwEvent {
            timestamp: start_utc,
            duration: duration_secs,
            data: json!({
                "app": app,
                "title": title,
                "url": url,
            }),
        }
    }

    /// Verifies `logical_day_start` rolls back to the previous calendar date when the current hour is before `day_start_hour`.
    #[test]
    fn test_logical_day_start() {
        let dt_early = Local.with_ymd_and_hms(2026, 10, 29, 2, 30, 0).unwrap();
        let l_start = logical_day_start(dt_early, 4);
        assert_eq!(l_start.date_naive(), NaiveDate::from_ymd_opt(2026, 10, 28).unwrap());
        assert_eq!(l_start.hour(), 4);

        let dt_late = Local.with_ymd_and_hms(2026, 10, 29, 5, 0, 0).unwrap();
        let l_start2 = logical_day_start(dt_late, 4);
        assert_eq!(l_start2.date_naive(), NaiveDate::from_ymd_opt(2026, 10, 29).unwrap());
        assert_eq!(l_start2.hour(), 4);
    }

    /// Verifies `calculate_continuous_streak_for_rule` across sub-reset gaps, AFK breaks, completion clipping, and overlapping events.
    #[test]
    fn test_continuous_streak_afk_gaps_clipping_and_overlap() {
        let now = Local.with_ymd_and_hms(2026, 10, 29, 12, 0, 0).unwrap();
        let now_utc = now.with_timezone(&Utc);
        let rule = Rule::default_template(0);
        let reset_gap_secs = 300; // 5 minutes

        // 1. Two 10-min events separated by 2 mins (< 5m reset gap) -> 20m (1200s) streak
        let ev1 = make_event(now_utc - Duration::minutes(22), 600.0, "code.exe", "main.rs", "");
        let ev2 = make_event(now_utc - Duration::minutes(10), 600.0, "code.exe", "main.rs", "");
        let streak = calculate_continuous_streak_for_rule(&[ev1.clone(), ev2.clone()], now, None, reset_gap_secs, &rule, None);
        assert_eq!(streak, 1200);

        // 2. Event separated by 6 mins (>= 5m reset gap) before ev1 is ignored
        let ev_old = make_event(now_utc - Duration::minutes(38), 600.0, "code.exe", "main.rs", "");
        let streak_with_gap = calculate_continuous_streak_for_rule(&[ev_old, ev1.clone(), ev2.clone()], now, None, reset_gap_secs, &rule, None);
        assert_eq!(streak_with_gap, 1200);

        // 3. Currently AFK for >= 5m -> streak resets to 0
        let future_now = now + Duration::minutes(5);
        let afk_streak = calculate_continuous_streak_for_rule(&[ev1.clone(), ev2.clone()], future_now, None, reset_gap_secs, &rule, None);
        assert_eq!(afk_streak, 0);

        // 4. Clipping against last_break_completed_at (5 mins into ev2 -> only 300s counted)
        let completed_at = (now_utc - Duration::minutes(5)).with_timezone(&Local);
        let clipped_streak = calculate_continuous_streak_for_rule(&[ev1, ev2], now, Some(completed_at), reset_gap_secs, &rule, None);
        assert_eq!(clipped_streak, 300);

        // 5. Overlapping events do not double-count
        let ov1 = make_event(now_utc - Duration::minutes(10), 600.0, "code.exe", "", "");
        let ov2 = make_event(now_utc - Duration::minutes(5), 300.0, "code.exe", "", "");
        let ov_streak = calculate_continuous_streak_for_rule(&[ov1, ov2], now, None, reset_gap_secs, &rule, None);
        assert_eq!(ov_streak, 600);
    }

    /// Verifies `calculate_usage_for_rule` clips cross-boundary events at `start_of_day_utc` and matches executables/URLs.
    #[test]
    fn test_calculate_usage_and_event_matching() {
        let now = Local.with_ymd_and_hms(2026, 10, 29, 5, 0, 0).unwrap();
        let start_of_day = logical_day_start(now, 4);
        let start_utc = start_of_day.with_timezone(&Utc);
        let now_utc = now.with_timezone(&Utc);

        // Event straddling 04:00 AM (03:30 to 04:30 = 30m before, 30m after)
        let ev_cross = make_event(start_utc - Duration::minutes(30), 3600.0, "STEAM", "Game", "");
        let mut rule = Rule::default_template(0);
        rule.monitor_selection = MonitorSelection::Executables;
        rule.target_executables = vec!["steam".to_string()]; // matches "STEAM" without .exe

        let usage = calculate_usage_for_rule(&[ev_cross], &rule, None, start_utc, now_utc);
        assert_eq!(usage, 1800); // Only the 30 minutes after 04:00 AM count

        // URL regex matching
        let mut url_rule = Rule::default_template(1);
        url_rule.monitor_selection = MonitorSelection::Urls;
        url_rule.url_regex = "(?i)youtube\\.com".to_string();
        let re = Regex::new(&url_rule.url_regex).ok();
        let ev_web = make_event(start_utc + Duration::minutes(10), 300.0, "chrome.exe", "Video", "https://www.youtube.com/watch?v=123");
        assert_eq!(calculate_usage_for_rule(&[ev_web], &url_rule, re.as_ref(), start_utc, now_utc), 300);
    }

    /// Verifies `OverlayMode::priority` prevents lower-priority overlays from clobbering active breaks, and tests strict-mode lockout.
    #[tokio::test]
    async fn test_overlay_priority_and_strict_mode_lockout() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();
        let mut strict_rule = Rule::default_template(0);
        strict_rule.action = RuleAction::ShowOverlay;
        strict_rule.strict_mode = true;
        cfg.rules = vec![strict_rule];
        let rule_id = cfg.rules[0].id.clone();
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        // 1. Higher-priority BreakEnforcer (3) is not overwritten by HudToast (2)
        engine.show_overlay(OverlayMode::BreakEnforcer {
            rule_id: rule_id.clone(),
            title: "Break".into(),
            paragraphs: Vec::new(),
            remaining_seconds: 60,
            allow_snooze: false,
            strict_mode: true,
            click_through: false,
        });
        engine.show_overlay(OverlayMode::HudToast {
            rule_id: "other".into(),
            title: "Toast".into(),
            paragraphs: Vec::new(),
            snooze_options_minutes: vec![5],
            duration_seconds: 10,
            remaining_seconds: 10,
            show_time_remaining: false,
        });
        assert!(matches!(engine.active_overlay_mode, OverlayMode::BreakEnforcer { .. }));

        // 2. Strict lock active blocks DismissBreakOverlay, SnoozeAll, and SetPauseAll
        if let Some(rt) = engine.runtimes.get_mut(&rule_id) {
            rt.active_break = Some((60, 60));
        }
        assert!(engine.is_strict_lock_active(Local::now()));
        engine.handle_command(DaemonCommand::DismissBreakOverlay).await;
        assert!(engine.runtimes.get(&rule_id).unwrap().active_break.is_some());

        engine.handle_command(DaemonCommand::SnoozeAll { minutes: 15 }).await;
        assert!(engine.global_snoozed_until.is_none());

        engine.handle_command(DaemonCommand::SetPauseAll { paused: true }).await;
        assert!(!engine.config.general.pause_all_rules);

        // 3. Once strict break finishes, SetPauseAll pauses all rules
        if let Some(rt) = engine.runtimes.get_mut(&rule_id) {
            rt.active_break = None;
        }
        engine.handle_command(DaemonCommand::SetPauseAll { paused: true }).await;
        assert!(engine.config.general.pause_all_rules);
    }

    /// Verifies `SnoozeBreak`, `SkipBreak`, and `DismissBreakOverlay` state transitions on `RuleRuntime`.
    #[tokio::test]
    async fn test_snooze_and_skip_break_state() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();
        cfg.rules = vec![Rule::default_template(0)];
        let rule_id = cfg.rules[0].id.clone();
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        // 1. First-cycle SnoozeBreak (when last_completed_at is None) initializes last_completed_at and resets action_executed_today
        if let Some(rt) = engine.runtimes.get_mut(&rule_id) {
            rt.active_break = Some((300, 300));
            rt.last_completed_at = None;
            rt.action_executed_today = true;
        }
        engine.handle_command(DaemonCommand::SnoozeBreak { rule_id: rule_id.clone(), minutes: 5 }).await;
        let rt = engine.runtimes.get(&rule_id).unwrap();
        assert!(rt.last_completed_at.is_some());
        assert!(!rt.action_executed_today);
        assert!(rt.active_break.is_none());
        assert!(rt.snoozed_until.is_some());

        // 2. Subsequent SnoozeBreak preserves existing last_completed_at
        let prev_completed = Local.with_ymd_and_hms(2026, 10, 29, 10, 0, 0).unwrap();
        if let Some(rt) = engine.runtimes.get_mut(&rule_id) {
            rt.last_completed_at = Some(prev_completed);
        }
        engine.handle_command(DaemonCommand::SnoozeBreak { rule_id: rule_id.clone(), minutes: 5 }).await;
        assert_eq!(engine.runtimes.get(&rule_id).unwrap().last_completed_at, Some(prev_completed));

        // 3. SkipBreak sets action_executed_today = true and updates last_completed_at
        engine.handle_command(DaemonCommand::SkipBreak { rule_id: rule_id.clone() }).await;
        let rt2 = engine.runtimes.get(&rule_id).unwrap();
        assert!(rt2.action_executed_today);
        assert!(rt2.last_completed_at.unwrap() >= prev_completed);

        // 4. DismissBreakOverlay sets action_executed_today = true
        if let Some(rt) = engine.runtimes.get_mut(&rule_id) {
            rt.active_break = Some((300, 300));
            rt.action_executed_today = false;
        }
        engine.handle_command(DaemonCommand::DismissBreakOverlay).await;
        let rt3 = engine.runtimes.get(&rule_id).unwrap();
        assert!(rt3.action_executed_today);
        assert!(rt3.active_break.is_none());
    }
}
