use chrono::format::{Item, StrftimeItems};
use chrono::Local;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// Serde default helper returning `true`.
fn default_true() -> bool {
    true
}

/// Serde default helper for the logical day boundary hour (04:00 AM).
fn default_day_start_hour() -> u32 {
    4
}

/// Serde default helper targeting all executables (`["*"]`).
fn default_wildcard_targets() -> Vec<String> {
    vec!["*".to_string()]
}

/// Specifies which telemetry property a rule inspects to match activity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MonitorSelection {
    #[default]
    Executables,
    Titles,
    Urls,
}

impl MonitorSelection {
    /// Returns the canonical UI and configuration string for this selection mode.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Executables => "Executables",
            Self::Titles => "Titles",
            Self::Urls => "Urls",
        }
    }

    /// Parses a selection mode from a string, falling back to `Executables`.
    pub fn from_str_lossy(s: &str) -> Self {
        match s.trim() {
            "Titles" => Self::Titles,
            "Urls" => Self::Urls,
            _ => Self::Executables,
        }
    }
}

/// Determines whether a rule triggers on continuous active session duration (`Period`) or cumulative daily usage (`Limit`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TriggerType {
    #[default]
    Period,
    Limit,
}

impl TriggerType {
    /// Returns the canonical UI and configuration string for this trigger type.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Period => "Period",
            Self::Limit => "Limit",
        }
    }

    /// Parses a trigger type from a string, falling back to `Period`.
    pub fn from_str_lossy(s: &str) -> Self {
        match s.trim() {
            "Limit" => Self::Limit,
            _ => Self::Period,
        }
    }
}

/// Specifies the enforcement action executed when a rule's threshold is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum RuleAction {
    #[default]
    #[serde(rename = "Show Overlay")]
    ShowOverlay,
    #[serde(rename = "Show Notification")]
    ShowNotification,
    #[serde(rename = "Show Statistics")]
    ShowStatistics,
    #[serde(rename = "Lock Workstation")]
    LockWorkstation,
    #[serde(rename = "Close Tab")]
    CloseTab,
    #[serde(rename = "Close Window")]
    CloseWindow,
    #[serde(rename = "Kill Process")]
    KillProcess,
}

impl RuleAction {
    /// Returns the canonical UI and configuration string for this action.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ShowOverlay => "Show Overlay",
            Self::ShowNotification => "Show Notification",
            Self::ShowStatistics => "Show Statistics",
            Self::LockWorkstation => "Lock Workstation",
            Self::CloseTab => "Close Tab",
            Self::CloseWindow => "Close Window",
            Self::KillProcess => "Kill Process",
        }
    }

    /// Parses an action from a string, falling back to `ShowOverlay`.
    pub fn from_str_lossy(s: &str) -> Self {
        match s.trim() {
            "Show Notification" => Self::ShowNotification,
            "Show Statistics" => Self::ShowStatistics,
            "Lock Workstation" => Self::LockWorkstation,
            "Close Tab" => Self::CloseTab,
            "Close Window" => Self::CloseWindow,
            "Kill Process" => Self::KillProcess,
            _ => Self::ShowOverlay,
        }
    }
}

/// Delivery channel for `RuleAction::ShowNotification`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum NotificationMethod {
    #[default]
    #[serde(rename = "HUD")]
    Hud,
    Windows,
}

impl NotificationMethod {
    /// Returns the canonical UI and configuration string for this notification method.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Hud => "HUD",
            Self::Windows => "Windows",
        }
    }

    /// Parses a notification method from a string, falling back to `Hud`.
    pub fn from_str_lossy(s: &str) -> Self {
        match s.trim() {
            "Windows" => Self::Windows,
            _ => Self::Hud,
        }
    }
}

/// Time unit for user-configured durations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimeUnit {
    Hr,
    Min,
    Sec,
}

impl Default for TimeUnit {
    fn default() -> Self {
        Self::Min
    }
}

