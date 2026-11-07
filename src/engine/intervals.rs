use chrono::{DateTime, Duration, Local, NaiveDateTime, Timelike, Utc};

use crate::aw_client::AwEvent;
use crate::enforcer;

// ============================================================================
// Interval Coalescing & Overlap Calculation
// ============================================================================

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

/// Converts a slice of `AwEvent`s into a merged, non-overlapping sorted list of `(start, end)` intervals.
pub fn events_to_merged_intervals(events: &[AwEvent]) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    let raw: Vec<(DateTime<Utc>, DateTime<Utc>)> = events
        .iter()
        .filter_map(|ev| {
            let e_start = ev.timestamp;
            let e_end = ev.timestamp + Duration::milliseconds((ev.duration * 1000.0) as i64);
            if e_end > e_start {
                Some((e_start, e_end))
            } else {
                None
            }
        })
        .collect();
    merge_overlapping_intervals(raw)
}

/// Computes the total non-overlapping duration in seconds between a pre-merged sorted slice of
/// `(DateTime<Utc>, DateTime<Utc>)` intervals and `[start_utc, end_utc]` without allocating or sorting.
pub fn compute_overlap_from_merged_intervals(
    merged_intervals: &[(DateTime<Utc>, DateTime<Utc>)],
    start_utc: DateTime<Utc>,
    end_utc: DateTime<Utc>,
) -> f64 {
    if start_utc >= end_utc || merged_intervals.is_empty() {
        return 0.0;
    }

    let start_idx = merged_intervals.partition_point(|&(_, i_end)| i_end <= start_utc);
    let mut total_ms = 0i64;
    for (i_start, i_end) in &merged_intervals[start_idx..] {
        if *i_start >= end_utc {
            break;
        }
        let s = (*i_start).max(start_utc);
        let e = (*i_end).min(end_utc);
        if e > s {
            total_ms += (e - s).num_milliseconds();
        }
    }
    total_ms as f64 / 1000.0
}

/// Intersects web events from `aw-watcher-web` with active browser window intervals from `window_events` (`aw-watcher-window`).
///
/// Prevents background/unfocused browser windows from accumulating URL usage while the user is actively working in non-browser applications.
pub fn filter_web_events_by_active_browser_windows(
    web_events: &[AwEvent],
    window_events: &[AwEvent],
) -> Vec<AwEvent> {
    if window_events.is_empty() || web_events.is_empty() {
        return web_events.to_vec();
    }

    let raw_browser_intervals: Vec<(DateTime<Utc>, DateTime<Utc>)> = window_events
        .iter()
        .filter_map(|ev| {
            let app = ev.data.app.as_deref()?;
            if enforcer::exe_name_matches_targets(app, enforcer::BROWSER_EXES) {
                let start = ev.timestamp;
                let end = event_end_utc(ev);
                if end > start {
                    Some((start, end))
                } else {
                    None
                }
            } else {
                None
            }
        })
        .collect();

    if raw_browser_intervals.is_empty() {
        return Vec::new();
    }

    let browser_intervals = merge_overlapping_intervals(raw_browser_intervals);

    let mut filtered = Vec::new();
    for web_ev in web_events {
        let w_start = web_ev.timestamp;
        let w_end = event_end_utc(web_ev);
        if w_end <= w_start {
            continue;
        }

        let start_idx = browser_intervals.partition_point(|&(_, b_end)| b_end <= w_start);
        for &(b_start, b_end) in &browser_intervals[start_idx..] {
            if b_start >= w_end {
                break;
            }
            let overlap_start = w_start.max(b_start);
            let overlap_end = w_end.min(b_end);
            if overlap_end > overlap_start {
                let duration = (overlap_end - overlap_start).num_milliseconds() as f64 / 1000.0;
                if duration > 0.0 {
                    let mut clipped_ev = web_ev.clone();
                    clipped_ev.timestamp = overlap_start;
                    clipped_ev.duration = duration;
                    filtered.push(clipped_ev);
                }
            }
        }
    }

    filtered
}

// ============================================================================
// Session Traversal Over Merged Slices
// ============================================================================

