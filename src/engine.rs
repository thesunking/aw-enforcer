use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, Timelike, Utc};
use regex::Regex;
use slint::Color;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::aw_client::{AwClient, AwEvent};
use crate::config::{
    AppConfig, MonitorSelection, NotificationMethod, OverlayConfig, Rule, RuleAction,
    TriggerType,
};
use crate::enforcer;
use crate::formatter::{self, FormattedParagraph, MessageContext, TextHorizontalAlignment};
use crate::overlay::OverlayMode;

pub mod intervals;
pub use intervals::*;

pub mod matcher;
pub use matcher::*;

pub mod runtime;
pub use runtime::*;

pub mod stats_hud;
pub use stats_hud::*;

pub mod preview;

pub use crate::enforcer::get_idle_time_millis;
pub use crate::formatter::{format_snooze_badge, rule_message_fallback};

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
    ResetRuleSession { rule_id: String },
    ClearSnoozeAll,
}

/// Tri-state indicator for the system tray icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayIconState {
    Normal,
    Paused,
    Alert,
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
        any_snoozed: bool,
        tray_state: TrayIconState,
        status_level: String,
        total_active_minutes: u32,
        target_minutes: u32,
        histogram_values: Vec<f32>,
        histogram_labels: Vec<String>,
        is_loading: bool,
    },
    Hide,
}

/// Core background daemon engine that polls ActivityWatch, evaluates rules, and dispatches overlays/enforcements.
pub struct Engine {
    pub(crate) config: AppConfig,
    pub(crate) aw_client: Arc<AwClient>,
    pub(crate) runtimes: HashMap<String, RuleRuntime>,
    pub(crate) global_snoozed_until: Option<DateTime<Local>>,
    pub(crate) last_reset_date: NaiveDate,
    pub(crate) overlay_tx: mpsc::UnboundedSender<OverlayAction>,
    pub(crate) active_overlay_mode: OverlayMode,
    pub(crate) stats_cache: Option<StatsHudCache>,
    pub(crate) last_global_session_seconds: u32,
}

impl Engine {

    // ========================================================================
    // Lifecycle & Daemon Loops
    // ========================================================================

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
            last_global_session_seconds: 0,
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
                rt.reset_for_new_day();
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
        let bin_count = self.config.stats.histogram_bin_count.max(1) as usize;
        let bin_size_mins = self.config.stats.histogram_bin_size_minutes.max(1) as i64;
        let new_bin_start = align_stats_bin_start(now, bin_size_mins);

        if let Some(ref cache) = self.stats_cache {
            let config_changed = cache.bin_seconds.len() != bin_count || cache.bin_size_mins != bin_size_mins;
            let elapsed_bins = if !config_changed && new_bin_start > cache.current_bin_start {
                ((new_bin_start - cache.current_bin_start).num_minutes() / bin_size_mins).max(0) as usize
            } else {
                0
            };
            if config_changed || elapsed_bins > 1 {
                let start_time = new_bin_start - Duration::minutes(((bin_count - 1) as i64) * bin_size_mins);
                min_start_utc = min_start_utc.min(start_time.with_timezone(&Utc));
            } else {
                min_start_utc = min_start_utc.min(cache.current_bin_start.with_timezone(&Utc));
            }
        } else {
            let start_time = new_bin_start - Duration::minutes(((bin_count - 1) as i64) * bin_size_mins);
            min_start_utc = min_start_utc.min(start_time.with_timezone(&Utc));
        }

        let mut has_url_rules = false;

