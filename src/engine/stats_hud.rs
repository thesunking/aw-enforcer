use chrono::{DateTime, Duration, Local, Utc};

use crate::config::StatsConfig;
use crate::engine::intervals::{
    compute_overlap_from_merged_intervals, events_to_merged_intervals, resolve_local_ndt,
};
use crate::engine::Engine;
use crate::overlay::OverlayMode;

impl Engine {
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

        if !matches!(self.active_overlay_mode, OverlayMode::HudStatus { .. }) {
            return;
        }

        let now = Local::now();
        let bin_count = self.config.stats.histogram_bin_count.max(1) as usize;
        let bin_size_mins = self.config.stats.histogram_bin_size_minutes.max(1) as i64;
        let current_bin_start = align_stats_bin_start(now, bin_size_mins);

        let start_time =
            current_bin_start - Duration::minutes(((bin_count - 1) as i64) * bin_size_mins);
        let start_utc = start_time.with_timezone(&Utc);
        let end_utc = (current_bin_start + Duration::minutes(bin_size_mins)).with_timezone(&Utc);

        let events = self
            .aw_client
            .query_intersected_events(start_utc, end_utc)
            .await
            .unwrap_or_default();

        // If the overlay was dismissed/closed or preempted by a higher-priority overlay while waiting for ActivityWatch, do not re-open it
        if !matches!(self.active_overlay_mode, OverlayMode::HudStatus { .. }) {
            return;
        }

        let merged_intervals = events_to_merged_intervals(&events);
        self.stats_cache = Some(StatsHudCache::from_intervals(
            now,
            &self.config.stats,
            &merged_intervals,
            click_through,
        ));

        if let Some(mode) = self.build_hud_status_from_cache() {
            self.show_overlay(mode);
        }
    }

    /// Incrementally updates the open 24-Hour Statistics HUD using the `merged_intervals` already merged by `poll_tick`.
    pub(crate) fn refresh_stats_overlay_from_events(
        &mut self,
        now: DateTime<Local>,
        merged_intervals: &[(DateTime<Utc>, DateTime<Utc>)],
    ) {
        if !matches!(
            self.active_overlay_mode,
            OverlayMode::HudStatus {
                is_loading: false,
                ..
            }
        ) {
            return;
        }

        let Some(ref mut cache) = self.stats_cache else {
            return;
        };

        cache.refresh_from_events(now, &self.config.stats, merged_intervals);

        if let Some(mode) = self.build_hud_status_from_cache() {
            self.show_overlay(mode);
        }
    }

    /// Builds an `OverlayMode::HudStatus` from the current `stats_cache` without showing the loading spinner.
    pub(crate) fn build_hud_status_from_cache(&self) -> Option<OverlayMode> {
        self.stats_cache
            .as_ref()
            .map(|cache| cache.build_hud_status(&self.config.stats))
    }
}

/// Cached 24-hour histogram bins used to live-update `OverlayMode::HudStatus` on each `poll_tick`
/// without re-querying historical ActivityWatch bins.
#[derive(Debug, Clone)]
pub struct StatsHudCache {
    pub current_bin_start: DateTime<Local>,
    pub bin_size_mins: i64,
    pub bin_seconds: Vec<f32>,
    pub click_through: bool,
}

impl StatsHudCache {
    /// Computes and returns a populated `StatsHudCache` from pre-merged intervals.
    pub fn from_intervals(
        now: DateTime<Local>,
        stats_cfg: &StatsConfig,
        merged_intervals: &[(DateTime<Utc>, DateTime<Utc>)],
        click_through: bool,
    ) -> Self {
        let bin_count = stats_cfg.histogram_bin_count.max(1) as usize;
        let bin_size_mins = stats_cfg.histogram_bin_size_minutes.max(1) as i64;
        let current_bin_start = align_stats_bin_start(now, bin_size_mins);

        let start_time =
            current_bin_start - Duration::minutes(((bin_count - 1) as i64) * bin_size_mins);
        let mut bin_seconds = vec![0.0f32; bin_count];

        for (i, bin_sec) in bin_seconds.iter_mut().enumerate().take(bin_count) {
            let b_start = start_time + Duration::minutes((i as i64) * bin_size_mins);
            let b_end = b_start + Duration::minutes(bin_size_mins);
            let b_start_utc = b_start.with_timezone(&Utc);
            let b_end_utc = b_end.with_timezone(&Utc);

            *bin_sec =
                compute_overlap_from_merged_intervals(merged_intervals, b_start_utc, b_end_utc)
                    as f32;
        }

        Self {
            current_bin_start,
            bin_size_mins,
            bin_seconds,
            click_through,
        }
    }