/// Computes continuous active session seconds walking backward from `now_utc` across a pre-merged interval slice with a given `reset_gap_secs`.
pub fn calculate_continuous_session_from_merged(
    merged: &[(DateTime<Utc>, DateTime<Utc>)],
    now_utc: DateTime<Utc>,
    reset_gap_secs: u32,
) -> u32 {
    if merged.is_empty() {
        return 0;
    }

    let reset_gap_sec = reset_gap_secs as i64;
    let last_end = merged.last().unwrap().1;
    if (now_utc - last_end).num_seconds() >= reset_gap_sec {
        return 0;
    }

    let mut session_millis = 0i64;
    let mut prev_start: Option<DateTime<Utc>> = None;

    for (start, end) in merged.iter().rev() {
        if let Some(p_start) = prev_start {
            let gap = (p_start - *end).num_seconds();
            if gap >= reset_gap_sec {
                break;
            }
        }
        session_millis += (*end - *start).num_milliseconds();
        prev_start = Some(*start);
    }

    ((session_millis.max(0) as f64) / 1000.0).round() as u32
}

/// Computes continuous active session seconds from pre-merged intervals, clipped to an optional minimum start time and `now_utc`.
pub fn calculate_continuous_session_from_merged_clipped(
    merged: &[(DateTime<Utc>, DateTime<Utc>)],
    now_utc: DateTime<Utc>,
    min_allowed_utc: Option<DateTime<Utc>>,
    reset_gap_secs: u32,
) -> u32 {
    let reset_gap_sec = reset_gap_secs as i64;
    let mut session_millis = 0i64;
    let mut prev_start: Option<DateTime<Utc>> = None;

    for &(s, e) in merged.iter().rev() {
        if let Some(min_dt) = min_allowed_utc {
            if e <= min_dt {
                break;
            }
        }

        let end = e.min(now_utc);
        let start = min_allowed_utc.map_or(s, |m| s.max(m));
        if end <= start {
            continue;
        }

        if let Some(p_start) = prev_start {
            let gap = (p_start - end).num_seconds();
            if gap >= reset_gap_sec {
                break;
            }
        } else if (now_utc - end).num_seconds() >= reset_gap_sec {
            return 0;
        }

        session_millis += (end - start).num_milliseconds();
        prev_start = Some(start);
    }

    ((session_millis.max(0) as f64) / 1000.0).round() as u32
}

// ============================================================================
// Logical Day & Timestamp Utilities
// ============================================================================

/// Returns the end timestamp of an ActivityWatch event in UTC.
pub fn event_end_utc(ev: &AwEvent) -> DateTime<Utc> {
    ev.timestamp + Duration::milliseconds((ev.duration * 1000.0) as i64)
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
        .and_then(resolve_local_ndt)
        .unwrap_or(now)
}

/// Resolves a `NaiveDateTime` to local timezone, safely handling DST gaps (spring-forward) and ambiguities (fall-back).
pub fn resolve_local_ndt(ndt: NaiveDateTime) -> Option<DateTime<Local>> {
    ndt.and_local_timezone(Local)
        .earliest()
        .or_else(|| (ndt + Duration::hours(1)).and_local_timezone(Local).earliest())
}

