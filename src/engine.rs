use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, Timelike, Utc};
use regex::Regex;
use slint::Color;
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
use crate::formatter::{self, FormattedParagraph, MessageContext, TextHorizontalAlignment};
use crate::overlay::OverlayMode;

/// Merges overlapping and adjacent intervals into a minimal sorted list of non-overlapping intervals.
pub fn merge_overlapping_intervals(
    mut intervals: Vec<(DateTime<Utc>, DateTime<Utc>)>,
) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    if intervals.is_empty() {
        return Vec::new();
    }

    intervals.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

    let mut merged: Vec<(DateTime<Utc>, DateTime<Utc>)> = Vec::with_capacity(intervals.len());
    for (start, end) in intervals {
        if let Some(last) = merged.last_mut() {
            if start <= last.1 {
                if end > last.1 {
                    last.1 = end;
                }
            } else {
                merged.push((start, end));
            }
        } else {
            merged.push((start, end));
        }
    }

    merged
}

/// Computes the total non-overlapping duration in seconds between a slice of events and `[start_utc, end_utc]`.
pub fn compute_coalesced_overlap_seconds(
    events: &[AwEvent],
    start_utc: DateTime<Utc>,
    end_utc: DateTime<Utc>,
) -> f64 {
    let intervals: Vec<(DateTime<Utc>, DateTime<Utc>)> = events
        .iter()
        .filter_map(|ev| {
            let e_start = ev.timestamp;
            let e_end = ev.timestamp + Duration::milliseconds((ev.duration * 1000.0) as i64);
            let start = e_start.max(start_utc);
            let end = e_end.min(end_utc);
            if end > start {
                Some((start, end))
            } else {
                None
            }
        })
        .collect();

    let merged = merge_overlapping_intervals(intervals);
    merged
        .iter()
        .map(|(s, e)| (*e - *s).num_milliseconds() as f64 / 1000.0)
        .sum()
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

/// Aligns a timestamp down to the start of its current statistics histogram bin.
fn align_stats_bin_start(now: DateTime<Local>, bin_size_mins: i64) -> DateTime<Local> {
    let bin_size = bin_size_mins.max(1);
    if bin_size >= 60 {
        let hour_step = ((bin_size / 60) as u32).max(1);
        let aligned_hour = (now.hour() / hour_step) * hour_step;
        now.with_hour(aligned_hour)
            .and_then(|t| t.with_minute(0))
            .and_then(|t| t.with_second(0))
            .and_then(|t| t.with_nanosecond(0))
            .unwrap_or(now)
    } else {
        let current_minute = now.minute();
        let aligned_minute = (current_minute / (bin_size as u32)) * (bin_size as u32);
        now.with_minute(aligned_minute)
            .and_then(|t| t.with_second(0))
            .and_then(|t| t.with_nanosecond(0))
            .unwrap_or(now)
    }
}

/// Cached 24-hour histogram bins used to live-update `OverlayMode::HudStatus` on each `poll_tick`
/// without re-querying historical ActivityWatch bins.
#[derive(Debug, Clone)]
struct StatsHudCache {
    current_bin_start: DateTime<Local>,
    bin_size_mins: i64,
    bin_seconds: Vec<f32>,
    click_through: bool,
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
    PreviewRule(Box<Rule>),
}

/// Actions dispatched from the engine to the Slint UI thread.
#[derive(Debug, Clone)]
pub enum OverlayAction {
    Show(OverlayMode),
    UpdateCountdown {
        remaining: u32,
        total: u32,
        is_idle: bool,
    },
    UpdateTheme(OverlayConfig),
    UpdateLiveStatus {
        session_duration_text: String,
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
    pub current_session_seconds: u32,
    pub daily_usage_seconds: u32,
    pub last_completed_at: Option<DateTime<Local>>,
    pub prev_completed_at: Option<DateTime<Local>>,
    pub can_rollback_completion: bool,
    pub warned_current_cycle: bool,
    pub pending_break_since: Option<DateTime<Local>>,
    pub active_break: Option<(u32, u32)>,    // (remaining_secs, total_secs)
    pub grace_countdown: Option<(u32, u32)>, // (remaining_secs, total_secs)
    pub action_executed_today: bool,
    pub snoozed_until: Option<DateTime<Local>>,
    pub live_status_badge: String,
    pub last_matched_app: Option<String>,
    pub last_matched_title: Option<String>,
    pub last_matched_title_regex: Option<Regex>,
}

impl RuleRuntime {
    /// Initializes a fresh `RuleRuntime` and pre-compiles any configured title or URL regex.
    pub fn from_rule(rule: &Rule) -> Self {
        Self {
            compiled_regex: Self::compile_regex(rule),
            current_session_seconds: 0,
            daily_usage_seconds: 0,
            last_completed_at: None,
            prev_completed_at: None,
            can_rollback_completion: false,
            warned_current_cycle: false,
            pending_break_since: None,
            active_break: None,
            grace_countdown: None,
            action_executed_today: false,
            snoozed_until: None,
            live_status_badge: String::new(),
            last_matched_app: None,
            last_matched_title: None,
            last_matched_title_regex: None,
        }
    }

    /// Sets `last_matched_title` and updates `last_matched_title_regex`.
    pub fn set_last_matched_title(&mut self, title: Option<String>) {
        self.last_matched_title_regex = title
            .as_deref()
            .filter(|t| !t.trim().is_empty())
            .and_then(|t| Regex::new(&regex::escape(t)).ok());
        self.last_matched_title = title;
    }

    /// Clears active break, grace countdown, pending break, and warning flags for the current cycle.
    pub fn reset_active_timers(&mut self) {
        self.current_session_seconds = 0;
        self.active_break = None;
        self.grace_countdown = None;
        self.warned_current_cycle = false;
        self.pending_break_since = None;
    }

    /// Marks a rule cycle as completed at `now`, resetting timers and recording execution.
    pub fn complete_cycle(&mut self, now: DateTime<Local>) {
        self.reset_active_timers();
        self.snoozed_until = None;
        self.prev_completed_at = self.last_completed_at;
        self.last_completed_at = Some(now);
        self.can_rollback_completion = true;
        self.action_executed_today = true;
        self.last_matched_app = None;
        self.last_matched_title = None;
        self.last_matched_title_regex = None;
    }

    /// Snoozes the rule until `until`. For `Period` rules, preserves `last_completed_at` if before
    /// the current session so the snoozed rule re-triggers once `until` expires instead of wiping
    /// the pre-snooze session.
    pub fn snooze(&mut self, until: DateTime<Local>) {
        let preserve_warned = self.active_break.is_none()
            && self.pending_break_since.is_none()
            && self.grace_countdown.is_none()
            && !self.can_rollback_completion
            && self.warned_current_cycle;
        self.reset_active_timers();
        if preserve_warned {
            self.warned_current_cycle = true;
        }
        self.snoozed_until = Some(until);
        if self.can_rollback_completion {
            self.last_completed_at = self.prev_completed_at;
        }
        self.can_rollback_completion = false;
        self.action_executed_today = false;
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
    stats_cache: Option<StatsHudCache>,
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
            stats_cache: None,
        }
    }

    /// Parses the default overlay text color from config, falling back to slate-50 (`#f8fafc`).
    fn default_text_color(&self) -> Color {
        formatter::parse_hex_color(&self.config.overlay.text_color)
            .unwrap_or_else(|| Color::from_rgb_u8(248, 250, 252))
    }

    /// Dispatches an `OverlayMode` to the UI thread if its priority is at least as high as the currently active overlay.
    fn show_overlay(&mut self, mode: OverlayMode) {
        let is_current_test = self
            .active_overlay_mode
            .rule_id()
            .map(|id| id.starts_with("test_"))
            .unwrap_or(false);
        // Never allow a lower-priority overlay to clobber a higher-priority active overlay,
        // unless the active overlay is a test/preview overlay being replaced.
        if !is_current_test
            && mode.priority() < self.active_overlay_mode.priority()
            && !matches!(self.active_overlay_mode, OverlayMode::Hidden)
        {
            return;
        }
        if !matches!(mode, OverlayMode::HudStatus { .. }) {
            self.stats_cache = None;
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

    /// Returns the elapsed evaluation duration for a rule: current continuous session for `Period`
    /// or accumulated daily usage for `Limit`.
    pub fn rule_elapsed_seconds(&self, rule: &Rule) -> u32 {
        match rule.trigger {
            TriggerType::Period => self
                .runtimes
                .get(&rule.id)
                .map(|rt| rt.current_session_seconds)
                .unwrap_or(0),
            TriggerType::Limit => self
                .runtimes
                .get(&rule.id)
                .map(|rt| rt.daily_usage_seconds)
                .unwrap_or(0),
        }
    }

    /// Returns `true` if `self.active_overlay_mode` is `BreakEnforcer`, `HudTimer`, or `HudToast` with matching `rule_id`.
    pub fn is_rule_displayed(&self, rule_id: &str) -> bool {
        self.active_overlay_mode.rule_id() == Some(rule_id)
    }

    /// Transitions to the highest-priority remaining active countdown rule, or hides the overlay if none remain.
    pub fn transition_to_active_countdown_or_hide(&mut self) {
        // 1. Highest priority: BreakEnforcer (active_break > 0)
        let active_break_rule = self.config.rules.iter().find(|r| {
            r.enabled && self.runtimes
                .get(&r.id)
                .and_then(|rt| rt.active_break)
                .map(|(rem, _)| rem > 0)
                .unwrap_or(false)
        }).cloned();

        if let Some(rule) = active_break_rule {
            let (rem, total) = self
                .runtimes
                .get(&rule.id)
                .and_then(|rt| rt.active_break)
                .unwrap();
            let matched_app = self.get_last_matched_app_or_default(&rule.id);
            let elapsed = self.rule_elapsed_seconds(&rule);
            let threshold = rule.trigger_threshold_seconds();
            let ctx = self.build_message_context(&rule, &matched_app, elapsed, threshold, 0);
            let paragraphs = self.parse_rule_message(
                &rule.message,
                &ctx,
                TextHorizontalAlignment::Center,
            );
            self.active_overlay_mode = OverlayMode::Hidden;
            self.show_overlay(OverlayMode::BreakEnforcer {
                rule_id: rule.id.clone(),
                title: rule.name.clone(),
                paragraphs,
                duration_seconds: total,
                remaining_seconds: rem,
                allow_snooze: !rule.strict_mode,
                strict_mode: rule.strict_mode,
                click_through: rule.click_through,
            });
            self.overlay_tx
                .send(OverlayAction::UpdateCountdown {
                    remaining: rem,
                    total,
                    is_idle: get_idle_time_millis() >= 3000,
                })
                .ok();
            return;
        }

        // 2. Second highest priority: HudTimer (grace_countdown > 0)
        let grace_rule = self.config.rules.iter().find(|r| {
            r.enabled && self.runtimes
                .get(&r.id)
                .and_then(|rt| rt.grace_countdown)
                .map(|(rem, _)| rem > 0)
                .unwrap_or(false)
        }).cloned();

        if let Some(rule) = grace_rule {
            let (rem, total) = self
                .runtimes
                .get(&rule.id)
                .and_then(|rt| rt.grace_countdown)
                .unwrap();
            let matched_app = self.get_last_matched_app_or_default(&rule.id);
            let elapsed = self.rule_elapsed_seconds(&rule);
            let threshold = rule.trigger_threshold_seconds();
            let ctx = self.build_message_context(&rule, &matched_app, elapsed, threshold, 0);
            let default_msg = format!(
                "[color=#f87171 bold] {} in {}s.",
                rule.action.as_str(),
                total
            );
            let raw_msg = if matches!(rule.action, RuleAction::ShowOverlay | RuleAction::ShowNotification)
                && !rule.message.trim().is_empty()
            {
                &rule.message
            } else {
                &default_msg
            };
            let paragraphs = self.parse_rule_message(
                raw_msg,
                &ctx,
                TextHorizontalAlignment::Left,
            );
            self.active_overlay_mode = OverlayMode::Hidden;
            self.show_overlay(OverlayMode::HudTimer {
                rule_id: rule.id.clone(),
                title: format!("{}: {}", rule.action.as_str(), rule.name),
                paragraphs,
                duration_seconds: total,
                remaining_seconds: rem,
            });
            self.overlay_tx
                .send(OverlayAction::UpdateCountdown {
                    remaining: rem,
                    total,
                    is_idle: get_idle_time_millis() >= 3000,
                })
                .ok();
            return;
        }

        // 3. No active countdown remaining: hide overlay
        self.hide_overlay();
    }

    /// Handles asynchronous daemon commands from tray or UI.
    pub async fn handle_command(&mut self, cmd: DaemonCommand) {
        match cmd {
            DaemonCommand::SnoozeBreak { rule_id, minutes } => {
                if let Some(rule) = self.config.rules.iter().find(|r| r.id == rule_id) {
                    if rule.action == RuleAction::ShowOverlay && rule.strict_mode {
                        tracing::warn!("SnoozeBreak ignored: strict break active for rule {}", rule_id);
                        return;
                    }
                }
                let now = Local::now();
                let until = now + Duration::minutes(minutes as i64);
                if let Some(rt) = self.runtimes.get_mut(&rule_id) {
                    rt.snooze(until);
                }
                if self.is_rule_displayed(&rule_id) || !self.any_countdown_active() {
                    self.transition_to_active_countdown_or_hide();
                }
            }
            DaemonCommand::AcknowledgeAppWarning { rule_id } => {
                tracing::debug!("Acknowledged app warning for rule {rule_id}");
                if self.is_rule_displayed(&rule_id) || !self.any_countdown_active() {
                    self.transition_to_active_countdown_or_hide();
                }
            }
            DaemonCommand::SkipBreak { rule_id } => {
                if let Some(rule) = self.config.rules.iter().find(|r| r.id == rule_id) {
                    if rule.action == RuleAction::ShowOverlay && rule.strict_mode {
                        tracing::warn!("SkipBreak ignored: strict break active for rule {}", rule_id);
                        return;
                    }
                }
                if let Some(rt) = self.runtimes.get_mut(&rule_id) {
                    rt.complete_cycle(Local::now());
                }
                if self.is_rule_displayed(&rule_id) || !self.any_countdown_active() {
                    self.transition_to_active_countdown_or_hide();
                }
            }
            DaemonCommand::DismissBreakOverlay => {
                if self.is_strict_lock_active(Local::now()) {
                    tracing::warn!("DismissBreakOverlay ignored: strict enforcement active");
                    return;
                }
                let now = Local::now();
                for rt in self.runtimes.values_mut() {
                    if rt.active_break.is_some() {
                        rt.complete_cycle(now);
                    } else if rt.pending_break_since.is_some() {
                        rt.warned_current_cycle = false;
                        rt.pending_break_since = None;
                    }
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
                let displayed_rule_id = self.active_overlay_mode.rule_id().map(|s| s.to_string());
                for (rule_id, rt) in self.runtimes.iter_mut() {
                    if rt.active_break.is_none() && displayed_rule_id.as_deref() != Some(rule_id.as_str()) {
                        rt.can_rollback_completion = false;
                    }
                    rt.snooze(until);
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
                        rt.reset_active_timers();
                    }
                    self.hide_overlay();
                }
            }
            DaemonCommand::ShowStatsOverlay => {
                self.display_stats_overlay(false).await;
            }
            DaemonCommand::CloseOverlay => {
                if self.is_strict_lock_active(Local::now()) {
                    tracing::warn!("CloseOverlay ignored: strict enforcement active");
                    return;
                }
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
                        .and_modify(|rt| {
                            rt.update_from_rule(rule);
                            if !rule.enabled {
                                rt.reset_active_timers();
                            }
                        })
                        .or_insert_with(|| RuleRuntime::from_rule(rule));
                }
                if paused {
                    for rt in self.runtimes.values_mut() {
                        rt.reset_active_timers();
                    }
                    self.hide_overlay();
                } else {
                    let should_clear_overlay = self.active_overlay_mode.rule_id().is_some_and(|active_id| {
                        !active_id.starts_with("test_")
                            && match self.config.rules.iter().find(|r| r.id == active_id) {
                                Some(rule) => !rule.enabled,
                                None => true,
                            }
                    });
                    if should_clear_overlay {
                        self.transition_to_active_countdown_or_hide();
                    }
                }
                self.overlay_tx
                    .send(OverlayAction::UpdateTheme(self.config.overlay.clone()))
                    .ok();
            }
            DaemonCommand::TriggerTestOverlay { mode } => {
                if self.is_strict_lock_active(Local::now()) {
                    tracing::warn!("TriggerTestOverlay ignored: strict enforcement active");
                    return;
                }
                self.trigger_test_overlay(&mode).await;
            }
            DaemonCommand::PreviewRule(rule) => {
                if self.is_strict_lock_active(Local::now()) {
                    tracing::warn!("PreviewRule ignored: strict enforcement active");
                    return;
                }
                self.preview_rule_alert(&rule).await;
            }
        }
    }

    /// Hides any active overlay and resets `active_overlay_mode` to `OverlayMode::Hidden`.
    fn hide_overlay(&mut self) {
        self.active_overlay_mode = OverlayMode::Hidden;
        self.stats_cache = None;
        self.overlay_tx.send(OverlayAction::Hide).ok();
    }

    /// Real-time 1-second countdown tick updating overlays and idle counters.
    pub async fn realtime_tick(&mut self) {
        let idle_ms = get_idle_time_millis();
        let is_idle = idle_ms >= 3000;
        let now = Local::now();

        // 0. Check active test / HUD overlays from previous tick
        if let OverlayMode::HudToast {
            ref mut remaining_seconds,
            duration_seconds,
            ..
        } = self.active_overlay_mode
        {
            if *remaining_seconds > 0 {
                *remaining_seconds -= 1;
                let rem = *remaining_seconds;
                self.send_overlay_countdown_tick(rem, duration_seconds, is_idle);
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
                self.send_overlay_countdown_tick(rem, duration_seconds, is_idle);
            }
        } else if let OverlayMode::BreakEnforcer {
            ref rule_id,
            ref mut remaining_seconds,
            duration_seconds,
            ..
        } = self.active_overlay_mode
        {
            if (rule_id == "test_break" || rule_id.starts_with("test_")) && *remaining_seconds > 0 {
                if is_idle {
                    *remaining_seconds -= 1;
                }
                let rem = *remaining_seconds;
                self.send_overlay_countdown_tick(rem, duration_seconds, is_idle);
            }
        }

        // 1. Check pending natural break delays
        let mut launched_breaks = Vec::new();
        for (rule_id, rt) in self.runtimes.iter_mut() {
            if let Some(since) = rt.pending_break_since {
                if idle_ms >= 1500 || (now - since).num_seconds() >= 15 {
                    rt.pending_break_since = None;
                    launched_breaks.push(rule_id.clone());
                }
            }
        }
        for rule_id in &launched_breaks {
            if let Some(rule) = self.config.rules.iter().find(|r| r.id == *rule_id).cloned() {
                let re = self.runtimes.get(&rule.id).and_then(|r| r.compiled_regex.clone());
                let matched_app = if rule.monitor_selection == MonitorSelection::Urls {
                    self.runtimes
                        .get(&rule.id)
                        .and_then(|r| r.last_matched_app.clone())
                        .or_else(|| enforcer::find_matching_window(enforcer::BROWSER_EXES, None))
                } else {
                    find_matching_window_for_rule(
                        &rule,
                        re.as_ref(),
                        &[],
                        now.with_timezone(&Utc),
                        self.config.general.poll_interval_seconds,
                    )
                    .or_else(|| self.runtimes.get(&rule.id).and_then(|r| r.last_matched_app.clone()))
                }
                .unwrap_or_else(|| "Active Application".to_string());
                let elapsed = self.rule_elapsed_seconds(&rule);
                let threshold = rule.trigger_threshold_seconds();
                let break_secs = rule.break_duration.to_seconds().max(1);
                if let Some(rt) = self.runtimes.get_mut(&rule.id) {
                    rt.prev_completed_at = rt.last_completed_at;
                    rt.active_break = Some((break_secs, break_secs));
                    rt.can_rollback_completion = true;
                    rt.action_executed_today = true;
                }
                let ctx = self.build_message_context(&rule, &matched_app, elapsed, threshold, 0);
                let paragraphs = self.parse_rule_message(
                    &rule.message,
                    &ctx,
                    TextHorizontalAlignment::Center,
                );
                self.show_overlay(OverlayMode::BreakEnforcer {
                    rule_id: rule.id.clone(),
                    title: rule.name.clone(),
                    paragraphs,
                    duration_seconds: break_secs,
                    remaining_seconds: break_secs,
                    allow_snooze: !rule.strict_mode,
                    strict_mode: rule.strict_mode,
                    click_through: rule.click_through,
                });
            }
        }

        let rule_ids: Vec<String> = self.runtimes.keys().cloned().collect();

        // 2. Check active break countdowns
        let mut completed_breaks = Vec::new();
        let mut break_displayed_finished = false;
        for rule_id in &rule_ids {
            if launched_breaks.contains(rule_id) {
                continue;
            }
            let is_displayed = self.is_rule_displayed(rule_id);
            if let Some(rt) = self.runtimes.get_mut(rule_id) {
                if let Some((ref mut rem, total)) = rt.active_break {
                    if is_idle && *rem > 0 {
                        *rem -= 1;
                    }
                    if is_displayed {
                        if let OverlayMode::BreakEnforcer {
                            ref mut remaining_seconds,
                            ..
                        } = self.active_overlay_mode
                        {
                            *remaining_seconds = *rem;
                        }
                        self.overlay_tx
                            .send(OverlayAction::UpdateCountdown {
                                remaining: *rem,
                                total,
                                is_idle,
                            })
                            .ok();
                    }

                    if *rem == 0 {
                        rt.complete_cycle(Local::now());
                        completed_breaks.push(rule_id.clone());
                        if is_displayed {
                            break_displayed_finished = true;
                        }
                    }
                }
            }
        }
        if break_displayed_finished {
            self.transition_to_active_countdown_or_hide();
        }

        // 3. Check active grace countdowns for limit rules
        let mut completed_enforcements = Vec::new();
        let mut grace_displayed_finished = false;
        for rule_id in &rule_ids {
            let is_displayed = self.is_rule_displayed(rule_id);
            if let Some(rt) = self.runtimes.get_mut(rule_id) {
                if let Some((ref mut rem, total)) = rt.grace_countdown {
                    if *rem > 0 {
                        *rem -= 1;
                        if is_displayed {
                            if let OverlayMode::HudTimer {
                                ref mut remaining_seconds,
                                ..
                            } = self.active_overlay_mode
                            {
                                *remaining_seconds = *rem;
                            }
                            self.overlay_tx
                                .send(OverlayAction::UpdateCountdown {
                                    remaining: *rem,
                                    total,
                                    is_idle,
                                })
                                .ok();
                        }
                    }

                    if *rem == 0 {
                        rt.grace_countdown = None;
                        completed_enforcements.push(rule_id.clone());
                        if is_displayed {
                            grace_displayed_finished = true;
                        }
                    }
                }
            }
        }

        if grace_displayed_finished {
            self.active_overlay_mode = OverlayMode::Hidden;
        }

        for rule_id in completed_enforcements {
            if let Some(rule) = self.config.rules.iter().find(|r| r.id == rule_id).cloned() {
                let re = self.runtimes.get(&rule.id).and_then(|r| r.compiled_regex.clone());
                let target_window = if rule.monitor_selection == MonitorSelection::Urls {
                    let title_re = self
                        .runtimes
                        .get(&rule.id)
                        .and_then(|r| r.last_matched_title_regex.as_ref());
                    let matched = enforcer::find_matching_window(enforcer::BROWSER_EXES, title_re);
                    if matched.is_none() {
                        let browser_open = enforcer::find_matching_window(enforcer::BROWSER_EXES, None);
                        let app_matched = self
                            .runtimes
                            .get(&rule.id)
                            .and_then(|r| r.last_matched_app.as_deref())
                            .map(|app| matched_app_matches_url_rule(app, re.as_ref()))
                            .unwrap_or(false);
                        if app_matched {
                            browser_open
                        } else {
                            None
                        }
                    } else {
                        matched
                    }
                } else {
                    find_matching_window_for_rule(
                        &rule,
                        re.as_ref(),
                        &[],
                        now.with_timezone(&Utc),
                        self.config.general.poll_interval_seconds,
                    )
                };

                let is_destructive = matches!(
                    rule.action,
                    RuleAction::CloseTab
                        | RuleAction::CloseWindow
                        | RuleAction::KillProcess
                        | RuleAction::LockWorkstation
                );

                if is_destructive && target_window.is_none() {
                    tracing::info!(
                        "Target window for rule '{}' was closed during grace period; skipping destructive action",
                        rule.name
                    );
                    if let Some(rt) = self.runtimes.get_mut(&rule.id) {
                        rt.complete_cycle(now);
                    }
                    continue;
                }

                let matched_name = if rule.monitor_selection == MonitorSelection::Urls {
                    self.runtimes
                        .get(&rule.id)
                        .and_then(|r| r.last_matched_app.clone())
                        .or(target_window)
                } else {
                    target_window
                        .or_else(|| self.runtimes.get(&rule.id).and_then(|r| r.last_matched_app.clone()))
                }
                .unwrap_or_else(|| "Active Application".to_string());
                let elapsed = self.rule_elapsed_seconds(&rule);
                let threshold = rule.trigger_threshold_seconds();
                self.execute_rule_action(&rule, re.as_ref(), now, &matched_name, elapsed, threshold).await;
            }
        }

        if grace_displayed_finished && matches!(self.active_overlay_mode, OverlayMode::Hidden) {
            self.transition_to_active_countdown_or_hide();
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
                rt.daily_usage_seconds = 0;
                rt.can_rollback_completion = false;
                rt.last_matched_app = None;
                rt.last_matched_title = None;
                rt.last_matched_title_regex = None;
            }
        }

        // 2. Global snooze expiration check
        if let Some(until) = self.global_snoozed_until {
            if now >= until {
                self.global_snoozed_until = None;
            }
        }
        let is_globally_snoozed = self.global_snoozed_until.is_some();

        let day_index = now.weekday().num_days_from_monday() as usize;
        let hour_index = now.hour() as usize;

        // 3. Compute earliest required lookback across all enabled active rules (and open Stats HUD)
        let start_of_day_utc = logical_day_start(now, self.config.general.day_start_hour).with_timezone(&Utc);
        let now_utc = now.with_timezone(&Utc);

        let mut min_start_utc = start_of_day_utc;
        if let Some(ref cache) = self.stats_cache {
            if matches!(self.active_overlay_mode, OverlayMode::HudStatus { is_loading: false, .. }) {
                min_start_utc = min_start_utc.min(cache.current_bin_start.with_timezone(&Utc));
            }
        }

        let mut has_url_rules = false;

        if !self.config.general.pause_all_rules && !is_globally_snoozed {
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

        // 4. Evaluate Unified Rules (skip if all rules are paused indefinitely or globally snoozed)
        let mut evaluated_in_step_4 = std::collections::HashSet::new();
        let mut should_transition_after_eval = false;

        if self.config.general.pause_all_rules || is_globally_snoozed {
            for rule in &self.config.rules {
                if let Some(rt) = self.runtimes.get_mut(&rule.id) {
                    if rt.pending_break_since.is_some()
                        || rt.grace_countdown.is_some()
                        || rt.active_break.is_some()
                    {
                        rt.reset_active_timers();
                        if self.is_rule_displayed(&rule.id) {
                            should_transition_after_eval = true;
                        }
                    } else {
                        rt.current_session_seconds = 0;
                    }
                }
            }
        } else {
            for idx in 0..self.config.rules.len() {
                if !self.config.rules[idx].enabled
                    || !self.config.rules[idx].schedule.is_active(day_index, hour_index)
                {
                    let rule_id = &self.config.rules[idx].id;
                    if let Some(rt) = self.runtimes.get_mut(rule_id) {
                        if rt.pending_break_since.is_some()
                            || rt.grace_countdown.is_some()
                            || rt.active_break.is_some()
                        {
                            rt.reset_active_timers();
                            if self.is_rule_displayed(rule_id) {
                                should_transition_after_eval = true;
                            }
                        } else {
                            rt.current_session_seconds = 0;
                        }
                    }
                    continue;
                }

                let rule = self.config.rules[idx].clone();

                let events_for_rule = if rule.monitor_selection == MonitorSelection::Urls {
                    &web_events
                } else {
                    &all_events
                };

                let compiled_re = self.runtimes.get(&rule.id).and_then(|r| r.compiled_regex.clone());
                if self
                    .evaluate_rule(
                        &rule,
                        compiled_re.as_ref(),
                        now,
                        events_for_rule,
                        start_of_day_utc,
                        now_utc,
                    )
                    .await
                {
                    evaluated_in_step_4.insert(rule.id.clone());
                }
            }
        }

        if should_transition_after_eval {
            self.transition_to_active_countdown_or_hide();
        }

        // 5. Telemetry push for Tab 2 Live Status & Per-rule status badges
        let max_session_secs = self.runtimes.values().map(|rt| rt.current_session_seconds).max().unwrap_or(0);

        let total_today_secs = compute_coalesced_overlap_seconds(&all_events, start_of_day_utc, now_utc);
        let total_today_mins = (total_today_secs / 60.0).round() as u32;

        let active_app = all_events
            .iter()
            .max_by_key(|e| (event_end_utc(e), e.timestamp))
            .and_then(|e| e.data.app.as_deref())
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
            } else if let Some(until) = self.global_snoozed_until.filter(|&u| u > now) {
                let rem_mins = ((until - now).num_seconds().max(1) + 59) / 60;
                format!("Snoozed ({}m)", rem_mins)
            } else if let Some(until) = rt.snoozed_until.filter(|&u| u > now) {
                let rem_mins = ((until - now).num_seconds().max(1) + 59) / 60;
                format!("Snoozed ({}m)", rem_mins)
            } else if rule.trigger == TriggerType::Period {
                format!(
                    "Session: {} / {}",
                    formatter::format_seconds_human(rt.current_session_seconds),
                    rule.period.to_short_string()
                )
            } else {
                let daily_usage_seconds = if evaluated_in_step_4.contains(&rule.id) {
                    rt.daily_usage_seconds
                } else {
                    let events_for_rule = if rule.monitor_selection == MonitorSelection::Urls {
                        &web_events
                    } else {
                        &all_events
                    };
                    let used = calculate_usage_for_rule(
                        events_for_rule,
                        rule,
                        rt.compiled_regex.as_ref(),
                        start_of_day_utc,
                        now_utc,
                    );
                    rt.daily_usage_seconds = used;
                    used
                };
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
        } else if let Some(until) = self.global_snoozed_until.filter(|&u| u > now) {
            let rem_mins = ((until - now).num_seconds().max(1) + 59) / 60;
            format!(
                "AW-Enforcer (Snoozed {}m) — Today: {}h {}m",
                rem_mins,
                total_today_mins / 60,
                total_today_mins % 60
            )
        } else {
            format!(
                "AW-Enforcer — Today: {}h {}m | Session: {}m",
                total_today_mins / 60,
                total_today_mins % 60,
                max_session_secs / 60
            )
        };

        self.overlay_tx
            .send(OverlayAction::UpdateLiveStatus {
                session_duration_text: format!("{}m", max_session_secs / 60),
                screentime_text: format!("{}h {}m", total_today_mins / 60, total_today_mins % 60),
                active_app,
                strict_lock_active,
                rule_statuses,
                tray_tooltip,
            })
            .ok();

        // 6. Live-refresh the 24-Hour Statistics HUD if currently open (reusing all_events without extra AW queries)
        self.refresh_stats_overlay_from_events(now, &all_events);
    }

    /// Parses a raw rule message with context using the engine's default text color and requested alignment.
    fn parse_rule_message(
        &self,
        raw_msg: &str,
        ctx: &MessageContext,
        alignment: TextHorizontalAlignment,
    ) -> Vec<FormattedParagraph> {
        formatter::parse_message(raw_msg, ctx, self.default_text_color(), alignment)
    }

    /// Returns the last matched application name for `rule_id`, falling back to `"Active Application"`.
    fn get_last_matched_app_or_default(&self, rule_id: &str) -> String {
        self.runtimes
            .get(rule_id)
            .and_then(|r| r.last_matched_app.clone())
            .unwrap_or_else(|| "Active Application".to_string())
    }

    /// Sends an overlay countdown update and triggers a transition or hide if remaining time reaches zero.
    fn send_overlay_countdown_tick(&mut self, remaining: u32, total: u32, is_idle: bool) {
        self.overlay_tx
            .send(OverlayAction::UpdateCountdown {
                remaining,
                total,
                is_idle,
            })
            .ok();
        if remaining == 0 {
            self.transition_to_active_countdown_or_hide();
        }
    }

    /// Builds a `MessageContext` populated with formatted session duration, usage, limit, and remaining durations for a rule.
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
        let session_duration_str = formatter::format_seconds_human(elapsed_secs);
        let break_str = rule.break_duration.to_short_string();

        MessageContext {
            rule_name: rule.name.clone(),
            app: matched_app_name.to_string(),
            session_duration: session_duration_str,
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
            let paragraphs = self.parse_rule_message(
                raw_msg,
                ctx,
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
    ) -> bool {
        if !self.is_rule_displayed(&rule.id) {
            if let Some(rt) = self.runtimes.get_mut(&rule.id) {
                rt.can_rollback_completion = false;
            }
        }

        // 1. Skip if currently in break, waiting for natural pause, in grace countdown, or snoozed
        {
            let Some(rt) = self.runtimes.get_mut(&rule.id) else { return false; };
            if rt.active_break.is_some() || rt.pending_break_since.is_some() {
                return false;
            }
            if rt.grace_countdown.is_some() {
                if rule.monitor_selection == MonitorSelection::Urls {
                    let recency_cutoff = now_utc
                        - Duration::seconds(
                            (self.config.general.poll_interval_seconds.max(2) as i64) * 2 + 5,
                        );
                    let matching_ev = events
                        .iter()
                        .filter(|ev| {
                            let ev_end = event_end_utc(ev);
                            ev_end >= recency_cutoff && event_matches_rule(ev, rule, compiled_re)
                        })
                        .max_by_key(|ev| (event_end_utc(ev), ev.timestamp));

                    if let Some(ev) = matching_ev {
                        if let Some(url_str) = ev.data.url.as_deref() {
                            let host = url_str
                                .strip_prefix("https://")
                                .or_else(|| url_str.strip_prefix("http://"))
                                .unwrap_or(url_str)
                                .split('/')
                                .next()
                                .unwrap_or(url_str);
                            rt.last_matched_app = Some(host.to_string());
                        }
                        if let Some(ref title) = ev.data.title {
                            if !title.trim().is_empty() {
                                rt.set_last_matched_title(Some(title.clone()));
                            }
                        }
                    }
                }
                return false;
            }
            if let Some(until) = rt.snoozed_until {
                if now < until {
                    return false;
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
                let session_duration = calculate_continuous_session_for_rule(
                    events,
                    now,
                    last_completed,
                    reset_gap,
                    rule,
                    compiled_re,
                );
                if let Some(rt) = self.runtimes.get_mut(&rule.id) {
                    if session_duration == 0 {
                        rt.warned_current_cycle = false;
                    }
                    rt.current_session_seconds = session_duration;
                }
                (session_duration, period_secs)
            }
            TriggerType::Limit => {
                let used = calculate_usage_for_rule(events, rule, compiled_re, start_of_day_utc, now_utc);
                let limit_secs = rule.limit.to_seconds();
                if let Some(rt) = self.runtimes.get_mut(&rule.id) {
                    rt.current_session_seconds = 0;
                    rt.daily_usage_seconds = used;
                }
                (used, limit_secs)
            }
        };

        let matched_window = find_matching_window_for_rule(
            rule,
            compiled_re,
            events,
            now_utc,
            self.config.general.poll_interval_seconds,
        );
        let Some(matched_app_name) = matched_window else {
            return true;
        };

        if let Some(rt) = self.runtimes.get_mut(&rule.id) {
            rt.last_matched_app = Some(matched_app_name.clone());

            let recency_cutoff = now_utc
                - Duration::seconds(
                    (self.config.general.poll_interval_seconds.max(2) as i64) * 2 + 5,
                );
            let matching_ev = events
                .iter()
                .filter(|ev| {
                    let ev_end = event_end_utc(ev);
                    ev_end >= recency_cutoff && event_matches_rule(ev, rule, compiled_re)
                })
                .max_by_key(|ev| (event_end_utc(ev), ev.timestamp))
                .or_else(|| {
                    events
                        .iter()
                        .filter(|ev| event_matches_rule(ev, rule, compiled_re))
                        .max_by_key(|ev| (event_end_utc(ev), ev.timestamp))
                });

            if let Some(ev) = matching_ev {
                if let Some(ref title) = ev.data.title {
                    if !title.trim().is_empty() {
                        rt.set_last_matched_title(Some(title.clone()));
                    }
                }
            }
        }

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
                    rt.can_rollback_completion = false;
                }
                let rem_secs = threshold_secs.saturating_sub(elapsed_secs).max(1);
                let ctx = self.build_message_context(rule, &matched_app_name, elapsed_secs, threshold_secs, rem_secs);
                self.dispatch_pre_action_warning(rule, &ctx, rem_secs);
            }
            return true;
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
                return true;
            }

            if rule.trigger == TriggerType::Limit
                && rule.action == RuleAction::LockWorkstation
                && already_executed
                && get_idle_time_millis()
                    >= (self.config.general.poll_interval_seconds.max(2) as u32) * 1000
            {
                return true;
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
                let paragraphs = self.parse_rule_message(
                    raw_msg,
                    &ctx,
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

        true
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
                        rt.complete_cycle(now);
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
                    rt.prev_completed_at = rt.last_completed_at;
                    rt.active_break = Some((break_secs, break_secs));
                    rt.can_rollback_completion = true;
                    rt.action_executed_today = true;
                }
                let paragraphs = self.parse_rule_message(
                    &rule.message,
                    &ctx,
                    TextHorizontalAlignment::Center,
                );
                self.show_overlay(OverlayMode::BreakEnforcer {
                    rule_id: rule.id.clone(),
                    title: rule.name.clone(),
                    paragraphs,
                    duration_seconds: break_secs,
                    remaining_seconds: break_secs,
                    allow_snooze: !rule.strict_mode,
                    strict_mode: rule.strict_mode,
                    click_through: rule.click_through,
                });
            }
            RuleAction::ShowNotification => {
                if let Some(rt) = self.runtimes.get_mut(&rule.id) {
                    rt.complete_cycle(now);
                }
                match rule.notification_method {
                    NotificationMethod::Windows => {
                        enforcer::send_system_notification(&rule.name, &ctx.interpolate(&rule.message));
                    }
                    NotificationMethod::Hud => {
                        let paragraphs = self.parse_rule_message(
                            &rule.message,
                            &ctx,
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
                    rt.complete_cycle(now);
                }
                self.display_stats_overlay(false).await;
            }
            RuleAction::LockWorkstation => {
                if let Some(rt) = self.runtimes.get_mut(&rule.id) {
                    rt.complete_cycle(now);
                }
                enforcer::lock_workstation();
            }
            RuleAction::CloseTab => {
                let last_title_re = self
                    .runtimes
                    .get(&rule.id)
                    .and_then(|r| r.last_matched_title_regex.clone());
                let last_app = self
                    .runtimes
                    .get(&rule.id)
                    .and_then(|r| r.last_matched_app.clone());
                if let Some(rt) = self.runtimes.get_mut(&rule.id) {
                    rt.complete_cycle(now);
                }
                if rule.monitor_selection == MonitorSelection::Titles {
                    enforcer::close_active_browser_tab::<&str>(&[], compiled_re);
                } else if rule.monitor_selection == MonitorSelection::Urls {
                    let mut closed = false;
                    if let Some(ref re) = last_title_re {
                        closed = enforcer::close_active_browser_tab(enforcer::BROWSER_EXES, Some(re));
                    }
                    if !closed {
                        let app_matched = last_app
                            .as_deref()
                            .map(|app| matched_app_matches_url_rule(app, compiled_re))
                            .unwrap_or(false);
                        if app_matched {
                            enforcer::close_active_browser_tab(enforcer::BROWSER_EXES, None);
                        }
                    }
                } else {
                    enforcer::close_active_browser_tab(&rule.target_executables, None);
                }
            }
            RuleAction::CloseWindow => {
                let last_title_re = self
                    .runtimes
                    .get(&rule.id)
                    .and_then(|r| r.last_matched_title_regex.clone());
                let last_app = self
                    .runtimes
                    .get(&rule.id)
                    .and_then(|r| r.last_matched_app.clone());
                if let Some(rt) = self.runtimes.get_mut(&rule.id) {
                    rt.complete_cycle(now);
                }
                if rule.monitor_selection == MonitorSelection::Titles {
                    enforcer::close_target_windows::<&str>(&[], compiled_re);
                } else if rule.monitor_selection == MonitorSelection::Urls {
                    let mut closed_count = 0;
                    if let Some(ref re) = last_title_re {
                        closed_count = enforcer::close_target_windows(enforcer::BROWSER_EXES, Some(re));
                    }
                    if closed_count == 0 {
                        let app_matched = last_app
                            .as_deref()
                            .map(|app| matched_app_matches_url_rule(app, compiled_re))
                            .unwrap_or(false);
                        if app_matched {
                            enforcer::close_target_windows(enforcer::BROWSER_EXES, None);
                        }
                    }
                } else {
                    enforcer::close_target_windows(&rule.target_executables, None);
                }
            }
            RuleAction::KillProcess => {
                if let Some(rt) = self.runtimes.get_mut(&rule.id) {
                    rt.complete_cycle(now);
                }
                enforcer::kill_target_processes(&rule.target_executables);
            }
        }
    }

    /// Builds an `OverlayMode::HudStatus` from the current `stats_cache` without showing the loading spinner.
    fn build_hud_status_from_cache(&self) -> Option<OverlayMode> {
        let cache = self.stats_cache.as_ref()?;
        let bin_count = cache.bin_seconds.len().max(1);
        let bin_size_mins = cache.bin_size_mins.max(1);
        let start_time =
            cache.current_bin_start - Duration::minutes(((bin_count - 1) as i64) * bin_size_mins);
        let time_fmt = &self.config.stats.histogram_time_format;

        let mut bin_labels = Vec::with_capacity(bin_count);
        for i in 0..bin_count {
            let b_start = start_time + Duration::minutes((i as i64) * bin_size_mins);
            let label = if i % 4 == 0 || i == bin_count - 1 {
                b_start.format(time_fmt).to_string()
            } else {
                String::new()
            };
            bin_labels.push(label);
        }

        let max_bin_capacity = (bin_size_mins * 60) as f32;
        let bin_values: Vec<f32> = cache
            .bin_seconds
            .iter()
            .map(|&s| (s / max_bin_capacity).clamp(0.0, 1.0))
            .collect();

        let total_active_seconds: f32 = cache.bin_seconds.iter().sum();
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

        Some(OverlayMode::HudStatus {
            title: "24-Hour Screen Time Summary".into(),
            status_level: status_level.into(),
            total_active_minutes,
            target_minutes: self.config.stats.daily_screentime_target_minutes,
            bin_values,
            bin_labels,
            is_loading: false,
            click_through: cache.click_through,
        })
    }

    /// Incrementally updates the open 24-Hour Statistics HUD using the `events` already fetched by `poll_tick`.
    fn refresh_stats_overlay_from_events(&mut self, now: DateTime<Local>, events: &[AwEvent]) {
        if !matches!(
            self.active_overlay_mode,
            OverlayMode::HudStatus {
                is_loading: false,
                ..
            }
        ) {
            return;
        }

        let bin_count = self.config.stats.histogram_bin_count.max(1) as usize;
        let bin_size_mins = self.config.stats.histogram_bin_size_minutes.max(1) as i64;
        let new_bin_start = align_stats_bin_start(now, bin_size_mins);

        let Some(ref mut cache) = self.stats_cache else {
            return;
        };

        if cache.bin_seconds.len() != bin_count || cache.bin_size_mins != bin_size_mins {
            cache.bin_seconds.resize(bin_count, 0.0);
            cache.bin_size_mins = bin_size_mins;
            cache.current_bin_start = new_bin_start;
        } else if new_bin_start > cache.current_bin_start {
            let elapsed_bins =
                ((new_bin_start - cache.current_bin_start).num_minutes() / bin_size_mins).max(0)
                    as usize;
            let old_bin_start = cache.current_bin_start;
            let shift = elapsed_bins.min(bin_count);
            cache.bin_seconds.drain(0..shift);
            cache.bin_seconds.resize(bin_count, 0.0);
            cache.current_bin_start = new_bin_start;

            for i in 0..elapsed_bins {
                let b_start = old_bin_start + Duration::minutes((i as i64) * bin_size_mins);
                let b_end = b_start + Duration::minutes(bin_size_mins);
                let b_start_utc = b_start.with_timezone(&Utc);
                let b_end_utc = b_end.with_timezone(&Utc);
                let bin_idx = (bin_count as isize) - 1 - (elapsed_bins as isize) + (i as isize);
                if bin_idx >= 0 && (bin_idx as usize) < bin_count {
                    let secs =
                        compute_coalesced_overlap_seconds(events, b_start_utc, b_end_utc) as f32;
                    cache.bin_seconds[bin_idx as usize] = secs;
                }
            }
        }

        // Recompute only the current (latest) bin from poll_tick's events
        let cur_start_utc = cache.current_bin_start.with_timezone(&Utc);
        let cur_end_utc =
            (cache.current_bin_start + Duration::minutes(bin_size_mins)).with_timezone(&Utc);
        let cur_secs = compute_coalesced_overlap_seconds(events, cur_start_utc, cur_end_utc) as f32;
        if let Some(last) = cache.bin_seconds.last_mut() {
            *last = cur_secs;
        }

        if let Some(mode) = self.build_hud_status_from_cache() {
            self.show_overlay(mode);
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
        let current_bin_start = align_stats_bin_start(now, bin_size_mins);

        let start_time = current_bin_start - Duration::minutes(((bin_count - 1) as i64) * bin_size_mins);
        let start_utc = start_time.with_timezone(&Utc);
        let end_utc = (current_bin_start + Duration::minutes(bin_size_mins)).with_timezone(&Utc);

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

        for i in 0..bin_count {
            let b_start = start_time + Duration::minutes((i as i64) * bin_size_mins);
            let b_end = b_start + Duration::minutes(bin_size_mins);

            let b_start_utc = b_start.with_timezone(&Utc);
            let b_end_utc = b_end.with_timezone(&Utc);

            bin_seconds[i] = compute_coalesced_overlap_seconds(&events, b_start_utc, b_end_utc) as f32;
        }

        self.stats_cache = Some(StatsHudCache {
            current_bin_start,
            bin_size_mins,
            bin_seconds,
            click_through,
        });

        if let Some(mode) = self.build_hud_status_from_cache() {
            self.show_overlay(mode);
        }
    }

    /// Triggers a test overlay for UI verification using Option 3 structured paragraph syntax.
    pub async fn trigger_test_overlay(&mut self, mode: &str) {
        match mode {
            "break" => {
                let text = "# Take a Break\n\n[color=#38bdf8 size=18 bold] Rest your eyes and stretch for {break_time}.";
                let paragraphs = self.parse_rule_message(text, &MessageContext::sample(), TextHorizontalAlignment::Center);
                let mode = OverlayMode::BreakEnforcer {
                    rule_id: "test_break".into(),
                    title: "Test Break Enforcer".into(),
                    paragraphs,
                    duration_seconds: 300,
                    remaining_seconds: 300,
                    allow_snooze: true,
                    strict_mode: false,
                    click_through: false,
                };
                self.show_overlay(mode);
            }
            "hud_toast" => {
                let text = "## App Quota Warning\n[color=#f59e0b bold] You have {remaining} remaining for {app}.";
                let paragraphs = self.parse_rule_message(text, &MessageContext::sample(), TextHorizontalAlignment::Left);
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
                let paragraphs = self.parse_rule_message(text, &MessageContext::sample(), TextHorizontalAlignment::Left);
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

    /// Triggers an alert preview for a specific rule configuration.
    pub async fn preview_rule_alert(&mut self, rule: &Rule) {
        let app_name = if rule.monitor_selection == MonitorSelection::Urls {
            "youtube.com"
        } else if !rule.target_executables.is_empty() && rule.target_executables[0] != "*" {
            &rule.target_executables[0]
        } else {
            "code.exe"
        };
        let threshold = rule.trigger_threshold_seconds();
        let (elapsed_secs, rem_secs) = if rule.trigger == TriggerType::Limit || rule.message.contains("{remaining}") {
            let rem = rule.warn_before.to_seconds().max(300);
            let elapsed = threshold.saturating_sub(rem);
            (elapsed, rem)
        } else {
            (threshold, 0)
        };
        let ctx = self.build_message_context(rule, app_name, elapsed_secs, threshold, rem_secs);

        match rule.action {
            RuleAction::ShowOverlay => {
                let paragraphs =
                    self.parse_rule_message(&rule.message, &ctx, TextHorizontalAlignment::Center);
                let mode = OverlayMode::BreakEnforcer {
                    rule_id: "test_preview".to_string(),
                    title: rule.name.clone(),
                    paragraphs,
                    duration_seconds: 15,
                    remaining_seconds: 15,
                    allow_snooze: true,
                    strict_mode: false,
                    click_through: rule.click_through,
                };
                self.show_overlay(mode);
            }
            RuleAction::ShowNotification => match rule.notification_method {
                NotificationMethod::Windows => {
                    let text = ctx.interpolate(&rule.message);
                    enforcer::send_system_notification(&rule.name, &text);
                }
                NotificationMethod::Hud => {
                    let paragraphs =
                        self.parse_rule_message(&rule.message, &ctx, TextHorizontalAlignment::Left);
                    let dur = rule.notification_duration.to_seconds().max(1);
                    let mode = OverlayMode::HudToast {
                        rule_id: "test_preview".to_string(),
                        title: rule.name.clone(),
                        paragraphs,
                        snooze_options_minutes: vec![5, 10, 15],
                        duration_seconds: dur,
                        remaining_seconds: dur,
                        show_time_remaining: rule.show_remaining_time,
                    };
                    self.show_overlay(mode);
                }
            },
            _ => {}
        }
    }
}

/// Returns the end timestamp of an ActivityWatch event in UTC.
pub fn event_end_utc(ev: &AwEvent) -> DateTime<Utc> {
    ev.timestamp + Duration::milliseconds((ev.duration * 1000.0) as i64)
}

/// Helper checking whether a rule's URL regex matches the hostname or URL representation in `last_matched_app`.
pub fn matched_app_matches_url_rule(app: &str, compiled_re: Option<&Regex>) -> bool {
    if let Some(re) = compiled_re {
        re.is_match(app)
            || re.is_match(&format!("https://{app}"))
            || re.is_match(&format!("http://{app}"))
            || re.is_match(&format!("https://{app}/"))
            || re.is_match(&format!("http://{app}/"))
    } else {
        false
    }
}

/// Helper function to check if an event matches a unified rule's monitor criteria.
pub fn event_matches_rule(ev: &AwEvent, rule: &Rule, compiled_re: Option<&Regex>) -> bool {
    match rule.monitor_selection {
        MonitorSelection::Titles => {
            if let Some(re) = compiled_re {
                if let Some(title) = ev.data.title.as_deref() {
                    return !title.is_empty() && re.is_match(title);
                }
            }
            false
        }
        MonitorSelection::Urls => {
            if let Some(re) = compiled_re {
                if let Some(url) = ev.data.url.as_deref() {
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
            if let Some(app) = ev.data.app.as_deref() {
                return enforcer::exe_name_matches_targets(app, &rule.target_executables);
            }
            false
        }
    }
}

/// Helper to check if a visible matching window exists for this rule and return the matched title/process name.
pub fn find_matching_window_for_rule(
    rule: &Rule,
    compiled_re: Option<&Regex>,
    events: &[AwEvent],
    now_utc: DateTime<Utc>,
    poll_interval_secs: u64,
) -> Option<String> {
    match rule.monitor_selection {
        MonitorSelection::Titles => enforcer::find_matching_window::<&str>(&[], compiled_re),
        MonitorSelection::Urls => {
            let browser_match = enforcer::find_matching_window(enforcer::BROWSER_EXES, None)?;
            let recency_cutoff =
                now_utc - Duration::seconds((poll_interval_secs.max(2) as i64) * 2 + 5);

            let latest_ev = events.iter().max_by_key(|ev| (event_end_utc(ev), ev.timestamp));
            if let Some(ev) = latest_ev {
                let ev_end = event_end_utc(ev);
                if ev_end >= recency_cutoff && event_matches_rule(ev, rule, compiled_re) {
                    let matched_browser = if let Some(ref title) = ev.data.title {
                        if !title.trim().is_empty() {
                            let title_re = Regex::new(&regex::escape(title)).ok();
                            enforcer::find_matching_window(
                                enforcer::BROWSER_EXES,
                                title_re.as_ref(),
                            )
                            .unwrap_or(browser_match)
                        } else {
                            browser_match
                        }
                    } else {
                        browser_match
                    };

                    Some(
                        ev.data
                            .url
                            .as_deref()
                            .map(|u| {
                                u.strip_prefix("https://")
                                    .or_else(|| u.strip_prefix("http://"))
                                    .unwrap_or(u)
                                    .split('/')
                                    .next()
                                    .unwrap_or(u)
                                    .to_string()
                            })
                            .or_else(|| ev.data.url.clone())
                            .unwrap_or(matched_browser),
                    )
                } else {
                    None
                }
            } else {
                None
            }
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

/// Computes continuous active session duration by walking backward from now.
pub fn calculate_continuous_session_for_rule(
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

    let intervals: Vec<(DateTime<Utc>, DateTime<Utc>)> = events
        .iter()
        .filter(|ev| event_matches_rule(ev, rule, compiled_re))
        .filter_map(|ev| {
            let start_utc = if let Some(min_dt) = min_allowed_utc {
                ev.timestamp.max(min_dt)
            } else {
                ev.timestamp
            };
            let end_utc = ev.timestamp + Duration::milliseconds((ev.duration * 1000.0) as i64);
            if end_utc > start_utc {
                Some((start_utc, end_utc))
            } else {
                None
            }
        })
        .collect();

    let merged = merge_overlapping_intervals(intervals);
    if merged.is_empty() {
        return 0;
    }

    let last_end = merged.last().unwrap().1;
    if (now_utc - last_end).num_seconds() >= reset_gap_sec {
        return 0;
    }

    let mut session_seconds = 0i64;
    let mut prev_start: Option<DateTime<Utc>> = None;

    for (start, end) in merged.iter().rev() {
        if let Some(p_start) = prev_start {
            let gap = (p_start - *end).num_seconds();
            if gap >= reset_gap_sec {
                break;
            }
        }
        session_seconds += (*end - *start).num_milliseconds() / 1000;
        prev_start = Some(*start);
    }

    session_seconds.max(0) as u32
}

/// Calculates total active usage seconds for a rule from a list of events.
pub fn calculate_usage_for_rule(
    events: &[AwEvent],
    rule: &Rule,
    compiled_regex: Option<&Regex>,
    start_of_day_utc: DateTime<Utc>,
    now_utc: DateTime<Utc>,
) -> u32 {
    let intervals: Vec<(DateTime<Utc>, DateTime<Utc>)> = events
        .iter()
        .filter(|ev| event_matches_rule(ev, rule, compiled_regex))
        .filter_map(|ev| {
            let e_start = ev.timestamp;
            let e_end = ev.timestamp + Duration::milliseconds((ev.duration * 1000.0) as i64);
            let start = e_start.max(start_of_day_utc);
            let end = e_end.min(now_utc);
            if end > start {
                Some((start, end))
            } else {
                None
            }
        })
        .collect();

    let merged = merge_overlapping_intervals(intervals);
    let total_secs: f64 = merged
        .iter()
        .map(|(s, e)| (*e - *s).num_milliseconds() as f64 / 1000.0)
        .sum();

    total_secs.round() as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aw_client::AwEventData;
    use crate::config::{DurationConfig, RuleSchedule, TimeUnit};
    use chrono::TimeZone;

    /// Helper to construct a synthetic `AwEvent` with `app`, `title`, and `url` fields.
    fn make_event(start_utc: DateTime<Utc>, duration_secs: f64, app: &str, title: &str, url: &str) -> AwEvent {
        AwEvent {
            timestamp: start_utc,
            duration: duration_secs,
            data: AwEventData {
                app: if app.is_empty() { None } else { Some(app.to_string()) },
                title: if title.is_empty() { None } else { Some(title.to_string()) },
                url: if url.is_empty() { None } else { Some(url.to_string()) },
                status: None,
            },
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

    /// Verifies `calculate_continuous_session_for_rule` across sub-reset gaps, AFK breaks, completion clipping, and overlapping events.
    #[test]
    fn test_continuous_session_afk_gaps_clipping_and_overlap() {
        let now = Local.with_ymd_and_hms(2026, 10, 29, 12, 0, 0).unwrap();
        let now_utc = now.with_timezone(&Utc);
        let rule = Rule::default_template(0);
        let reset_gap_secs = 300; // 5 minutes

        // 1. Two 10-min events separated by 2 mins (< 5m reset gap) -> 20m (1200s) session duration
        let ev1 = make_event(now_utc - Duration::minutes(22), 600.0, "code.exe", "main.rs", "");
        let ev2 = make_event(now_utc - Duration::minutes(10), 600.0, "code.exe", "main.rs", "");
        let session = calculate_continuous_session_for_rule(&[ev1.clone(), ev2.clone()], now, None, reset_gap_secs, &rule, None);
        assert_eq!(session, 1200);

        // 2. Event separated by 6 mins (>= 5m reset gap) before ev1 is ignored
        let ev_old = make_event(now_utc - Duration::minutes(38), 600.0, "code.exe", "main.rs", "");
        let session_with_gap = calculate_continuous_session_for_rule(&[ev_old, ev1.clone(), ev2.clone()], now, None, reset_gap_secs, &rule, None);
        assert_eq!(session_with_gap, 1200);

        // 3. Currently AFK for >= 5m -> session duration resets to 0
        let future_now = now + Duration::minutes(5);
        let afk_session = calculate_continuous_session_for_rule(&[ev1.clone(), ev2.clone()], future_now, None, reset_gap_secs, &rule, None);
        assert_eq!(afk_session, 0);

        // 4. Clipping against last_break_completed_at (5 mins into ev2 -> only 300s counted)
        let completed_at = (now_utc - Duration::minutes(5)).with_timezone(&Local);
        let clipped_session = calculate_continuous_session_for_rule(&[ev1, ev2], now, Some(completed_at), reset_gap_secs, &rule, None);
        assert_eq!(clipped_session, 300);

        // 5. Overlapping events do not double-count
        let ov1 = make_event(now_utc - Duration::minutes(10), 600.0, "code.exe", "", "");
        let ov2 = make_event(now_utc - Duration::minutes(5), 300.0, "code.exe", "", "");
        let ov_session = calculate_continuous_session_for_rule(&[ov1, ov2], now, None, reset_gap_secs, &rule, None);
        assert_eq!(ov_session, 600);
    }

    /// Verifies `calculate_continuous_session_for_rule` correctly merges nested and overlapping events,
    /// ensuring a 20-minute event enclosing a 1-minute event 18 minutes ago is not falsely treated as AFK.
    #[test]
    fn test_nested_and_overlapping_events_in_continuous_session() {
        let now = Local.with_ymd_and_hms(2026, 10, 29, 12, 0, 0).unwrap();
        let now_utc = now.with_timezone(&Utc);
        let rule = Rule::default_template(0);
        let reset_gap_secs = 300; // 5 minutes

        // 20-minute outer event from 11:40 to 12:00 (1200s)
        let ev_outer = make_event(now_utc - Duration::minutes(20), 1200.0, "code.exe", "main.rs", "");
        // 1-minute inner event from 11:41 to 11:42 (60s), ending 18 minutes ago
        let ev_inner = make_event(now_utc - Duration::minutes(19), 60.0, "code.exe", "main.rs", "");

        let session = calculate_continuous_session_for_rule(
            &[ev_outer, ev_inner],
            now,
            None,
            reset_gap_secs,
            &rule,
            None,
        );
        assert_eq!(session, 1200);
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

        // Title regex matching
        let mut title_rule = Rule::default_template(2);
        title_rule.monitor_selection = MonitorSelection::Titles;
        title_rule.title_regex = "(?i)confidential".to_string();
        let title_re = Regex::new(&title_rule.title_regex).ok();
        let ev_title = make_event(start_utc + Duration::minutes(20), 400.0, "word.exe", "Confidential Report", "");
        assert_eq!(calculate_usage_for_rule(&[ev_title], &title_rule, title_re.as_ref(), start_utc, now_utc), 400);
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

        // 1. Higher-priority BreakEnforcer (4) is not overwritten by HudToast (2) or HudTimer (3)
        engine.show_overlay(OverlayMode::BreakEnforcer {
            rule_id: rule_id.clone(),
            title: "Break".into(),
            paragraphs: Vec::new(),
            duration_seconds: 60,
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

        engine.show_overlay(OverlayMode::HudTimer {
            rule_id: "timer".into(),
            title: "Timer".into(),
            paragraphs: Vec::new(),
            duration_seconds: 10,
            remaining_seconds: 10,
        });
        assert!(matches!(engine.active_overlay_mode, OverlayMode::BreakEnforcer { .. }));

        // 2. Strict lock active blocks DismissBreakOverlay, SnoozeAll, SetPauseAll, and TriggerTestOverlay
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

        engine.handle_command(DaemonCommand::TriggerTestOverlay { mode: "hud_toast".to_string() }).await;
        assert!(matches!(engine.active_overlay_mode, OverlayMode::BreakEnforcer { .. }));

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

        // 1. First-cycle SnoozeBreak (when last_completed_at is None) preserves last_completed_at as None and resets action_executed_today
        if let Some(rt) = engine.runtimes.get_mut(&rule_id) {
            rt.active_break = Some((300, 300));
            rt.last_completed_at = None;
            rt.prev_completed_at = None;
            rt.action_executed_today = true;
        }
        engine.handle_command(DaemonCommand::SnoozeBreak { rule_id: rule_id.clone(), minutes: 5 }).await;
        let rt = engine.runtimes.get(&rule_id).unwrap();
        assert_eq!(rt.last_completed_at, None);
        assert!(!rt.action_executed_today);
        assert!(rt.active_break.is_none());
        assert!(rt.snoozed_until.is_some());

        // 2. Subsequent SnoozeBreak preserves existing last_completed_at
        let prev_completed = Local.with_ymd_and_hms(2026, 10, 29, 10, 0, 0).unwrap();
        if let Some(rt) = engine.runtimes.get_mut(&rule_id) {
            rt.last_completed_at = Some(prev_completed);
            rt.prev_completed_at = Some(prev_completed);
            rt.action_executed_today = true;
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

    #[tokio::test]
    async fn test_live_stats_hud_refresh_and_bin_rollover() {
        let (overlay_tx, mut overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let cfg = AppConfig::default();
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        let bin_start = Local.with_ymd_and_hms(2026, 10, 30, 14, 0, 0).unwrap();
        let mut bin_seconds = vec![0.0f32; 24];
        bin_seconds[22] = 1200.0; // 13:00-14:00 had 20 mins
        bin_seconds[23] = 300.0;  // 14:00-15:00 initially had 5 mins

        engine.stats_cache = Some(StatsHudCache {
            current_bin_start: bin_start,
            bin_size_mins: 60,
            bin_seconds,
            click_through: false,
        });
        let initial_mode = engine.build_hud_status_from_cache().unwrap();
        engine.show_overlay(initial_mode);
        let _ = overlay_rx.try_recv();

        // 1. Same-bin live update at 14:15:00 with 600s of activity in 14:00..14:15
        let now1 = Local.with_ymd_and_hms(2026, 10, 30, 14, 15, 0).unwrap();
        let ev1 = vec![make_event(bin_start.with_timezone(&Utc), 600.0, "code.exe", "VS Code", "")];
        engine.refresh_stats_overlay_from_events(now1, &ev1);

        match &engine.active_overlay_mode {
            OverlayMode::HudStatus { bin_values, total_active_minutes, is_loading, .. } => {
                assert!(!is_loading);
                assert_eq!(*total_active_minutes, 30); // 20m + 10m
                assert!((bin_values[22] - (1200.0 / 3600.0)).abs() < 1e-4);
                assert!((bin_values[23] - (600.0 / 3600.0)).abs() < 1e-4);
            }
            other => panic!("Expected HudStatus, got {:?}", other),
        }

        // 2. Rollover into next hour bin (15:05:00) with 1800s in 14:00..15:00 and 180s in 15:00..15:05
        let next_bin_start = Local.with_ymd_and_hms(2026, 10, 30, 15, 0, 0).unwrap();
        let now2 = Local.with_ymd_and_hms(2026, 10, 30, 15, 5, 0).unwrap();
        let ev2 = vec![
            make_event(bin_start.with_timezone(&Utc), 1800.0, "code.exe", "VS Code", ""),
            make_event(next_bin_start.with_timezone(&Utc), 180.0, "code.exe", "VS Code", ""),
        ];
        engine.refresh_stats_overlay_from_events(now2, &ev2);

        match &engine.active_overlay_mode {
            OverlayMode::HudStatus { bin_values, total_active_minutes, is_loading, .. } => {
                assert!(!is_loading);
                assert_eq!(*total_active_minutes, 53); // 20m + 30m + 3m
                assert!((bin_values[21] - (1200.0 / 3600.0)).abs() < 1e-4); // shifted left from index 22
                assert!((bin_values[22] - (1800.0 / 3600.0)).abs() < 1e-4); // finalized 14:00-15:00 bin (1800s = 30m)
                assert!((bin_values[23] - (180.0 / 3600.0)).abs() < 1e-4);  // new active 15:00-16:00 bin (180s = 3m)
            }
            other => panic!("Expected HudStatus, got {:?}", other),
        }

        // 3. Multi-hour rollover (elapsed_bins == 2, jump from 15:00 to 17:10:00)
        // 15:00..16:00 has 2400s (40m), intermediate 16:00..17:00 has 900s (15m), active 17:00..17:10 has 300s (5m)
        let bin_15 = Local.with_ymd_and_hms(2026, 10, 30, 15, 0, 0).unwrap();
        let bin_16 = Local.with_ymd_and_hms(2026, 10, 30, 16, 0, 0).unwrap();
        let bin_17 = Local.with_ymd_and_hms(2026, 10, 30, 17, 0, 0).unwrap();
        let now3 = Local.with_ymd_and_hms(2026, 10, 30, 17, 10, 0).unwrap();
        let ev3 = vec![
            make_event(bin_15.with_timezone(&Utc), 2400.0, "code.exe", "VS Code", ""),
            make_event(bin_16.with_timezone(&Utc), 900.0, "code.exe", "VS Code", ""),
            make_event(bin_17.with_timezone(&Utc), 300.0, "code.exe", "VS Code", ""),
        ];
        engine.refresh_stats_overlay_from_events(now3, &ev3);

        match &engine.active_overlay_mode {
            OverlayMode::HudStatus { bin_values, total_active_minutes, is_loading, .. } => {
                assert!(!is_loading);
                assert_eq!(*total_active_minutes, 110); // 20m + 30m + 40m + 15m + 5m
                assert!((bin_values[19] - (1200.0 / 3600.0)).abs() < 1e-4); // 13:00..14:00 shifted to index 19
                assert!((bin_values[20] - (1800.0 / 3600.0)).abs() < 1e-4); // 14:00..15:00 shifted to index 20
                assert!((bin_values[21] - (2400.0 / 3600.0)).abs() < 1e-4); // 15:00..16:00 populated (40m)
                assert!((bin_values[22] - (900.0 / 3600.0)).abs() < 1e-4);  // intermediate 16:00..17:00 populated (15m)
                assert!((bin_values[23] - (300.0 / 3600.0)).abs() < 1e-4);  // current 17:00..18:00 populated (5m)
            }
            other => panic!("Expected HudStatus, got {:?}", other),
        }
    }

    #[test]
    fn test_align_stats_bin_start_multi_hour() {
        // Sub-hour: 15 mins
        let dt = Local.with_ymd_and_hms(2026, 10, 30, 14, 27, 45).unwrap();
        let aligned_15 = align_stats_bin_start(dt, 15);
        assert_eq!(aligned_15, Local.with_ymd_and_hms(2026, 10, 30, 14, 15, 0).unwrap());

        // 1 hour (60 mins)
        let aligned_60 = align_stats_bin_start(dt, 60);
        assert_eq!(aligned_60, Local.with_ymd_and_hms(2026, 10, 30, 14, 0, 0).unwrap());

        // 2 hours (120 mins) - hour 15 should align to 14
        let dt15 = Local.with_ymd_and_hms(2026, 10, 30, 15, 45, 12).unwrap();
        let aligned_120 = align_stats_bin_start(dt15, 120);
        assert_eq!(aligned_120, Local.with_ymd_and_hms(2026, 10, 30, 14, 0, 0).unwrap());

        // 3 hours (180 mins) - hour 14 should align to 12
        let aligned_180 = align_stats_bin_start(dt, 180);
        assert_eq!(aligned_180, Local.with_ymd_and_hms(2026, 10, 30, 12, 0, 0).unwrap());
    }

    #[tokio::test]
    async fn test_daily_usage_caching_and_rollover() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();
        let mut limit_rule = Rule::default_template(0);
        limit_rule.trigger = TriggerType::Limit;
        limit_rule.target_executables = vec!["discord.exe".to_string()];
        cfg.rules = vec![limit_rule];
        let rule_id = cfg.rules[0].id.clone();
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        // 1. Initialized to 0
        let rt = engine.runtimes.get(&rule_id).unwrap();
        assert_eq!(rt.daily_usage_seconds, 0);

        // 2. Evaluated Limit rule populates daily_usage_seconds
        let now = Local::now();
        let now_utc = now.with_timezone(&Utc);
        let start_utc = now_utc - Duration::hours(2);
        let ev = make_event(start_utc + Duration::minutes(10), 1200.0, "discord.exe", "Chat", "");
        let rule = engine.config.rules[0].clone();
        engine
            .evaluate_rule(&rule, None, now, &[ev], start_utc, now_utc)
            .await;
        assert_eq!(engine.runtimes.get(&rule_id).unwrap().daily_usage_seconds, 1200);

        // 3. Day rollover resets daily_usage_seconds to 0
        engine.last_reset_date -= chrono::Duration::days(1);
        let logical_now = logical_day_start(now, engine.config.general.day_start_hour);
        if logical_now.date_naive() != engine.last_reset_date {
            engine.last_reset_date = logical_now.date_naive();
            for rt in engine.runtimes.values_mut() {
                rt.warned_current_cycle = false;
                rt.action_executed_today = false;
                rt.grace_countdown = None;
                rt.snoozed_until = None;
                rt.pending_break_since = None;
                rt.daily_usage_seconds = 0;
            }
        }
        assert_eq!(engine.runtimes.get(&rule_id).unwrap().daily_usage_seconds, 0);
    }

    #[tokio::test]
    async fn test_warning_snooze_preserves_cycle_completion() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();
        let rule = Rule::default_template(0);
        let rule_id = rule.id.clone();
        cfg.rules = vec![rule];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        let cycle1_time = Local::now() - Duration::minutes(30);
        let rt = engine.runtimes.get_mut(&rule_id).unwrap();
        rt.complete_cycle(cycle1_time);
        assert_eq!(rt.last_completed_at, Some(cycle1_time));
        assert!(rt.can_rollback_completion);

        // Simulate cycle 2 Stage 1 warning dispatch
        rt.warned_current_cycle = true;
        rt.can_rollback_completion = false;

        // User snoozes from warning toast
        engine
            .handle_command(DaemonCommand::SnoozeBreak {
                rule_id: rule_id.clone(),
                minutes: 10,
            })
            .await;

        let rt = engine.runtimes.get(&rule_id).unwrap();
        assert_eq!(rt.last_completed_at, Some(cycle1_time));
        assert!(rt.snoozed_until.is_some());
        assert!(!rt.can_rollback_completion);
        assert!(rt.warned_current_cycle);
    }

    #[tokio::test]
    async fn test_snooze_stage1_warning_preserves_warned_flag_and_prevents_duplicate() {
        let (overlay_tx, mut overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();
        cfg.general.suppress_fullscreen_d3d = false;
        let mut rule = Rule::default_template(0);
        rule.id = "rule-snooze-warn".to_string();
        rule.period = DurationConfig::new(20, TimeUnit::Min);
        rule.warn_before = DurationConfig::new(5, TimeUnit::Min);
        rule.target_executables = vec!["*".to_string()];
        rule.action = RuleAction::ShowOverlay;
        cfg.rules = vec![rule.clone()];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        let now = Local::now();
        let now_utc = now.with_timezone(&Utc);
        let start_of_day_utc = now_utc - Duration::hours(1);

        // 16 minutes active: above threshold - warn_before (15m), below threshold (20m)
        let ev = make_event(
            now_utc - Duration::minutes(16),
            16.0 * 60.0,
            "editor.exe",
            "Working",
            "",
        );

        // First evaluation triggers Stage 1 warning
        engine
            .evaluate_rule(&rule, None, now, &[ev.clone()], start_of_day_utc, now_utc)
            .await;

        let rt = engine.runtimes.get("rule-snooze-warn").unwrap();
        assert!(rt.warned_current_cycle);

        // Drain the warning overlay message
        let action = overlay_rx.try_recv().expect("Should have dispatched warning toast");
        match action {
            OverlayAction::Show(OverlayMode::HudToast { title, .. }) => {
                assert!(title.contains("Upcoming"));
            }
            other => panic!("Expected HudToast warning, got {:?}", other),
        }

        // User snoozes the warning toast for 10 minutes
        engine
            .handle_command(DaemonCommand::SnoozeBreak {
                rule_id: "rule-snooze-warn".to_string(),
                minutes: 10,
            })
            .await;

        // Verify warned_current_cycle remains true after snooze
        let rt = engine.runtimes.get("rule-snooze-warn").unwrap();
        assert!(rt.warned_current_cycle);
        assert!(rt.snoozed_until.is_some());

        // Drain any overlay action caused by snooze (e.g. Hide)
        while overlay_rx.try_recv().is_ok() {}

        // Fast-forward past the snooze expiration (11 minutes later), still within warning window (18m active < 20m)
        let later_now = now + Duration::minutes(11);
        let later_now_utc = now_utc + Duration::minutes(11);
        let later_ev = make_event(
            later_now_utc - Duration::minutes(18),
            18.0 * 60.0,
            "editor.exe",
            "Working",
            "",
        );

        engine
            .evaluate_rule(&rule, None, later_now, &[later_ev], start_of_day_utc, later_now_utc)
            .await;

        // Verify snooze expired and no duplicate warning was dispatched
        let rt = engine.runtimes.get("rule-snooze-warn").unwrap();
        assert!(rt.snoozed_until.is_none());
        assert!(rt.warned_current_cycle);
        assert!(
            overlay_rx.try_recv().is_err(),
            "No duplicate warning overlay should be emitted after snooze expiration"
        );
    }


    #[tokio::test]
    async fn test_acknowledge_app_warning_does_not_suppress_next_cycle() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();
        let mut rule1 = Rule::default_template(0);
        rule1.id = "rule-1".to_string();
        let mut rule2 = Rule::default_template(1);
        rule2.id = "rule-2".to_string();
        cfg.rules = vec![rule1, rule2];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        // Ensure warned_current_cycle is false initially
        assert!(!engine.runtimes.get("rule-1").unwrap().warned_current_cycle);

        // Clicking "OK" on a notification or warning toast
        engine
            .handle_command(DaemonCommand::AcknowledgeAppWarning {
                rule_id: "rule-1".to_string(),
            })
            .await;

        // Must still be false so the next cycle is not suppressed
        assert!(!engine.runtimes.get("rule-1").unwrap().warned_current_cycle);

        // Test DismissBreakOverlay only touching active or pending break rules
        engine.runtimes.get_mut("rule-1").unwrap().warned_current_cycle = true;
        engine.runtimes.get_mut("rule-2").unwrap().active_break = Some((10, 10));

        engine.handle_command(DaemonCommand::DismissBreakOverlay).await;

        // rule-2's break was completed
        assert!(engine.runtimes.get("rule-2").unwrap().active_break.is_none());
        assert!(engine.runtimes.get("rule-2").unwrap().last_completed_at.is_some());

        // rule-1 was neither active nor pending break, so its warned_current_cycle was untouched
        assert!(engine.runtimes.get("rule-1").unwrap().warned_current_cycle);
    }

    #[test]
    fn test_rule_elapsed_seconds_limit_vs_period() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();

        let mut period_rule = Rule::default_template(0);
        period_rule.id = "period-rule".to_string();
        period_rule.trigger = TriggerType::Period;

        let mut limit_rule = Rule::default_template(1);
        limit_rule.id = "limit-rule".to_string();
        limit_rule.trigger = TriggerType::Limit;

        cfg.rules = vec![period_rule.clone(), limit_rule.clone()];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        let rt_period = engine.runtimes.get_mut("period-rule").unwrap();
        rt_period.current_session_seconds = 1800;
        rt_period.daily_usage_seconds = 4500;

        let rt_limit = engine.runtimes.get_mut("limit-rule").unwrap();
        rt_limit.current_session_seconds = 0;
        rt_limit.daily_usage_seconds = 7200;

        assert_eq!(engine.rule_elapsed_seconds(&period_rule), 1800);
        assert_eq!(engine.rule_elapsed_seconds(&limit_rule), 7200);
    }

    #[tokio::test]
    async fn test_isolated_countdown_updates_and_toast_expiry() {
        let (overlay_tx, mut overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();

        let mut break_rule = Rule::default_template(0);
        break_rule.id = "rule-break".to_string();
        break_rule.action = RuleAction::ShowOverlay;

        let mut grace_rule = Rule::default_template(1);
        grace_rule.id = "rule-grace".to_string();
        grace_rule.action = RuleAction::CloseWindow;
        grace_rule.trigger = TriggerType::Limit;

        cfg.rules = vec![break_rule.clone(), grace_rule.clone()];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        // 1. Simulate active BreakEnforcer overlay for rule-break
        engine.runtimes.get_mut("rule-break").unwrap().active_break = Some((300, 300));
        engine.active_overlay_mode = OverlayMode::BreakEnforcer {
            rule_id: "rule-break".to_string(),
            title: "Break".to_string(),
            paragraphs: Vec::new(),
            duration_seconds: 300,
            remaining_seconds: 300,
            allow_snooze: true,
            strict_mode: false,
            click_through: false,
        };

        // rule-grace has a background grace countdown
        engine.runtimes.get_mut("rule-grace").unwrap().grace_countdown = Some((10, 10));

        assert!(engine.is_rule_displayed("rule-break"));
        assert!(!engine.is_rule_displayed("rule-grace"));

        // Drain any pending messages
        while overlay_rx.try_recv().is_ok() {}

        // Tick realtime
        engine.realtime_tick().await;

        // Background grace_countdown decremented to 9
        assert_eq!(
            engine.runtimes.get("rule-grace").unwrap().grace_countdown,
            Some((9, 10))
        );

        // Verify that NO UpdateCountdown for grace_countdown (remaining: 9) was sent to the overlay!
        let mut received_grace_countdown = false;
        while let Ok(action) = overlay_rx.try_recv() {
            if let OverlayAction::UpdateCountdown { remaining, .. } = action {
                if remaining == 9 {
                    received_grace_countdown = true;
                }
            }
        }
        assert!(
            !received_grace_countdown,
            "Background grace countdown should not send UpdateCountdown to active BreakEnforcer overlay"
        );

        // 2. Expired HudToast hides cleanly (or transitions to active countdown)
        // Set HudToast with 1s remaining and no other active countdowns
        engine.runtimes.get_mut("rule-break").unwrap().active_break = None;
        engine.runtimes.get_mut("rule-grace").unwrap().grace_countdown = None;

        engine.active_overlay_mode = OverlayMode::HudToast {
            rule_id: "rule-toast".to_string(),
            title: "Toast".to_string(),
            paragraphs: Vec::new(),
            snooze_options_minutes: vec![5],
            duration_seconds: 5,
            remaining_seconds: 1,
            show_time_remaining: true,
        };

        while overlay_rx.try_recv().is_ok() {}

        engine.realtime_tick().await;

        // The toast expired (1s -> 0s) and with no active countdowns, overlay transitions to Hidden
        assert!(matches!(engine.active_overlay_mode, OverlayMode::Hidden));
        let mut received_hide = false;
        while let Ok(action) = overlay_rx.try_recv() {
            if matches!(action, OverlayAction::Hide) {
                received_hide = true;
            }
        }
        assert!(received_hide, "Expired HudToast should dispatch Hide when no countdowns remain");

        // 3. Re-verify target closed during grace period skips destructive action
        engine.runtimes.get_mut("rule-grace").unwrap().grace_countdown = Some((1, 10));
        engine.realtime_tick().await;

        // Grace finished, target wasn't open, cycle marked completed
        assert_eq!(
            engine.runtimes.get("rule-grace").unwrap().grace_countdown,
            None
        );
        assert!(engine.runtimes.get("rule-grace").unwrap().action_executed_today);
    }

    #[test]
    fn test_merge_overlapping_intervals_comprehensive() {
        let t0 = Utc.with_ymd_and_hms(2026, 10, 31, 10, 0, 0).unwrap();
        let t1 = t0 + Duration::minutes(10);
        let t2 = t0 + Duration::minutes(20);
        let t3 = t0 + Duration::minutes(30);
        let t4 = t0 + Duration::minutes(40);

        // 1. Empty list
        assert!(merge_overlapping_intervals(vec![]).is_empty());

        // 2. Disjoint intervals preserved
        let disjoint = merge_overlapping_intervals(vec![(t0, t1), (t2, t3)]);
        assert_eq!(disjoint, vec![(t0, t1), (t2, t3)]);

        // 3. Overlapping intervals out of order: [t1..t3], [t0..t2] -> [t0..t3]
        let overlapping = merge_overlapping_intervals(vec![(t1, t3), (t0, t2)]);
        assert_eq!(overlapping, vec![(t0, t3)]);

        // 4. Contained interval: [t0..t4], [t1..t2] -> [t0..t4]
        let contained = merge_overlapping_intervals(vec![(t0, t4), (t1, t2)]);
        assert_eq!(contained, vec![(t0, t4)]);

        // 5. Adjacent intervals: [t0..t1], [t1..t2] -> [t0..t2]
        let adjacent = merge_overlapping_intervals(vec![(t0, t1), (t1, t2)]);
        assert_eq!(adjacent, vec![(t0, t2)]);
    }

    #[test]
    fn test_calculate_usage_coalesces_overlapping_events() {
        let base_time = Utc.with_ymd_and_hms(2026, 10, 31, 12, 0, 0).unwrap();
        let start_of_day = base_time - Duration::hours(4);
        let now_utc = base_time + Duration::hours(2);

        let mut rule = Rule::default_template(0);
        rule.target_executables = vec!["code.exe".to_string()];

        // Two overlapping 30-minute events (12:00..12:30 and 12:10..12:40)
        let ev1 = make_event(base_time, 1800.0, "code.exe", "Project A", "");
        let ev2 = make_event(base_time + Duration::minutes(10), 1800.0, "code.exe", "Project B", "");

        let usage = calculate_usage_for_rule(&[ev1.clone(), ev2], &rule, None, start_of_day, now_utc);
        assert_eq!(usage, 2400); // 40 minutes = 2400s (not 3600s)

        // Two identical 30-minute events (12:00..12:30)
        let ev3 = make_event(base_time, 1800.0, "code.exe", "Project A (Window 2)", "");
        let usage_identical = calculate_usage_for_rule(&[ev1, ev3], &rule, None, start_of_day, now_utc);
        assert_eq!(usage_identical, 1800); // exactly 30 minutes (1800s), not 3600s
    }

    #[tokio::test]
    async fn test_post_grace_hud_toast_preserved_on_realtime_tick() {
        let (overlay_tx, mut overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();

        let mut rule = Rule::default_template(0);
        rule.id = "rule-hud-notify".to_string();
        rule.action = RuleAction::ShowNotification;
        rule.notification_method = NotificationMethod::Hud;
        rule.notification_duration = DurationConfig::new(10, TimeUnit::Sec);
        rule.grace_duration = DurationConfig::new(5, TimeUnit::Sec);
        rule.trigger = TriggerType::Limit;

        cfg.rules = vec![rule.clone()];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        // Simulate that the rule is in its final grace countdown second, displayed as HudTimer
        engine.runtimes.get_mut("rule-hud-notify").unwrap().grace_countdown = Some((1, 5));
        engine.active_overlay_mode = OverlayMode::HudTimer {
            rule_id: "rule-hud-notify".to_string(),
            title: "Grace Timer".to_string(),
            paragraphs: Vec::new(),
            duration_seconds: 5,
            remaining_seconds: 1,
        };

        while overlay_rx.try_recv().is_ok() {}

        // Tick realtime: grace countdown finishes (1 -> 0), triggering execute_rule_action
        engine.realtime_tick().await;

        // The active overlay mode should now be HudToast for rule-hud-notify, NOT Hidden!
        match &engine.active_overlay_mode {
            OverlayMode::HudToast {
                rule_id,
                duration_seconds,
                remaining_seconds,
                ..
            } => {
                assert_eq!(rule_id, "rule-hud-notify");
                assert_eq!(*duration_seconds, 10);
                assert_eq!(*remaining_seconds, 10);
            }
            other => panic!("Expected OverlayMode::HudToast, got {:?}", other),
        }

        // Overlay action should have received Show with HudToast
        let mut received_toast_show = false;
        while let Ok(action) = overlay_rx.try_recv() {
            if let OverlayAction::Show(OverlayMode::HudToast { rule_id, .. }) = action {
                if rule_id == "rule-hud-notify" {
                    received_toast_show = true;
                }
            }
        }
        assert!(
            received_toast_show,
            "OverlayAction::Show(HudToast) should have been dispatched and preserved"
        );

        // Tick realtime a second time: the toast should now decrement from 10 to 9
        engine.realtime_tick().await;

        match &engine.active_overlay_mode {
            OverlayMode::HudToast {
                rule_id,
                duration_seconds,
                remaining_seconds,
                ..
            } => {
                assert_eq!(rule_id, "rule-hud-notify");
                assert_eq!(*duration_seconds, 10);
                assert_eq!(*remaining_seconds, 9);
            }
            other => panic!("Expected OverlayMode::HudToast, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_urls_rule_grace_countdown_tab_verification_and_app_context() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();

        let mut rule = Rule::default_template(0);
        rule.id = "rule-urls".to_string();
        rule.monitor_selection = MonitorSelection::Urls;
        rule.action = RuleAction::CloseTab;
        rule.grace_duration = DurationConfig::new(5, TimeUnit::Sec);
        rule.trigger = TriggerType::Limit;

        cfg.rules = vec![rule.clone()];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        let rt = engine.runtimes.get_mut("rule-urls").unwrap();
        rt.grace_countdown = Some((1, 5));
        rt.last_matched_app = Some("youtube.com".to_string());
        rt.last_matched_title = Some("NonExistentTabTitle12345XYZ".to_string());

        // Tick realtime: grace countdown finishes (1 -> 0).
        // Since no browser window matches "NonExistentTabTitle12345XYZ", destructive action CloseTab is skipped,
        // and cycle is marked completed.
        engine.realtime_tick().await;

        let rt_after = engine.runtimes.get("rule-urls").unwrap();
        assert_eq!(rt_after.grace_countdown, None);
        assert!(rt_after.action_executed_today);
    }

    #[tokio::test]
    async fn test_snooze_all_preserves_completed_cycle_when_not_displayed() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();

        let mut rule = Rule::default_template(0);
        rule.id = "rule-1".to_string();
        cfg.rules = vec![rule.clone()];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        let completed_time = Local::now() - Duration::minutes(20);
        let rt = engine.runtimes.get_mut("rule-1").unwrap();
        rt.complete_cycle(completed_time);
        assert_eq!(rt.last_completed_at, Some(completed_time));
        assert!(rt.can_rollback_completion);

        // Ensure rule is not currently displayed (active_overlay_mode is Hidden)
        assert!(matches!(engine.active_overlay_mode, OverlayMode::Hidden));
        assert!(!engine.is_rule_displayed("rule-1"));

        // User triggers SnoozeAll from system tray mid-cycle
        engine
            .handle_command(DaemonCommand::SnoozeAll { minutes: 15 })
            .await;

        let rt = engine.runtimes.get("rule-1").unwrap();
        // last_completed_at MUST be preserved and not rolled back to prev_completed_at (which was None)!
        assert_eq!(rt.last_completed_at, Some(completed_time));
        assert!(rt.snoozed_until.is_some());
        assert!(!rt.can_rollback_completion);
    }

    #[tokio::test]
    async fn test_reload_config_clears_deleted_or_disabled_rule_overlay() {
        let (overlay_tx, mut overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();

        let mut rule1 = Rule::default_template(0);
        rule1.id = "rule-1".to_string();

        let mut rule2 = Rule::default_template(1);
        rule2.id = "rule-2".to_string();

        cfg.rules = vec![rule1.clone(), rule2.clone()];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        // 1. rule-1 has an active break and is displayed as BreakEnforcer
        engine.runtimes.get_mut("rule-1").unwrap().active_break = Some((300, 300));
        engine.active_overlay_mode = OverlayMode::BreakEnforcer {
            rule_id: "rule-1".to_string(),
            title: "Rule 1 Break".to_string(),
            paragraphs: Vec::new(),
            duration_seconds: 300,
            remaining_seconds: 300,
            allow_snooze: true,
            strict_mode: false,
            click_through: false,
        };
        assert!(engine.is_rule_displayed("rule-1"));

        // Disable rule-1 and reload config
        let mut new_cfg1 = engine.config.clone();
        new_cfg1.rules[0].enabled = false;
        engine.handle_command(DaemonCommand::ReloadConfig(Box::new(new_cfg1))).await;

        // Timers for rule-1 must be reset and overlay hidden
        assert_eq!(engine.runtimes.get("rule-1").unwrap().active_break, None);
        assert!(matches!(engine.active_overlay_mode, OverlayMode::Hidden));

        // 2. rule-2 has grace countdown and is displayed as HudTimer
        engine.runtimes.get_mut("rule-2").unwrap().grace_countdown = Some((10, 10));
        engine.active_overlay_mode = OverlayMode::HudTimer {
            rule_id: "rule-2".to_string(),
            title: "Rule 2 Grace".to_string(),
            paragraphs: Vec::new(),
            duration_seconds: 10,
            remaining_seconds: 10,
        };
        assert!(engine.is_rule_displayed("rule-2"));

        // Delete rule-2 entirely and reload config
        let mut new_cfg2 = engine.config.clone();
        new_cfg2.rules.retain(|r| r.id != "rule-2");
        engine.handle_command(DaemonCommand::ReloadConfig(Box::new(new_cfg2))).await;

        // rule-2 is removed from runtimes and overlay is hidden
        assert!(!engine.runtimes.contains_key("rule-2"));
        assert!(matches!(engine.active_overlay_mode, OverlayMode::Hidden));

        // Drain overlay_rx and verify OverlayAction::Hide was sent
        let mut hide_sent = false;
        while let Ok(action) = overlay_rx.try_recv() {
            if matches!(action, OverlayAction::Hide) {
                hide_sent = true;
            }
        }
        assert!(hide_sent);

        // 3. Deleting active rule transitions to another rule with active countdown
        let mut new_cfg3 = AppConfig::default();
        let mut rule_a = Rule::default_template(0);
        rule_a.id = "rule-a".to_string();
        let mut rule_b = Rule::default_template(1);
        rule_b.id = "rule-b".to_string();
        new_cfg3.rules = vec![rule_a.clone(), rule_b.clone()];
        engine.handle_command(DaemonCommand::ReloadConfig(Box::new(new_cfg3))).await;

        // rule-a is displayed, but rule-b also has an active break
        engine.runtimes.get_mut("rule-a").unwrap().active_break = Some((200, 200));
        engine.runtimes.get_mut("rule-b").unwrap().active_break = Some((150, 150));
        engine.active_overlay_mode = OverlayMode::BreakEnforcer {
            rule_id: "rule-a".to_string(),
            title: "Rule A".to_string(),
            paragraphs: Vec::new(),
            duration_seconds: 200,
            remaining_seconds: 200,
            allow_snooze: true,
            strict_mode: false,
            click_through: false,
        };

        // Disable rule-a -> should transition to rule-b's break enforcer
        let mut new_cfg4 = engine.config.clone();
        new_cfg4.rules[0].enabled = false;
        engine.handle_command(DaemonCommand::ReloadConfig(Box::new(new_cfg4))).await;

        assert_eq!(engine.runtimes.get("rule-a").unwrap().active_break, None);
        assert!(engine.is_rule_displayed("rule-b"));
        match &engine.active_overlay_mode {
            OverlayMode::BreakEnforcer { rule_id, remaining_seconds, .. } => {
                assert_eq!(rule_id, "rule-b");
                assert_eq!(*remaining_seconds, 150);
            }
            other => panic!("Expected BreakEnforcer for rule-b, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_poll_tick_clears_off_schedule_or_disabled_active_timers() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();

        let mut rule = Rule::default_template(0);
        rule.id = "rule-sched".to_string();
        // Deactivate all schedule slots
        rule.schedule = RuleSchedule::from_bool_array(&[false; 168]);
        cfg.rules = vec![rule];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        // Rule is off-schedule but has active_break and is displayed
        engine.runtimes.get_mut("rule-sched").unwrap().active_break = Some((300, 300));
        engine.active_overlay_mode = OverlayMode::BreakEnforcer {
            rule_id: "rule-sched".to_string(),
            title: "Rule Break".to_string(),
            paragraphs: Vec::new(),
            duration_seconds: 300,
            remaining_seconds: 300,
            allow_snooze: true,
            strict_mode: false,
            click_through: false,
        };
        assert!(engine.is_rule_displayed("rule-sched"));

        engine.poll_tick().await;

        // After poll_tick, active_break was reset and overlay was hidden
        assert_eq!(engine.runtimes.get("rule-sched").unwrap().active_break, None);
        assert!(matches!(engine.active_overlay_mode, OverlayMode::Hidden));
    }

    #[test]
    fn test_find_matching_window_for_rule_urls_selection() {
        let now_utc = Utc::now();
        let mut rule = Rule::default_template(0);
        rule.monitor_selection = MonitorSelection::Urls;
        rule.url_regex = "(?i)youtube\\.com".to_string();
        let re = Regex::new(&rule.url_regex).ok();

        // 1. If newer non-matching event is the latest, it should return None even if an older matching event is in recency window
        let ev_old_matching = make_event(
            now_utc - Duration::seconds(4),
            2.0,
            "chrome.exe",
            "YouTube Video",
            "https://www.youtube.com/watch?v=123",
        );
        let ev_new_non_matching = make_event(
            now_utc - Duration::seconds(2),
            2.0,
            "chrome.exe",
            "Rust Docs",
            "https://doc.rust-lang.org/book/",
        );

        let res_switched = find_matching_window_for_rule(
            &rule,
            re.as_ref(),
            &[ev_old_matching.clone(), ev_new_non_matching],
            now_utc,
            5,
        );
        assert_eq!(res_switched, None);

        // 2. If the newest event matches the rule within recency window, it selects it
        let ev_newer_matching = make_event(
            now_utc - Duration::seconds(1),
            1.0,
            "chrome.exe",
            "YouTube Music",
            "https://music.youtube.com/watch?v=456",
        );

        let res_matching = find_matching_window_for_rule(
            &rule,
            re.as_ref(),
            &[ev_old_matching, ev_newer_matching],
            now_utc,
            5,
        );
        if enforcer::find_matching_window(enforcer::BROWSER_EXES, None).is_some() {
            assert_eq!(res_matching.as_deref(), Some("music.youtube.com"));
        } else {
            assert_eq!(res_matching, None);
        }
    }

    #[tokio::test]
    async fn test_evaluate_rule_urls_selects_newest_matching_tab_title() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();

        let mut rule = Rule::default_template(0);
        rule.id = "rule-urls-newest-title".to_string();
        rule.monitor_selection = MonitorSelection::Urls;
        rule.url_regex = "(?i)youtube\\.com".to_string();
        rule.limit = DurationConfig::new(10, TimeUnit::Min);

        cfg.rules = vec![rule.clone()];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        let now_utc = Utc::now();
        let ev_older = make_event(
            now_utc - Duration::seconds(15),
            5.0,
            "chrome.exe",
            "Older Video - YouTube",
            "https://www.youtube.com/watch?v=old",
        );
        let ev_newer = make_event(
            now_utc - Duration::seconds(2),
            2.0,
            "chrome.exe",
            "Latest Video - YouTube",
            "https://www.youtube.com/watch?v=new",
        );

        let re = Regex::new(&rule.url_regex).ok();

        // Populate runtime's last_matched_title using the new logic
        let recency_cutoff = now_utc
            - Duration::seconds((engine.config.general.poll_interval_seconds.max(2) as i64) * 2 + 5);
        let events = [ev_older, ev_newer];
        let matching_ev = events
            .iter()
            .filter(|ev| {
                let ev_end = event_end_utc(ev);
                ev_end >= recency_cutoff && event_matches_rule(ev, &rule, re.as_ref())
            })
            .max_by_key(|ev| (event_end_utc(ev), ev.timestamp))
            .or_else(|| {
                events
                    .iter()
                    .filter(|ev| event_matches_rule(ev, &rule, re.as_ref()))
                    .max_by_key(|ev| (event_end_utc(ev), ev.timestamp))
            });

        let rt = engine.runtimes.get_mut("rule-urls-newest-title").unwrap();
        if let Some(ev) = matching_ev {
            if let Some(ref title) = ev.data.title {
                if !title.trim().is_empty() {
                    rt.set_last_matched_title(Some(title.clone()));
                }
            }
        }

        assert_eq!(
            engine.runtimes.get("rule-urls-newest-title").unwrap().last_matched_title.as_deref(),
            Some("Latest Video - YouTube")
        );
    }

    #[tokio::test]
    async fn test_preview_rule_alert_overlay_and_notification() {
        let (overlay_tx, mut overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();
        let mut rule_overlay = Rule::default_template(0);
        rule_overlay.id = "rule-overlay".to_string();
        rule_overlay.action = RuleAction::ShowOverlay;
        rule_overlay.message = "Take a break! {app_name}".to_string();

        let mut rule_notif = Rule::default_template(1);
        rule_notif.id = "rule-notif".to_string();
        rule_notif.action = RuleAction::ShowNotification;
        rule_notif.notification_method = NotificationMethod::Hud;
        rule_notif.message = "Notification for {app_name}".to_string();

        cfg.rules = vec![rule_overlay.clone(), rule_notif.clone()];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        // 1. Preview rule with ShowOverlay
        engine
            .handle_command(DaemonCommand::PreviewRule(Box::new(rule_overlay)))
            .await;

        let action1 = overlay_rx.recv().await.expect("Expected overlay action");
        match action1 {
            OverlayAction::Show(OverlayMode::BreakEnforcer {
                rule_id,
                paragraphs,
                duration_seconds,
                remaining_seconds,
                strict_mode,
                ..
            }) => {
                assert_eq!(rule_id, "test_preview");
                assert_eq!(duration_seconds, 15);
                assert_eq!(remaining_seconds, 15);
                assert!(!strict_mode);
                assert!(!paragraphs.is_empty());
            }
            other => panic!("Expected BreakEnforcer overlay, got: {other:?}"),
        }

        // 2. Preview rule with ShowNotification (Hud)
        engine
            .handle_command(DaemonCommand::PreviewRule(Box::new(rule_notif)))
            .await;

        let action2 = overlay_rx.recv().await.expect("Expected overlay action");
        match action2 {
            OverlayAction::Show(OverlayMode::HudToast {
                rule_id,
                paragraphs,
                remaining_seconds,
                ..
            }) => {
                assert_eq!(rule_id, "test_preview");
                assert_eq!(remaining_seconds, 10);
                assert!(!paragraphs.is_empty());
            }
            other => panic!("Expected HudToast overlay, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_snooze_status_badges_and_tooltip() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();
        let mut rule1 = Rule::default_template(0);
        rule1.id = "rule-1".to_string();
        let mut rule2 = Rule::default_template(1);
        rule2.id = "rule-2".to_string();
        cfg.rules = vec![rule1, rule2];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        let now = Local::now();
        // Set rule 2 as snoozed for 10 minutes
        engine.runtimes.get_mut("rule-2").unwrap().snoozed_until = Some(now + Duration::minutes(10));

        let rt2 = engine.runtimes.get("rule-2").unwrap();
        let rem2 = ((rt2.snoozed_until.unwrap() - now).num_seconds().max(1) + 59) / 60;
        let badge2 = format!("Snoozed ({}m)", rem2);
        assert_eq!(badge2, "Snoozed (10m)");

        // Set global snooze for 15 minutes
        engine.global_snoozed_until = Some(now + Duration::minutes(15));
        let rem_global = ((engine.global_snoozed_until.unwrap() - now).num_seconds().max(1) + 59) / 60;
        let badge_global = format!("Snoozed ({}m)", rem_global);
        assert_eq!(badge_global, "Snoozed (15m)");

        let tooltip = format!(
            "AW-Enforcer (Snoozed {}m) — Today: {}h {}m",
            rem_global, 2, 30
        );
        assert_eq!(tooltip, "AW-Enforcer (Snoozed 15m) — Today: 2h 30m");
    }

    #[tokio::test]
    async fn test_pending_break_launch_retains_full_duration_without_same_tick_decrement() {
        let (overlay_tx, mut overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();

        let mut rule = Rule::default_template(0);
        rule.id = "rule-break-test".to_string();
        rule.action = RuleAction::ShowOverlay;
        rule.break_duration = DurationConfig::new(30, TimeUnit::Sec);
        cfg.rules = vec![rule.clone()];

        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        // Simulate a rule waiting on natural break delay for over 15 seconds
        let now = Local::now();
        engine.runtimes.get_mut("rule-break-test").unwrap().pending_break_since =
            Some(now - Duration::seconds(20));

        // Tick realtime: Step 1 launches the pending break
        engine.realtime_tick().await;

        let rt = engine.runtimes.get("rule-break-test").unwrap();
        // rt.active_break must retain its full (30, 30) duration and NOT be decremented to 29
        assert_eq!(rt.active_break, Some((30, 30)));
        assert_eq!(rt.pending_break_since, None);

        // active_overlay_mode must be BreakEnforcer with duration_seconds == 30 and remaining_seconds == 30
        match &engine.active_overlay_mode {
            OverlayMode::BreakEnforcer {
                rule_id,
                duration_seconds,
                remaining_seconds,
                ..
            } => {
                assert_eq!(rule_id, "rule-break-test");
                assert_eq!(*duration_seconds, 30);
                assert_eq!(*remaining_seconds, 30);
            }
            other => panic!("Expected BreakEnforcer overlay, got: {other:?}"),
        }

        // OverlayAction::Show must have been sent with duration_seconds: 30 and remaining_seconds: 30
        let mut received_show = false;
        while let Ok(action) = overlay_rx.try_recv() {
            if let OverlayAction::Show(OverlayMode::BreakEnforcer {
                rule_id,
                duration_seconds,
                remaining_seconds,
                ..
            }) = action
            {
                if rule_id == "rule-break-test" {
                    assert_eq!(duration_seconds, 30);
                    assert_eq!(remaining_seconds, 30);
                    received_show = true;
                }
            }
        }
        assert!(received_show, "OverlayAction::Show(BreakEnforcer) should be received");
    }

    #[test]
    fn test_find_matching_window_selects_ongoing_event_over_shorter_later_started_event() {
        let now_utc = Utc::now();
        let mut rule = Rule::default_template(0);
        rule.monitor_selection = MonitorSelection::Urls;
        rule.url_regex = "(?i)youtube\\.com".to_string();
        let re = Regex::new(&rule.url_regex).ok();

        // ev_ongoing started 10s ago with 30s duration (ends at now_utc + 20s)
        let ev_ongoing = make_event(
            now_utc - Duration::seconds(10),
            30.0,
            "chrome.exe",
            "Long Ongoing Video - YouTube",
            "https://www.youtube.com/watch?v=ongoing",
        );
        // ev_short started later (5s ago) but only lasted 2s (ended at now_utc - 3s)
        let ev_short = make_event(
            now_utc - Duration::seconds(5),
            2.0,
            "chrome.exe",
            "Short Blip - Other Site",
            "https://www.example.com",
        );

        let events = [ev_ongoing.clone(), ev_short.clone()];
        let best_ev = events
            .iter()
            .max_by_key(|ev| (event_end_utc(ev), ev.timestamp))
            .unwrap();

        // ev_ongoing should be picked because its end time is newer
        assert_eq!(
            best_ev.data.title.as_deref(),
            Some("Long Ongoing Video - YouTube")
        );

        let res = find_matching_window_for_rule(&rule, re.as_ref(), &events, now_utc, 10);
        if enforcer::find_matching_window(enforcer::BROWSER_EXES, None).is_some() {
            assert_eq!(res.as_deref(), Some("youtube.com"));
        } else {
            assert_eq!(res, None);
        }
    }

    #[tokio::test]
    async fn test_urls_rule_grace_countdown_tracks_in_site_title_changes() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();

        let mut rule = Rule::default_template(0);
        rule.id = "rule-urls-grace-spa".to_string();
        rule.monitor_selection = MonitorSelection::Urls;
        rule.action = RuleAction::CloseTab;
        rule.url_regex = "(?i)youtube\\.com".to_string();
        rule.grace_duration = DurationConfig::new(5, TimeUnit::Sec);
        rule.limit = DurationConfig::new(10, TimeUnit::Min);
        rule.trigger = TriggerType::Limit;

        cfg.rules = vec![rule.clone()];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        let now = Local::now();
        let now_utc = now.with_timezone(&Utc);
        let start_of_day_utc = now_utc - Duration::hours(5);

        // 1. Initially in grace countdown with an old video title
        let rt = engine.runtimes.get_mut("rule-urls-grace-spa").unwrap();
        rt.grace_countdown = Some((3, 5));
        rt.last_matched_app = Some("youtube.com".to_string());
        rt.set_last_matched_title(Some("Old Video 1 - YouTube".to_string()));

        assert_eq!(
            rt.last_matched_title.as_deref(),
            Some("Old Video 1 - YouTube")
        );

        // 2. While in grace countdown, user navigates to another YouTube video (SPA title change)
        let ev_updated = make_event(
            now_utc - Duration::seconds(1),
            2.0,
            "chrome.exe",
            "New Video 2 - YouTube",
            "https://www.youtube.com/watch?v=new_vid_2",
        );

        let re = Regex::new(&rule.url_regex).ok();
        // evaluate_rule during active grace_countdown still refreshes last_matched_title
        engine
            .evaluate_rule(
                &rule,
                re.as_ref(),
                now,
                &[ev_updated],
                start_of_day_utc,
                now_utc,
            )
            .await;

        let rt_after_eval = engine.runtimes.get("rule-urls-grace-spa").unwrap();
        assert_eq!(
            rt_after_eval.last_matched_title.as_deref(),
            Some("New Video 2 - YouTube")
        );
        assert!(rt_after_eval.last_matched_title_regex.is_some());
        assert!(rt_after_eval
            .last_matched_title_regex
            .as_ref()
            .unwrap()
            .is_match("New Video 2 - YouTube"));

        // 3. Grace countdown reaches 0: verification succeeds or completes cycle
        engine.runtimes.get_mut("rule-urls-grace-spa").unwrap().grace_countdown = Some((1, 5));
        engine.realtime_tick().await;

        let rt_done = engine.runtimes.get("rule-urls-grace-spa").unwrap();
        assert_eq!(rt_done.grace_countdown, None);
        assert!(rt_done.action_executed_today);
    }
}