impl TimeUnit {
    /// Returns the short UI label (`"Hr"`, `"Min"`, or `"Sec"`).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Hr => "Hr",
            Self::Min => "Min",
            Self::Sec => "Sec",
        }
    }

    /// Parses a `TimeUnit` from a string, falling back to `Min`.
    pub fn from_str_lossy(s: &str) -> Self {
        match s.trim() {
            "Hr" => Self::Hr,
            "Sec" => Self::Sec,
            _ => Self::Min,
        }
    }
}

/// Pairing of a numeric value and a `TimeUnit` for rule timing parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurationConfig {
    pub value: u32,
    pub unit: TimeUnit,
}

impl DurationConfig {
    /// Constructs a new `DurationConfig` with the given magnitude and unit.
    pub fn new(value: u32, unit: TimeUnit) -> Self {
        Self { value, unit }
    }

    /// Converts the duration into total seconds using saturating multiplication.
    pub fn to_seconds(&self) -> u32 {
        match self.unit {
            TimeUnit::Hr => self.value.saturating_mul(3600),
            TimeUnit::Min => self.value.saturating_mul(60),
            TimeUnit::Sec => self.value,
        }
    }

    /// Formats the duration as a compact string such as `"2h"`, `"45m"`, or `"30s"`.
    #[allow(dead_code)]
    pub fn to_short_string(&self) -> String {
        match self.unit {
            TimeUnit::Hr => format!("{}h", self.value),
            TimeUnit::Min => format!("{}m", self.value),
            TimeUnit::Sec => format!("{}s", self.value),
        }
    }
}

/// Weekly 7x24 hourly activity schedule stored as 24-character `'0'`/`'1'` strings per weekday.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleSchedule {
    pub mon: String,
    pub tue: String,
    pub wed: String,
    pub thu: String,
    pub fri: String,
    pub sat: String,
    pub sun: String,
}

impl Default for RuleSchedule {
    fn default() -> Self {
        // Default: 09:00 - 17:59 active on weekdays
        Self {
            mon: "000000000111111111000000".to_string(),
            tue: "000000000111111111000000".to_string(),
            wed: "000000000111111111000000".to_string(),
            thu: "000000000111111111000000".to_string(),
            fri: "000000000111111111000000".to_string(),
            sat: "000000000000000000000000".to_string(),
            sun: "000000000000000000000000".to_string(),
        }
    }
}

impl RuleSchedule {
    /// Returns `true` if the rule is scheduled to be active on `day` (0 = Monday .. 6 = Sunday) at `hour` (0..23).
    pub fn is_active(&self, day: usize, hour: usize) -> bool {
        if hour >= 24 {
            return false;
        }
        let day_str = match day {
            0 => &self.mon,
            1 => &self.tue,
            2 => &self.wed,
            3 => &self.thu,
            4 => &self.fri,
            5 => &self.sat,
            6 => &self.sun,
            _ => return false,
        };
        day_str.chars().nth(hour).map(|c| c == '1').unwrap_or(false)
    }

    /// Flattens the 7x24 schedule into a 168-element boolean array for the Slint UI matrix.
    pub fn to_bool_array(&self) -> [bool; 168] {
        let mut slots = [false; 168];
        let days = [
            &self.mon, &self.tue, &self.wed, &self.thu, &self.fri, &self.sat, &self.sun,
        ];
        for (d, day_str) in days.iter().enumerate() {
            for (h, c) in day_str.chars().take(24).enumerate() {
                slots[d * 24 + h] = c == '1';
            }
        }
        slots
    }

    /// Reconstructs a `RuleSchedule` from a flattened 168-element boolean slice.
    pub fn from_bool_array(slots: &[bool]) -> Self {
        let day_str = |d: usize| -> String {
            (0..24)
                .map(|h| if slots.get(d * 24 + h).copied().unwrap_or(false) { '1' } else { '0' })
                .collect()
        };
        Self {
            mon: day_str(0),
            tue: day_str(1),
            wed: day_str(2),
            thu: day_str(3),
            fri: day_str(4),
            sat: day_str(5),
            sun: day_str(6),
        }
    }
}