    /// Incrementally updates the cached bins using `merged_intervals`, handling bin rollovers and dimension changes.
    pub fn refresh_from_events(
        &mut self,
        now: DateTime<Local>,
        stats_cfg: &StatsConfig,
        merged_intervals: &[(DateTime<Utc>, DateTime<Utc>)],
    ) {
        let bin_count = stats_cfg.histogram_bin_count.max(1) as usize;
        let bin_size_mins = stats_cfg.histogram_bin_size_minutes.max(1) as i64;
        let new_bin_start = align_stats_bin_start(now, bin_size_mins);

        if self.bin_seconds.len() != bin_count || self.bin_size_mins != bin_size_mins {
            *self = Self::from_intervals(now, stats_cfg, merged_intervals, self.click_through);
        } else if new_bin_start > self.current_bin_start {
            let elapsed_bins =
                ((new_bin_start - self.current_bin_start).num_minutes() / bin_size_mins).max(0)
                    as usize;
            let old_bin_start = self.current_bin_start;
            let shift = elapsed_bins.min(bin_count);
            self.bin_seconds.drain(0..shift);
            self.bin_seconds.resize(bin_count, 0.0);
            self.current_bin_start = new_bin_start;

            for i in 0..elapsed_bins {
                let b_start = old_bin_start + Duration::minutes((i as i64) * bin_size_mins);
                let b_end = b_start + Duration::minutes(bin_size_mins);
                let b_start_utc = b_start.with_timezone(&Utc);
                let b_end_utc = b_end.with_timezone(&Utc);
                let bin_idx = (bin_count as isize) - 1 - (elapsed_bins as isize) + (i as isize);
                if bin_idx >= 0 && (bin_idx as usize) < bin_count {
                    let secs =
                        compute_overlap_from_merged_intervals(merged_intervals, b_start_utc, b_end_utc)
                            as f32;
                    self.bin_seconds[bin_idx as usize] = secs;
                }
            }
        }

        // Recompute only the current (latest) bin from poll_tick's events
        let cur_start_utc = self.current_bin_start.with_timezone(&Utc);
        let cur_end_utc =
            (self.current_bin_start + Duration::minutes(bin_size_mins)).with_timezone(&Utc);
        let cur_secs =
            compute_overlap_from_merged_intervals(merged_intervals, cur_start_utc, cur_end_utc)
                as f32;
        if let Some(last) = self.bin_seconds.last_mut() {
            *last = cur_secs;
        }
    }

    /// Builds an `OverlayMode::HudStatus` from the cached histogram data without showing the loading spinner.
    pub fn build_hud_status(&self, stats_cfg: &StatsConfig) -> OverlayMode {
        let bin_count = self.bin_seconds.len().max(1);
        let bin_size_mins = self.bin_size_mins.max(1);
        let start_time =
            self.current_bin_start - Duration::minutes(((bin_count - 1) as i64) * bin_size_mins);
        let time_fmt = &stats_cfg.histogram_time_format;

        let label_step = (bin_count / 6).max(1);
        let mut bin_labels = Vec::with_capacity(bin_count);
        for i in 0..bin_count {
            let b_start = start_time + Duration::minutes((i as i64) * bin_size_mins);
            let label = if i % label_step == 0 || i == bin_count - 1 {
                b_start.format(time_fmt).to_string()
            } else {
                String::new()
            };
            bin_labels.push(label);
        }

        let max_bin_capacity = (bin_size_mins * 60) as f32;
        let bin_values: Vec<f32> = self
            .bin_seconds
            .iter()
            .map(|&s| (s / max_bin_capacity).clamp(0.0, 1.0))
            .collect();

        let total_active_seconds: f32 = self.bin_seconds.iter().sum();
        let total_active_minutes = (total_active_seconds / 60.0).round() as u32;

        let status_level = if total_active_minutes <= stats_cfg.daily_screentime_target_minutes {
            "Great"
        } else if total_active_minutes <= stats_cfg.threshold_excessive_minutes {
            "Okay"
        } else {
            "Excessive"
        };

        OverlayMode::HudStatus {
            title: "24-Hour Screen Time Summary".into(),
            status_level: status_level.into(),
            total_active_minutes,
            target_minutes: stats_cfg.daily_screentime_target_minutes,
            bin_values,
            bin_labels,
            is_loading: false,
            click_through: self.click_through,
        }
    }
}