        if !self.config.general.pause_all_rules {
            let session_lookback_secs = (3600 + 300).max((self.last_global_session_seconds as i64) + 3600);
            let default_session_lookback = (now - Duration::seconds(session_lookback_secs)).with_timezone(&Utc);
            min_start_utc = min_start_utc.min(default_session_lookback);

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

        let buckets_ready = self.aw_client.prepare_poll_buckets(has_url_rules).await;
        let (all_events, mut web_events) = if !buckets_ready {
            (Vec::new(), Vec::new())
        } else if has_url_rules {
            let (all_res, web_res) = tokio::join!(
                self.aw_client.query_intersected_events(min_start_utc, now_utc),
                self.aw_client.query_web_events(min_start_utc, now_utc)
            );
            (all_res.unwrap_or_default(), web_res)
        } else {
            (
                self.aw_client
                    .query_intersected_events(min_start_utc, now_utc)
                    .await
                    .unwrap_or_default(),
                Vec::new(),
            )
        };

        if has_url_rules && buckets_ready {
            if all_events.is_empty() {
                web_events.clear();
            } else {
                web_events = filter_web_events_by_active_browser_windows(&web_events, &all_events);
            }
        }

        let merged_all_intervals = events_to_merged_intervals(&all_events);

        // 4. Evaluate Unified Rules (skip if all rules are paused indefinitely or globally snoozed)
        let mut evaluated_in_step_4 = std::collections::HashSet::new();
        let mut should_transition_after_eval = false;

        if self.config.general.pause_all_rules {
            for rule in self.config.rules.clone() {
                if self.deactivate_rule_runtime(&rule.id) {
                    should_transition_after_eval = true;
                }
            }
        } else if is_globally_snoozed {
            for rule in self.config.rules.clone() {
                if self.deactivate_rule_runtime(&rule.id) {
                    should_transition_after_eval = true;
                }
                if rule.enabled
                    && rule.schedule.is_active(day_index, hour_index)
                    && rule.trigger == TriggerType::Period
                {
                    let events_for_rule = if rule.monitor_selection == MonitorSelection::Urls {
                        &web_events
                    } else {
                        &all_events
                    };
                    if let Some(rt) = self.runtimes.get_mut(&rule.id) {
                        let last_completed = rt.last_completed_at;
                        let reset_gap = rule.reset_after_afk.to_seconds().max(1);
                        let compiled_re = rt.compiled_regex.as_ref();
                        let session_duration = calculate_rule_session_duration(
                            &rule,
                            compiled_re,
                            events_for_rule,
                            &merged_all_intervals,
                            now,
                            now_utc,
                            last_completed,
                            reset_gap,
                        );
                        if session_duration == 0 {
                            rt.warned_current_cycle = false;
                        }
                        rt.current_session_seconds = session_duration;
                    }
                }
            }
        } else {
            for rule in self.config.rules.clone() {
                if !rule.enabled
                    || !rule.schedule.is_active(day_index, hour_index)
                {
                    if self.deactivate_rule_runtime(&rule.id) {
                        should_transition_after_eval = true;
                    }
                    continue;
                }

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
                        &merged_all_intervals,
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
        let max_rule_session_secs = self.runtimes.values().map(|rt| rt.current_session_seconds).max().unwrap_or(0);

        let global_session_secs = if self.config.general.pause_all_rules {
            0
        } else {
            calculate_continuous_session_from_merged(&merged_all_intervals, now_utc, 300)
        };
        self.last_global_session_seconds = global_session_secs;
        let max_session_secs = global_session_secs.max(max_rule_session_secs);

        let total_today_secs = compute_overlap_from_merged_intervals(&merged_all_intervals, start_of_day_utc, now_utc);
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
                format_snooze_badge(until, now)
            } else if let Some(until) = rt.snoozed_until.filter(|&u| u > now) {
                format_snooze_badge(until, now)
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
                    let effective_start_utc = rt
                        .daily_reset_at
                        .map(|dt| dt.with_timezone(&Utc).max(start_of_day_utc))
                        .unwrap_or(start_of_day_utc);
                    let used = if is_wildcard_executables_rule(rule) {
                        compute_overlap_from_merged_intervals(
                            &merged_all_intervals,
                            effective_start_utc,
                            now_utc,
                        )
                        .round() as u32
                    } else {
                        let events_for_rule = if rule.monitor_selection == MonitorSelection::Urls {
                            &web_events
                        } else {
                            &all_events
                        };
                        calculate_usage_for_rule(
                            events_for_rule,
                            rule,
                            rt.compiled_regex.as_ref(),
                            effective_start_utc,
                            now_utc,
                        )
                    };
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

        let today_hm = format!("{}h {}m", total_today_mins / 60, total_today_mins % 60);
        let strict_lock_active = self.is_strict_lock_active(now);
        let tray_tooltip = if self.config.general.pause_all_rules {
            format!("AW-Enforcer (Paused) — Today: {today_hm}")
        } else if let Some(until) = self.global_snoozed_until.filter(|&u| u > now) {
            let rem_mins = ((until - now).num_seconds().max(1) + 59) / 60;
            format!("AW-Enforcer (Snoozed {rem_mins}m) — Today: {today_hm}")
        } else {
            format!(
                "AW-Enforcer — Today: {today_hm} | Session: {}m",
                max_session_secs / 60
            )
        };

        let any_snoozed = self.global_snoozed_until.filter(|&u| u > now).is_some()
            || self.config.rules.iter().any(|r| {
                r.enabled
                    && self
                        .runtimes
                        .get(&r.id)
                        .and_then(|rt| rt.snoozed_until)
                        .filter(|&u| u > now)
                        .is_some()
            });

        // 6. Live-refresh or initialize the 24-Hour Statistics HUD cache
        if let Some(ref mut cache) = self.stats_cache {
            cache.refresh_from_events(now, &self.config.stats, &merged_all_intervals);
        } else {
            self.stats_cache = Some(StatsHudCache::from_intervals(
                now,
                &self.config.stats,
                &merged_all_intervals,
                false,
            ));
        }

        let (status_level, total_active_minutes, target_minutes, histogram_values, histogram_labels, is_loading) =
            if let Some(OverlayMode::HudStatus {
                status_level,
                total_active_minutes,
                target_minutes,
                bin_values,
                bin_labels,
                is_loading,
                ..
            }) = self.build_hud_status_from_cache()
            {
                (
                    status_level,
                    total_active_minutes,
                    target_minutes,
                    bin_values,
                    bin_labels,
                    is_loading,
                )
            } else {
                (
                    "Loading...".to_string(),
                    0,
                    self.config.stats.daily_screentime_target_minutes,
                    Vec::new(),
                    Vec::new(),
                    true,
                )
            };

        let tray_state = self.current_tray_state(now);

        self.overlay_tx
            .send(OverlayAction::UpdateLiveStatus {
                session_duration_text: format!("{}m", max_session_secs / 60),
                screentime_text: today_hm,
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
            })
            .ok();

        // 7. Live-refresh the 24-Hour Statistics HUD if currently open (reusing all_events without extra AW queries)
        self.refresh_stats_overlay_from_events(now, &merged_all_intervals);
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
            if rule_id.starts_with("test_") && *remaining_seconds > 0 {
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
                        .unwrap_or_else(|| "Active Application".to_string())
                } else {
                    let win = find_matching_window_for_rule(
                        &rule,
                        re.as_ref(),
                        &[],
                        now.with_timezone(&Utc),
                        self.config.general.poll_interval_seconds,
                    );
                    let rt_app = self.runtimes.get(&rule.id).and_then(|r| r.last_matched_app.clone());
                    resolve_effective_app_name(win, rt_app)
                };
                let elapsed = self.rule_elapsed_seconds(&rule);
                let threshold = rule.trigger_threshold_seconds();
                let break_secs = rule.break_duration.to_seconds().max(1);
                let ctx = self.build_message_context(&rule, &matched_app, elapsed, threshold, 0);
                let raw_msg = self.get_effective_rule_message(&rule);

                if self.config.general.suppress_fullscreen_d3d
                    && enforcer::is_fullscreen_or_presentation_active()
                {
                    if let Some(rt) = self.runtimes.get_mut(&rule.id) {
                        rt.complete_cycle(now);
                    }
                    enforcer::send_system_notification(
                        &rule.name,
                        &formatter::format_plain_message(raw_msg, &ctx),
                    );
                    continue;
                }

                if let Some(rt) = self.runtimes.get_mut(&rule.id) {
                    rt.prev_completed_at = rt.last_completed_at;
                    rt.active_break = Some((break_secs, break_secs));
                    rt.can_rollback_completion = true;
                    rt.action_executed_today = true;
                }
                let mode = self.build_break_enforcer_overlay(&rule, &ctx, break_secs, break_secs);
                self.show_overlay(mode);
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
                        rt.grace_dismissed = false;
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

        for rule_id in &completed_enforcements {
            if let Some(rule) = self.config.rules.iter().find(|r| r.id == *rule_id).cloned() {
                let re = self.runtimes.get(&rule.id).and_then(|r| r.compiled_regex.clone());
                let target_window = if rule.monitor_selection == MonitorSelection::Urls {
                    let maybe_title_re = self
                        .runtimes
                        .get(&rule.id)
                        .and_then(|r| r.last_matched_title_regex.as_ref());
                    if let Some(title_re) = maybe_title_re {
                        if rule.action == RuleAction::CloseTab {
                            enforcer::find_matching_foreground_window(enforcer::BROWSER_EXES, Some(title_re))
                        } else {
                            enforcer::find_matching_window(enforcer::BROWSER_EXES, Some(title_re))
                        }
                    } else if matches!(rule.action, RuleAction::CloseTab | RuleAction::CloseWindow) {
                        None
                    } else {
                        let browser_open = enforcer::find_matching_window(enforcer::BROWSER_EXES, None);
                        let app_matched = self
                            .runtimes
                            .get(&rule.id)
                            .map(|r| {
                                matched_url_or_app_matches_rule(
                                    r.last_matched_url.as_deref(),
                                    r.last_matched_app.as_deref(),
                                    re.as_ref(),
                                )
                            })
                            .unwrap_or(false);
                        if app_matched {
                            browser_open
                        } else {
                            None
                        }
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
                        .unwrap_or_else(|| "Active Application".to_string())
                } else {
                    let rt_app = self.runtimes.get(&rule.id).and_then(|r| r.last_matched_app.clone());
                    resolve_effective_app_name(target_window, rt_app)
                };
                let elapsed = self.rule_elapsed_seconds(&rule);
                let threshold = rule.trigger_threshold_seconds();
                self.execute_rule_action(&rule, re.as_ref(), now, &matched_name, elapsed, threshold).await;
            }
        }

        if grace_displayed_finished && matches!(self.active_overlay_mode, OverlayMode::Hidden) {
            self.transition_to_active_countdown_or_hide();
        }
    }


    /// Handles asynchronous daemon commands from tray or UI.
    pub async fn handle_command(&mut self, cmd: DaemonCommand) {
        match cmd {
            DaemonCommand::SnoozeBreak { rule_id, minutes } => {
                if let Some(rule) = self.config.rules.iter().find(|r| r.id == rule_id) {
                    if rule.strict_mode {
                        tracing::warn!("SnoozeBreak ignored: strict mode active for rule {}", rule_id);
                        return;
                    }
                }
                let now = Local::now();
                let until = now + Duration::minutes(minutes as i64);
                if let Some(rt) = self.runtimes.get_mut(&rule_id) {
                    rt.snooze(until);
                }
                self.transition_if_displayed_or_idle(&rule_id);
                self.poll_tick().await;
            }
            DaemonCommand::AcknowledgeAppWarning { rule_id } => {
                tracing::debug!("Acknowledged app warning for rule {rule_id}");
                let is_strict = self
                    .config
                    .rules
                    .iter()
                    .any(|r| r.id == rule_id && r.strict_mode);
                if is_strict {
                    if let Some(rt) = self.runtimes.get(&rule_id) {
                        if rt.grace_countdown.is_some()
                            || rt.active_break.is_some()
                            || rt.pending_break_since.is_some()
                        {
                            tracing::warn!("AcknowledgeAppWarning ignored: strict mode active for rule {}", rule_id);
                            return;
                        }
                    }
                }

                let is_destructive = self
                    .config
                    .rules
                    .iter()
                    .any(|r| r.id == rule_id && r.action.is_destructive());

                let mut destructive_grace_dismissed = false;
                if let Some(rt) = self.runtimes.get_mut(&rule_id) {
                    if rt.grace_countdown.is_some() {
                        if is_destructive {
                            destructive_grace_dismissed = true;
                            rt.grace_dismissed = true;
                        } else {
                            rt.complete_cycle(Local::now());
                        }
                    }
                    rt.can_rollback_completion = false;
                }

                if destructive_grace_dismissed {
                    self.transition_to_active_countdown_or_hide();
                } else {
                    self.transition_if_displayed_or_idle(&rule_id);
                }
            }
            DaemonCommand::SkipBreak { rule_id } => {
                if let Some(rule) = self.config.rules.iter().find(|r| r.id == rule_id) {
                    if rule.strict_mode {
                        tracing::warn!("SkipBreak ignored: strict mode active for rule {}", rule_id);
                        return;
                    }
                }
                if let Some(rt) = self.runtimes.get_mut(&rule_id) {
                    rt.complete_cycle(Local::now());
                }
                self.transition_if_displayed_or_idle(&rule_id);
                self.poll_tick().await;
            }
            DaemonCommand::DismissBreakOverlay => {
                if self.is_strict_lock_active(Local::now()) {
                    tracing::warn!("DismissBreakOverlay ignored: strict enforcement active");
                    return;
                }
                let now = Local::now();
                for rt in self.runtimes.values_mut() {
                    if rt.active_break.is_some() || rt.pending_break_since.is_some() {
                        rt.complete_cycle(now);
                    }
                }
                self.hide_overlay();
                self.poll_tick().await;
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
                self.poll_tick().await;
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
                self.poll_tick().await;
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
                if !self.aw_client.matches_config(self.config.general.aw_port, &self.config.general.api_key) {
                    self.aw_client = Arc::new(AwClient::new(
                        self.config.general.aw_port,
                        &self.config.general.api_key,
                    ));
                }
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
                self.poll_tick().await;
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
            DaemonCommand::ResetRuleSession { rule_id } => {
                if self.is_strict_lock_active(Local::now()) {
                    tracing::warn!("ResetRuleSession ignored: strict enforcement active");
                    return;
                }
                let now = Local::now();
                if let Some(rt) = self.runtimes.get_mut(&rule_id) {
                    rt.reset_session(now);
                }
                self.transition_if_displayed_or_idle(&rule_id);
                self.poll_tick().await;
            }
            DaemonCommand::ClearSnoozeAll => {
                self.global_snoozed_until = None;
                for rt in self.runtimes.values_mut() {
                    rt.snoozed_until = None;
                }
                self.poll_tick().await;
            }
        }
    }



    // ========================================================================
    // Rule Evaluation & Enforcement
    // ========================================================================

    /// Evaluates a single enabled, in-schedule rule against recent ActivityWatch events and triggers warnings or actions.
    pub async fn evaluate_rule(
        &mut self,
        rule: &Rule,
        compiled_re: Option<&Regex>,
        now: DateTime<Local>,
        events: &[AwEvent],
        start_of_day_utc: DateTime<Utc>,
        now_utc: DateTime<Utc>,
        merged_all_intervals: &[(DateTime<Utc>, DateTime<Utc>)],
    ) -> bool {
        let merged_fallback;
        let merged_all_intervals = if merged_all_intervals.is_empty()
            && !events.is_empty()
            && is_wildcard_executables_rule(rule)
        {
            merged_fallback = events_to_merged_intervals(events);
            &merged_fallback[..]
        } else {
            merged_all_intervals
        };

        let is_displayed = self.is_rule_displayed(&rule.id);
        if !is_displayed {
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
            if is_displayed && rt.can_rollback_completion {
                return false;
            }
            if rt.grace_countdown.is_some() {
                if rule.monitor_selection == MonitorSelection::Urls {
                    let recency_cutoff = calculate_recency_cutoff(now_utc, self.config.general.poll_interval_seconds);
                    let matching_ev = find_latest_matching_event(events, rule, compiled_re, recency_cutoff);
                    rt.update_matched_metadata(matching_ev, true);
                }
                return false;
            }
            if let Some(until) = rt.snoozed_until {
                if now < until {
                    if rule.trigger == TriggerType::Period {
                        let last_completed = rt.last_completed_at;
                        let reset_gap = rule.reset_after_afk.to_seconds().max(1);
                        let session_duration = calculate_rule_session_duration(
                            rule,
                            compiled_re,
                            events,
                            merged_all_intervals,
                            now,
                            now_utc,
                            last_completed,
                            reset_gap,
                        );
                        rt.current_session_seconds = session_duration;
                    }
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
                let session_duration = calculate_rule_session_duration(
                    rule,
                    compiled_re,
                    events,
                    merged_all_intervals,
                    now,
                    now_utc,
                    last_completed,
                    reset_gap,
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
                let effective_start_utc = self
                    .runtimes
                    .get(&rule.id)
                    .and_then(|rt| rt.daily_reset_at)
                    .map(|dt| dt.with_timezone(&Utc).max(start_of_day_utc))
                    .unwrap_or(start_of_day_utc);
                let used = if is_wildcard_executables_rule(rule) {
                    compute_overlap_from_merged_intervals(
                        merged_all_intervals,
                        effective_start_utc,
                        now_utc,
                    )
                    .round() as u32
                } else {
                    calculate_usage_for_rule(events, rule, compiled_re, effective_start_utc, now_utc)
                };
                let limit_secs = rule.limit.to_seconds();
                if let Some(rt) = self.runtimes.get_mut(&rule.id) {
                    rt.current_session_seconds = 0;
                    rt.daily_usage_seconds = used;
                }
                (used, limit_secs)
            }
        };

        let warn_secs = rule.warn_before.to_seconds();
        let should_warn = threshold_secs > 0
            && warn_secs > 0
            && warn_secs < threshold_secs
            && elapsed_secs >= threshold_secs.saturating_sub(warn_secs)
            && elapsed_secs < threshold_secs;

        let should_trigger = if threshold_secs == 0 {
            if elapsed_secs > 0 {
                true
            } else {
                is_target_actively_present(
                    events,
                    rule,
                    compiled_re,
                    now_utc,
                    self.config.general.poll_interval_seconds,
                )
            }
        } else {
            elapsed_secs >= threshold_secs
        };

        if !should_warn && !should_trigger {
            if let Some(rt) = self.runtimes.get_mut(&rule.id) {
                let recency_cutoff = calculate_recency_cutoff(now_utc, self.config.general.poll_interval_seconds);
                let matching_ev = find_latest_matching_event(events, rule, compiled_re, recency_cutoff);
                rt.update_matched_metadata(matching_ev, rule.monitor_selection == MonitorSelection::Urls);
            }
            return true;
        }

        let matched_window = find_matching_window_for_rule(
            rule,
            compiled_re,
            events,
            now_utc,
            self.config.general.poll_interval_seconds,
        );
        let Some(matched_app_name) = matched_window else {
            if rule.monitor_selection == MonitorSelection::Urls {
                if let Some(rt) = self.runtimes.get_mut(&rule.id) {
                    rt.clear_matched_metadata();
                }
            }
            return true;
        };

        if let Some(rt) = self.runtimes.get_mut(&rule.id) {
            rt.last_matched_app = Some(matched_app_name.clone());

            let recency_cutoff = calculate_recency_cutoff(now_utc, self.config.general.poll_interval_seconds);
            let matching_ev = find_latest_matching_event(events, rule, compiled_re, recency_cutoff);
            rt.update_matched_metadata(matching_ev, rule.monitor_selection == MonitorSelection::Urls);
        }

        let grace_secs = rule.grace_duration.to_seconds();

        // 3. Stage 1: Warn Before Action (0 = disabled)
        if should_warn {
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
        if should_trigger {
            // For Limit triggers with non-destructive actions (ShowOverlay, ShowNotification, ShowStatistics),
            // only fire once per logical day unless snoozed.
            let is_destructive = rule.action.is_destructive();
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
            {
                if get_idle_time_millis()
                    >= (self.config.general.poll_interval_seconds.max(2) as u32) * 1000
                {
                    return true;
                }
                if !is_wildcard_executables_rule(rule)
                    && !is_target_actively_present(
                        events,
                        rule,
                        compiled_re,
                        now_utc,
                        self.config.general.poll_interval_seconds,
                    )
                {
                    return true;
                }
            }

            let effective_grace = if rule.trigger == TriggerType::Limit && already_executed {
                if rule.action == RuleAction::LockWorkstation {
                    grace_secs.max(15)
                } else if grace_secs > 0 {
                    grace_secs.min(5)
                } else {
                    0
                }
            } else {
                grace_secs
            };

            if effective_grace > 0 {
                if let Some(rt) = self.runtimes.get_mut(&rule.id) {
                    rt.grace_countdown = Some((effective_grace, effective_grace));
                    rt.grace_dismissed = false;
                }
                let ctx = self.build_message_context(rule, &matched_app_name, elapsed_secs, threshold_secs, effective_grace);
                let mode = self.build_hud_timer_overlay(rule, &ctx, effective_grace, effective_grace);
                self.show_overlay(mode);
            } else {
                self.execute_rule_action(rule, compiled_re, now, &matched_app_name, elapsed_secs, threshold_secs).await;
            }
        }

        true
    }


    /// Dispatches a Stage-1 pre-action warning via either a HUD toast or a native Windows notification.
    fn dispatch_pre_action_warning(&mut self, rule: &Rule, ctx: &MessageContext, rem_secs: u32) {
        let suppress_intrusive = self.config.general.suppress_fullscreen_d3d
            && enforcer::is_fullscreen_or_presentation_active();
        let use_windows = suppress_intrusive || rule.notification_method == NotificationMethod::Windows;

        let warn_title = format!("Upcoming: {}", rule.name);
        let warn_body = if rule.action != RuleAction::ShowOverlay && !rule.message.trim().is_empty() {
            formatter::format_plain_message(&rule.message, ctx)
        } else {
            format!("{} in {}.", rule.action.as_str(), formatter::format_seconds_human(rem_secs))
        };

        if use_windows {
            enforcer::send_system_notification(&warn_title, &warn_body);
        } else {
            let default_msg = format!("[color=#f59e0b bold] {} in {}.", rule.action.as_str(), formatter::format_seconds_human(rem_secs));
            let raw_msg = if rule.action != RuleAction::ShowOverlay && !rule.message.trim().is_empty() {
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
                title: warn_title.clone(),
                paragraphs,
                snooze_options_minutes: if rule.strict_mode { Vec::new() } else { vec![5, 10, 15] },
                duration_seconds: dur,
                remaining_seconds: dur,
                show_time_remaining: rule.show_remaining_time,
            });
            if !self.is_rule_displayed(&rule.id) {
                enforcer::send_system_notification(&warn_title, &warn_body);
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
                        rt.complete_cycle(now);
                    }
                    let raw_msg = self.get_effective_rule_message(rule);
                    enforcer::send_system_notification(&rule.name, &formatter::format_plain_message(raw_msg, &ctx));
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
                let mode = self.build_break_enforcer_overlay(rule, &ctx, break_secs, break_secs);
                self.show_overlay(mode);
            }
            RuleAction::ShowNotification => {
                if let Some(rt) = self.runtimes.get_mut(&rule.id) {
                    rt.complete_cycle(now);
                }
                match rule.notification_method {
                    NotificationMethod::Windows => {
                        let raw_msg = self.get_effective_rule_message(rule);
                        enforcer::send_system_notification(&rule.name, &formatter::format_plain_message(raw_msg, &ctx));
                    }
                    NotificationMethod::Hud => {
                        let raw_msg = self.get_effective_rule_message(rule);
                        let paragraphs = self.parse_rule_message(
                            raw_msg,
                            &ctx,
                            TextHorizontalAlignment::Left,
                        );
                        let dur = rule.notification_duration.to_seconds().max(1);
                        self.show_overlay(OverlayMode::HudToast {
                            rule_id: rule.id.clone(),
                            title: rule.name.clone(),
                            paragraphs,
                            snooze_options_minutes: if rule.strict_mode { Vec::new() } else { vec![5, 10, 15] },
                            duration_seconds: dur,
                            remaining_seconds: dur,
                            show_time_remaining: rule.show_remaining_time,
                        });
                        if self.is_rule_displayed(&rule.id) {
                            if let Some(rt) = self.runtimes.get_mut(&rule.id) {
                                rt.can_rollback_completion = true;
                            }
                        } else {
                            enforcer::send_system_notification(
                                &rule.name,
                                &formatter::format_plain_message(raw_msg, &ctx),
                            );
                        }
                    }
                }
            }
            RuleAction::ShowStatistics => {
                if let Some(rt) = self.runtimes.get_mut(&rule.id) {
                    rt.complete_cycle(now);
                }
                self.display_stats_overlay(rule.click_through).await;
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
                if let Some(rt) = self.runtimes.get_mut(&rule.id) {
                    rt.complete_cycle(now);
                }
                if rule.monitor_selection == MonitorSelection::Titles {
                    enforcer::close_active_browser_tab::<&str>(&[], compiled_re);
                } else if rule.monitor_selection == MonitorSelection::Urls {
                    if let Some(ref re) = last_title_re {
                        enforcer::close_active_browser_tab(enforcer::BROWSER_EXES, Some(re));
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
                if let Some(rt) = self.runtimes.get_mut(&rule.id) {
                    rt.complete_cycle(now);
                }
                if rule.monitor_selection == MonitorSelection::Titles {
                    enforcer::close_target_windows::<&str>(&[], compiled_re);
                } else if rule.monitor_selection == MonitorSelection::Urls {
                    if let Some(ref re) = last_title_re {
                        enforcer::close_target_windows(enforcer::BROWSER_EXES, Some(re));
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


    /// Deactivates a rule's active timers, pending breaks, or grace countdowns (e.g. when paused or off-schedule).
    /// Returns true if the deactivated rule was actively displayed on screen.
    fn deactivate_rule_runtime(&mut self, rule_id: &str) -> bool {
        if let Some(rt) = self.runtimes.get_mut(rule_id) {
            if rt.pending_break_since.is_some()
                || rt.grace_countdown.is_some()
                || rt.active_break.is_some()
            {
                rt.reset_active_timers();
                return self.is_rule_displayed(rule_id);
            } else {
                rt.current_session_seconds = 0;
            }
        }
        false
    }



    // ========================================================================
    // Overlay State Transitions & Dispatch
    // ========================================================================

    /// Dispatches an `OverlayMode` to the UI thread if its priority is at least as high as the currently active overlay.
    pub(crate) fn show_overlay(&mut self, mode: OverlayMode) {
        let is_current_test = self
            .active_overlay_mode
            .rule_id()
            .map(|id| id.starts_with("test_"))
            .unwrap_or(false);
        // Never allow a lower-priority overlay to clobber a higher-priority active overlay,
        // unless the active overlay is a test/preview overlay being replaced.
        if !is_current_test {
            if mode.priority() < self.active_overlay_mode.priority()
                && !matches!(self.active_overlay_mode, OverlayMode::Hidden)
            {
                return;
            }

            if let (
                OverlayMode::BreakEnforcer {
                    rule_id: cur_rule_id,
                    strict_mode: cur_strict,
                    click_through: cur_click,
                    remaining_seconds: cur_rem,
                    ..
                },
                OverlayMode::BreakEnforcer {
                    rule_id: new_rule_id,
                    strict_mode: new_strict,
                    click_through: new_click,
                    remaining_seconds: new_rem,
                    ..
                },
            ) = (&self.active_overlay_mode, &mode)
            {
                if cur_rule_id != new_rule_id {
                    let cannot_replace = (*cur_strict && !*new_strict)
                        || (*cur_strict == *new_strict && !*cur_click && *new_click)
                        || (*cur_strict == *new_strict && *cur_click == *new_click && *cur_rem >= *new_rem);
                    if cannot_replace {
                        return;
                    }
                }
            }

            if let (
                OverlayMode::HudTimer {
                    rule_id: cur_rule_id,
                    strict_mode: cur_strict,
                    remaining_seconds: cur_rem,
                    ..
                },
                OverlayMode::HudTimer {
                    rule_id: new_rule_id,
                    strict_mode: new_strict,
                    remaining_seconds: new_rem,
                    ..
                },
            ) = (&self.active_overlay_mode, &mode)
            {
                if cur_rule_id != new_rule_id {
                    let cur_is_destructive = self
                        .config
                        .rules
                        .iter()
                        .any(|r| &r.id == cur_rule_id && r.action.is_destructive());
                    let new_is_destructive = self
                        .config
                        .rules
                        .iter()
                        .any(|r| &r.id == new_rule_id && r.action.is_destructive());
                    let cur_urgency = (*cur_strict, cur_is_destructive, std::cmp::Reverse(*cur_rem));
                    let new_urgency = (*new_strict, new_is_destructive, std::cmp::Reverse(*new_rem));
                    if cur_urgency >= new_urgency {
                        return;
                    }
                }
            }

            if let (
                OverlayMode::HudToast {
                    rule_id: cur_rule_id,
                    snooze_options_minutes: cur_snooze,
                    ..
                },
                OverlayMode::HudToast {
                    rule_id: new_rule_id,
                    snooze_options_minutes: new_snooze,
                    ..
                },
            ) = (&self.active_overlay_mode, &mode)
            {
                if cur_rule_id != new_rule_id && cur_snooze.is_empty() && !new_snooze.is_empty() {
                    return;
                }
            }
        }
        if !matches!(mode, OverlayMode::HudStatus { .. }) {
            self.stats_cache = None;
        }
        self.active_overlay_mode = mode.clone();
        self.overlay_tx.send(OverlayAction::Show(mode)).ok();
    }


    /// Hides any active overlay and resets `active_overlay_mode` to `OverlayMode::Hidden`.
    fn hide_overlay(&mut self) {
        self.active_overlay_mode = OverlayMode::Hidden;
        self.stats_cache = None;
        self.overlay_tx.send(OverlayAction::Hide).ok();
    }


    /// Transitions to the highest-priority remaining active countdown rule, or hides the overlay if none remain.
    pub fn transition_to_active_countdown_or_hide(&mut self) {
        // 1. Highest priority: BreakEnforcer (active_break > 0)
        let active_break_rule = self
            .config
            .rules
            .iter()
            .filter(|r| {
                r.enabled
                    && self
                        .runtimes
                        .get(&r.id)
                        .and_then(|rt| rt.active_break)
                        .map(|(rem, _)| rem > 0)
                        .unwrap_or(false)
            })
            .max_by_key(|r| {
                let rem = self
                    .runtimes
                    .get(&r.id)
                    .and_then(|rt| rt.active_break)
                    .map(|(rem, _)| rem)
                    .unwrap_or(0);
                (r.strict_mode, !r.click_through, rem)
            })
            .cloned();

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
            let mode = self.build_break_enforcer_overlay(&rule, &ctx, total, rem);
            self.active_overlay_mode = OverlayMode::Hidden;
            self.show_overlay(mode);
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
        let grace_rule = self
            .config
            .rules
            .iter()
            .filter(|r| {
                r.enabled
                    && self
                        .runtimes
                        .get(&r.id)
                        .map(|rt| !rt.grace_dismissed && rt.grace_countdown.map(|(rem, _)| rem > 0).unwrap_or(false))
                        .unwrap_or(false)
            })
            .max_by_key(|r| {
                let rem = self
                    .runtimes
                    .get(&r.id)
                    .and_then(|rt| rt.grace_countdown)
                    .map(|(rem, _)| rem)
                    .unwrap_or(u32::MAX);
                (
                    r.strict_mode,
                    r.action.is_destructive(),
                    std::cmp::Reverse(rem),
                )
            })
            .cloned();

        if let Some(rule) = grace_rule {
            let (rem, total) = self
                .runtimes
                .get(&rule.id)
                .and_then(|rt| rt.grace_countdown)
                .unwrap();
            let matched_app = self.get_last_matched_app_or_default(&rule.id);
            let elapsed = self.rule_elapsed_seconds(&rule);
            let threshold = rule.trigger_threshold_seconds();
            let ctx = self.build_message_context(&rule, &matched_app, elapsed, threshold, rem);
            let mode = self.build_hud_timer_overlay(&rule, &ctx, total, rem);
            self.active_overlay_mode = OverlayMode::Hidden;
            self.show_overlay(mode);
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


    /// Transitions to another active countdown or hides the overlay if the rule is currently displayed or no countdowns are active.
    fn transition_if_displayed_or_idle(&mut self, rule_id: &str) {
        if self.is_rule_displayed(rule_id) || !self.any_countdown_active() {
            self.transition_to_active_countdown_or_hide();
        }
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
            if let Some(rule_id) = self.active_overlay_mode.rule_id() {
                if let Some(rt) = self.runtimes.get_mut(rule_id) {
                    rt.can_rollback_completion = false;
                }
            }
            self.transition_to_active_countdown_or_hide();
        }
    }



    // ========================================================================
    // Presentation Context & Overlay Builders
    // ========================================================================

    /// Constructs an `OverlayMode::BreakEnforcer` mode for the given rule and context.
    fn build_break_enforcer_overlay(
        &self,
        rule: &Rule,
        ctx: &MessageContext,
        duration_seconds: u32,
        remaining_seconds: u32,
    ) -> OverlayMode {
        let raw_msg = self.get_effective_rule_message(rule);
        let paragraphs = self.parse_rule_message(
            raw_msg,
            ctx,
            TextHorizontalAlignment::Center,
        );
        OverlayMode::BreakEnforcer {
            rule_id: rule.id.clone(),
            title: rule.name.clone(),
            paragraphs,
            duration_seconds,
            remaining_seconds,
            allow_snooze: !rule.strict_mode,
            strict_mode: rule.strict_mode,
            click_through: rule.click_through,
        }
    }


    /// Constructs an `OverlayMode::HudTimer` mode for the given rule and context.
    fn build_hud_timer_overlay(
        &self,
        rule: &Rule,
        ctx: &MessageContext,
        duration_seconds: u32,
        remaining_seconds: u32,
    ) -> OverlayMode {
        let default_msg = format!(
            "[color=#f87171 bold] {} in {}s.",
            rule.action.as_str(),
            duration_seconds
        );
        let raw_msg = if rule.action != RuleAction::ShowOverlay && !rule.message.trim().is_empty() {
            &rule.message
        } else {
            &default_msg
        };
        let paragraphs = self.parse_rule_message(
            raw_msg,
            ctx,
            TextHorizontalAlignment::Left,
        );
        OverlayMode::HudTimer {
            rule_id: rule.id.clone(),
            title: format!("{}: {}", rule.action.as_str(), rule.name),
            paragraphs,
            duration_seconds,
            remaining_seconds,
            allow_snooze: !rule.strict_mode,
            strict_mode: rule.strict_mode,
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


    /// Parses a raw rule message with context using the engine's default text color and requested alignment.
    pub(crate) fn parse_rule_message(
        &self,
        raw_msg: &str,
        ctx: &MessageContext,
        alignment: TextHorizontalAlignment,
    ) -> Vec<FormattedParagraph> {
        formatter::parse_message(raw_msg, ctx, self.default_text_color(), alignment)
    }


    /// Returns the effective template message for a rule, falling back to canonical defaults if empty.
    pub fn get_effective_rule_message<'a>(&self, rule: &'a Rule) -> &'a str {
        if !rule.message.trim().is_empty() {
            &rule.message
        } else {
            rule_message_fallback(rule)
        }
    }


    /// Parses the default overlay text color from config, falling back to slate-50 (`#f8fafc`).
    fn default_text_color(&self) -> Color {
        formatter::parse_hex_color(&self.config.overlay.text_color)
            .unwrap_or_else(|| Color::from_rgb_u8(248, 250, 252))
    }



    // ========================================================================
    // State Query & Low-Level Helpers
    // ========================================================================

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


    /// Returns the last matched application name for `rule_id`, falling back to `"Active Application"`.
    fn get_last_matched_app_or_default(&self, rule_id: &str) -> String {
        self.runtimes
            .get(rule_id)
            .and_then(|r| r.last_matched_app.clone())
            .unwrap_or_else(|| "Active Application".to_string())
    }


    /// Returns `true` if `self.active_overlay_mode` is `BreakEnforcer`, `HudTimer`, or `HudToast` with matching `rule_id`.
    pub fn is_rule_displayed(&self, rule_id: &str) -> bool {
        self.active_overlay_mode.rule_id() == Some(rule_id)
    }


    /// Returns `true` if any rule currently has an active break or grace countdown running.
    fn any_countdown_active(&self) -> bool {
        self.runtimes
            .values()
            .any(|rt| rt.active_break.is_some() || rt.grace_countdown.is_some())
    }


    /// Returns `true` if any enabled rule with `strict_mode` is currently in a break or grace countdown.
    pub fn is_strict_lock_active(&self, _now: DateTime<Local>) -> bool {
        for rule in &self.config.rules {
            if !rule.enabled || !rule.strict_mode {
                continue;
            }
            if let Some(rt) = self.runtimes.get(&rule.id) {
                if rt.active_break.is_some() || rt.grace_countdown.is_some() || rt.pending_break_since.is_some() {
                    return true;
                }
            }
        }
        false
    }


    /// Evaluates the current system tray icon state based on global pause and active rule alerts.
    pub fn current_tray_state(&self, now: DateTime<Local>) -> TrayIconState {
        if self.config.general.pause_all_rules {
            return TrayIconState::Paused;
        }

        let day_index = now.weekday().num_days_from_monday() as usize;
        let hour_index = now.hour() as usize;

        let is_alert = self.config.rules.iter().any(|rule| {
            if !rule.enabled {
                return false;
            }
            if !rule.schedule.is_active(day_index, hour_index) {
                return false;
            }

            let is_snoozed = self
                .global_snoozed_until
                .filter(|&u| u > now)
                .is_some()
                || self
                    .runtimes
                    .get(&rule.id)
                    .and_then(|rt| rt.snoozed_until)
                    .filter(|&u| u > now)
                    .is_some();

            if is_snoozed {
                return false;
            }

            if let Some(rt) = self.runtimes.get(&rule.id) {
                if rt.warned_current_cycle {
                    return true;
                }
                if rt.grace_countdown.is_some() {
                    return true;
                }
                if rt.active_break.is_some() || rt.pending_break_since.is_some() {
                    return true;
                }
            }

            if self.is_rule_displayed(&rule.id) {
                return true;
            }

            false
        });

        if is_alert {
            TrayIconState::Alert
        } else {
            TrayIconState::Normal
        }
    }
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
            allow_snooze: true,
            strict_mode: false,
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
    async fn test_dismiss_break_overlay_completes_cycle_when_pending_break_active() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();
        cfg.rules = vec![Rule::default_template(0)];
        let rule_id = cfg.rules[0].id.clone();
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        let now = Local::now();
        if let Some(rt) = engine.runtimes.get_mut(&rule_id) {
            rt.pending_break_since = Some(now - Duration::seconds(1));
            rt.warned_current_cycle = true;
            rt.action_executed_today = false;
        }

        engine.handle_command(DaemonCommand::DismissBreakOverlay).await;

        let rt = engine.runtimes.get(&rule_id).unwrap();
        assert!(rt.pending_break_since.is_none());
        assert!(rt.last_completed_at.is_some());
        assert!(rt.action_executed_today);
        assert!(!rt.warned_current_cycle);
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
            .evaluate_rule(&rule, None, now, &[ev], start_utc, now_utc, &[])
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
                rt.daily_reset_at = None;
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
        assert!(!rt.can_rollback_completion);

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
            .evaluate_rule(&rule, None, now, &[ev.clone()], start_of_day_utc, now_utc, &[])
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
            .evaluate_rule(&rule, None, later_now, &[later_ev], start_of_day_utc, later_now_utc, &[])
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
        rule1.trigger = TriggerType::Limit;
        rule1.limit = DurationConfig::new(60, TimeUnit::Min);
        rule1.target_executables = vec!["nonexistent_unit_test_app.exe".to_string()];
        let mut rule2 = Rule::default_template(1);
        rule2.id = "rule-2".to_string();
        rule2.target_executables = vec!["nonexistent_unit_test_app.exe".to_string()];
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
            allow_snooze: true,
            strict_mode: false,
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
        rule.url_regex = "(?i)youtube\\.com".to_string();
        rule.action = RuleAction::CloseTab;
        rule.grace_duration = DurationConfig::new(5, TimeUnit::Sec);
        rule.trigger = TriggerType::Limit;

        cfg.rules = vec![rule.clone()];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        let rt = engine.runtimes.get_mut("rule-urls").unwrap();
        assert!(rt.compiled_regex.is_some());
        rt.grace_countdown = Some((1, 5));
        rt.last_matched_app = Some("youtube.com".to_string());
        rt.set_last_matched_title(Some("NonExistentTabTitle12345XYZ".to_string()));

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
        assert!(!rt.can_rollback_completion);
        rt.can_rollback_completion = true;

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
            allow_snooze: true,
            strict_mode: false,
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
        rule_a.schedule = RuleSchedule::from_bool_array(&[true; 168]);
        let mut rule_b = Rule::default_template(1);
        rule_b.id = "rule-b".to_string();
        rule_b.schedule = RuleSchedule::from_bool_array(&[true; 168]);
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

        let switched_events = [ev_old_matching.clone(), ev_new_non_matching.clone()];
        let res_switched = find_matching_window_for_rule(
            &rule,
            re.as_ref(),
            &switched_events,
            now_utc,
            5,
        );
        assert_eq!(res_switched, None);

        // Verify find_latest_matching_event inspects newest event overall for Urls vs Executables
        let recency_cutoff = calculate_recency_cutoff(now_utc, 5);
        let latest_url_ev = find_latest_matching_event(
            &switched_events,
            &rule,
            re.as_ref(),
            recency_cutoff,
        );
        assert!(latest_url_ev.is_none());

        let mut exe_rule = rule.clone();
        exe_rule.monitor_selection = MonitorSelection::Executables;
        exe_rule.target_executables = vec!["chrome.exe".to_string()];
        let latest_exe_ev = find_latest_matching_event(
            &switched_events,
            &exe_rule,
            None,
            recency_cutoff,
        );
        assert!(latest_exe_ev.is_some());

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
            &[ev_old_matching, ev_newer_matching.clone()],
            now_utc,
            5,
        );
        if enforcer::find_matching_window(enforcer::BROWSER_EXES, None).is_some() {
            assert_eq!(res_matching.as_deref(), Some("music.youtube.com"));
        } else {
            assert_eq!(res_matching, None);
        }

        // 3. For CloseWindow with non-empty title, verifies titled window exists (with #[cfg(test)] fallback to BROWSER_EXES)
        let mut rule_close_win = rule.clone();
        rule_close_win.action = RuleAction::CloseWindow;
        let res_close_win = find_matching_window_for_rule(
            &rule_close_win,
            re.as_ref(),
            &[ev_newer_matching],
            now_utc,
            5,
        );
        if enforcer::find_matching_window(enforcer::BROWSER_EXES, None).is_some() {
            assert_eq!(res_close_win.as_deref(), Some("music.youtube.com"));
        } else {
            assert_eq!(res_close_win, None);
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

        // Populate runtime's last_matched_title using the helper logic
        let recency_cutoff = calculate_recency_cutoff(now_utc, engine.config.general.poll_interval_seconds);
        let events = [ev_older, ev_newer];
        let matching_ev = find_latest_matching_event(&events, &rule, re.as_ref(), recency_cutoff);

        let rt = engine.runtimes.get_mut("rule-urls-newest-title").unwrap();
        rt.update_matched_metadata(matching_ev, true);

        assert_eq!(
            engine.runtimes.get("rule-urls-newest-title").unwrap().last_matched_title.as_deref(),
            Some("Latest Video - YouTube")
        );
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
        let badge2 = format_snooze_badge(rt2.snoozed_until.unwrap(), now);
        assert_eq!(badge2, "Snoozed (10m)");

        // Set global snooze for 15 minutes
        engine.global_snoozed_until = Some(now + Duration::minutes(15));
        let badge_global = format_snooze_badge(engine.global_snoozed_until.unwrap(), now);
        assert_eq!(badge_global, "Snoozed (15m)");

        let rem_global = ((engine.global_snoozed_until.unwrap() - now).num_seconds().max(1) + 59) / 60;
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
        cfg.general.suppress_fullscreen_d3d = false;

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
                &[],
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

    #[tokio::test]
    async fn test_urls_rule_grace_countdown_navigating_away_clears_title_and_skips_enforcement() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();

        let mut rule = Rule::default_template(0);
        rule.id = "rule-urls-nav-away".to_string();
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

        // 1. Initially in grace countdown with active youtube title
        let rt = engine.runtimes.get_mut("rule-urls-nav-away").unwrap();
        rt.grace_countdown = Some((3, 5));
        rt.last_matched_app = Some("youtube.com".to_string());
        rt.last_matched_url = Some("https://www.youtube.com/watch?v=123".to_string());
        rt.set_last_matched_title(Some("Distracting Video - YouTube".to_string()));

        assert_eq!(
            rt.last_matched_title.as_deref(),
            Some("Distracting Video - YouTube")
        );
        assert!(rt.last_matched_title_regex.is_some());

        // 2. While in grace countdown, user navigates away to a non-matching URL
        let ev_matching = make_event(
            now_utc - Duration::seconds(3),
            2.0,
            "chrome.exe",
            "Distracting Video - YouTube",
            "https://www.youtube.com/watch?v=123",
        );
        let ev_other = make_event(
            now_utc - Duration::seconds(1),
            2.0,
            "chrome.exe",
            "Rust Documentation",
            "https://doc.rust-lang.org/book/",
        );

        let re = Regex::new(&rule.url_regex).ok();
        engine
            .evaluate_rule(
                &rule,
                re.as_ref(),
                now,
                &[ev_matching, ev_other],
                start_of_day_utc,
                now_utc,
                &[],
            )
            .await;

        let rt_after_eval = engine.runtimes.get("rule-urls-nav-away").unwrap();
        assert_eq!(rt_after_eval.last_matched_app, None);
        assert_eq!(rt_after_eval.last_matched_url, None);
        assert_eq!(rt_after_eval.last_matched_title, None);
        assert!(rt_after_eval.last_matched_title_regex.is_none());

        // 3. Grace countdown reaches 0: because title and app are cleared, destructive enforcement is skipped
        engine.runtimes.get_mut("rule-urls-nav-away").unwrap().grace_countdown = Some((1, 5));
        engine.realtime_tick().await;

        let rt_done = engine.runtimes.get("rule-urls-nav-away").unwrap();
        assert_eq!(rt_done.grace_countdown, None);
        assert!(rt_done.action_executed_today);
    }

    #[tokio::test]
    async fn test_urls_rule_empty_title_destructive_action_safe_completion() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();

        let mut rule = Rule::default_template(0);
        rule.id = "rule-urls-empty-title-destr".to_string();
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

        // 1. Evaluate rule in grace countdown with an event having an empty/whitespace title
        let rt_init = engine.runtimes.get_mut("rule-urls-empty-title-destr").unwrap();
        rt_init.grace_countdown = Some((3, 5));
        rt_init.last_matched_app = Some("youtube.com".to_string());
        rt_init.set_last_matched_title(Some("Old Video - YouTube".to_string()));

        let ev_empty = make_event(
            now_utc - Duration::seconds(1),
            2.0,
            "chrome.exe",
            "   ",
            "https://www.youtube.com/watch?v=empty_title",
        );
        let re = Regex::new(&rule.url_regex).ok();
        engine
            .evaluate_rule(
                &rule,
                re.as_ref(),
                now,
                &[ev_empty],
                start_of_day_utc,
                now_utc,
                &[],
            )
            .await;

        let rt = engine.runtimes.get_mut("rule-urls-empty-title-destr").unwrap();
        assert_eq!(rt.last_matched_url.as_deref(), Some("https://www.youtube.com/watch?v=empty_title"));
        assert_eq!(rt.last_matched_title, None);
        assert!(rt.last_matched_title_regex.is_none());

        // 2. Set grace countdown expiring (1 -> 0) and tick realtime
        rt.grace_countdown = Some((1, 5));
        engine.realtime_tick().await;

        // Since last_matched_title_regex is None, realtime_tick skips destructive action and marks cycle completed
        let rt_after = engine.runtimes.get("rule-urls-empty-title-destr").unwrap();
        assert_eq!(rt_after.grace_countdown, None);
        assert!(rt_after.action_executed_today);

        // 3. Direct execution of execute_rule_action for CloseTab and CloseWindow with None title regex safely completes
        let mut rule_win = rule.clone();
        rule_win.id = "rule-urls-close-win".to_string();
        rule_win.action = RuleAction::CloseWindow;
        engine.runtimes.insert(
            rule_win.id.clone(),
            RuleRuntime::from_rule(&rule_win),
        );
        let rt_win = engine.runtimes.get_mut(&rule_win.id).unwrap();
        rt_win.last_matched_url = Some("https://www.youtube.com/watch?v=no_title".to_string());
        rt_win.set_last_matched_title(None);
        assert!(rt_win.last_matched_title_regex.is_none());

        engine.execute_rule_action(&rule_win, re.as_ref(), now, "chrome.exe", 600, 600).await;
        let rt_win_after = engine.runtimes.get(&rule_win.id).unwrap();
        assert!(rt_win_after.action_executed_today);
        assert_eq!(rt_win_after.last_matched_url, None);
    }

    #[tokio::test]
    async fn test_urls_rule_clears_stale_title_when_newer_matching_event_has_empty_title() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();

        let mut rule = Rule::default_template(0);
        rule.id = "rule-urls-clear-title".to_string();
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

        // 1. Test during active grace countdown: previous title is set, then newer matching event has empty/whitespace title
        let rt = engine.runtimes.get_mut("rule-urls-clear-title").unwrap();
        rt.grace_countdown = Some((3, 5));
        rt.last_matched_app = Some("youtube.com".to_string());
        rt.set_last_matched_title(Some("Stale Video - YouTube".to_string()));
        assert_eq!(rt.last_matched_title.as_deref(), Some("Stale Video - YouTube"));
        assert!(rt.last_matched_title_regex.is_some());

        let ev_empty_title = make_event(
            now_utc - Duration::seconds(1),
            2.0,
            "chrome.exe",
            "   ",
            "https://www.youtube.com/watch?v=empty_title",
        );

        let re = Regex::new(&rule.url_regex).ok();
        engine
            .evaluate_rule(
                &rule,
                re.as_ref(),
                now,
                &[ev_empty_title.clone()],
                start_of_day_utc,
                now_utc,
                &[],
            )
            .await;

        let rt_after_grace = engine.runtimes.get("rule-urls-clear-title").unwrap();
        assert_eq!(rt_after_grace.last_matched_title, None);
        assert!(rt_after_grace.last_matched_title_regex.is_none());

        // 2. Test during normal evaluation (without grace countdown):
        if enforcer::find_matching_window(enforcer::BROWSER_EXES, None).is_some() {
            let rt_mut = engine.runtimes.get_mut("rule-urls-clear-title").unwrap();
            rt_mut.grace_countdown = None;
            rt_mut.set_last_matched_title(Some("Another Stale Title - YouTube".to_string()));
            assert_eq!(rt_mut.last_matched_title.as_deref(), Some("Another Stale Title - YouTube"));
            assert!(rt_mut.last_matched_title_regex.is_some());

            engine
                .evaluate_rule(
                    &rule,
                    re.as_ref(),
                    now,
                    &[ev_empty_title],
                    start_of_day_utc,
                    now_utc,
                    &[],
                )
                .await;

            let rt_after_normal = engine.runtimes.get("rule-urls-clear-title").unwrap();
            assert_eq!(rt_after_normal.last_matched_title, None);
            assert!(rt_after_normal.last_matched_title_regex.is_none());
        }
    }

    #[tokio::test]
    async fn test_strict_break_not_clobbered_by_non_strict_break_and_pending_break_locks() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();

        let mut non_strict_rule = Rule::default_template(0);
        non_strict_rule.id = "rule-non-strict".to_string();
        non_strict_rule.name = "Non-Strict Break".to_string();
        non_strict_rule.action = RuleAction::ShowOverlay;
        non_strict_rule.strict_mode = false;
        non_strict_rule.click_through = false;

        let mut strict_rule = Rule::default_template(1);
        strict_rule.id = "rule-strict".to_string();
        strict_rule.name = "Strict Break".to_string();
        strict_rule.action = RuleAction::ShowOverlay;
        strict_rule.strict_mode = true;
        strict_rule.click_through = false;

        // Place non_strict_rule before strict_rule in cfg.rules to test priority selection over list ordering
        cfg.rules = vec![non_strict_rule.clone(), strict_rule.clone()];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        let now = Local::now();

        // (a) Strict rule with pending_break_since: Some(now) sets is_strict_lock_active == true
        // and blocks DismissBreakOverlay, SnoozeAll, and SetPauseAll(true)
        if let Some(rt) = engine.runtimes.get_mut("rule-strict") {
            rt.pending_break_since = Some(now);
        }
        assert!(engine.is_strict_lock_active(now));

        engine.handle_command(DaemonCommand::DismissBreakOverlay).await;
        assert!(engine.runtimes.get("rule-strict").unwrap().pending_break_since.is_some());

        engine.handle_command(DaemonCommand::SnoozeAll { minutes: 15 }).await;
        assert!(engine.global_snoozed_until.is_none());

        engine.handle_command(DaemonCommand::SetPauseAll { paused: true }).await;
        assert!(!engine.config.general.pause_all_rules);

        // Clear pending break
        if let Some(rt) = engine.runtimes.get_mut("rule-strict") {
            rt.pending_break_since = None;
        }

        // (b) When an active strict BreakEnforcer is showing, calling show_overlay with a
        // non-strict BreakEnforcer for another rule does NOT overwrite active_overlay_mode.
        engine.show_overlay(OverlayMode::BreakEnforcer {
            rule_id: "rule-strict".to_string(),
            title: "Strict Break".to_string(),
            paragraphs: Vec::new(),
            duration_seconds: 300,
            remaining_seconds: 300,
            allow_snooze: false,
            strict_mode: true,
            click_through: false,
        });
        assert_eq!(engine.active_overlay_mode.rule_id(), Some("rule-strict"));

        // Attempt to show non-strict break from another rule
        engine.show_overlay(OverlayMode::BreakEnforcer {
            rule_id: "rule-non-strict".to_string(),
            title: "Non-Strict Break".to_string(),
            paragraphs: Vec::new(),
            duration_seconds: 600,
            remaining_seconds: 600,
            allow_snooze: true,
            strict_mode: false,
            click_through: false,
        });
        // Still rule-strict!
        assert_eq!(engine.active_overlay_mode.rule_id(), Some("rule-strict"));
        if let OverlayMode::BreakEnforcer { strict_mode, .. } = engine.active_overlay_mode {
            assert!(strict_mode);
        } else {
            panic!("Expected BreakEnforcer");
        }

        // (c) When transitioning to an active countdown, an active strict break rule is selected
        // over an active non-strict break rule regardless of configuration list ordering.
        engine.runtimes.get_mut("rule-non-strict").unwrap().active_break = Some((500, 500));
        engine.runtimes.get_mut("rule-strict").unwrap().active_break = Some((200, 200));

        engine.active_overlay_mode = OverlayMode::Hidden;
        engine.transition_to_active_countdown_or_hide();

        assert_eq!(engine.active_overlay_mode.rule_id(), Some("rule-strict"));
        if let OverlayMode::BreakEnforcer { strict_mode, remaining_seconds, .. } = engine.active_overlay_mode {
            assert!(strict_mode);
            assert_eq!(remaining_seconds, 200);
        } else {
            panic!("Expected BreakEnforcer for strict rule");
        }
    }

    #[tokio::test]
    async fn test_reset_rule_session_resets_timer_and_clears_snooze() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();

        let mut rule = Rule::default_template(0);
        rule.id = "rule-reset-test".to_string();
        rule.trigger = TriggerType::Period;
        rule.period = DurationConfig::new(20, TimeUnit::Min);

        cfg.rules = vec![rule.clone()];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        let now = Local::now();
        {
            let rt = engine.runtimes.get_mut("rule-reset-test").unwrap();
            rt.current_session_seconds = 1200;
            rt.snoozed_until = Some(now + Duration::minutes(15));
            rt.warned_current_cycle = true;
        }

        engine
            .handle_command(DaemonCommand::ResetRuleSession {
                rule_id: "rule-reset-test".to_string(),
            })
            .await;

        let rt = engine.runtimes.get("rule-reset-test").unwrap();
        assert!(rt.snoozed_until.is_none());
        assert!(rt.last_completed_at.is_some());
        assert!(!rt.warned_current_cycle);
        assert_eq!(rt.current_session_seconds, 0);
        assert_eq!(rt.daily_usage_seconds, 0);
        assert!(!rt.can_rollback_completion);
        assert!(!rt.action_executed_today);
        assert!(rt.daily_reset_at.is_some());
    }

    #[tokio::test]
    async fn test_clear_snooze_all_clears_global_and_per_rule_snoozes() {
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
        engine.global_snoozed_until = Some(now + Duration::minutes(30));
        engine.runtimes.get_mut("rule-1").unwrap().snoozed_until = Some(now + Duration::minutes(10));
        engine.runtimes.get_mut("rule-2").unwrap().snoozed_until = Some(now + Duration::minutes(20));

        engine.handle_command(DaemonCommand::ClearSnoozeAll).await;

        assert!(engine.global_snoozed_until.is_none());
        assert!(engine.runtimes.get("rule-1").unwrap().snoozed_until.is_none());
        assert!(engine.runtimes.get("rule-2").unwrap().snoozed_until.is_none());
    }

    #[tokio::test]
    async fn test_snooze_after_reset_skip_or_dismiss_does_not_rollback_completed_at() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();

        let mut rule = Rule::default_template(0);
        rule.id = "rule-rollback-test".to_string();
        rule.trigger = TriggerType::Period;
        rule.period = DurationConfig::new(20, TimeUnit::Min);

        cfg.rules = vec![rule];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        let t0 = Local::now() - Duration::hours(2);
        let t1 = Local::now() - Duration::hours(1);

        // 1. ResetRuleSession: Snoozing after reset must not rollback last_completed_at
        {
            let rt = engine.runtimes.get_mut("rule-rollback-test").unwrap();
            rt.prev_completed_at = Some(t0);
            rt.last_completed_at = Some(t1);
        }
        engine
            .handle_command(DaemonCommand::ResetRuleSession {
                rule_id: "rule-rollback-test".to_string(),
            })
            .await;
        let reset_time = engine.runtimes.get("rule-rollback-test").unwrap().last_completed_at.unwrap();
        assert!(!engine.runtimes.get("rule-rollback-test").unwrap().can_rollback_completion);

        engine
            .handle_command(DaemonCommand::SnoozeBreak {
                rule_id: "rule-rollback-test".to_string(),
                minutes: 15,
            })
            .await;
        let rt = engine.runtimes.get("rule-rollback-test").unwrap();
        assert_eq!(rt.last_completed_at, Some(reset_time));

        // 2. SkipBreak: Snoozing after SkipBreak must not rollback last_completed_at
        {
            let rt = engine.runtimes.get_mut("rule-rollback-test").unwrap();
            rt.prev_completed_at = Some(t0);
            rt.last_completed_at = Some(t1);
            rt.active_break = Some((300, 300));
            rt.can_rollback_completion = true;
        }
        engine
            .handle_command(DaemonCommand::SkipBreak {
                rule_id: "rule-rollback-test".to_string(),
            })
            .await;
        let skip_time = engine.runtimes.get("rule-rollback-test").unwrap().last_completed_at.unwrap();
        assert!(!engine.runtimes.get("rule-rollback-test").unwrap().can_rollback_completion);

        engine
            .handle_command(DaemonCommand::SnoozeBreak {
                rule_id: "rule-rollback-test".to_string(),
                minutes: 15,
            })
            .await;
        let rt = engine.runtimes.get("rule-rollback-test").unwrap();
        assert_eq!(rt.last_completed_at, Some(skip_time));

        // 3. DismissBreakOverlay: Snoozing after DismissBreakOverlay must not rollback last_completed_at
        {
            let rt = engine.runtimes.get_mut("rule-rollback-test").unwrap();
            rt.prev_completed_at = Some(t0);
            rt.last_completed_at = Some(t1);
            rt.active_break = Some((300, 300));
            rt.can_rollback_completion = true;
        }
        engine
            .handle_command(DaemonCommand::DismissBreakOverlay)
            .await;
        let dismiss_time = engine.runtimes.get("rule-rollback-test").unwrap().last_completed_at.unwrap();
        assert!(!engine.runtimes.get("rule-rollback-test").unwrap().can_rollback_completion);

        engine
            .handle_command(DaemonCommand::SnoozeBreak {
                rule_id: "rule-rollback-test".to_string(),
                minutes: 15,
            })
            .await;
        let rt = engine.runtimes.get("rule-rollback-test").unwrap();
        assert_eq!(rt.last_completed_at, Some(dismiss_time));
    }

    #[tokio::test]
    async fn test_reset_rule_session_limit_rule_resets_usage_and_clears_action_executed() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();

        let mut rule = Rule::default_template(0);
        rule.id = "rule-limit-reset".to_string();
        rule.trigger = TriggerType::Limit;
        rule.limit = DurationConfig::new(30, TimeUnit::Min); // 1800s
        rule.action = RuleAction::ShowNotification;
        rule.notification_method = NotificationMethod::Hud;

        cfg.rules = vec![rule.clone()];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        let now = Local::now();
        let now_utc = now.with_timezone(&Utc);
        let start_of_day_utc = logical_day_start(now, engine.config.general.day_start_hour).with_timezone(&Utc);

        // Pre-reset event 1 hour ago for 2000s (exceeding 1800s limit)
        let ev = make_event(
            now_utc - Duration::minutes(70),
            2000.0,
            "code.exe",
            "main.rs",
            "",
        );

        // Simulate pre-reset state where quota had triggered
        {
            let rt = engine.runtimes.get_mut("rule-limit-reset").unwrap();
            rt.daily_usage_seconds = 2000;
            rt.action_executed_today = true;
        }

        // User clicks Reset Timer
        engine
            .handle_command(DaemonCommand::ResetRuleSession {
                rule_id: "rule-limit-reset".to_string(),
            })
            .await;

        let rt = engine.runtimes.get("rule-limit-reset").unwrap();
        assert_eq!(rt.daily_usage_seconds, 0);
        assert!(!rt.action_executed_today);
        assert!(rt.daily_reset_at.is_some());
        assert!(!rt.can_rollback_completion);

        // Now evaluate rule against the event stream from earlier in the day
        let re = Regex::new("(?i)code\\.exe").ok();
        engine
            .evaluate_rule(&rule, re.as_ref(), now, &[ev], start_of_day_utc, now_utc, &[])
            .await;

        // Effective start timestamp should ignore the 1-hour-old event, keeping usage at 0
        let rt = engine.runtimes.get("rule-limit-reset").unwrap();
        assert_eq!(rt.daily_usage_seconds, 0);
        assert!(!rt.action_executed_today);
        assert!(rt.grace_countdown.is_none());
        assert!(rt.active_break.is_none());
        assert!(matches!(engine.active_overlay_mode, OverlayMode::Hidden));
    }

    #[tokio::test]
    async fn test_evaluate_rule_preserves_active_stage3_hud_toast() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();

        let mut rule = Rule::default_template(0);
        rule.id = "rule-hud-toast-eval".to_string();
        rule.action = RuleAction::ShowNotification;
        rule.notification_method = NotificationMethod::Hud;
        rule.period = DurationConfig::new(20, TimeUnit::Min);

        cfg.rules = vec![rule.clone()];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        let now = Local::now();
        let now_utc = now.with_timezone(&Utc);
        let start_of_day_utc = logical_day_start(now, engine.config.general.day_start_hour).with_timezone(&Utc);

        // Display Stage 3 HudToast for this rule
        engine.active_overlay_mode = OverlayMode::HudToast {
            rule_id: "rule-hud-toast-eval".to_string(),
            title: "Notification Toast".to_string(),
            paragraphs: Vec::new(),
            snooze_options_minutes: vec![5, 10, 15],
            duration_seconds: 10,
            remaining_seconds: 10,
            show_time_remaining: true,
        };

        let completed_time = now - Duration::seconds(2);
        {
            let rt = engine.runtimes.get_mut("rule-hud-toast-eval").unwrap();
            rt.last_completed_at = Some(completed_time);
            rt.can_rollback_completion = true;
            rt.action_executed_today = true;
        }

        let ev = make_event(
            now_utc - Duration::minutes(5),
            300.0,
            "code.exe",
            "test.rs",
            "",
        );

        let re = Regex::new("(?i)code\\.exe").ok();

        let evaluated = engine
            .evaluate_rule(&rule, re.as_ref(), now, &[ev], start_of_day_utc, now_utc, &[])
            .await;

        // Evaluation returns false early and preserves can_rollback_completion == true
        assert!(!evaluated);
        let rt = engine.runtimes.get("rule-hud-toast-eval").unwrap();
        assert!(rt.can_rollback_completion);
        assert_eq!(rt.last_completed_at, Some(completed_time));
    }

    #[tokio::test]
    async fn test_commands_emit_immediate_live_status_update() {
        let (overlay_tx, mut overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();

        let mut rule = Rule::default_template(0);
        rule.id = "rule-live-update-test".to_string();
        rule.action = RuleAction::ShowOverlay;
        rule.break_duration = DurationConfig::new(10, TimeUnit::Min);
        rule.schedule = RuleSchedule::from_bool_array(&[true; 168]);

        cfg.rules = vec![rule.clone()];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        let recv_last_live_status = |rx: &mut mpsc::UnboundedReceiver<OverlayAction>| {
            let mut last = None;
            while let Ok(action) = rx.try_recv() {
                if let OverlayAction::UpdateLiveStatus {
                    rule_statuses,
                    tray_tooltip,
                    any_snoozed,
                    ..
                } = action
                {
                    last = Some((tray_tooltip, any_snoozed, rule_statuses));
                }
            }
            last
        };

        // 1. SnoozeAll: Immediately emits UpdateLiveStatus with any_snoozed == true and Snoozed tooltip
        engine
            .handle_command(DaemonCommand::SnoozeAll { minutes: 15 })
            .await;
        let (tooltip, any_snoozed, statuses) = recv_last_live_status(&mut overlay_rx)
            .expect("SnoozeAll must emit UpdateLiveStatus");
        assert!(any_snoozed);
        assert!(tooltip.contains("Snoozed 15m"));
        assert!(!statuses.is_empty());
        assert!(statuses.iter().any(|(id, badge)| id == "rule-live-update-test" && badge.contains("Snoozed")));

        // 2. SetPauseAll(true): Immediately emits UpdateLiveStatus with Paused tooltip
        engine
            .handle_command(DaemonCommand::SetPauseAll { paused: true })
            .await;
        let (tooltip, _any_snoozed, _statuses) = recv_last_live_status(&mut overlay_rx)
            .expect("SetPauseAll(true) must emit UpdateLiveStatus");
        assert!(tooltip.contains("Paused"));

        // Unpause to test break commands
        engine
            .handle_command(DaemonCommand::SetPauseAll { paused: false })
            .await;
        let _ = recv_last_live_status(&mut overlay_rx);

        // Clear snooze
        engine.handle_command(DaemonCommand::ClearSnoozeAll).await;
        let _ = recv_last_live_status(&mut overlay_rx);

        // 3. SkipBreak: Set active break, skip it, ensure UpdateLiveStatus emitted immediately
        {
            let rt = engine.runtimes.get_mut("rule-live-update-test").unwrap();
            rt.active_break = Some((600, 600));
        }
        engine
            .handle_command(DaemonCommand::SkipBreak {
                rule_id: "rule-live-update-test".to_string(),
            })
            .await;
        let (_tooltip, any_snoozed, statuses) = recv_last_live_status(&mut overlay_rx)
            .expect("SkipBreak must emit UpdateLiveStatus");
        assert!(!any_snoozed);
        let rt = engine.runtimes.get("rule-live-update-test").unwrap();
        assert!(rt.active_break.is_none());
        assert!(statuses.iter().any(|(id, _)| id == "rule-live-update-test"));

        // 4. DismissBreakOverlay: Set active break, dismiss, ensure UpdateLiveStatus emitted immediately
        {
            let rt = engine.runtimes.get_mut("rule-live-update-test").unwrap();
            rt.active_break = Some((600, 600));
        }
        engine
            .handle_command(DaemonCommand::DismissBreakOverlay)
            .await;
        let (_tooltip, any_snoozed, statuses) = recv_last_live_status(&mut overlay_rx)
            .expect("DismissBreakOverlay must emit UpdateLiveStatus");
        assert!(!any_snoozed);
        let rt = engine.runtimes.get("rule-live-update-test").unwrap();
        assert!(rt.active_break.is_none());
        assert!(statuses.iter().any(|(id, _)| id == "rule-live-update-test"));
    }

    #[tokio::test]
    async fn test_acknowledge_or_expire_toast_clears_rollback_flag() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();

        let mut rule = Rule::default_template(0);
        rule.id = "rule-toast-rollback".to_string();
        rule.action = RuleAction::ShowNotification;
        rule.trigger = TriggerType::Period;
        rule.period = DurationConfig::new(20, TimeUnit::Min);

        cfg.rules = vec![rule];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        let t0 = Local::now() - Duration::hours(2);
        let t1 = Local::now() - Duration::hours(1);

        // 1. AcknowledgeAppWarning: Acknowledging Stage 3 toast locks in completion
        {
            let rt = engine.runtimes.get_mut("rule-toast-rollback").unwrap();
            rt.prev_completed_at = Some(t0);
            rt.last_completed_at = Some(t1);
            rt.can_rollback_completion = true;
        }
        engine.active_overlay_mode = OverlayMode::HudToast {
            rule_id: "rule-toast-rollback".to_string(),
            title: "Warning".to_string(),
            paragraphs: Vec::new(),
            snooze_options_minutes: vec![15],
            duration_seconds: 10,
            remaining_seconds: 8,
            show_time_remaining: true,
        };

        engine
            .handle_command(DaemonCommand::AcknowledgeAppWarning {
                rule_id: "rule-toast-rollback".to_string(),
            })
            .await;

        assert!(!engine.runtimes.get("rule-toast-rollback").unwrap().can_rollback_completion);

        // Subsequent snooze should not roll back last_completed_at to t0
        engine
            .handle_command(DaemonCommand::SnoozeBreak {
                rule_id: "rule-toast-rollback".to_string(),
                minutes: 15,
            })
            .await;
        assert_eq!(
            engine.runtimes.get("rule-toast-rollback").unwrap().last_completed_at,
            Some(t1)
        );

        // 2. Toast expiry via countdown tick: reaching 0 clears can_rollback_completion
        {
            let rt = engine.runtimes.get_mut("rule-toast-rollback").unwrap();
            rt.prev_completed_at = Some(t0);
            rt.last_completed_at = Some(t1);
            rt.can_rollback_completion = true;
        }
        engine.active_overlay_mode = OverlayMode::HudToast {
            rule_id: "rule-toast-rollback".to_string(),
            title: "Warning".to_string(),
            paragraphs: Vec::new(),
            snooze_options_minutes: vec![15],
            duration_seconds: 10,
            remaining_seconds: 1,
            show_time_remaining: true,
        };

        engine.send_overlay_countdown_tick(0, 10, true);
        assert!(!engine.runtimes.get("rule-toast-rollback").unwrap().can_rollback_completion);

        // Subsequent snooze should preserve last_completed_at
        engine
            .handle_command(DaemonCommand::SnoozeBreak {
                rule_id: "rule-toast-rollback".to_string(),
                minutes: 15,
            })
            .await;
        assert_eq!(
            engine.runtimes.get("rule-toast-rollback").unwrap().last_completed_at,
            Some(t1)
        );
    }

    #[tokio::test]
    async fn test_urls_rule_grace_countdown_stale_or_empty_events_clears_title_and_skips_enforcement() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();

        let mut rule = Rule::default_template(0);
        rule.id = "rule-urls-stale-grace".to_string();
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

        // 1. Initially in grace countdown with active youtube title
        let rt = engine.runtimes.get_mut("rule-urls-stale-grace").unwrap();
        rt.grace_countdown = Some((3, 5));
        rt.last_matched_app = Some("youtube.com".to_string());
        rt.last_matched_url = Some("https://youtube.com/watch?v=123".to_string());
        rt.set_last_matched_title(Some("Video - YouTube".to_string()));
        assert!(rt.last_matched_title_regex.is_some());

        // 2. Events contain only stale event outside recency window
        let ev_stale = make_event(
            now_utc - Duration::seconds(60),
            2.0,
            "chrome.exe",
            "Video - YouTube",
            "https://youtube.com/watch?v=123",
        );

        let re = Regex::new(&rule.url_regex).ok();
        engine
            .evaluate_rule(
                &rule,
                re.as_ref(),
                now,
                &[ev_stale],
                start_of_day_utc,
                now_utc,
                &[],
            )
            .await;

        let rt_after_stale = engine.runtimes.get("rule-urls-stale-grace").unwrap();
        assert_eq!(rt_after_stale.last_matched_app, None);
        assert_eq!(rt_after_stale.last_matched_url, None);
        assert_eq!(rt_after_stale.last_matched_title, None);
        assert!(rt_after_stale.last_matched_title_regex.is_none());

        // 3. Reset title, then evaluate with empty events list
        let rt = engine.runtimes.get_mut("rule-urls-stale-grace").unwrap();
        rt.last_matched_app = Some("youtube.com".to_string());
        rt.last_matched_url = Some("https://youtube.com/watch?v=123".to_string());
        rt.set_last_matched_title(Some("Video - YouTube".to_string()));

        engine
            .evaluate_rule(
                &rule,
                re.as_ref(),
                now,
                &[],
                start_of_day_utc,
                now_utc,
                &[],
            )
            .await;

        let rt_after_empty = engine.runtimes.get("rule-urls-stale-grace").unwrap();
        assert_eq!(rt_after_empty.last_matched_app, None);
        assert_eq!(rt_after_empty.last_matched_url, None);
        assert_eq!(rt_after_empty.last_matched_title, None);
        assert!(rt_after_empty.last_matched_title_regex.is_none());

        // 4. Countdown reaches 0: destructive enforcement is skipped and cycle completes safely
        engine.runtimes.get_mut("rule-urls-stale-grace").unwrap().grace_countdown = Some((1, 5));
        engine.realtime_tick().await;

        let rt_done = engine.runtimes.get("rule-urls-stale-grace").unwrap();
        assert_eq!(rt_done.grace_countdown, None);
        assert!(rt_done.action_executed_today);
    }

    #[tokio::test]
    async fn test_empty_message_fallbacks_in_preview_and_execution() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();
        cfg.general.suppress_fullscreen_d3d = false;

        let mut rule_break = Rule::default_template(0);
        rule_break.id = "rule-empty-break".to_string();
        rule_break.action = RuleAction::ShowOverlay;
        rule_break.message = "".to_string();
        rule_break.break_duration = DurationConfig::new(5, TimeUnit::Min);
        rule_break.period = DurationConfig::new(20, TimeUnit::Min);
        rule_break.grace_duration = DurationConfig::new(5, TimeUnit::Sec);

        let mut rule_notify_period = Rule::default_template(1);
        rule_notify_period.id = "rule-empty-notify-p".to_string();
        rule_notify_period.action = RuleAction::ShowNotification;
        rule_notify_period.notification_method = NotificationMethod::Hud;
        rule_notify_period.trigger = TriggerType::Period;
        rule_notify_period.period = DurationConfig::new(25, TimeUnit::Min);
        rule_notify_period.message = "   ".to_string();

        let mut rule_notify_limit = Rule::default_template(2);
        rule_notify_limit.id = "rule-empty-notify-l".to_string();
        rule_notify_limit.action = RuleAction::ShowNotification;
        rule_notify_limit.notification_method = NotificationMethod::Hud;
        rule_notify_limit.trigger = TriggerType::Limit;
        rule_notify_limit.limit = DurationConfig::new(60, TimeUnit::Min);
        rule_notify_limit.message = "".to_string();

        cfg.rules = vec![
            rule_break.clone(),
            rule_notify_period.clone(),
            rule_notify_limit.clone(),
        ];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        // 1. Preview rule alert for ShowOverlay with empty message
        engine.preview_rule_alert(&rule_break).await;
        if let OverlayMode::BreakEnforcer { paragraphs, .. } = &engine.active_overlay_mode {
            assert!(!paragraphs.is_empty());
            assert!(paragraphs.iter().any(|p| p.text.contains("Time for a Break")));
        } else {
            panic!("Expected BreakEnforcer mode");
        }

        // 2. Preview rule alert for ShowNotification (Period) with whitespace message
        engine.preview_rule_alert(&rule_notify_period).await;
        if let OverlayMode::HudToast { paragraphs, .. } = &engine.active_overlay_mode {
            assert!(!paragraphs.is_empty());
            assert!(paragraphs.iter().any(|p| p.text.contains("Active for")));
        } else {
            panic!("Expected HudToast mode");
        }

        // 3. Preview rule alert for ShowNotification (Limit) with empty message
        engine.preview_rule_alert(&rule_notify_limit).await;
        if let OverlayMode::HudToast { paragraphs, .. } = &engine.active_overlay_mode {
            assert!(!paragraphs.is_empty());
            assert!(paragraphs.iter().any(|p| p.text.contains("Daily limit")));
        } else {
            panic!("Expected HudToast mode");
        }

        // 4. execute_rule_action for ShowOverlay with empty message
        let now = Local::now();
        engine.execute_rule_action(&rule_break, None, now, "chrome.exe", 1200, 1200).await;
        if let OverlayMode::BreakEnforcer { paragraphs, .. } = &engine.active_overlay_mode {
            assert!(!paragraphs.is_empty());
            assert!(paragraphs.iter().any(|p| p.text.contains("Time for a Break")));
        } else {
            panic!("Expected BreakEnforcer mode on execute_rule_action");
        }

        // 5. execute_rule_action for ShowNotification (HUD) with empty message
        engine.hide_overlay();
        engine.execute_rule_action(&rule_notify_limit, None, now, "chrome.exe", 3600, 3600).await;
        if let OverlayMode::HudToast { paragraphs, .. } = &engine.active_overlay_mode {
            assert!(!paragraphs.is_empty());
            assert!(paragraphs.iter().any(|p| p.text.contains("Daily limit")));
        } else {
            panic!("Expected HudToast mode on execute_rule_action");
        }

        // 6. Windows notification plain-text fallback formatting
        let mut rule_notify_win = rule_notify_period.clone();
        rule_notify_win.notification_method = NotificationMethod::Windows;
        let effective_msg = engine.get_effective_rule_message(&rule_notify_win);
        let ctx = formatter::build_preview_context(&rule_notify_win);
        let plain = formatter::format_plain_message(effective_msg, &ctx);
        assert!(!plain.is_empty());
        assert!(plain.contains("Active for"));
    }

    #[tokio::test]
    async fn test_acknowledge_hud_timer_grace_cancellation_and_priority() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();

        let mut r_notify = Rule::default_template(0);
        r_notify.id = "rule-notify".to_string();
        r_notify.action = RuleAction::ShowNotification;

        let mut r_close = Rule::default_template(1);
        r_close.id = "rule-close".to_string();
        r_close.action = RuleAction::CloseWindow;

        let mut r_kill = Rule::default_template(2);
        r_kill.id = "rule-kill".to_string();
        r_kill.action = RuleAction::KillProcess;

        let mut r_strict_break = Rule::default_template(3);
        r_strict_break.id = "rule-strict".to_string();
        r_strict_break.action = RuleAction::ShowOverlay;
        r_strict_break.strict_mode = true;

        cfg.rules = vec![
            r_notify.clone(),
            r_close.clone(),
            r_kill.clone(),
            r_strict_break.clone(),
        ];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        // 1. Test prioritization:
        // Set grace_countdown on notify (10s), close (15s), kill (5s)
        engine.runtimes.get_mut("rule-notify").unwrap().grace_countdown = Some((10, 10));
        engine.runtimes.get_mut("rule-close").unwrap().grace_countdown = Some((15, 15));
        engine.runtimes.get_mut("rule-kill").unwrap().grace_countdown = Some((5, 5));

        // transition_to_active_countdown_or_hide should choose destructive action with shortest remaining: rule-kill (5s)
        engine.transition_to_active_countdown_or_hide();
        assert_eq!(engine.active_overlay_mode.rule_id(), Some("rule-kill"));

        // If strict break also has grace_countdown (20s), strict break should have top priority even over kill (5s)
        engine.runtimes.get_mut("rule-strict").unwrap().grace_countdown = Some((20, 20));
        engine.transition_to_active_countdown_or_hide();
        assert_eq!(engine.active_overlay_mode.rule_id(), Some("rule-strict"));

        // 2. AcknowledgeAppWarning on strict break does NOT cancel grace countdown
        engine
            .handle_command(DaemonCommand::AcknowledgeAppWarning {
                rule_id: "rule-strict".to_string(),
            })
            .await;
        assert!(engine.runtimes.get("rule-strict").unwrap().grace_countdown.is_some());

        // 3. AcknowledgeAppWarning on non-strict non-destructive rule (rule-notify) cancels grace countdown and marks cycle complete
        engine
            .handle_command(DaemonCommand::AcknowledgeAppWarning {
                rule_id: "rule-notify".to_string(),
            })
            .await;
        let rt_notify = engine.runtimes.get("rule-notify").unwrap();
        assert!(rt_notify.grace_countdown.is_none());
        assert!(rt_notify.last_completed_at.is_some());
        assert!(!rt_notify.can_rollback_completion);

        // 4. AcknowledgeAppWarning on non-strict destructive rule (rule-kill) hides overlay but preserves grace countdown
        engine.runtimes.get_mut("rule-strict").unwrap().grace_countdown = None;
        engine.transition_to_active_countdown_or_hide();
        assert!(engine.is_rule_displayed("rule-kill"));
        engine
            .handle_command(DaemonCommand::AcknowledgeAppWarning {
                rule_id: "rule-kill".to_string(),
            })
            .await;
        let rt_kill = engine.runtimes.get("rule-kill").unwrap();
        assert!(rt_kill.grace_countdown.is_some(), "Dismissing a destructive grace timer must not abort grace countdown");
        assert!(!engine.is_rule_displayed("rule-kill"), "Overlay should be hidden after dismissing");

        // SnoozeBreak on non-strict destructive rule pauses enforcement
        engine
            .handle_command(DaemonCommand::SnoozeBreak {
                rule_id: "rule-kill".to_string(),
                minutes: 5,
            })
            .await;
        let rt_kill_snoozed = engine.runtimes.get("rule-kill").unwrap();
        assert!(rt_kill_snoozed.snoozed_until.is_some());
    }

    #[tokio::test]
    async fn test_strict_mode_on_destructive_actions_locks_and_blocks_commands() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();

        let mut r_kill = Rule::default_template(0);
        r_kill.id = "rule-strict-kill".to_string();
        r_kill.action = RuleAction::KillProcess;
        r_kill.strict_mode = true;
        r_kill.target_executables = vec!["game.exe".to_string()];

        let mut r_close = Rule::default_template(1);
        r_close.id = "rule-strict-close".to_string();
        r_close.action = RuleAction::CloseWindow;
        r_close.strict_mode = true;
        r_close.target_executables = vec!["browser.exe".to_string()];

        cfg.rules = vec![r_kill.clone(), r_close.clone()];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        // 1. Setting grace_countdown on strict KillProcess rule activates strict lock
        engine.runtimes.get_mut("rule-strict-kill").unwrap().grace_countdown = Some((30, 30));
        assert!(engine.is_strict_lock_active(Local::now()));

        // 2. Strict lock blocks SnoozeBreak
        engine
            .handle_command(DaemonCommand::SnoozeBreak {
                rule_id: "rule-strict-kill".to_string(),
                minutes: 5,
            })
            .await;
        assert!(engine.runtimes.get("rule-strict-kill").unwrap().snoozed_until.is_none());
        assert_eq!(engine.runtimes.get("rule-strict-kill").unwrap().grace_countdown, Some((30, 30)));

        // 3. Strict lock blocks SnoozeAll
        engine.handle_command(DaemonCommand::SnoozeAll { minutes: 15 }).await;
        assert!(engine.global_snoozed_until.is_none());

        // 4. Strict lock blocks SetPauseAll(true)
        engine.handle_command(DaemonCommand::SetPauseAll { paused: true }).await;
        assert!(!engine.config.general.pause_all_rules);

        // 5. AcknowledgeAppWarning on strict rule is rejected and does not cancel grace
        engine
            .handle_command(DaemonCommand::AcknowledgeAppWarning {
                rule_id: "rule-strict-kill".to_string(),
            })
            .await;
        assert_eq!(engine.runtimes.get("rule-strict-kill").unwrap().grace_countdown, Some((30, 30)));

        // 6. Strict CloseWindow with active grace also maintains strict lock
        engine.runtimes.get_mut("rule-strict-kill").unwrap().grace_countdown = None;
        assert!(!engine.is_strict_lock_active(Local::now()));
        engine.runtimes.get_mut("rule-strict-close").unwrap().grace_countdown = Some((20, 20));
        assert!(engine.is_strict_lock_active(Local::now()));
    }

    #[tokio::test]
    async fn test_lock_workstation_and_kill_process_grace_not_bypassed_when_windows_minimized() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();

        // 1. Rule with LockWorkstation targeting a nonexistent window (simulating minimized or desktop active)
        let mut lock_rule = Rule::default_template(0);
        lock_rule.id = "rule-lock-test".to_string();
        lock_rule.action = RuleAction::LockWorkstation;
        lock_rule.trigger = TriggerType::Limit;
        lock_rule.limit = DurationConfig::new(10, TimeUnit::Min);
        lock_rule.grace_duration = DurationConfig::new(5, TimeUnit::Sec);
        lock_rule.monitor_selection = MonitorSelection::Executables;
        lock_rule.target_executables = vec!["nonexistent_closed_app_12345.exe".to_string()];

        // 2. Rule with KillProcess targeting a running process (explorer.exe) that may have no visible window
        let mut kill_rule = Rule::default_template(1);
        kill_rule.id = "rule-kill-test".to_string();
        kill_rule.action = RuleAction::KillProcess;
        kill_rule.trigger = TriggerType::Limit;
        kill_rule.limit = DurationConfig::new(10, TimeUnit::Min);
        kill_rule.grace_duration = DurationConfig::new(5, TimeUnit::Sec);
        kill_rule.monitor_selection = MonitorSelection::Executables;
        kill_rule.target_executables = vec!["explorer.exe".to_string()];

        cfg.rules = vec![lock_rule.clone(), kill_rule.clone()];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        // Verify find_matching_window_for_rule for KillProcess falls back to find_matching_process
        let re_kill = engine.runtimes.get("rule-kill-test").and_then(|r| r.compiled_regex.clone());
        let matched = find_matching_window_for_rule(
            &kill_rule,
            re_kill.as_ref(),
            &[],
            Utc::now(),
            10,
        );
        assert!(matched.is_some());
        let matched_name = matched.unwrap();
        assert!(matched_name.eq_ignore_ascii_case("explorer.exe") || matched_name.eq_ignore_ascii_case("explorer"));

        // Set grace_countdown to 1s on both rules
        engine.runtimes.get_mut("rule-lock-test").unwrap().grace_countdown = Some((1, 5));
        engine.runtimes.get_mut("rule-kill-test").unwrap().grace_countdown = Some((1, 5));

        // Tick realtime: grace countdown finishes (1 -> 0)
        engine.realtime_tick().await;

        // Verify LockWorkstation executed action (grace cleared, action_executed_today is true)
        let rt_lock = engine.runtimes.get("rule-lock-test").unwrap();
        assert_eq!(rt_lock.grace_countdown, None);
        assert!(
            rt_lock.action_executed_today,
            "LockWorkstation should execute action and mark today completed even when no matching window is visible"
        );

        // Verify KillProcess executed action (grace cleared, action_executed_today is true)
        let rt_kill = engine.runtimes.get("rule-kill-test").unwrap();
        assert_eq!(rt_kill.grace_countdown, None);
        assert!(
            rt_kill.action_executed_today,
            "KillProcess should execute action and mark today completed when target process is running"
        );
    }