/// Serde default helper for continuous active period (50 minutes).
fn default_period() -> DurationConfig {
    DurationConfig::new(50, TimeUnit::Min)
}

/// Serde default helper for daily cumulative usage limit (1 hour).
fn default_limit() -> DurationConfig {
    DurationConfig::new(1, TimeUnit::Hr)
}

/// Serde default helper for pre-action warning lead time (5 minutes).
fn default_warn_before() -> DurationConfig {
    DurationConfig::new(5, TimeUnit::Min)
}

/// Serde default helper for pre-enforcement grace countdown (0 seconds / disabled).
fn default_grace() -> DurationConfig {
    DurationConfig::new(0, TimeUnit::Sec)
}

/// Serde default helper for break overlay duration (10 minutes).
fn default_break_duration() -> DurationConfig {
    DurationConfig::new(10, TimeUnit::Min)
}

/// Serde default helper for HUD notification display duration (10 seconds).
fn default_notification_duration() -> DurationConfig {
    DurationConfig::new(10, TimeUnit::Sec)
}

/// Serde default helper for AFK duration required to reset a continuous session duration (5 minutes).
fn default_reset_after_afk() -> DurationConfig {
    DurationConfig::new(5, TimeUnit::Min)
}

/// Represents a single user-configured monitoring and enforcement rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    // --- 1. General ---
    pub id: String,
    pub enabled: bool,
    pub name: String,

    // --- 2. Monitor ---
    #[serde(default)]
    pub monitor_selection: MonitorSelection, // "Executables" | "Titles" | "Urls"
    #[serde(default = "default_wildcard_targets")]
    pub target_executables: Vec<String>,
    #[serde(default)]
    pub title_regex: String,
    #[serde(default)]
    pub url_regex: String,

    // --- 3. Trigger ---
    #[serde(default)]
    pub trigger: TriggerType, // "Period" | "Limit"
    #[serde(default = "default_period")]
    pub period: DurationConfig, // Continuous active session duration
    #[serde(default = "default_reset_after_afk")]
    pub reset_after_afk: DurationConfig, // AFK duration to reset session
    #[serde(default = "default_limit")]
    pub limit: DurationConfig, // Daily cumulative usage limit (0 = immediate)

    // --- 4. Action ---
    #[serde(default)]
    pub action: RuleAction, // "Show Overlay" | "Show Notification" | "Show Statistics" | "Lock Workstation" | "Close Tab" | "Close Window" | "Kill Process"
    #[serde(default = "default_warn_before")]
    pub warn_before: DurationConfig, // Warn before action (0 = disabled)
    #[serde(default = "default_grace")]
    pub grace_duration: DurationConfig, // Grace countdown (0 = disabled)
    #[serde(default = "default_break_duration")]
    pub break_duration: DurationConfig, // Break duration (when action == ShowOverlay)
    #[serde(default)]
    pub click_through: bool, // Click-through overlay (when action == ShowOverlay)
    #[serde(default)]
    pub strict_mode: bool, // Strict mode (when action == ShowOverlay)
    #[serde(default)]
    pub notification_method: NotificationMethod, // "HUD" | "Windows" (when action == ShowNotification)
    #[serde(default = "default_notification_duration")]
    pub notification_duration: DurationConfig, // Duration HUD notification is visible (when action == ShowNotification && notification_method == HUD)
    #[serde(default)]
    pub show_remaining_time: bool, // Show countdown timer on HUD notification
    #[serde(default)]
    pub message: String, // Used when action == ShowOverlay or ShowNotification

    // --- 5. Schedule ---
    #[serde(default)]
    pub schedule: RuleSchedule,
}

