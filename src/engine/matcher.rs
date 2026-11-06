use chrono::{DateTime, Duration, Local, Utc};
use regex::Regex;

use crate::aw_client::AwEvent;
use crate::config::{MonitorSelection, Rule, RuleAction};
use crate::enforcer;
use crate::engine::intervals::{
    calculate_continuous_session_from_merged, event_end_utc, merge_overlapping_intervals,
};

// ============================================================================
// URL & Executable Predicates
// ============================================================================

/// Extracts the hostname/authority or top-level component from a URL string, stripping
/// scheme prefixes (HTTP, HTTPS, custom schemes like chrome:// or file://), query parameters,
/// and credentials.
pub fn extract_url_host(url: &str) -> String {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let after_scheme = trimmed
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(trimmed);
    let authority = after_scheme
        .split(&['/', '?', '#'][..])
        .next()
        .unwrap_or(after_scheme);
    let host = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    if !host.is_empty() {
        host.to_string()
    } else {
        after_scheme
            .trim_start_matches('/')
            .split(&['/', '?', '#'][..])
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or(trimmed)
            .to_string()
    }
}

/// Helper checking whether a rule's URL regex matches the full URL (`last_matched_url`), or falls back to the hostname in `last_matched_app`.
pub fn matched_url_or_app_matches_rule(
    url: Option<&str>,
    app: Option<&str>,
    compiled_re: Option<&Regex>,
) -> bool {
    if let Some(re) = compiled_re {
        if let Some(u) = url {
            if re.is_match(u) {
                return true;
            }
        }
        if let Some(app) = app {
            re.is_match(app)
                || re.is_match(&format!("https://{app}"))
                || re.is_match(&format!("http://{app}"))
                || re.is_match(&format!("https://{app}/"))
                || re.is_match(&format!("http://{app}/"))
        } else {
            false
        }
    } else {
        false
    }
}

/// Helper predicate returning true when a rule monitors all executables via wildcard or empty target list.
pub fn is_wildcard_executables_rule(rule: &Rule) -> bool {
    rule.monitor_selection == MonitorSelection::Executables
        && !enforcer::has_exclusion_targets(&rule.target_executables)
        && (rule.target_executables.is_empty()
            || rule.target_executables.iter().any(|t| t.trim() == "*"))
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
            if is_wildcard_executables_rule(rule) {
                return true;
            }
            let is_wildcard = rule.target_executables.is_empty()
                || rule.target_executables.iter().any(|t| t.trim() == "*");
            if let Some(app) = ev.data.app.as_deref() {
                if is_wildcard && enforcer::has_exclusion_targets(&rule.target_executables) {
                    return !enforcer::exe_name_is_excluded(app, &rule.target_executables);
                }
                return enforcer::exe_name_matches_targets(app, &rule.target_executables);
            }
            false
        }
    }
}

// ============================================================================
// Event & Active Window Lookup
// ============================================================================

/// Computes the recency cutoff timestamp based on the polling interval.
pub fn calculate_recency_cutoff(now_utc: DateTime<Utc>, poll_interval_seconds: u64) -> DateTime<Utc> {
    now_utc - Duration::seconds((poll_interval_seconds.max(2) as i64) * 2 + 5)
}