    #[tokio::test]
    async fn test_hud_timer_tie_breaking_in_show_overlay() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();

        let mut r_nonstrict_short = Rule::default_template(0);
        r_nonstrict_short.id = "rule-timer-short".to_string();
        r_nonstrict_short.action = RuleAction::CloseWindow;
        r_nonstrict_short.strict_mode = false;

        let mut r_nonstrict_long = Rule::default_template(1);
        r_nonstrict_long.id = "rule-timer-long".to_string();
        r_nonstrict_long.action = RuleAction::ShowNotification;
        r_nonstrict_long.strict_mode = false;

        let mut r_strict = Rule::default_template(2);
        r_strict.id = "rule-timer-strict".to_string();
        r_strict.action = RuleAction::ShowOverlay;
        r_strict.strict_mode = true;

        cfg.rules = vec![
            r_nonstrict_short.clone(),
            r_nonstrict_long.clone(),
            r_strict.clone(),
        ];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        // 1. Display non-strict 10s destructive HudTimer
        engine.show_overlay(OverlayMode::HudTimer {
            rule_id: "rule-timer-short".to_string(),
            title: "Short Timer".into(),
            paragraphs: Vec::new(),
            duration_seconds: 10,
            remaining_seconds: 10,
            allow_snooze: true,
            strict_mode: false,
        });
        assert_eq!(engine.active_overlay_mode.rule_id(), Some("rule-timer-short"));