impl Rule {
    /// Creates a new rule populated with sensible ergonomic defaults for the UI "+ Add Rule" action.
    pub fn default_template(index: usize) -> Self {
        Self {
            id: format!("rule-{}", index + 1),
            enabled: true,
            name: format!("New Rule {}", index + 1),
            monitor_selection: MonitorSelection::Executables,
            target_executables: vec!["*".to_string()],
            title_regex: String::new(),
            url_regex: String::new(),
            trigger: TriggerType::Period,
            period: DurationConfig::new(50, TimeUnit::Min),
            reset_after_afk: DurationConfig::new(5, TimeUnit::Min),
            limit: DurationConfig::new(1, TimeUnit::Hr),
            action: RuleAction::ShowOverlay,
            warn_before: DurationConfig::new(5, TimeUnit::Min),
            grace_duration: DurationConfig::new(0, TimeUnit::Sec),
            break_duration: DurationConfig::new(10, TimeUnit::Min),
            click_through: false,
            strict_mode: false,
            notification_method: NotificationMethod::Hud,
            notification_duration: DurationConfig::new(10, TimeUnit::Sec),
            show_remaining_time: false,
            message: "# Time to stretch!\n[color=#38bdf8 size=18 bold] You have been active for {session_duration}.\nPlease step away for {break_time} to recharge.".to_string(),
            schedule: RuleSchedule::default(),
        }
    }
}

/// Global daemon and ActivityWatch connection settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneralConfig {
    pub aw_port: u16,
    pub poll_interval_seconds: u64,
    pub autostart: bool,
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_day_start_hour")]
    pub day_start_hour: u32,
    #[serde(default = "default_true")]
    pub suppress_fullscreen_d3d: bool,
    #[serde(default)]
    pub pause_all_rules: bool,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            aw_port: 5600,
            poll_interval_seconds: 10,
            autostart: true,
            api_key: String::new(),
            day_start_hour: 4,
            suppress_fullscreen_d3d: true,
            pause_all_rules: false,
        }
    }
}

/// Visual theme and opacity settings for the fullscreen and HUD overlay windows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverlayConfig {
    pub opacity: f32,
    pub theme: String,
    pub accent_color: String,
    pub background_color: String,
    pub text_color: String,
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self {
            opacity: 0.90,
            theme: "dark".to_string(),
            accent_color: "#3b82f6".to_string(),
            background_color: "#0f172a".to_string(),
            text_color: "#f8fafc".to_string(),
        }
    }
}

/// Configuration for the daily usage statistics dashboard and activity histogram.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatsConfig {
    pub daily_screentime_target_minutes: u32,
    pub threshold_excessive_minutes: u32,
    pub histogram_bin_size_minutes: u32,
    pub histogram_bin_count: u32,
    pub histogram_time_format: String,
}

impl Default for StatsConfig {
    fn default() -> Self {
        Self {
            daily_screentime_target_minutes: 480,
            threshold_excessive_minutes: 600,
            histogram_bin_size_minutes: 60,
            histogram_bin_count: 24,
            histogram_time_format: "%H:%M".to_string(),
        }
    }
}

/// Validates a chrono strftime format string and returns `(is_valid, preview_or_error)`.
pub fn validate_strftime_format(code: &str) -> (bool, String) {
    if code.trim().is_empty() {
        return (false, "Format cannot be empty".to_string());
    }
    let items = StrftimeItems::new(code);
    for item in items {
        if matches!(item, Item::Error) {
            return (false, "Invalid format specifier".to_string());
        }
    }
    let sample = Local::now().format(code).to_string();
    if sample.is_empty() {
        (false, "Produces empty output".to_string())
    } else {
        (true, sample)
    }
}

/// Top-level application configuration persisted to `%LOCALAPPDATA%\aw-enforcer\config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub general: GeneralConfig,
    pub overlay: OverlayConfig,
    pub stats: StatsConfig,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

pub type AppConfig = Config;