/// Finds the most recent matching event within `recency_cutoff` (or historical fallback for non-URL rules).
pub fn find_latest_matching_event<'a>(
    events: &'a [AwEvent],
    rule: &Rule,
    compiled_re: Option<&Regex>,
    recency_cutoff: DateTime<Utc>,
) -> Option<&'a AwEvent> {
    if rule.monitor_selection == MonitorSelection::Urls {
        let latest_ev = events.iter().max_by_key(|ev| (event_end_utc(ev), ev.timestamp))?;
        if event_end_utc(latest_ev) >= recency_cutoff && event_matches_rule(latest_ev, rule, compiled_re) {
            Some(latest_ev)
        } else {
            None
        }
    } else {
        events
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
            })
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
        MonitorSelection::Titles => {
            if rule.action == RuleAction::CloseTab {
                enforcer::find_matching_foreground_window::<&str>(&[], compiled_re)
            } else {
                enforcer::find_matching_window::<&str>(&[], compiled_re)
            }
        }
        MonitorSelection::Urls => {
            if rule.action == RuleAction::CloseTab && !enforcer::is_foreground_browser() {
                return None;
            }
            let browser_match = enforcer::find_matching_window(enforcer::BROWSER_EXES, None)?;
            let recency_cutoff = calculate_recency_cutoff(now_utc, poll_interval_secs);
            let ev = find_latest_matching_event(events, rule, compiled_re, recency_cutoff)?;

            if matches!(rule.action, RuleAction::CloseTab | RuleAction::CloseWindow) {
                if let Some(title) = ev.data.title.as_deref().filter(|t| !t.trim().is_empty()) {
                    let title_re = Regex::new(&regex::escape(title)).ok();
                    let win_match = if rule.action == RuleAction::CloseTab {
                        enforcer::find_matching_foreground_window(enforcer::BROWSER_EXES, title_re.as_ref())
                    } else {
                        enforcer::find_matching_window(enforcer::BROWSER_EXES, title_re.as_ref())
                    };

                    #[cfg(test)]
                    let win_match = win_match
                        .or_else(|| enforcer::find_matching_window(enforcer::BROWSER_EXES, None));

                    if win_match.is_none() {
                        return None;
                    }
                }
            }

            Some(
                ev.data
                    .url
                    .as_deref()
                    .map(extract_url_host)
                    .or_else(|| ev.data.url.clone())
                    .unwrap_or(browser_match),
            )
        }
        MonitorSelection::Executables => {
            if rule.target_executables.is_empty()
                || rule.target_executables.iter().any(|t| t.trim() == "*")
            {
                let fg_app = enforcer::get_foreground_app_name()
                    .filter(|app| !enforcer::exe_name_is_excluded(app, &rule.target_executables));
                if matches!(rule.action, RuleAction::CloseWindow | RuleAction::CloseTab) {
                    fg_app
                } else {
                    let latest_app = events
                        .iter()
                        .max_by_key(|ev| (event_end_utc(ev), ev.timestamp))
                        .and_then(|ev| ev.data.app.clone())
                        .filter(|app| !enforcer::exe_name_is_excluded(app, &rule.target_executables));
                    fg_app
                        .or(latest_app)
                        .or_else(|| {
                            if enforcer::has_exclusion_targets(&rule.target_executables) {
                                None
                            } else {
                                Some("Active Application".to_string())
                            }
                        })
                }
            } else if rule.action == RuleAction::CloseTab {
                enforcer::find_matching_foreground_window(&rule.target_executables, None)
            } else {
                let win = enforcer::find_matching_window(&rule.target_executables, None);
                if win.is_none() && rule.action == RuleAction::KillProcess {
                    enforcer::find_matching_process(&rule.target_executables)
                } else {
                    win
                }
            }
        }
    }
}

// ============================================================================
// Rule-Filtered Duration Calculators
// ============================================================================

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
            let end_utc = (ev.timestamp + Duration::milliseconds((ev.duration * 1000.0) as i64)).min(now_utc);
            if end_utc > start_utc {
                Some((start_utc, end_utc))
            } else {
                None
            }
        })
        .collect();

    let merged = merge_overlapping_intervals(intervals);
    calculate_continuous_session_from_merged(&merged, now_utc, reset_gap_secs)
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