        // 2. Attempt to overwrite with a non-strict 60s non-destructive HudTimer:
        // Because "rule-timer-short" is destructive and has fewer remaining seconds,
        // it has higher urgency and must NOT be replaced!
        engine.show_overlay(OverlayMode::HudTimer {
            rule_id: "rule-timer-long".to_string(),
            title: "Long Timer".into(),
            paragraphs: Vec::new(),
            duration_seconds: 60,
            remaining_seconds: 60,
            allow_snooze: true,
            strict_mode: false,
        });
        assert_eq!(
            engine.active_overlay_mode.rule_id(),
            Some("rule-timer-short"),
            "Non-destructive 60s timer must not replace 10s destructive timer"
        );

        // 3. Attempt to overwrite with a strict 30s HudTimer:
        // Strict mode takes precedence over non-strict, so it replaces the active timer!
        engine.show_overlay(OverlayMode::HudTimer {
            rule_id: "rule-timer-strict".to_string(),
            title: "Strict Timer".into(),
            paragraphs: Vec::new(),
            duration_seconds: 30,
            remaining_seconds: 30,
            allow_snooze: false,
            strict_mode: true,
        });
        assert_eq!(
            engine.active_overlay_mode.rule_id(),
            Some("rule-timer-strict"),
            "Strict timer must replace non-strict timer"
        );