impl Default for Config {
    fn default() -> Self {
        Self {
            general: GeneralConfig::default(),
            overlay: OverlayConfig::default(),
            stats: StatsConfig::default(),
            rules: vec![
                Rule {
                    id: "rule-ergo-break".to_string(),
                    enabled: true,
                    name: "Workday Ergonomic Break".to_string(),
                    monitor_selection: MonitorSelection::Executables,
                    target_executables: vec!["*".to_string()],
                    title_regex: String::new(),
                    url_regex: String::new(),
                    trigger: TriggerType::Period,
                    period: DurationConfig::new(50, TimeUnit::Min),
                    reset_after_afk: DurationConfig::new(5, TimeUnit::Min),
                    limit: DurationConfig::new(8, TimeUnit::Hr),
                    action: RuleAction::ShowOverlay,
                    warn_before: DurationConfig::new(5, TimeUnit::Min),
                    grace_duration: DurationConfig::new(0, TimeUnit::Sec),
                    break_duration: DurationConfig::new(10, TimeUnit::Min),
                    click_through: false,
                    strict_mode: false,
                    notification_method: NotificationMethod::Hud,
                    notification_duration: DurationConfig::new(10, TimeUnit::Sec),
                    show_remaining_time: false,
                    message: "# Time to stretch!\n[color=#38bdf8 size=18 bold] You have been active for {session_duration}.\nPlease step away for {break_time} to recharge.".to_string(),
                    schedule: RuleSchedule {
                        mon: "000000001111111111000000".to_string(),
                        tue: "000000001111111111000000".to_string(),
                        wed: "000000001111111111000000".to_string(),
                        thu: "000000001111111111000000".to_string(),
                        fri: "000000001111111111000000".to_string(),
                        sat: "000000000000000000000000".to_string(),
                        sun: "000000000000000000000000".to_string(),
                    },
                },
                Rule {
                    id: "rule-gaming-limit".to_string(),
                    enabled: true,
                    name: "Weeknight Gaming Limit".to_string(),
                    monitor_selection: MonitorSelection::Executables,
                    target_executables: vec!["steam.exe".to_string(), "discord.exe".to_string()],
                    title_regex: String::new(),
                    url_regex: String::new(),
                    trigger: TriggerType::Limit,
                    period: DurationConfig::new(50, TimeUnit::Min),
                    reset_after_afk: DurationConfig::new(5, TimeUnit::Min),
                    limit: DurationConfig::new(1, TimeUnit::Hr),
                    action: RuleAction::CloseWindow,
                    warn_before: DurationConfig::new(5, TimeUnit::Min),
                    grace_duration: DurationConfig::new(30, TimeUnit::Sec),
                    break_duration: DurationConfig::new(10, TimeUnit::Min),
                    click_through: false,
                    strict_mode: false,
                    notification_method: NotificationMethod::Hud,
                    notification_duration: DurationConfig::new(10, TimeUnit::Sec),
                    show_remaining_time: false,
                    message: "## Quota Warning: {app}\n[color=#f59e0b bold] {remaining} remaining out of your {limit} daily limit.\nPlease wrap up and save your work.".to_string(),
                    schedule: RuleSchedule {
                        mon: "000000000111111111111100".to_string(),
                        tue: "000000000111111111111100".to_string(),
                        wed: "000000000111111111111100".to_string(),
                        thu: "000000000111111111111100".to_string(),
                        fri: "000000000111111111111100".to_string(),
                        sat: "000000000000000000000000".to_string(),
                        sun: "000000000000000000000000".to_string(),
                    },
                },
            ],
        }
    }
}

impl Config {
    /// Returns the platform-specific path to `config.toml` inside the local app data directory.
    pub fn config_path() -> PathBuf {
        let mut path = dirs::data_local_dir().unwrap_or_else(|| PathBuf::from("."));
        path.push("aw-enforcer");
        path.push("config.toml");
        path
    }

    /// Loads `config.toml` from disk, falling back to `Config::default()` if reading or parsing fails.
    pub fn load() -> Self {
        match Self::try_load() {
            Ok(cfg) => cfg,
            Err(e) => {
                tracing::warn!("Failed to load config.toml ({e}); using defaults.");
                Self::default()
            }
        }
    }

