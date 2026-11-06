use chrono::{DateTime, Local};
use regex::Regex;

use crate::aw_client::AwEvent;
use crate::config::{MonitorSelection, Rule};
use crate::engine::matcher::extract_url_host;

/// Ephemeral runtime state tracked per configured `Rule` across polling and countdown ticks.
#[derive(Debug, Clone)]
pub struct RuleRuntime {
    pub compiled_regex: Option<Regex>,
    pub current_session_seconds: u32,
    pub daily_usage_seconds: u32,
    pub daily_reset_at: Option<DateTime<Local>>,
    pub last_completed_at: Option<DateTime<Local>>,
    pub prev_completed_at: Option<DateTime<Local>>,
    pub can_rollback_completion: bool,
    pub warned_current_cycle: bool,
    pub pending_break_since: Option<DateTime<Local>>,
    pub active_break: Option<(u32, u32)>,    // (remaining_secs, total_secs)
    pub grace_countdown: Option<(u32, u32)>, // (remaining_secs, total_secs)
    pub grace_dismissed: bool,
    pub action_executed_today: bool,
    pub snoozed_until: Option<DateTime<Local>>,
    pub live_status_badge: String,
    pub last_matched_app: Option<String>,
    pub last_matched_url: Option<String>,
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
            daily_reset_at: None,
            last_completed_at: None,
            prev_completed_at: None,
            can_rollback_completion: false,
            warned_current_cycle: false,
            pending_break_since: None,
            active_break: None,
            grace_countdown: None,
            grace_dismissed: false,
            action_executed_today: false,
            snoozed_until: None,
            live_status_badge: String::new(),
            last_matched_app: None,
            last_matched_url: None,
            last_matched_title: None,
            last_matched_title_regex: None,
        }
    }

    /// Sets `last_matched_title` and updates `last_matched_title_regex`.
    /// Normalizes empty/whitespace titles to `None` and avoids recompiling regex if unchanged.
    pub fn set_last_matched_title(&mut self, title: Option<String>) {
        let normalized = title.filter(|t| !t.trim().is_empty());
        if self.last_matched_title == normalized {
            return;
        }
        self.last_matched_title_regex = normalized
            .as_deref()
            .and_then(|t| Regex::new(&regex::escape(t)).ok());
        self.last_matched_title = normalized;
    }

    /// Clears all matched window/tab metadata and cached title regex.
    pub fn clear_matched_metadata(&mut self) {
        self.last_matched_app = None;
        self.last_matched_url = None;
        self.set_last_matched_title(None);
        self.last_matched_title_regex = None;
    }

    /// Resets all state for a rule on a new logical day rollover.
    pub fn reset_for_new_day(&mut self) {
        self.warned_current_cycle = false;
        self.action_executed_today = false;
        self.grace_countdown = None;
        self.grace_dismissed = false;
        self.snoozed_until = None;
        self.pending_break_since = None;
        self.daily_usage_seconds = 0;
        self.daily_reset_at = None;
        self.can_rollback_completion = false;
        self.clear_matched_metadata();
    }

    /// Updates matched metadata (`last_matched_url`, `last_matched_app`, and `last_matched_title`)
    /// from an ActivityWatch event, or clears them if `ev` is `None` for a URL rule.
    pub fn update_matched_metadata(&mut self, ev: Option<&AwEvent>, is_urls_rule: bool) {
        if let Some(ev) = ev {
            if is_urls_rule {
                self.last_matched_url = ev.data.url.clone();
                if let Some(url_str) = ev.data.url.as_deref() {
                    self.last_matched_app = Some(extract_url_host(url_str));
                }
            } else if let Some(ref app) = ev.data.app {
                self.last_matched_app = Some(app.clone());
            }
            self.set_last_matched_title(ev.data.title.clone());
        } else if is_urls_rule {
            self.clear_matched_metadata();
        }
    }

    /// Clears active break, grace countdown, pending break, and warning flags for the current cycle.
    pub fn reset_active_timers(&mut self) {
        if self.active_break.is_some() {
            self.action_executed_today = false;
            self.can_rollback_completion = false;
        }
        self.current_session_seconds = 0;
        self.active_break = None;
        self.grace_countdown = None;
        self.grace_dismissed = false;
        self.warned_current_cycle = false;
        self.pending_break_since = None;
    }

    /// Marks a rule cycle as completed at `now`, resetting timers and recording execution.
    pub fn complete_cycle(&mut self, now: DateTime<Local>) {
        self.reset_active_timers();
        self.grace_dismissed = false;
        self.snoozed_until = None;
        self.prev_completed_at = self.last_completed_at;
        self.last_completed_at = Some(now);
        self.can_rollback_completion = false;
        self.action_executed_today = true;
        self.clear_matched_metadata();
    }

    /// Resets the rule's active session or daily quota, clearing timers, snooze, and daily limit locks.
    pub fn reset_session(&mut self, now: DateTime<Local>) {
        self.reset_active_timers();
        self.grace_dismissed = false;
        self.snoozed_until = None;
        self.prev_completed_at = Some(now);
        self.last_completed_at = Some(now);
        self.daily_reset_at = Some(now);
        self.can_rollback_completion = false;
        self.action_executed_today = false;
        self.daily_usage_seconds = 0;
        self.clear_matched_metadata();
    }

    /// Snoozes the rule until `until`. For `Period` rules, preserves `last_completed_at` if before
    /// the current session so the snoozed rule re-triggers once `until` expires instead of wiping
    /// the pre-snooze session.
    pub fn snooze(&mut self, until: DateTime<Local>) {
        let was_enforcing = self.active_break.is_some()
            || self.pending_break_since.is_some()
            || self.grace_countdown.is_some()
            || self.can_rollback_completion;
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
        if was_enforcing {
            self.action_executed_today = false;
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use crate::aw_client::AwEventData;

    #[test]
    fn test_set_last_matched_title_deduplication_and_whitespace_clearing() {
        let rule = Rule::default_template(0);
        let mut rt = RuleRuntime::from_rule(&rule);

        // 1. Initial state is None
        assert!(rt.last_matched_title.is_none());
        assert!(rt.last_matched_title_regex.is_none());

        // 2. Setting whitespace-only title normalizes to None without compiling regex
        rt.set_last_matched_title(Some("   \t\n  ".to_string()));
        assert!(rt.last_matched_title.is_none());
        assert!(rt.last_matched_title_regex.is_none());

        // 3. Setting a valid title compiles regex and stores normalized title
        rt.set_last_matched_title(Some("My Document - Editor".to_string()));
        assert_eq!(rt.last_matched_title.as_deref(), Some("My Document - Editor"));
        assert!(rt.last_matched_title_regex.is_some());
        assert!(rt.last_matched_title_regex.as_ref().unwrap().is_match("My Document - Editor"));

        // 4. Setting identical title preserves existing regex reference without re-compiling
        let re_ptr = rt.last_matched_title_regex.as_ref().unwrap().as_str().to_string();
        rt.set_last_matched_title(Some("My Document - Editor".to_string()));
        assert_eq!(rt.last_matched_title.as_deref(), Some("My Document - Editor"));
        assert_eq!(rt.last_matched_title_regex.as_ref().unwrap().as_str(), re_ptr);

        // 5. Setting empty string clears title and regex
        rt.set_last_matched_title(Some("".to_string()));
        assert!(rt.last_matched_title.is_none());
        assert!(rt.last_matched_title_regex.is_none());

        // 6. clear_matched_metadata clears app, url, title, and cached regex
        rt.last_matched_app = Some("chrome.exe".to_string());
        rt.last_matched_url = Some("https://example.com".to_string());
        rt.set_last_matched_title(Some("Example Page".to_string()));
        assert!(rt.last_matched_title_regex.is_some());
        rt.clear_matched_metadata();
        assert!(rt.last_matched_app.is_none());
        assert!(rt.last_matched_url.is_none());
        assert!(rt.last_matched_title.is_none());
        assert!(rt.last_matched_title_regex.is_none());
    }

    #[test]
    fn test_rule_runtime_initialization_and_regex_compilation() {
        let mut rule = Rule::default_template(0);
        rule.monitor_selection = MonitorSelection::Titles;
        rule.title_regex = "(?i)work".to_string();

        let mut rt = RuleRuntime::from_rule(&rule);
        assert!(rt.compiled_regex.is_some());
        assert!(rt.compiled_regex.as_ref().unwrap().is_match("Workplace Chat"));
        assert!(!rt.compiled_regex.as_ref().unwrap().is_match("Gaming"));

        // Update regex
        rule.title_regex = "^Slack$".to_string();
        rt.update_from_rule(&rule);
        assert!(rt.compiled_regex.as_ref().unwrap().is_match("Slack"));
        assert!(!rt.compiled_regex.as_ref().unwrap().is_match("Slack App"));

        // URL rule regex compilation
        let mut url_rule = Rule::default_template(1);
        url_rule.monitor_selection = MonitorSelection::Urls;
        url_rule.url_regex = r"https://github\.com/.*".to_string();
        let url_rt = RuleRuntime::from_rule(&url_rule);
        assert!(url_rt.compiled_regex.is_some());
        assert!(url_rt.compiled_regex.as_ref().unwrap().is_match("https://github.com/rust-lang/rust"));
    }

    #[test]
    fn test_rule_runtime_update_matched_metadata() {
        let mut rt = RuleRuntime::from_rule(&Rule::default_template(0));

        // Window event with app and title
        let win_ev = AwEvent {
            timestamp: Local::now().to_utc(),
            duration: 10.0,
            data: AwEventData {
                app: Some("code.exe".to_string()),
                title: Some("main.rs - Project".to_string()),
                url: None,
                status: None,
            },
        };
        rt.update_matched_metadata(Some(&win_ev), false);
        assert_eq!(rt.last_matched_app.as_deref(), Some("code.exe"));
        assert_eq!(rt.last_matched_title.as_deref(), Some("main.rs - Project"));
        assert!(rt.last_matched_url.is_none());

        // Web event with url and title for a URL rule
        let web_ev = AwEvent {
            timestamp: Local::now().to_utc(),
            duration: 15.0,
            data: AwEventData {
                app: None,
                title: Some("Documentation - Rust".to_string()),
                url: Some("https://doc.rust-lang.org/book/".to_string()),
                status: None,
            },
        };
        rt.update_matched_metadata(Some(&web_ev), true);
        assert_eq!(rt.last_matched_app.as_deref(), Some("doc.rust-lang.org"));
        assert_eq!(rt.last_matched_url.as_deref(), Some("https://doc.rust-lang.org/book/"));
        assert_eq!(rt.last_matched_title.as_deref(), Some("Documentation - Rust"));

        // If web event is None for a URL rule, clears matched metadata
        rt.update_matched_metadata(None, true);
        assert!(rt.last_matched_app.is_none());
        assert!(rt.last_matched_url.is_none());
        assert!(rt.last_matched_title.is_none());
    }

    #[test]
    fn test_rule_runtime_lifecycle_resets_and_snooze() {
        let rule = Rule::default_template(0);
        let mut rt = RuleRuntime::from_rule(&rule);
        let now = Local::now();

        // 1. Complete cycle sets action_executed_today and last_completed_at
        rt.current_session_seconds = 1800;
        rt.active_break = Some((300, 300));
        rt.warned_current_cycle = true;
        rt.complete_cycle(now);

        assert_eq!(rt.current_session_seconds, 0);
        assert!(rt.active_break.is_none());
        assert!(!rt.warned_current_cycle);
        assert!(rt.action_executed_today);
        assert_eq!(rt.last_completed_at, Some(now));

        // 2. Snooze while idle preserves action_executed_today
        let until = now + Duration::minutes(5);
        rt.snooze(until);
        assert!(rt.action_executed_today);
        assert_eq!(rt.snoozed_until, Some(until));

        // 3. Snooze while enforcing resets action_executed_today
        rt.active_break = Some((100, 300));
        rt.snooze(until + Duration::minutes(5));
        assert!(!rt.action_executed_today);
        assert!(rt.active_break.is_none());

        // 4. Snooze with rollback restores prev_completed_at
        let prev_time = now - Duration::hours(1);
        rt.prev_completed_at = Some(prev_time);
        rt.last_completed_at = Some(now);
        rt.can_rollback_completion = true;
        rt.snooze(until + Duration::minutes(10));
        assert_eq!(rt.last_completed_at, Some(prev_time));
        assert!(!rt.can_rollback_completion);

        // 5. Reset session clears daily usage and action executed flag
        rt.daily_usage_seconds = 3600;
        rt.action_executed_today = true;
        rt.reset_session(now);
        assert_eq!(rt.daily_usage_seconds, 0);
        assert!(!rt.action_executed_today);
        assert_eq!(rt.daily_reset_at, Some(now));

        // 6. Reset for new day resets usage, warning, snooze, and metadata
        rt.warned_current_cycle = true;
        rt.snoozed_until = Some(until);
        rt.daily_usage_seconds = 500;
        rt.set_last_matched_title(Some("Test Title".to_string()));
        rt.reset_for_new_day();
        assert!(!rt.warned_current_cycle);
        assert!(rt.snoozed_until.is_none());
        assert_eq!(rt.daily_usage_seconds, 0);
        assert!(rt.last_matched_title.is_none());
    }
}