// ============================================================================
// Unit Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, TimeZone};
    use crate::aw_client::AwEventData;

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
    fn test_compute_overlap_from_merged_intervals_coalesced_seconds() {
        let base_time = Utc.with_ymd_and_hms(2026, 11, 2, 10, 0, 0).unwrap();
        // Event 1: 10:00 - 10:45 (2700s)
        let ev1 = make_event(base_time, 2700.0, "code.exe", "File 1", "");
        // Event 2: 10:30 - 11:15 (2700s, overlapping with ev1)
        let ev2 = make_event(base_time + Duration::minutes(30), 2700.0, "code.exe", "File 2", "");
        // Event 3: 11:30 - 12:30 (3600s, separate)
        let ev3 = make_event(base_time + Duration::minutes(90), 3600.0, "code.exe", "File 3", "");
        let events = vec![ev1, ev2, ev3];

        let merged = events_to_merged_intervals(&events);

        let expected_seconds = [
            (-30, 0.0),
            (0, 1800.0),
            (30, 1800.0),
            (60, 900.0),
            (90, 1800.0),
            (120, 1800.0),
            (150, 0.0),
        ];

        for (offset_min, expected) in expected_seconds {
            let b_start = base_time + Duration::minutes(offset_min);
            let b_end = b_start + Duration::minutes(30);

            let actual = compute_overlap_from_merged_intervals(&merged, b_start, b_end);

            assert!(
                (actual - expected).abs() < 1e-4,
                "Mismatch at bin [{b_start}, {b_end}]: expected {expected}, got {actual}"
            );
        }
    }

    #[test]
    fn test_compute_overlap_from_merged_intervals_large_dataset_and_boundaries() {
        let base_time = Utc.with_ymd_and_hms(2026, 11, 2, 8, 0, 0).unwrap();
        // 100 intervals: [base + 20*i min, base + 20*i + 10 min] (10 min active, 10 min gap)
        let mut intervals = Vec::new();
        for i in 0..100 {
            let start = base_time + Duration::minutes(i * 20);
            let end = start + Duration::minutes(10);
            intervals.push((start, end));
        }

        // 1. Boundary: start_utc >= end_utc returns 0.0
        assert_eq!(
            compute_overlap_from_merged_intervals(&intervals, base_time, base_time),
            0.0
        );
        assert_eq!(
            compute_overlap_from_merged_intervals(&intervals, base_time + Duration::minutes(10), base_time),
            0.0
        );

        // 2. Boundary: empty intervals returns 0.0
        assert_eq!(
            compute_overlap_from_merged_intervals(&[], base_time, base_time + Duration::hours(1)),
            0.0
        );

        // 3. Query range strictly before all intervals
        let q_before_start = base_time - Duration::hours(5);
        let q_before_end = base_time - Duration::seconds(1);
        assert_eq!(
            compute_overlap_from_merged_intervals(&intervals, q_before_start, q_before_end),
            0.0
        );

        // 4. Query range strictly after all intervals
        let last_end = intervals.last().unwrap().1;
        let q_after_start = last_end + Duration::seconds(1);
        let q_after_end = last_end + Duration::hours(2);
        assert_eq!(
            compute_overlap_from_merged_intervals(&intervals, q_after_start, q_after_end),
            0.0
        );

        // 5. Query range entirely within a gap between interval 0 and 1
        let gap_start = base_time + Duration::minutes(11);
        let gap_end = base_time + Duration::minutes(19);
        assert_eq!(
            compute_overlap_from_merged_intervals(&intervals, gap_start, gap_end),
            0.0
        );

        // 6. Query range exactly matching one interval (interval 5)
        let (int5_start, int5_end) = intervals[5];
        assert_eq!(
            compute_overlap_from_merged_intervals(&intervals, int5_start, int5_end),
            600.0
        );

        // 7. Query range touching exact boundaries: [int5_end, int6_start] (exact gap)
        assert_eq!(
            compute_overlap_from_merged_intervals(&intervals, int5_end, intervals[6].0),
            0.0
        );

        // 8. Large span covering intervals 40..60 (20 full intervals of 600s = 12000s, plus partial edges)
        let q_span_start = base_time + Duration::minutes(40 * 20 + 5);
        let q_span_end = base_time + Duration::minutes(59 * 20 + 5);
        let actual = compute_overlap_from_merged_intervals(&intervals, q_span_start, q_span_end);
        assert_eq!(actual, 11400.0);
    }

    #[test]
    fn test_filter_web_events_clips_to_active_browser_windows() {
        let base_time = Utc.with_ymd_and_hms(2026, 11, 2, 12, 0, 0).unwrap();

        let win_chrome1 = make_event(base_time, 300.0, "chrome.exe", "YouTube", "");
        let win_chrome2 = make_event(base_time + Duration::seconds(600), 300.0, "chrome.exe", "Reddit", "");
        let win_events = vec![win_chrome1, win_chrome2];

        let web_ev = make_event(
            base_time - Duration::seconds(60),
            1200.0,
            "",
            "YouTube - Video",
            "https://www.youtube.com/watch?v=123",
        );

        let filtered = filter_web_events_by_active_browser_windows(&[web_ev], &win_events);

        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].timestamp, base_time);
        assert_eq!(filtered[0].duration, 300.0);
        assert_eq!(filtered[0].data.url, Some("https://www.youtube.com/watch?v=123".to_string()));

        assert_eq!(filtered[1].timestamp, base_time + Duration::seconds(600));
        assert_eq!(filtered[1].duration, 300.0);
        assert_eq!(filtered[1].data.url, Some("https://www.youtube.com/watch?v=123".to_string()));
    }

    #[test]
    fn test_filter_web_events_excludes_non_browser_window_activity() {
        let base_time = Utc.with_ymd_and_hms(2026, 11, 2, 12, 0, 0).unwrap();

        let win_code = make_event(base_time, 600.0, "code.exe", "engine.rs", "");
        let web_ev = make_event(
            base_time,
            600.0,
            "",
            "YouTube - LoFi",
            "https://www.youtube.com/watch?v=lofi",
        );

        let filtered = filter_web_events_by_active_browser_windows(&[web_ev], &[win_code]);
        assert!(filtered.is_empty());
    }

    #[test]
    fn test_filter_web_events_preserves_when_window_events_empty() {
        let base_time = Utc.with_ymd_and_hms(2026, 11, 2, 12, 0, 0).unwrap();
        let web_ev = make_event(
            base_time,
            300.0,
            "",
            "YouTube",
            "https://www.youtube.com",
        );

        let filtered = filter_web_events_by_active_browser_windows(std::slice::from_ref(&web_ev), &[]);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].timestamp, web_ev.timestamp);
        assert_eq!(filtered[0].duration, web_ev.duration);
    }

    #[test]
    fn test_filter_web_events_handles_empty_web_events() {
        let base_time = Utc.with_ymd_and_hms(2026, 11, 2, 12, 0, 0).unwrap();
        let win_ev = make_event(base_time, 300.0, "chrome.exe", "YouTube", "");
        let filtered = filter_web_events_by_active_browser_windows(&[], &[win_ev]);
        assert!(filtered.is_empty());
    }

    #[test]
    fn test_filter_web_events_large_dataset_and_boundaries() {
        let base_time = Utc.with_ymd_and_hms(2026, 11, 2, 8, 0, 0).unwrap();

        let mut win_events = Vec::new();
        for i in 0..50 {
            let start = base_time + Duration::minutes(i * 10);
            let app = if i % 2 == 0 { "msedge.exe" } else { "code.exe" };
            win_events.push(make_event(start, 300.0, app, "Activity", ""));
        }

        let mut input_web = Vec::new();
        for i in 0..25 {
            let start = base_time + Duration::minutes(i * 20 + 2);
            input_web.push(make_event(
                start,
                480.0,
                "",
                "Docs",
                "https://docs.rs/tokio",
            ));
        }

        let filtered = filter_web_events_by_active_browser_windows(&input_web, &win_events);
        assert!(!filtered.is_empty());
        for ev in &filtered {
            assert!(ev.duration > 0.0);
            assert!(ev.duration <= 300.0);
            assert_eq!(ev.data.url.as_deref(), Some("https://docs.rs/tokio"));
        }
    }

    #[test]
    fn test_calculate_continuous_session_from_merged_clipped_edge_cases() {
        let now_utc = Utc::now();

        // 1. Empty slices
        assert_eq!(calculate_continuous_session_from_merged_clipped(&[], now_utc, None, 300), 0);
        let min_dt = now_utc - Duration::minutes(30);
        assert_eq!(calculate_continuous_session_from_merged_clipped(&[], now_utc, Some(min_dt), 300), 0);

        // 2. Intervals completely before min_allowed_utc (early break triggers)
        let merged_old = vec![
            (now_utc - Duration::minutes(70), now_utc - Duration::minutes(65)),
            (now_utc - Duration::minutes(60), now_utc - Duration::minutes(40)),
        ];
        assert_eq!(
            calculate_continuous_session_from_merged_clipped(&merged_old, now_utc, Some(min_dt), 300),
            0
        );

        // 3. Boundary clipping where s < min_dt < e
        let merged_boundary = vec![
            (now_utc - Duration::minutes(20), now_utc), // 20m span, min_dt is 15m ago
        ];
        let min_dt_15m = now_utc - Duration::minutes(15);
        assert_eq!(
            calculate_continuous_session_from_merged_clipped(&merged_boundary, now_utc, Some(min_dt_15m), 300),
            900
        );

        // 4. Boundary clipping where e > now_utc
        let merged_future = vec![
            (now_utc - Duration::minutes(10), now_utc + Duration::minutes(5)),
        ];
        assert_eq!(
            calculate_continuous_session_from_merged_clipped(&merged_future, now_utc, None, 300),
            600
        );

        // 5. Gap exceeding reset_gap_secs between intervals
        let merged_gap = vec![
            (now_utc - Duration::minutes(20), now_utc - Duration::minutes(12)), // gap of 7m (420s > 300s)
            (now_utc - Duration::minutes(5), now_utc),                          // 5m span (300s)
        ];
        assert_eq!(
            calculate_continuous_session_from_merged_clipped(&merged_gap, now_utc, None, 300),
            300
        );

        // 6. Gap from now_utc exceeding reset_gap_secs
        let merged_stale = vec![
            (now_utc - Duration::minutes(20), now_utc - Duration::minutes(10)), // ended 10m ago (> 5m gap)
        ];
        assert_eq!(
            calculate_continuous_session_from_merged_clipped(&merged_stale, now_utc, None, 300),
            0
        );
    }

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

    #[test]
    fn test_event_end_utc() {
        let t0 = Utc.with_ymd_and_hms(2026, 11, 2, 10, 0, 0).unwrap();
        let ev = AwEvent {
            timestamp: t0,
            duration: 120.5,
            data: Default::default(),
        };
        assert_eq!(event_end_utc(&ev), t0 + Duration::milliseconds(120500));
    }
}