    /// Attempts to read, parse, validate, and sanitize `config.toml`, creating a default file if none exists.
    pub fn try_load() -> Result<Self, Box<dyn std::error::Error>> {
        let path = Self::config_path();
        if !path.exists() {
            let default_cfg = Self::default();
            default_cfg.save()?;
            return Ok(default_cfg);
        }

        let content = fs::read_to_string(&path)?;
        let mut cfg: Config = toml::from_str(&content)?;

        // Validate time format code
        let (valid, _) = validate_strftime_format(&cfg.stats.histogram_time_format);
        if !valid {
            tracing::warn!(
                "Invalid histogram_time_format '{}' in config; falling back to '%H:%M'",
                cfg.stats.histogram_time_format
            );
            cfg.stats.histogram_time_format = "%H:%M".to_string();
        }

        cfg.overlay.opacity = cfg.overlay.opacity.clamp(0.1, 1.0);

        if cfg.rules.is_empty() {
            cfg.rules = Self::default().rules;
        }

        cfg.sanitize_rules();

        Ok(cfg)
    }

    /// Enforces safety invariants on rules, downgrading `KillProcess` to `CloseWindow`
    /// whenever a rule does not target at least one specific executable name.
    pub fn sanitize_rules(&mut self) {
        for rule in &mut self.rules {
            if rule.monitor_selection == MonitorSelection::Executables {
                let has_specific = rule.target_executables.iter().any(|t| {
                    let s = t.trim();
                    !s.is_empty() && s != "*"
                });
                if !has_specific && rule.action == RuleAction::KillProcess {
                    rule.action = RuleAction::CloseWindow;
                }
            } else if rule.action == RuleAction::KillProcess {
                rule.action = RuleAction::CloseWindow;
            }
        }
    }