/// Aligns a timestamp down to the start of its current statistics histogram bin.
pub fn align_stats_bin_start(now: DateTime<Local>, bin_size_mins: i64) -> DateTime<Local> {
    let bin_size = bin_size_mins.max(1);
    let midnight = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(resolve_local_ndt)
        .unwrap_or(now);
    let elapsed_mins = (now - midnight).num_minutes().max(0);
    let aligned_mins = (elapsed_mins / bin_size) * bin_size;
    midnight + Duration::minutes(aligned_mins)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    use crate::aw_client::{AwClient, AwEvent, AwEventData};
    use crate::config::AppConfig;

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
                app: if app.is_empty() {
                    None
                } else {
                    Some(app.to_string())
                },
                title: if title.is_empty() {
                    None
                } else {
                    Some(title.to_string())
                },
                url: if url.is_empty() {
                    None
                } else {
                    Some(url.to_string())
                },
                status: None,
            },
        }
    }

    #[test]
    fn test_align_stats_bin_start_multi_hour() {
        // Sub-hour: 15 mins
        let dt = Local.with_ymd_and_hms(2026, 10, 30, 14, 27, 45).unwrap();
        let aligned_15 = align_stats_bin_start(dt, 15);
        assert_eq!(
            aligned_15,
            Local.with_ymd_and_hms(2026, 10, 30, 14, 15, 0).unwrap()
        );

        // 1 hour (60 mins)
        let aligned_60 = align_stats_bin_start(dt, 60);
        assert_eq!(
            aligned_60,
            Local.with_ymd_and_hms(2026, 10, 30, 14, 0, 0).unwrap()
        );

        // 2 hours (120 mins) - hour 15 should align to 14
        let dt15 = Local.with_ymd_and_hms(2026, 10, 30, 15, 45, 12).unwrap();
        let aligned_120 = align_stats_bin_start(dt15, 120);
        assert_eq!(
            aligned_120,
            Local.with_ymd_and_hms(2026, 10, 30, 14, 0, 0).unwrap()
        );

        // 3 hours (180 mins) - hour 14 should align to 12
        let aligned_180 = align_stats_bin_start(dt, 180);
        assert_eq!(
            aligned_180,
            Local.with_ymd_and_hms(2026, 10, 30, 12, 0, 0).unwrap()
        );

        // Arbitrary bin: 90 mins (14:27 should align to 13:30)
        let aligned_90 = align_stats_bin_start(dt, 90);
        assert_eq!(
            aligned_90,
            Local.with_ymd_and_hms(2026, 10, 30, 13, 30, 0).unwrap()
        );

        // 90 mins (01:45 should align to 01:30)
        let dt_early = Local.with_ymd_and_hms(2026, 10, 30, 1, 45, 0).unwrap();
        let aligned_90_early = align_stats_bin_start(dt_early, 90);
        assert_eq!(
            aligned_90_early,
            Local.with_ymd_and_hms(2026, 10, 30, 1, 30, 0).unwrap()
        );
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
        bin_seconds[23] = 300.0; // 14:00-15:00 initially had 5 mins

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
        let ev1 = vec![make_event(
            bin_start.with_timezone(&Utc),
            600.0,
            "code.exe",
            "VS Code",
            "",
        )];
        engine.refresh_stats_overlay_from_events(now1, &events_to_merged_intervals(&ev1));

        match &engine.active_overlay_mode {
            OverlayMode::HudStatus {
                bin_values,
                total_active_minutes,
                is_loading,
                ..
            } => {
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
            make_event(
                bin_start.with_timezone(&Utc),
                1800.0,
                "code.exe",
                "VS Code",
                "",
            ),
            make_event(
                next_bin_start.with_timezone(&Utc),
                180.0,
                "code.exe",
                "VS Code",
                "",
            ),
        ];
        engine.refresh_stats_overlay_from_events(now2, &events_to_merged_intervals(&ev2));

        match &engine.active_overlay_mode {
            OverlayMode::HudStatus {
                bin_values,
                total_active_minutes,
                is_loading,
                ..
            } => {
                assert!(!is_loading);
                assert_eq!(*total_active_minutes, 53); // 20m + 30m + 3m
                assert!((bin_values[21] - (1200.0 / 3600.0)).abs() < 1e-4); // shifted left from index 22
                assert!((bin_values[22] - (1800.0 / 3600.0)).abs() < 1e-4); // finalized 14:00-15:00 bin (1800s = 30m)
                assert!((bin_values[23] - (180.0 / 3600.0)).abs() < 1e-4); // new active 15:00-16:00 bin (180s = 3m)
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
            make_event(
                bin_15.with_timezone(&Utc),
                2400.0,
                "code.exe",
                "VS Code",
                "",
            ),
            make_event(
                bin_16.with_timezone(&Utc),
                900.0,
                "code.exe",
                "VS Code",
                "",
            ),
            make_event(
                bin_17.with_timezone(&Utc),
                300.0,
                "code.exe",
                "VS Code",
                "",
            ),
        ];
        engine.refresh_stats_overlay_from_events(now3, &events_to_merged_intervals(&ev3));

        match &engine.active_overlay_mode {
            OverlayMode::HudStatus {
                bin_values,
                total_active_minutes,
                is_loading,
                ..
            } => {
                assert!(!is_loading);
                assert_eq!(*total_active_minutes, 110); // 20m + 30m + 40m + 15m + 5m
                assert!((bin_values[19] - (1200.0 / 3600.0)).abs() < 1e-4); // 13:00..14:00 shifted to index 19
                assert!((bin_values[20] - (1800.0 / 3600.0)).abs() < 1e-4); // 14:00..15:00 shifted to index 20
                assert!((bin_values[21] - (2400.0 / 3600.0)).abs() < 1e-4); // 15:00..16:00 populated (40m)
                assert!((bin_values[22] - (900.0 / 3600.0)).abs() < 1e-4); // intermediate 16:00..17:00 populated (15m)
                assert!((bin_values[23] - (300.0 / 3600.0)).abs() < 1e-4); // current 17:00..18:00 populated (5m)
            }
            other => panic!("Expected HudStatus, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_live_stats_hud_recomputes_bins_on_config_change() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();
        cfg.stats.histogram_bin_count = 24;
        cfg.stats.histogram_bin_size_minutes = 60;
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        let now = Local.with_ymd_and_hms(2026, 10, 30, 14, 30, 0).unwrap();
        let bin_14 = Local.with_ymd_and_hms(2026, 10, 30, 14, 0, 0).unwrap();
        let bin_13 = Local.with_ymd_and_hms(2026, 10, 30, 13, 0, 0).unwrap();
        let bin_12 = Local.with_ymd_and_hms(2026, 10, 30, 12, 0, 0).unwrap();

        // 3 hours of activity:
        // 12:00..12:30 has 1800s (30m)
        // 13:00..13:40 has 2400s (40m)
        // 14:00..14:10 has 600s (10m)
        let ev = vec![
            make_event(
                bin_12.with_timezone(&Utc),
                1800.0,
                "code.exe",
                "VS Code",
                "",
            ),
            make_event(
                bin_13.with_timezone(&Utc),
                2400.0,
                "code.exe",
                "VS Code",
                "",
            ),
            make_event(
                bin_14.with_timezone(&Utc),
                600.0,
                "code.exe",
                "VS Code",
                "",
            ),
        ];
        let merged = events_to_merged_intervals(&ev);

        // Initially setup stats_cache for 24 bins of 60 mins
        let mut initial_bins = vec![0.0f32; 24];
        initial_bins[21] = 1800.0;
        initial_bins[22] = 2400.0;
        initial_bins[23] = 600.0;
        engine.stats_cache = Some(StatsHudCache {
            current_bin_start: bin_14,
            bin_size_mins: 60,
            bin_seconds: initial_bins,
            click_through: false,
        });
        let initial_mode = engine.build_hud_status_from_cache().unwrap();
        engine.show_overlay(initial_mode);

        match &engine.active_overlay_mode {
            OverlayMode::HudStatus {
                total_active_minutes,
                bin_values,
                ..
            } => {
                assert_eq!(*total_active_minutes, 80); // 30m + 40m + 10m
                assert_eq!(bin_values.len(), 24);
            }
            other => panic!("Expected HudStatus, got {:?}", other),
        }

        // 1. Change bin_size_minutes to 30 and bin_count to 12
        engine.config.stats.histogram_bin_size_minutes = 30;
        engine.config.stats.histogram_bin_count = 12;

        engine.refresh_stats_overlay_from_events(now, &merged);

        let cache = engine.stats_cache.as_ref().unwrap();
        assert_eq!(cache.bin_size_mins, 30);
        assert_eq!(cache.bin_seconds.len(), 12);
        assert_eq!(
            cache.current_bin_start,
            Local.with_ymd_and_hms(2026, 10, 30, 14, 30, 0).unwrap()
        );

        match &engine.active_overlay_mode {
            OverlayMode::HudStatus {
                total_active_minutes,
                bin_values,
                is_loading,
                ..
            } => {
                assert!(!is_loading);
                assert_eq!(*total_active_minutes, 80); // recomputed across all 30m bins: 30m + 30m + 10m + 10m
                assert_eq!(bin_values.len(), 12);
                // Index 11 (14:30..15:00): 0s
                assert_eq!(bin_values[11], 0.0);
                // Index 10 (14:00..14:30): 600s / 1800s
                assert!((bin_values[10] - (600.0 / 1800.0)).abs() < 1e-4);
                // Index 9 (13:30..14:00): 600s / 1800s
                assert!((bin_values[9] - (600.0 / 1800.0)).abs() < 1e-4);
                // Index 8 (13:00..13:30): 1800s / 1800s
                assert!((bin_values[8] - 1.0).abs() < 1e-4);
                // Index 7 (12:30..13:00): 0s
                assert_eq!(bin_values[7], 0.0);
                // Index 6 (12:00..12:30): 1800s / 1800s
                assert!((bin_values[6] - 1.0).abs() < 1e-4);
            }
            other => panic!("Expected HudStatus, got {:?}", other),
        }

        // 2. Change bin_count to 6 (keeping bin_size_minutes = 30)
        engine.config.stats.histogram_bin_count = 6;
        engine.refresh_stats_overlay_from_events(now, &merged);

        match &engine.active_overlay_mode {
            OverlayMode::HudStatus {
                total_active_minutes,
                bin_values,
                ..
            } => {
                assert_eq!(*total_active_minutes, 80);
                assert_eq!(bin_values.len(), 6);
                // Indices 0..=5 correspond to 12:00..15:00
                assert!((bin_values[0] - 1.0).abs() < 1e-4); // 12:00..12:30
                assert_eq!(bin_values[1], 0.0); // 12:30..13:00
                assert!((bin_values[2] - 1.0).abs() < 1e-4); // 13:00..13:30
                assert!((bin_values[3] - (600.0 / 1800.0)).abs() < 1e-4); // 13:30..14:00
                assert!((bin_values[4] - (600.0 / 1800.0)).abs() < 1e-4); // 14:00..14:30
                assert_eq!(bin_values[5], 0.0); // 14:30..15:00
            }
            other => panic!("Expected HudStatus, got {:?}", other),
        }
    }
}