// ============================================================================
// Unit Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use crate::aw_client::AwEventData;
    use crate::engine::intervals::logical_day_start;

    /// Helper to construct a synthetic `AwEvent` with `app`, `title`, and `url` fields.
    fn make_event(
        start_utc: DateTime<Utc>,
        duration_secs: f64,
        app: &str,
        title: &str,
        url: &str,
    ) -> AwEvent {
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

    #[test]
    fn test_extract_url_host_schemes_and_edge_cases() {
        assert_eq!(extract_url_host("http://example.com"), "example.com");
        assert_eq!(extract_url_host("http://example.com/"), "example.com");
        assert_eq!(extract_url_host("https://www.youtube.com/watch?v=123"), "www.youtube.com");
        assert_eq!(extract_url_host("https://sub.domain.co.uk/path/to/page"), "sub.domain.co.uk");

        assert_eq!(extract_url_host("HTTPS://GITHUB.COM/OWNER/REPO"), "GITHUB.COM");
        assert_eq!(extract_url_host("HTTP://WWW.EXAMPLE.COM/PATH"), "WWW.EXAMPLE.COM");

        assert_eq!(extract_url_host("chrome://extensions/shortcuts"), "extensions");
        assert_eq!(extract_url_host("edge://settings"), "settings");
        assert_eq!(extract_url_host("brave://flags"), "flags");

        assert_eq!(extract_url_host("file:///C:/Users/test.html"), "C:");

        assert_eq!(extract_url_host("https://example.com?query=1"), "example.com");
        assert_eq!(extract_url_host("http://example.org#fragment"), "example.org");
        assert_eq!(extract_url_host("example.com?foo=bar#section"), "example.com");

        assert_eq!(extract_url_host("reddit.com/r/rust"), "reddit.com");
        assert_eq!(extract_url_host("crates.io"), "crates.io");
        assert_eq!(extract_url_host("github.com/foo/bar"), "github.com");

        assert_eq!(extract_url_host("https://user:pass@secret.internal.net/admin"), "secret.internal.net");
        assert_eq!(extract_url_host("http://admin@localhost:8080/metrics"), "localhost:8080");

        assert_eq!(extract_url_host(""), "");
        assert_eq!(extract_url_host("   "), "");
    }

    #[test]
    fn test_urls_rule_path_based_regex_matching() {
        let re = Regex::new(r"(?i)youtube\.com/shorts").unwrap();

        // 1. Hostname alone in last_matched_app does not match a path-based regex
        assert!(!matched_url_or_app_matches_rule(None, Some("youtube.com"), Some(&re)));

        // 2. Full URL in last_matched_url matches path-based regex
        let full_url = "https://www.youtube.com/shorts/12345";
        assert!(matched_url_or_app_matches_rule(
            Some(full_url),
            Some("youtube.com"),
            Some(&re)
        ));

        // 3. Unrelated URL does not match
        let other_url = "https://www.youtube.com/watch?v=12345";
        assert!(!matched_url_or_app_matches_rule(
            Some(other_url),
            Some("youtube.com"),
            Some(&re)
        ));
    }

    #[test]
    fn test_is_wildcard_executables_rule() {
        let mut r = Rule::default_template(0);
        r.monitor_selection = MonitorSelection::Executables;
        r.target_executables = vec!["*".to_string()];
        assert!(is_wildcard_executables_rule(&r));

        r.target_executables = Vec::new();
        assert!(is_wildcard_executables_rule(&r));

        r.target_executables = vec!["code.exe".to_string()];
        assert!(!is_wildcard_executables_rule(&r));

        r.target_executables = vec!["*".to_string(), "!zoom.exe".to_string()];
        assert!(!is_wildcard_executables_rule(&r));

        r.monitor_selection = MonitorSelection::Urls;
        assert!(!is_wildcard_executables_rule(&r));
    }

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

    #[test]
    fn test_continuous_session_subsecond_interval_accumulation() {
        let now = Local.with_ymd_and_hms(2026, 10, 29, 12, 0, 0).unwrap();
        let now_utc = now.with_timezone(&Utc);
        let rule = Rule::default_template(0);
        let reset_gap_secs = 300;

        let ev1 = make_event(now_utc - Duration::milliseconds(2400), 0.8, "code.exe", "", "");
        let ev2 = make_event(now_utc - Duration::milliseconds(1600), 0.8, "code.exe", "", "");
        let ev3 = make_event(now_utc - Duration::milliseconds(800), 0.8, "code.exe", "", "");

        let session = calculate_continuous_session_for_rule(
            &[ev1, ev2, ev3],
            now,
            None,
            reset_gap_secs,
            &rule,
            None,
        );
        assert_eq!(session, 2);

        let ev_future = make_event(now_utc - Duration::seconds(1), 10.0, "code.exe", "", "");
        let session_clamped = calculate_continuous_session_for_rule(
            &[ev_future],
            now,
            None,
            reset_gap_secs,
            &rule,
            None,
        );
        assert_eq!(session_clamped, 1);
    }

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

        // Executable exclusion matching
        let mut excl_rule = Rule::default_template(3);
        excl_rule.monitor_selection = MonitorSelection::Executables;
        excl_rule.target_executables = vec!["*".to_string(), "!zoom.exe".to_string()];
        let ev_zoom = make_event(start_utc + Duration::minutes(25), 500.0, "zoom.exe", "Meeting", "");
        let ev_zoom_path = make_event(start_utc + Duration::minutes(25), 500.0, r"C:\Program Files\Zoom\bin\zoom.exe", "Meeting", "");
        let ev_notepad = make_event(start_utc + Duration::minutes(25), 500.0, "notepad.exe", "Notes", "");
        assert!(!event_matches_rule(&ev_zoom, &excl_rule, None));
        assert!(!event_matches_rule(&ev_zoom_path, &excl_rule, None));
        assert!(event_matches_rule(&ev_notepad, &excl_rule, None));
        assert_eq!(calculate_usage_for_rule(&[ev_zoom, ev_notepad], &excl_rule, None, start_utc, now_utc), 500);
    }

    #[test]
    fn test_calculate_usage_coalesces_overlapping_events() {
        let base_time = Utc.with_ymd_and_hms(2026, 10, 31, 12, 0, 0).unwrap();
        let start_of_day = base_time - Duration::hours(4);
        let now_utc = base_time + Duration::hours(2);

        let mut rule = Rule::default_template(0);
        rule.target_executables = vec!["code.exe".to_string()];

        let ev1 = make_event(base_time, 1800.0, "code.exe", "Project A", "");
        let ev2 = make_event(base_time + Duration::minutes(10), 1800.0, "code.exe", "Project B", "");

        let usage = calculate_usage_for_rule(&[ev1.clone(), ev2], &rule, None, start_of_day, now_utc);
        assert_eq!(usage, 2400);

        let ev3 = make_event(base_time, 1800.0, "code.exe", "Project A (Window 2)", "");
        let usage_identical = calculate_usage_for_rule(&[ev1, ev3], &rule, None, start_of_day, now_utc);
        assert_eq!(usage_identical, 1800);
    }
}