    /// Serializes and writes the configuration to `config.toml`, creating parent directories as needed.
    pub fn save(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let serialized = toml::to_string_pretty(self)?;
        fs::write(path, serialized)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies `TimeUnit` parsing, `DurationConfig` conversion to seconds/short strings, and `u32::MAX` overflow saturation.
    #[test]
    fn test_time_unit_and_duration_config() {
        assert_eq!(TimeUnit::from_str_lossy("Hr"), TimeUnit::Hr);
        assert_eq!(TimeUnit::from_str_lossy("Min"), TimeUnit::Min);
        assert_eq!(TimeUnit::from_str_lossy("Sec"), TimeUnit::Sec);
        assert_eq!(TimeUnit::from_str_lossy("unknown"), TimeUnit::Min);

        let d_hr = DurationConfig::new(2, TimeUnit::Hr);
        assert_eq!(d_hr.to_seconds(), 7200);
        assert_eq!(d_hr.to_short_string(), "2h");

        let d_min = DurationConfig::new(45, TimeUnit::Min);
        assert_eq!(d_min.to_seconds(), 2700);
        assert_eq!(d_min.to_short_string(), "45m");

        let d_sec = DurationConfig::new(30, TimeUnit::Sec);
        assert_eq!(d_sec.to_seconds(), 30);
        assert_eq!(d_sec.to_short_string(), "30s");

        // Overflow saturation check
        let d_max = DurationConfig::new(u32::MAX, TimeUnit::Hr);
        assert_eq!(d_max.to_seconds(), u32::MAX);
    }

    /// Verifies lossless roundtrip conversion between enum variants and their canonical string representations.
    #[test]
    fn test_enum_string_roundtrips() {
        for m in [MonitorSelection::Executables, MonitorSelection::Titles, MonitorSelection::Urls] {
            assert_eq!(MonitorSelection::from_str_lossy(m.as_str()), m);
        }
        for t in [TriggerType::Period, TriggerType::Limit] {
            assert_eq!(TriggerType::from_str_lossy(t.as_str()), t);
        }
        for a in [
            RuleAction::ShowOverlay,
            RuleAction::ShowNotification,
            RuleAction::ShowStatistics,
            RuleAction::LockWorkstation,
            RuleAction::CloseTab,
            RuleAction::CloseWindow,
            RuleAction::KillProcess,
        ] {
            assert_eq!(RuleAction::from_str_lossy(a.as_str()), a);
        }
        for n in [NotificationMethod::Hud, NotificationMethod::Windows] {
            assert_eq!(NotificationMethod::from_str_lossy(n.as_str()), n);
        }
    }

    /// Verifies 7x24 schedule bitmask roundtripping and out-of-bounds day/hour safety guards.
    #[test]
    fn test_schedule_bitmask_roundtrip_and_bounds() {
        let mut slots = [false; 168];
        slots[0] = true;
        slots[23] = true;
        slots[167] = true;

        let sched = RuleSchedule::from_bool_array(&slots);
        assert!(sched.is_active(0, 0));
        assert!(sched.is_active(0, 23));
        assert!(sched.is_active(6, 23));
        assert!(!sched.is_active(1, 12));

        // Out-of-bounds day/hour safety
        assert!(!sched.is_active(7, 0));
        assert!(!sched.is_active(0, 24));

        let recovered = sched.to_bool_array();
        assert_eq!(slots, recovered);
    }

    /// Verifies `validate_strftime_format` rejects empty or malformed format specifiers and accepts valid ones.
    #[test]
    fn test_validate_strftime_format() {
        let (valid_empty, _) = validate_strftime_format("   ");
        assert!(!valid_empty);

        let (valid_bad, _) = validate_strftime_format("%Q");
        assert!(!valid_bad);

        let (valid_ok, preview) = validate_strftime_format("%H:%M");
        assert!(valid_ok);
        assert!(!preview.is_empty());
    }

    /// Verifies `Config::sanitize_rules` downgrades `KillProcess` to `CloseWindow` unless specific executables are targeted.
    #[test]
    fn test_sanitize_rules_invariants() {
        let mut cfg = Config::default();

        // 1. Wildcard Executables + KillProcess -> downgraded to CloseWindow
        let mut r_wildcard_kill = Rule::default_template(0);
        r_wildcard_kill.monitor_selection = MonitorSelection::Executables;
        r_wildcard_kill.target_executables = vec!["*".to_string()];
        r_wildcard_kill.action = RuleAction::KillProcess;

        // 2. Specific Executables + KillProcess -> preserved
        let mut r_specific_kill = Rule::default_template(1);
        r_specific_kill.monitor_selection = MonitorSelection::Executables;
        r_specific_kill.target_executables = vec!["steam.exe".to_string()];
        r_specific_kill.action = RuleAction::KillProcess;

        // 3. Executables + CloseWindow / CloseTab -> preserved
        let mut r_exe_close = Rule::default_template(2);
        r_exe_close.monitor_selection = MonitorSelection::Executables;
        r_exe_close.action = RuleAction::CloseTab;

        // 4. Titles / Urls + KillProcess -> downgraded to CloseWindow
        let mut r_titles_kill = Rule::default_template(3);
        r_titles_kill.monitor_selection = MonitorSelection::Titles;
        r_titles_kill.action = RuleAction::KillProcess;

        cfg.rules = vec![r_wildcard_kill, r_specific_kill, r_exe_close, r_titles_kill];
        cfg.sanitize_rules();

        assert_eq!(cfg.rules[0].action, RuleAction::CloseWindow);
        assert_eq!(cfg.rules[1].action, RuleAction::KillProcess);
        assert_eq!(cfg.rules[2].action, RuleAction::CloseTab);
        assert_eq!(cfg.rules[3].action, RuleAction::CloseWindow);
    }

    /// Verifies TOML serialization and deserialization roundtrip for `Rule::default_template`.
    #[test]
    fn test_config_rules_loading_and_roundtrip() {
        let rule = Rule::default_template(0);
        assert_eq!(rule.id, "rule-1");
        assert_eq!(rule.monitor_selection, MonitorSelection::Executables);
        assert_eq!(rule.target_executables, vec!["*"]);
        assert_eq!(rule.action, RuleAction::ShowOverlay);
        assert_eq!(rule.trigger, TriggerType::Period);

        let serialized = toml::to_string_pretty(&rule).expect("Must serialize rule");
        let deserialized: Rule = toml::from_str(&serialized).expect("Must deserialize rule");
        assert_eq!(deserialized.id, rule.id);
        assert_eq!(deserialized.period.value, rule.period.value);
        assert_eq!(deserialized.action, rule.action);
        assert_eq!(deserialized.trigger, rule.trigger);
    }
}