        // 4. A non-strict 5s timer cannot replace a strict 30s timer
        engine.show_overlay(OverlayMode::HudTimer {
            rule_id: "rule-timer-short".to_string(),
            title: "Short Timer".into(),
            paragraphs: Vec::new(),
            duration_seconds: 5,
            remaining_seconds: 5,
            allow_snooze: true,
            strict_mode: false,
        });
        assert_eq!(
            engine.active_overlay_mode.rule_id(),
            Some("rule-timer-strict"),
            "Non-strict timer cannot replace active strict timer"
        );
    }

    #[tokio::test]
    async fn test_delayed_break_fullscreen_d3d_suppression() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();
        cfg.general.suppress_fullscreen_d3d = true;

        let mut rule = Rule::default_template(0);
        rule.id = "rule-delayed-d3d".to_string();
        rule.action = RuleAction::ShowOverlay;
        rule.break_duration = DurationConfig::new(5, TimeUnit::Min);

        cfg.rules = vec![rule.clone()];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        let past = Local::now() - Duration::seconds(20);
        engine.runtimes.get_mut("rule-delayed-d3d").unwrap().pending_break_since = Some(past);

        engine.realtime_tick().await;
        assert_eq!(engine.runtimes.get("rule-delayed-d3d").unwrap().pending_break_since, None);
    }

    #[tokio::test]
    async fn test_preview_hud_timer_countdown_and_autohide() {
        let (overlay_tx, mut overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();

        let mut rule = Rule::default_template(0);
        rule.id = "rule-preview-test".to_string();
        rule.action = RuleAction::CloseWindow;
        rule.grace_duration = DurationConfig::new(2, TimeUnit::Sec);

        cfg.rules = vec![rule.clone()];
        let mut engine = Engine::new(cfg, aw_client.clone(), overlay_tx.clone());

        engine.handle_command(DaemonCommand::PreviewRule(Box::new(rule))).await;

        let action = overlay_rx.recv().await.expect("Expected preview overlay action");
        assert!(matches!(action, OverlayAction::Show(OverlayMode::HudTimer { ref rule_id, remaining_seconds: 2, .. }) if rule_id == "test_preview"));

        // Tick 1: decrements remaining_seconds to 1
        engine.realtime_tick().await;
        if let OverlayMode::HudTimer { remaining_seconds, .. } = engine.active_overlay_mode {
            assert_eq!(remaining_seconds, 1);
        } else {
            panic!("Expected HudTimer active overlay mode after tick 1");
        }

        // Tick 2: decrements remaining_seconds to 0 and auto-hides overlay
        engine.realtime_tick().await;
        assert!(matches!(engine.active_overlay_mode, OverlayMode::Hidden));
    }

    #[tokio::test]
    async fn test_acknowledge_app_warning_on_strict_rule_toast_vs_grace() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();

        let mut strict_rule = Rule::default_template(0);
        strict_rule.id = "rule-strict-toast".to_string();
        strict_rule.strict_mode = true;
        strict_rule.action = RuleAction::CloseWindow;
        strict_rule.grace_duration = DurationConfig::new(30, TimeUnit::Sec);

        cfg.rules = vec![strict_rule.clone()];
        let mut engine = Engine::new(cfg, aw_client.clone(), overlay_tx.clone());

        // 1. When a strict rule has a HudToast (e.g. Stage 1 warning) with grace_countdown: None
        engine.active_overlay_mode = OverlayMode::HudToast {
            rule_id: "rule-strict-toast".to_string(),
            title: "Warning".to_string(),
            paragraphs: Vec::new(),
            snooze_options_minutes: Vec::new(),
            duration_seconds: 10,
            remaining_seconds: 10,
            show_time_remaining: false,
        };
        let rt = engine.runtimes.get_mut("rule-strict-toast").unwrap();
        rt.can_rollback_completion = true;
        rt.grace_countdown = None;

        engine
            .handle_command(DaemonCommand::AcknowledgeAppWarning {
                rule_id: "rule-strict-toast".to_string(),
            })
            .await;

        let rt_after = engine.runtimes.get("rule-strict-toast").unwrap();
        assert!(!rt_after.can_rollback_completion, "AcknowledgeAppWarning should clear can_rollback_completion on strict toast");
        assert!(matches!(engine.active_overlay_mode, OverlayMode::Hidden), "AcknowledgeAppWarning should dismiss strict toast");

        // 2. When a strict rule has active grace_countdown: Some((30, 30))
        engine.active_overlay_mode = OverlayMode::HudTimer {
            rule_id: "rule-strict-toast".to_string(),
            title: "Grace".to_string(),
            paragraphs: Vec::new(),
            duration_seconds: 30,
            remaining_seconds: 30,
            allow_snooze: false,
            strict_mode: true,
        };
        let rt_grace = engine.runtimes.get_mut("rule-strict-toast").unwrap();
        rt_grace.grace_countdown = Some((30, 30));

        engine
            .handle_command(DaemonCommand::AcknowledgeAppWarning {
                rule_id: "rule-strict-toast".to_string(),
            })
            .await;

        let rt_grace_after = engine.runtimes.get("rule-strict-toast").unwrap();
        assert_eq!(rt_grace_after.grace_countdown, Some((30, 30)), "AcknowledgeAppWarning must be rejected when strict grace countdown is active");
        assert!(matches!(engine.active_overlay_mode, OverlayMode::HudTimer { .. }), "Overlay must not be dismissed when strict grace countdown is active");
    }

    #[tokio::test]
    async fn test_execute_rule_action_show_statistics_forwards_click_through() {
        let (overlay_tx, mut overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();

        let mut rule = Rule::default_template(0);
        rule.id = "rule-stats-exec".to_string();
        rule.action = RuleAction::ShowStatistics;
        rule.click_through = true;

        cfg.rules = vec![rule.clone()];
        let mut engine = Engine::new(cfg, aw_client.clone(), overlay_tx.clone());

        while overlay_rx.try_recv().is_ok() {}

        engine
            .execute_rule_action(
                &rule,
                None,
                Local::now(),
                "test_app",
                3600,
                3600,
            )
            .await;

        let action = overlay_rx.recv().await.expect("Expected overlay action from execute_rule_action");
        match action {
            OverlayAction::Show(OverlayMode::HudStatus { click_through, .. }) => {
                assert!(click_through, "execute_rule_action must forward rule.click_through to HudStatus");
            }
            other => panic!("Expected HudStatus from execute_rule_action for ShowStatistics, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_urls_rule_does_not_populate_from_stale_events_outside_recency_cutoff() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();

        let mut rule = Rule::default_template(0);
        rule.id = "rule-urls-stale".to_string();
        rule.monitor_selection = MonitorSelection::Urls;
        rule.action = RuleAction::CloseTab;
        rule.url_regex = "(?i)youtube\\.com".to_string();
        rule.limit = DurationConfig::new(10, TimeUnit::Hr);
        rule.trigger = TriggerType::Limit;

        cfg.rules = vec![rule.clone()];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        let now = Local::now();
        let now_utc = now.with_timezone(&Utc);
        let start_of_day_utc = now_utc - Duration::hours(5);

        // Stale event ended 2 hours ago (outside poll_interval recency cutoff)
        let stale_ev = make_event(
            now_utc - Duration::hours(2),
            60.0,
            "chrome.exe",
            "Old Video - YouTube",
            "https://www.youtube.com/watch?v=old",
        );

        let re = Regex::new(&rule.url_regex).ok();
        engine
            .evaluate_rule(
                &rule,
                re.as_ref(),
                now,
                &[stale_ev],
                start_of_day_utc,
                now_utc,
                &[],
            )
            .await;

        let rt = engine.runtimes.get("rule-urls-stale").unwrap();
        assert_eq!(rt.last_matched_url, None, "Stale URL event outside recency_cutoff must not populate last_matched_url");
        assert_eq!(rt.last_matched_title, None, "Stale URL event outside recency_cutoff must not populate last_matched_title");
    }

    #[tokio::test]
    async fn test_limit_lock_workstation_repeat_execution_enforces_15s_min_grace() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();
        cfg.general.poll_interval_seconds = 3600;

        let mut rule_zero_grace = Rule::default_template(0);
        rule_zero_grace.id = "rule-lock-zero".to_string();
        rule_zero_grace.action = RuleAction::LockWorkstation;
        rule_zero_grace.trigger = TriggerType::Limit;
        rule_zero_grace.limit = DurationConfig::new(1, TimeUnit::Hr);
        rule_zero_grace.grace_duration = DurationConfig::new(0, TimeUnit::Sec);

        let mut rule_long_grace = Rule::default_template(1);
        rule_long_grace.id = "rule-lock-long".to_string();
        rule_long_grace.action = RuleAction::LockWorkstation;
        rule_long_grace.trigger = TriggerType::Limit;
        rule_long_grace.limit = DurationConfig::new(1, TimeUnit::Hr);
        rule_long_grace.grace_duration = DurationConfig::new(30, TimeUnit::Sec);

        cfg.rules = vec![rule_zero_grace.clone(), rule_long_grace.clone()];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        let now = Local::now();
        let now_utc = now.with_timezone(&Utc);
        let start_of_day_utc = now_utc - Duration::hours(5);

        let ev = make_event(
            now_utc - Duration::hours(2),
            7200.0,
            "work.exe",
            "Working",
            "",
        );

        // Case A: rule_zero_grace with already_executed = true
        engine.runtimes.get_mut("rule-lock-zero").unwrap().action_executed_today = true;
        engine
            .evaluate_rule(
                &rule_zero_grace,
                None,
                now,
                &[ev.clone()],
                start_of_day_utc,
                now_utc,
                &[],
            )
            .await;

        let rt_zero = engine.runtimes.get("rule-lock-zero").unwrap();
        assert_eq!(
            rt_zero.grace_countdown,
            Some((15, 15)),
            "Zero-grace LockWorkstation repeat execution must enforce 15s minimum grace"
        );

        // Case B: rule_long_grace with already_executed = true
        engine.runtimes.get_mut("rule-lock-long").unwrap().action_executed_today = true;
        engine
            .evaluate_rule(
                &rule_long_grace,
                None,
                now,
                &[ev.clone()],
                start_of_day_utc,
                now_utc,
                &[],
            )
            .await;

        let rt_long = engine.runtimes.get("rule-lock-long").unwrap();
        assert_eq!(
            rt_long.grace_countdown,
            Some((30, 30)),
            "Longer grace duration (30s) must not be clamped down on LockWorkstation repeat execution"
        );
    }

    #[tokio::test]
    async fn test_dispatch_pre_action_warning_respects_notification_method_and_duration() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();
        cfg.general.suppress_fullscreen_d3d = false;

        // Rule 1: CloseWindow action with NotificationMethod::Hud, 25s duration, show_remaining_time = true
        let mut rule_hud = Rule::default_template(0);
        rule_hud.id = "rule-warn-hud".to_string();
        rule_hud.action = RuleAction::CloseWindow;
        rule_hud.notification_method = NotificationMethod::Hud;
        rule_hud.notification_duration = DurationConfig::new(25, TimeUnit::Sec);
        rule_hud.show_remaining_time = true;

        // Rule 2: ShowOverlay action with NotificationMethod::Windows
        let mut rule_win = Rule::default_template(1);
        rule_win.id = "rule-warn-win".to_string();
        rule_win.action = RuleAction::ShowOverlay;
        rule_win.notification_method = NotificationMethod::Windows;

        cfg.rules = vec![rule_hud.clone(), rule_win.clone()];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        let ctx = formatter::build_preview_context(&rule_hud);

        // 1. Dispatch warning for rule_hud: must display HudToast with 25s duration and show_time_remaining = true
        engine.dispatch_pre_action_warning(&rule_hud, &ctx, 120);
        match &engine.active_overlay_mode {
            OverlayMode::HudToast {
                rule_id,
                duration_seconds,
                remaining_seconds,
                show_time_remaining,
                ..
            } => {
                assert_eq!(rule_id, "rule-warn-hud");
                assert_eq!(*duration_seconds, 25);
                assert_eq!(*remaining_seconds, 25);
                assert!(*show_time_remaining);
            }
            other => panic!("Expected HudToast for NotificationMethod::Hud, got {:?}", other),
        }

        // 2. Dispatch warning for rule_win: NotificationMethod::Windows routes to system toast, NOT HudToast
        let count_before = enforcer::get_test_notifications_sent();
        engine.active_overlay_mode = OverlayMode::Hidden;
        let ctx_win = formatter::build_preview_context(&rule_win);
        engine.dispatch_pre_action_warning(&rule_win, &ctx_win, 60);
        assert!(
            matches!(engine.active_overlay_mode, OverlayMode::Hidden),
            "NotificationMethod::Windows must not emit HudToast overlay"
        );
        assert!(
            enforcer::get_test_notifications_sent() > count_before,
            "NotificationMethod::Windows should dispatch a native system notification"
        );
    }

    #[tokio::test]
    async fn test_snooze_all_preserves_action_executed_today_on_completed_limit_rules() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();
        cfg.general.poll_interval_seconds = 3600;

        let mut rule = Rule::default_template(0);
        rule.id = "rule-limit-complete".to_string();
        rule.trigger = TriggerType::Limit;
        rule.action = RuleAction::ShowNotification;
        rule.limit = DurationConfig::new(1, TimeUnit::Hr);

        cfg.rules = vec![rule.clone()];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        let now = Local::now();
        let now_utc = now.with_timezone(&Utc);
        let start_of_day_utc = now_utc - Duration::hours(5);

        // Mark rule cycle completed earlier today
        let rt = engine.runtimes.get_mut("rule-limit-complete").unwrap();
        rt.complete_cycle(now - Duration::hours(2));
        assert!(rt.action_executed_today);
        assert!(!rt.can_rollback_completion);

        // User executes SnoozeAll
        engine.handle_command(DaemonCommand::SnoozeAll { minutes: 15 }).await;

        let rt = engine.runtimes.get("rule-limit-complete").unwrap();
        assert!(
            rt.action_executed_today,
            "action_executed_today must be preserved on idle completed Limit rules"
        );

        // Simulate expiration of snooze
        let later = now + Duration::minutes(20);
        let later_utc = later.with_timezone(&Utc);
        let ev = make_event(now_utc - Duration::hours(2), 7200.0, "work.exe", "Working", "");

        engine
            .evaluate_rule(&rule, None, later, &[ev], start_of_day_utc, later_utc, &[])
            .await;

        let rt = engine.runtimes.get("rule-limit-complete").unwrap();
        assert!(rt.action_executed_today);
        assert!(rt.grace_countdown.is_none());
        assert!(matches!(engine.active_overlay_mode, OverlayMode::Hidden));
    }

    #[tokio::test]
    async fn test_aborted_active_break_resets_action_executed_today() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();
        cfg.general.suppress_fullscreen_d3d = false;
        cfg.general.poll_interval_seconds = 3600;

        let mut rule = Rule::default_template(0);
        rule.id = "rule-limit-break".to_string();
        rule.trigger = TriggerType::Limit;
        rule.action = RuleAction::ShowOverlay;
        rule.limit = DurationConfig::new(1, TimeUnit::Hr);
        rule.break_duration = DurationConfig::new(10, TimeUnit::Min);

        cfg.rules = vec![rule.clone()];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        let now = Local::now();
        let now_utc = now.with_timezone(&Utc);
        let start_of_day_utc = now_utc - Duration::hours(5);

        let ev = make_event(now_utc - Duration::hours(2), 7200.0, "work.exe", "Working", "");

        // Trigger break
        engine
            .evaluate_rule(&rule, None, now, &[ev.clone()], start_of_day_utc, now_utc, &[])
            .await;

        // If system was active (< 1500ms idle), evaluate_rule placed rule in pending_break_since.
        // Fast-forward pending_break_since past natural delay and tick realtime to launch active break.
        if engine.runtimes.get("rule-limit-break").unwrap().active_break.is_none() {
            if let Some(rt) = engine.runtimes.get_mut("rule-limit-break") {
                rt.pending_break_since = Some(now - Duration::seconds(20));
            }
            engine.realtime_tick().await;
        }

        let rt = engine.runtimes.get("rule-limit-break").unwrap();
        assert!(rt.active_break.is_some());
        assert!(rt.action_executed_today);

        // Abort active break via pause
        engine.handle_command(DaemonCommand::SetPauseAll { paused: true }).await;

        let rt = engine.runtimes.get("rule-limit-break").unwrap();
        assert!(rt.active_break.is_none());
        assert!(
            !rt.action_executed_today,
            "Aborted active break must reset action_executed_today to false"
        );

        // Unpause and evaluate again: break should re-trigger
        engine.handle_command(DaemonCommand::SetPauseAll { paused: false }).await;
        engine
            .evaluate_rule(
                &rule,
                None,
                now + Duration::seconds(1),
                &[ev],
                start_of_day_utc,
                now_utc + Duration::seconds(1),
                &[],
            )
            .await;

        if engine.runtimes.get("rule-limit-break").unwrap().active_break.is_none() {
            if let Some(rt) = engine.runtimes.get_mut("rule-limit-break") {
                rt.pending_break_since = Some(now - Duration::seconds(20));
            }
            engine.realtime_tick().await;
        }

        let rt = engine.runtimes.get("rule-limit-break").unwrap();
        assert!(
            rt.active_break.is_some(),
            "Break must re-trigger after previous attempt was aborted"
        );
    }

    #[tokio::test]
    async fn test_display_stats_overlay_blocked_by_higher_priority_overlay() {
        let (overlay_tx, mut overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let cfg = AppConfig::default();
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        // 1. BreakEnforcer overlay is active (priority 4)
        engine.active_overlay_mode = OverlayMode::BreakEnforcer {
            rule_id: "rule-active-break".into(),
            title: "Mandatory Break".into(),
            paragraphs: vec![],
            duration_seconds: 300,
            remaining_seconds: 300,
            allow_snooze: false,
            strict_mode: true,
            click_through: false,
        };

        engine.display_stats_overlay(false).await;

        assert!(
            engine.stats_cache.is_none(),
            "stats_cache must not be populated when BreakEnforcer is active"
        );
        match &engine.active_overlay_mode {
            OverlayMode::BreakEnforcer { rule_id, .. } => {
                assert_eq!(rule_id, "rule-active-break");
            }
            other => panic!("Expected BreakEnforcer to remain active, got {:?}", other),
        }
        assert!(overlay_rx.try_recv().is_err(), "No overlay action should be sent");

        // 2. HudTimer overlay is active (priority 3)
        engine.active_overlay_mode = OverlayMode::HudTimer {
            rule_id: "rule-active-timer".into(),
            title: "Grace Timer".into(),
            paragraphs: vec![],
            duration_seconds: 30,
            remaining_seconds: 30,
            allow_snooze: false,
            strict_mode: true,
        };

        engine.display_stats_overlay(false).await;

        assert!(
            engine.stats_cache.is_none(),
            "stats_cache must not be populated when HudTimer is active"
        );
        match &engine.active_overlay_mode {
            OverlayMode::HudTimer { rule_id, .. } => {
                assert_eq!(rule_id, "rule-active-timer");
            }
            other => panic!("Expected HudTimer to remain active, got {:?}", other),
        }
        assert!(overlay_rx.try_recv().is_err(), "No overlay action should be sent");
    }

    #[tokio::test]
    async fn test_continuous_session_from_merged_and_snoozed_period_telemetry() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();
        cfg.general.suppress_fullscreen_d3d = false;

        let mut rule = Rule::default_template(0);
        rule.id = "rule-period-snoozed".to_string();
        rule.trigger = TriggerType::Period;
        rule.period = DurationConfig::new(50, TimeUnit::Min);
        rule.reset_after_afk = DurationConfig::new(5, TimeUnit::Min);

        cfg.rules = vec![rule.clone()];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        let now = Local::now();
        let now_utc = now.with_timezone(&Utc);
        let start_of_day_utc = now_utc - Duration::hours(5);

        // 30 minutes of continuous activity ending at now_utc
        let ev1 = make_event(now_utc - Duration::minutes(30), 600.0, "work.exe", "Work 1", "");
        let ev2 = make_event(now_utc - Duration::minutes(20), 600.0, "work.exe", "Work 2", "");
        let ev3 = make_event(now_utc - Duration::minutes(10), 600.0, "work.exe", "Work 3", "");
        let events = vec![ev1, ev2, ev3];

        // 1. Verify calculate_continuous_session_from_merged matches calculate_continuous_session_for_rule
        let merged = events_to_merged_intervals(&events);
        let session_from_merged = calculate_continuous_session_from_merged(&merged, now_utc, 300);
        let session_from_rule = calculate_continuous_session_for_rule(&events, now, None, 300, &rule, None);
        assert_eq!(session_from_merged, 1800);
        assert_eq!(session_from_rule, 1800);
        assert_eq!(session_from_merged, session_from_rule);

        // 2. Snooze the period rule
        let rt = engine.runtimes.get_mut("rule-period-snoozed").unwrap();
        rt.snoozed_until = Some(now + Duration::minutes(15));
        assert_eq!(rt.current_session_seconds, 0);

        // Evaluate rule: returns false because rule is snoozed, but updates current_session_seconds
        let triggered = engine.evaluate_rule(&rule, None, now, &events, start_of_day_utc, now_utc, &merged).await;
        assert!(!triggered);

        let rt = engine.runtimes.get("rule-period-snoozed").unwrap();
        assert_eq!(rt.current_session_seconds, 1800, "Snoozed Period rule must still update current_session_seconds");
    }

    #[tokio::test]
    async fn test_wildcard_rule_reuses_merged_intervals_identically() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();
        cfg.general.suppress_fullscreen_d3d = false;

        let mut rule_period = Rule::default_template(0);
        rule_period.id = "rule-wildcard-period".to_string();
        rule_period.trigger = TriggerType::Period;
        rule_period.period = DurationConfig::new(50, TimeUnit::Min);
        rule_period.reset_after_afk = DurationConfig::new(5, TimeUnit::Min);
        rule_period.target_executables = vec!["*".to_string()];

        let mut rule_limit = Rule::default_template(1);
        rule_limit.id = "rule-wildcard-limit".to_string();
        rule_limit.trigger = TriggerType::Limit;
        rule_limit.limit = DurationConfig::new(2, TimeUnit::Hr);
        rule_limit.target_executables = Vec::new(); // empty means wildcard

        let mut rule_specific = Rule::default_template(2);
        rule_specific.id = "rule-specific".to_string();
        rule_specific.target_executables = vec!["code.exe".to_string()];

        let mut rule_url = Rule::default_template(3);
        rule_url.id = "rule-url".to_string();
        rule_url.monitor_selection = MonitorSelection::Urls;

        let mut rule_excluded = Rule::default_template(4);
        rule_excluded.id = "rule-excluded".to_string();
        rule_excluded.target_executables = vec!["*".to_string(), "!zoom.exe".to_string()];

        assert!(is_wildcard_executables_rule(&rule_period));
        assert!(is_wildcard_executables_rule(&rule_limit));
        assert!(!is_wildcard_executables_rule(&rule_specific));
        assert!(!is_wildcard_executables_rule(&rule_url));
        assert!(!is_wildcard_executables_rule(&rule_excluded));

        cfg.rules = vec![rule_period.clone(), rule_limit.clone()];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        let now = Local::now();
        let now_utc = now.with_timezone(&Utc);
        let start_of_day_utc = now_utc - Duration::hours(8);

        // Synthetic events with overlap, AFK gap, and boundary crossing
        let ev1 = make_event(now_utc - Duration::minutes(60), 1200.0, "app1.exe", "Title 1", ""); // -60m..-40m
        let ev2 = make_event(now_utc - Duration::minutes(45), 1500.0, "app2.exe", "Title 2", ""); // -45m..-20m (overlaps ev1)
        let ev3 = make_event(now_utc - Duration::minutes(18), 780.0, "app3.exe", "Title 3", "");  // -18m..-5m (gap of 2m < 5m)
        let ev4 = make_event(now_utc - Duration::minutes(4), 360.0, "app4.exe", "Title 4", "");   // -4m..+2m (crosses now_utc boundary)
        let ev5 = make_event(start_of_day_utc - Duration::minutes(10), 2400.0, "app5.exe", "Title 5", ""); // spans across start_of_day_utc
        let ev6 = make_event(start_of_day_utc + Duration::hours(1), 3600.0, "app6.exe", "Title 6", ""); // +1h..+2h

        let events = vec![ev1, ev2, ev3, ev4, ev5, ev6];
        let merged_all = events_to_merged_intervals(&events);

        // 1. Verify Period calculations match identically:
        // Case A: Unclipped (no previous break)
        let legacy_period_a = calculate_continuous_session_for_rule(&events, now, None, 300, &rule_period, None);
        let fast_period_a = calculate_continuous_session_from_merged_clipped(&merged_all, now_utc, None, 300);
        assert_eq!(fast_period_a, legacy_period_a);
        assert!(fast_period_a > 0);

        // Case B: Clipped by last_break_completed_at inside overlapping event span
        let break_b = Some(now - Duration::minutes(30));
        let min_dt_b = break_b.map(|dt| dt.with_timezone(&Utc));
        let legacy_period_b = calculate_continuous_session_for_rule(&events, now, break_b, 300, &rule_period, None);
        let fast_period_b = calculate_continuous_session_from_merged_clipped(&merged_all, now_utc, min_dt_b, 300);
        assert_eq!(fast_period_b, legacy_period_b);
        assert!(fast_period_b < fast_period_a);

        // Case C: Clipped near now_utc boundary
        let break_c = Some(now - Duration::minutes(2));
        let min_dt_c = break_c.map(|dt| dt.with_timezone(&Utc));
        let legacy_period_c = calculate_continuous_session_for_rule(&events, now, break_c, 300, &rule_period, None);
        let fast_period_c = calculate_continuous_session_from_merged_clipped(&merged_all, now_utc, min_dt_c, 300);
        assert_eq!(fast_period_c, legacy_period_c);
        assert_eq!(fast_period_c, 120); // 2 minutes from now_utc - 2m to now_utc

        // 2. Verify Limit calculations match identically:
        // Case A: Full day usage
        let legacy_limit_a = calculate_usage_for_rule(&events, &rule_limit, None, start_of_day_utc, now_utc);
        let fast_limit_a = compute_overlap_from_merged_intervals(&merged_all, start_of_day_utc, now_utc).round() as u32;
        assert_eq!(fast_limit_a, legacy_limit_a);
        assert!(fast_limit_a > 0);

        // Case B: Mid-day reset usage
        let mid_day = start_of_day_utc + Duration::hours(3);
        let legacy_limit_b = calculate_usage_for_rule(&events, &rule_limit, None, mid_day, now_utc);
        let fast_limit_b = compute_overlap_from_merged_intervals(&merged_all, mid_day, now_utc).round() as u32;
        assert_eq!(fast_limit_b, legacy_limit_b);

        // 3. Verify evaluate_rule produces identical runtime state when passing merged_all
        engine.evaluate_rule(&rule_period, None, now, &events, start_of_day_utc, now_utc, &merged_all).await;
        let rt_period = engine.runtimes.get("rule-wildcard-period").unwrap();
        assert_eq!(rt_period.current_session_seconds, fast_period_a);

        engine.evaluate_rule(&rule_limit, None, now, &events, start_of_day_utc, now_utc, &merged_all).await;
        let rt_limit = engine.runtimes.get("rule-wildcard-limit").unwrap();
        assert_eq!(rt_limit.daily_usage_seconds, fast_limit_a);
    }

    #[tokio::test]
    async fn test_limit_zero_does_not_trigger_when_no_recent_events_and_not_foreground() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();

        let mut rule = Rule::default_template(0);
        rule.id = "rule-limit-zero".to_string();
        rule.monitor_selection = MonitorSelection::Executables;
        rule.target_executables = vec!["nonexistent_app_99999.exe".to_string()];
        rule.trigger = TriggerType::Limit;
        rule.limit = DurationConfig::new(0, TimeUnit::Sec);
        rule.grace_duration = DurationConfig::new(0, TimeUnit::Sec);
        rule.action = RuleAction::CloseWindow;

        cfg.rules = vec![rule.clone()];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        let now = Local::now();
        let now_utc = now.with_timezone(&Utc);
        let start_of_day_utc = now_utc - Duration::hours(5);

        // Evaluate with empty events and nonexistent app (so it is not in foreground)
        engine
            .evaluate_rule(
                &rule,
                None,
                now,
                &[],
                start_of_day_utc,
                now_utc,
                &[],
            )
            .await;

        let rt = engine.runtimes.get("rule-limit-zero").unwrap();
        assert!(!rt.action_executed_today);
        assert!(rt.grace_countdown.is_none());
    }

    #[tokio::test]
    async fn test_show_overlay_pending_break_preserves_last_matched_app() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();
        cfg.general.suppress_fullscreen_d3d = false;

        let mut rule = Rule::default_template(0);
        rule.id = "rule-overlay-preserve-app".to_string();
        rule.action = RuleAction::ShowOverlay;
        rule.monitor_selection = MonitorSelection::Executables;
        rule.target_executables = vec!["*".to_string()];
        rule.message = "Take a break from {app}!".to_string();

        cfg.rules = vec![rule.clone()];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        let now = Local::now();
        let rt = engine.runtimes.get_mut("rule-overlay-preserve-app").unwrap();
        rt.pending_break_since = Some(now - Duration::seconds(20)); // > 15s to force launch
        rt.last_matched_app = Some("code.exe".to_string());

        engine.realtime_tick().await;

        let rt_after = engine.runtimes.get("rule-overlay-preserve-app").unwrap();
        assert!(rt_after.active_break.is_some());

        // Verify that the overlay mode paragraphs contain "code.exe" instead of "Active Application"
        if let OverlayMode::BreakEnforcer { ref paragraphs, .. } = engine.active_overlay_mode {
            let full_text = paragraphs
                .iter()
                .map(|p| p.text.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            assert!(
                full_text.contains("code.exe"),
                "Break overlay must preserve last_matched_app 'code.exe', found: {:?}",
                full_text
            );
            assert!(
                !full_text.contains("Active Application"),
                "Break overlay should not contain 'Active Application', found: {:?}",
                full_text
            );
        } else {
            panic!("Expected BreakEnforcer overlay, got {:?}", engine.active_overlay_mode);
        }
    }

    #[tokio::test]
    async fn test_limit_repeat_lock_workstation_requires_active_target() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();

        let mut rule = Rule::default_template(0);
        rule.id = "rule-repeat-lock".to_string();
        rule.monitor_selection = MonitorSelection::Executables;
        rule.target_executables = vec!["nonexistent_game_99999.exe".to_string()];
        rule.trigger = TriggerType::Limit;
        rule.limit = DurationConfig::new(10, TimeUnit::Min);
        rule.grace_duration = DurationConfig::new(15, TimeUnit::Sec);
        rule.action = RuleAction::LockWorkstation;

        cfg.rules = vec![rule.clone()];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        let now = Local::now();
        let now_utc = now.with_timezone(&Utc);
        let start_of_day_utc = now_utc - Duration::hours(5);

        // Rule already executed today, elapsed usage exceeds limit
        let rt = engine.runtimes.get_mut("rule-repeat-lock").unwrap();
        rt.action_executed_today = true;
        rt.daily_usage_seconds = 1200; // 20m > 10m

        // With empty events and target not in foreground, repeat lock should NOT re-trigger
        engine
            .evaluate_rule(
                &rule,
                None,
                now,
                &[],
                start_of_day_utc,
                now_utc,
                &[],
            )
            .await;

        let rt_after = engine.runtimes.get("rule-repeat-lock").unwrap();
        assert!(rt_after.grace_countdown.is_none());
    }

    #[tokio::test]
    async fn test_dismiss_destructive_grace_timer_transitions_and_prevents_resurrection() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();

        let mut r_kill = Rule::default_template(0);
        r_kill.id = "rule-kill-dismiss".to_string();
        r_kill.action = RuleAction::KillProcess;
        r_kill.target_executables = vec!["badapp.exe".to_string()];
        r_kill.grace_duration = DurationConfig::new(5, TimeUnit::Sec);

        let mut r_close = Rule::default_template(1);
        r_close.id = "rule-close-dismiss".to_string();
        r_close.action = RuleAction::CloseWindow;
        r_close.target_executables = vec!["game.exe".to_string()];
        r_close.grace_duration = DurationConfig::new(10, TimeUnit::Sec);

        cfg.rules = vec![r_kill.clone(), r_close.clone()];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        // Set grace_countdown on both rules (5s on kill, 10s on close)
        engine.runtimes.get_mut("rule-kill-dismiss").unwrap().grace_countdown = Some((5, 5));
        engine.runtimes.get_mut("rule-close-dismiss").unwrap().grace_countdown = Some((10, 10));

        // transition_to_active_countdown_or_hide picks rule-kill-dismiss (shortest remaining)
        engine.transition_to_active_countdown_or_hide();
        assert!(engine.is_rule_displayed("rule-kill-dismiss"));

        // User dismisses rule-kill-dismiss
        engine
            .handle_command(DaemonCommand::AcknowledgeAppWarning {
                rule_id: "rule-kill-dismiss".to_string(),
            })
            .await;

        let rt_kill = engine.runtimes.get("rule-kill-dismiss").unwrap();
        assert!(rt_kill.grace_dismissed, "grace_dismissed must be true");
        assert!(rt_kill.grace_countdown.is_some(), "grace countdown must continue in background");

        // Engine should have transitioned to the other active countdown (rule-close-dismiss)
        assert!(engine.is_rule_displayed("rule-close-dismiss"));

        // Calling transition_to_active_countdown_or_hide again does not resurrect rule-kill-dismiss
        engine.transition_to_active_countdown_or_hide();
        assert!(engine.is_rule_displayed("rule-close-dismiss"));

        // User dismisses rule-close-dismiss
        engine
            .handle_command(DaemonCommand::AcknowledgeAppWarning {
                rule_id: "rule-close-dismiss".to_string(),
            })
            .await;

        let rt_close = engine.runtimes.get("rule-close-dismiss").unwrap();
        assert!(rt_close.grace_dismissed, "grace_dismissed must be true");
        assert!(rt_close.grace_countdown.is_some());

        // Both dismissed: overlay should be hidden
        assert!(matches!(engine.active_overlay_mode, OverlayMode::Hidden));

        // Calling transition_to_active_countdown_or_hide again does not resurrect either dismissed timer
        engine.transition_to_active_countdown_or_hide();
        assert!(matches!(engine.active_overlay_mode, OverlayMode::Hidden));
    }

    #[tokio::test]
    async fn test_show_notification_hud_blocked_by_break_keeps_rollback_false() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();

        let mut break_rule = Rule::default_template(0);
        break_rule.id = "rule-break-active".to_string();
        break_rule.action = RuleAction::ShowOverlay;

        let mut notify_rule = Rule::default_template(1);
        notify_rule.id = "rule-notify-hud".to_string();
        notify_rule.action = RuleAction::ShowNotification;
        notify_rule.notification_method = NotificationMethod::Hud;

        cfg.rules = vec![break_rule.clone(), notify_rule.clone()];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        let now = Local::now();

        // Simulate an active BreakEnforcer overlay on screen
        let break_mode = OverlayMode::BreakEnforcer {
            rule_id: "rule-break-active".into(),
            title: "Active Break".into(),
            paragraphs: Vec::new(),
            allow_snooze: true,
            strict_mode: false,
            click_through: false,
            remaining_seconds: 60,
            duration_seconds: 60,
        };
        engine.active_overlay_mode = break_mode;
        assert!(engine.is_rule_displayed("rule-break-active"));

        // Execute notify rule action (ShowNotification via Hud)
        engine
            .execute_rule_action(&notify_rule, None, now, "work.exe", 3600, 3600)
            .await;

        // BreakEnforcer is priority 4, HudToast is priority 2, so HudToast was rejected
        assert!(engine.is_rule_displayed("rule-break-active"));
        assert!(!engine.is_rule_displayed("rule-notify-hud"));

        // can_rollback_completion must remain false because the toast was blocked
        let rt_notify = engine.runtimes.get("rule-notify-hud").unwrap();
        assert!(
            !rt_notify.can_rollback_completion,
            "can_rollback_completion must remain false when HudToast is blocked by BreakEnforcer"
        );
        assert!(rt_notify.last_completed_at.is_some());
    }

    #[tokio::test]
    async fn test_dispatch_pre_action_warning_hud_blocked_by_break_falls_back_to_windows() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();
        cfg.general.suppress_fullscreen_d3d = false;

        let mut break_rule = Rule::default_template(0);
        break_rule.id = "rule-break-active-warn".to_string();
        break_rule.action = RuleAction::ShowOverlay;

        let mut warn_rule = Rule::default_template(1);
        warn_rule.id = "rule-warn-hud".to_string();
        warn_rule.name = "Warning Rule".to_string();
        warn_rule.notification_method = NotificationMethod::Hud;

        cfg.rules = vec![break_rule.clone(), warn_rule.clone()];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        // BreakEnforcer is active
        engine.active_overlay_mode = OverlayMode::BreakEnforcer {
            rule_id: "rule-break-active-warn".into(),
            title: "Active Break".into(),
            paragraphs: Vec::new(),
            allow_snooze: true,
            strict_mode: false,
            click_through: false,
            remaining_seconds: 60,
            duration_seconds: 60,
        };

        let ctx = MessageContext::sample();
        // Dispatches pre-action warning; since BreakEnforcer is priority 4, show_overlay is rejected
        engine.dispatch_pre_action_warning(&warn_rule, &ctx, 60);

        // Toast was blocked, BreakEnforcer is still displayed
        assert!(engine.is_rule_displayed("rule-break-active-warn"));
        assert!(!engine.is_rule_displayed("rule-warn-hud"));
    }

    #[tokio::test]
    async fn test_global_snooze_updates_session_and_resets_warned_on_afk() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(56999, ""));
        let mut cfg = AppConfig::default();
        cfg.general.suppress_fullscreen_d3d = false;

        let mut rule = Rule::default_template(0);
        rule.id = "rule-period-global-snooze".to_string();
        rule.trigger = TriggerType::Period;
        rule.period = DurationConfig::new(50, TimeUnit::Min);
        rule.warn_before = DurationConfig::new(5, TimeUnit::Min);
        rule.reset_after_afk = DurationConfig::new(5, TimeUnit::Min);
        rule.schedule = RuleSchedule::from_bool_array(&[true; 168]);

        cfg.rules = vec![rule.clone()];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        // Set warned_current_cycle to true
        engine.runtimes.get_mut("rule-period-global-snooze").unwrap().warned_current_cycle = true;

        // Engage global snooze for 15 minutes
        engine.handle_command(DaemonCommand::SnoozeAll { minutes: 15 }).await;
        assert!(engine.global_snoozed_until.is_some());

        // Run poll_tick (no events from client mock -> AFK gap / empty intervals)
        engine.poll_tick().await;

        let rt = engine.runtimes.get("rule-period-global-snooze").unwrap();
        assert_eq!(rt.current_session_seconds, 0, "AFK during global snooze should reset current_session_seconds to 0");
        assert_eq!(rt.warned_current_cycle, false, "AFK during global snooze should reset warned_current_cycle");
    }

    // ========================================================================
    // System Tray Icon Tri-State Unit Tests
    // ========================================================================

    #[test]
    fn test_tray_icon_assets_integrity() {
        let normal_bytes = include_bytes!("../assets/tray_icon_32.rgba");
        let paused_bytes = include_bytes!("../assets/tray_icon_paused_32.rgba");
        let alert_bytes = include_bytes!("../assets/tray_icon_alert_32.rgba");

        assert_eq!(normal_bytes.len(), 4096);
        assert_eq!(paused_bytes.len(), 4096);
        assert_eq!(alert_bytes.len(), 4096);

        for i in (0..4096).step_by(4) {
            let a = normal_bytes[i + 3];
            assert_eq!(paused_bytes[i + 3], a);
            assert_eq!(alert_bytes[i + 3], a);

            if a > 0 {
                assert_eq!(paused_bytes[i], 148);
                assert_eq!(paused_bytes[i + 1], 163);
                assert_eq!(paused_bytes[i + 2], 184);

                assert_eq!(alert_bytes[i], 239);
                assert_eq!(alert_bytes[i + 1], 68);
                assert_eq!(alert_bytes[i + 2], 68);
            }
        }
    }

    #[tokio::test]
    async fn test_tray_state_normal_during_active_monitoring() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();
        let mut rule = Rule::default_template(0);
        rule.id = "rule-normal".to_string();
        rule.schedule = RuleSchedule::from_bool_array(&[true; 168]);
        cfg.rules = vec![rule];

        let engine = Engine::new(cfg, aw_client, overlay_tx);
        assert_eq!(engine.current_tray_state(Local::now()), TrayIconState::Normal);
    }

    #[tokio::test]
    async fn test_tray_state_paused_overrides_alerts_and_breaks() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();
        cfg.general.pause_all_rules = true;
        let mut rule = Rule::default_template(0);
        rule.id = "rule-paused".to_string();
        rule.schedule = RuleSchedule::from_bool_array(&[true; 168]);
        cfg.rules = vec![rule];

        let mut engine = Engine::new(cfg, aw_client, overlay_tx);
        engine.runtimes.get_mut("rule-paused").unwrap().active_break = Some((60, 60));
        engine.runtimes.get_mut("rule-paused").unwrap().warned_current_cycle = true;

        assert_eq!(engine.current_tray_state(Local::now()), TrayIconState::Paused);
    }

    #[tokio::test]
    async fn test_tray_state_alert_during_warning_period() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();
        let mut rule = Rule::default_template(0);
        rule.id = "rule-warn".to_string();
        rule.schedule = RuleSchedule::from_bool_array(&[true; 168]);
        cfg.rules = vec![rule];

        let mut engine = Engine::new(cfg, aw_client, overlay_tx);
        engine.runtimes.get_mut("rule-warn").unwrap().warned_current_cycle = true;

        assert_eq!(engine.current_tray_state(Local::now()), TrayIconState::Alert);
    }

    #[tokio::test]
    async fn test_tray_state_alert_during_grace_period() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();
        let mut rule = Rule::default_template(0);
        rule.id = "rule-grace".to_string();
        rule.schedule = RuleSchedule::from_bool_array(&[true; 168]);
        cfg.rules = vec![rule];

        let mut engine = Engine::new(cfg, aw_client, overlay_tx);
        engine.runtimes.get_mut("rule-grace").unwrap().grace_countdown = Some((30, 30));

        assert_eq!(engine.current_tray_state(Local::now()), TrayIconState::Alert);
    }

    #[tokio::test]
    async fn test_tray_state_alert_during_active_break() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();
        let mut rule = Rule::default_template(0);
        rule.id = "rule-break".to_string();
        rule.schedule = RuleSchedule::from_bool_array(&[true; 168]);
        cfg.rules = vec![rule];

        let mut engine = Engine::new(cfg, aw_client, overlay_tx);
        engine.runtimes.get_mut("rule-break").unwrap().active_break = Some((120, 120));

        assert_eq!(engine.current_tray_state(Local::now()), TrayIconState::Alert);
    }

    #[tokio::test]
    async fn test_tray_state_alert_during_pending_break() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();
        let mut rule = Rule::default_template(0);
        rule.id = "rule-pending".to_string();
        rule.schedule = RuleSchedule::from_bool_array(&[true; 168]);
        cfg.rules = vec![rule];

        let mut engine = Engine::new(cfg, aw_client, overlay_tx);
        engine.runtimes.get_mut("rule-pending").unwrap().pending_break_since = Some(Local::now());

        assert_eq!(engine.current_tray_state(Local::now()), TrayIconState::Alert);
    }

    #[tokio::test]
    async fn test_tray_state_normal_when_rule_snoozed() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();
        let mut rule = Rule::default_template(0);
        rule.id = "rule-snoozed".to_string();
        rule.schedule = RuleSchedule::from_bool_array(&[true; 168]);
        cfg.rules = vec![rule];

        let mut engine = Engine::new(cfg, aw_client, overlay_tx);
        let now = Local::now();
        let rt = engine.runtimes.get_mut("rule-snoozed").unwrap();
        rt.warned_current_cycle = true;
        rt.snoozed_until = Some(now + Duration::minutes(15));

        assert_eq!(engine.current_tray_state(now), TrayIconState::Normal);
    }

    #[tokio::test]
    async fn test_tray_state_normal_when_off_schedule() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();
        let mut rule = Rule::default_template(0);
        rule.id = "rule-off-sched".to_string();
        rule.schedule = RuleSchedule::from_bool_array(&[false; 168]);
        cfg.rules = vec![rule];

        let mut engine = Engine::new(cfg, aw_client, overlay_tx);
        let now = Local::now();
        engine.runtimes.get_mut("rule-off-sched").unwrap().warned_current_cycle = true;

        assert_eq!(engine.current_tray_state(now), TrayIconState::Normal);
    }

    #[tokio::test]
    async fn test_update_live_status_emits_tray_state() {
        let (overlay_tx, mut overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();
        let mut rule = Rule::default_template(0);
        rule.id = "rule-live-tray".to_string();
        rule.schedule = RuleSchedule::from_bool_array(&[true; 168]);
        cfg.rules = vec![rule];

        let mut engine = Engine::new(cfg, aw_client, overlay_tx);
        engine.poll_tick().await;

        let mut last_tray_state = None;
        while let Ok(action) = overlay_rx.try_recv() {
            if let OverlayAction::UpdateLiveStatus { tray_state, .. } = action {
                last_tray_state = Some(tray_state);
            }
        }

        assert_eq!(last_tray_state, Some(TrayIconState::Normal));

        // Now pause all rules and verify tray state transitions to Paused
        engine.handle_command(DaemonCommand::SetPauseAll { paused: true }).await;

        let mut paused_tray_state = None;
        while let Ok(action) = overlay_rx.try_recv() {
            if let OverlayAction::UpdateLiveStatus { tray_state, .. } = action {
                paused_tray_state = Some(tray_state);
            }
        }

        assert_eq!(paused_tray_state, Some(TrayIconState::Paused));
    }

    #[tokio::test]
    async fn test_update_live_status_emits_histogram_telemetry() {
        let (overlay_tx, mut overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(56999, ""));
        let mut cfg = AppConfig::default();
        cfg.stats.histogram_bin_count = 24;
        cfg.stats.daily_screentime_target_minutes = 480;

        let mut engine = Engine::new(cfg, aw_client, overlay_tx);
        engine.poll_tick().await;

        let mut received_status = None;
        while let Ok(action) = overlay_rx.try_recv() {
            if let OverlayAction::UpdateLiveStatus {
                status_level,
                target_minutes,
                histogram_values,
                histogram_labels,
                is_loading,
                ..
            } = action
            {
                received_status = Some((
                    status_level,
                    target_minutes,
                    histogram_values,
                    histogram_labels,
                    is_loading,
                ));
            }
        }

        let (status_level, target_minutes, values, labels, is_loading) =
            received_status.expect("UpdateLiveStatus must be emitted");
        assert_eq!(target_minutes, 480);
        assert!(!is_loading);
        assert!(matches!(status_level.as_str(), "Great" | "Okay" | "Excessive"));
        assert_eq!(values.len(), 24);
        assert_eq!(labels.len(), 24);
    }
}
