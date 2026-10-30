# Technical Specification: "AW-Enforcer" (Unified Rule Engine & Slint UI)

## 1. Project Overview & Orthogonal Trigger/Action Architecture

Build a local, lightweight Windows desktop application in **Rust** (`aw-enforcer`) that runs persistently in the system tray, polls the local ActivityWatch REST API (`http://localhost:5600/api/0/`), and enforces user-configured usage and ergonomic rules.

### Key Architectural Highlights in v1.0.0

1. **Orthogonal `Trigger` and `Action` Rule Model (`Rule`):**
   - Decouples **how a rule is triggered** (`TriggerType`: `Period` vs. `Limit`) from **what happens when it triggers** (`RuleAction`: `Show Overlay`, `Show Notification`, `Show Statistics`, `Lock Workstation`, `Close Tab`, `Close Window`, `Kill Process`).
   - Any trigger (`Period` or `Limit`) can be paired with any of the 7 actions, with universal pre-action warnings (`Warn Before Action`) and pre-execution countdowns (`Grace Countdown`).
2. **Unified Three-Stage Evaluation Pipeline (`src/engine.rs`):**
   - Replaces separate frequency/limit evaluation paths with a single `evaluate_rule()` pipeline:
     - **Stage 1 (Warn Before Action):** Fires when `warn_before > 0` and `remaining <= warn_before`.
     - **Stage 2 (Grace Countdown):** Displays a top-right `HudTimer` countdown when `grace_duration > 0` before executing the configured action.
     - **Stage 3 (Action Execution):** Executes `Show Overlay`, `Show Notification` (`HUD` or `Windows`), `Show Statistics`, `Lock Workstation`, `Close Tab`, `Close Window`, or `Kill Process`.
3. **Five-Section Rule Editor (`ui/settings.slint`):**
   - Each rule's configuration page is organized into five clean sections with headers placed outside their cards: **1. General Settings**, **2. Monitor Target**, **3. Trigger**, **4. Action**, and **5. Active Hours Schedule** (each enclosed in a dedicated `SectionCard`).
   - The Message Template & Formatting Toolbar section is hosted in a dedicated sub-card within the **4. Action** card.
4. **Ergonomic & Anti-Bypass Safeguards:**
   - **Logical Day-Start Hour Offset (`day_start_hour`, default `4` = `04:00 AM`):** Prevents midnight rollover from resetting daily quotas during late-night sessions.
   - **1.5s Natural Break Delay & Visual Idle Feedback:** Waits for a 1.5s pause in user input before launching a full-screen `Show Overlay` break (when `grace_duration == 0`), and displays `"⏸ Input detected — timer paused until you step away"` when mouse/keyboard activity occurs during a break.
   - **Fullscreen / Presentation DND (`SHQueryUserNotificationState`):** Automatically downgrades `Show Overlay` breaks to passive Windows notifications during fullscreen D3D games, presentations, or Focus Assist.
   - **Strict Mode Lockout with 15-Second Confirmation Delay:** Available when `Action == "Show Overlay"`; hides Skip/Snooze buttons on the overlay, disables tray bypass items, and locks rule/settings edits behind a 15-second confirmation timer while actively enforcing.

---

## 2. Configuration Schema (`src/config.rs`)

Store configuration at `%LOCALAPPDATA%\aw-enforcer\config.toml`.

### A. Time Units & Duration Struct
```rust
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurationConfig {
    pub value: u32,
    pub unit: TimeUnit,
}
```

### B. Strongly-Typed `Rule` Enums & Struct
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MonitorSelection {
    #[default]
    Executables,
    Titles,
    Urls,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TriggerType {
    #[default]
    Period,
    Limit,
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum NotificationMethod {
    #[default]
    #[serde(rename = "HUD")]
    Hud,
    Windows,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    // 1. General
    pub id: String,
    pub enabled: bool,
    pub name: String,

    // 2. Monitor
    #[serde(default)]
    pub monitor_selection: MonitorSelection, // "Executables" | "Titles" | "Urls"
    #[serde(default = "default_wildcard_targets")]
    pub target_executables: Vec<String>,     // Active when monitor_selection == Executables ("*" = all)
    #[serde(default)]
    pub title_regex: String,                 // Active when monitor_selection == Titles
    #[serde(default)]
    pub url_regex: String,                   // Active when monitor_selection == Urls

    // 3. Trigger
    #[serde(default)]
    pub trigger: TriggerType,                // "Period" | "Limit"
    #[serde(default = "default_period")]
    pub period: DurationConfig,              // Continuous active session threshold (when trigger == Period)
    #[serde(default = "default_reset_after_afk")]
    pub reset_after_afk: DurationConfig,     // Idle AFK gap that resets session (when trigger == Period)
    #[serde(default = "default_limit")]
    pub limit: DurationConfig,               // Daily cumulative active usage limit (when trigger == Limit; 0 = immediate)

    // 4. Action
    #[serde(default)]
    pub action: RuleAction,                  // "Show Overlay" | "Show Notification" | "Show Statistics" | "Lock Workstation" | "Close Tab" | "Close Window" | "Kill Process"
    #[serde(default = "default_warn_before")]
    pub warn_before: DurationConfig,         // Pre-action warning lead time (0 = disabled)
    #[serde(default = "default_grace")]
    pub grace_duration: DurationConfig,      // Grace countdown before action executes (0 = disabled)
    #[serde(default = "default_break_duration")]
    pub break_duration: DurationConfig,      // Required idle break duration (when action == ShowOverlay)
    #[serde(default)]
    pub click_through: bool,                 // Passes input through overlay (when action == ShowOverlay)
    #[serde(default)]
    pub strict_mode: bool,                   // Hides Skip/Snooze & locks settings (when action == ShowOverlay)
    #[serde(default)]
    pub notification_method: NotificationMethod, // "HUD" | "Windows" (when action == ShowNotification)
    #[serde(default = "default_notification_duration")]
    pub notification_duration: DurationConfig, // Duration HUD notification is visible (when action == ShowNotification && notification_method == HUD)
    #[serde(default)]
    pub show_remaining_time: bool,           // Live countdown timer on HUD notification
    #[serde(default)]
    pub message: String,                     // Rich formatted message template (when action == ShowOverlay || ShowNotification)

    // 5. Schedule
    #[serde(default)]
    pub schedule: RuleSchedule,              // 7x24 bitmask matrix
}
```

### C. Example `config.toml` (v1.0.0)
```toml
[general]
aw_port = 5600
poll_interval_seconds = 10
autostart = true
api_key = ""
day_start_hour = 4
suppress_fullscreen_d3d = true

[overlay]
opacity = 0.90
theme = "dark"
accent_color = "#3b82f6"
background_color = "#0f172a"
text_color = "#f8fafc"

[stats]
daily_screentime_target_minutes = 480
threshold_excessive_minutes = 600
histogram_bin_size_minutes = 60
histogram_bin_count = 24
histogram_time_format = "%H:%M"

# Rule 1: Ergonomic Break Reminder (Period + Show Overlay)
[[rules]]
id = "rule-ergo-break"
enabled = true
name = "Workday Ergonomic Break"
monitor_selection = "Executables"
target_executables = ["*"]
title_regex = ""
url_regex = ""
trigger = "Period"
period = { value = 50, unit = "Min" }
reset_after_afk = { value = 5, unit = "Min" }
limit = { value = 8, unit = "Hr" }
action = "Show Overlay"
warn_before = { value = 5, unit = "Min" }
grace_duration = { value = 0, unit = "Sec" }
break_duration = { value = 10, unit = "Min" }
click_through = false
strict_mode = false
notification_method = "HUD"
message = "# Time to stretch!\n[color=#38bdf8 size=18 bold] You have been active for {streak}.\nPlease step away for {break_time} to recharge."

[rules.schedule]
mon = "000000001111111111000000"
tue = "000000001111111111000000"
wed = "000000001111111111000000"
thu = "000000001111111111000000"
fri = "000000001111111111000000"
sat = "000000000000000000000000"
sun = "000000000000000000000000"

# Rule 2: Application Quota Blocker (Limit + Close Window)
[[rules]]
id = "rule-gaming-limit"
enabled = true
name = "Weeknight Gaming Limit"
monitor_selection = "Executables"
target_executables = ["steam.exe", "discord.exe"]
title_regex = ""
url_regex = ""
trigger = "Limit"
period = { value = 60, unit = "Min" }
reset_after_afk = { value = 5, unit = "Min" }
limit = { value = 1, unit = "Hr" }
action = "Close Window"
warn_before = { value = 5, unit = "Min" }
grace_duration = { value = 30, unit = "Sec" }
break_duration = { value = 10, unit = "Min" }
click_through = false
strict_mode = false
notification_method = "HUD"
message = "## Quota Warning: {app}\n[color=#f59e0b bold] {remaining} remaining out of your {limit} daily limit.\nPlease wrap up and save your work."

[rules.schedule]
mon = "000000000111111111111100"
tue = "000000000111111111111100"
wed = "000000000111111111111100"
thu = "000000000111111111111100"
fri = "000000000111111111111100"
sat = "000000000000000000000000"
sun = "000000000000000000000000"
```

---

## 3. Rule Child Tab Sections & Conditional Visibility Matrix (`ui/settings.slint`)

When `active-tab == 1 && selected-rule-idx >= 0`, `child_rule_view` binds reactive properties for the selected rule (`cur-monitor`, `cur-trigger`, `cur-action`) and renders five structured `SectionCard` groups:

### Section 1: General
| Control | Type | Visibility Condition | Description |
| :--- | :--- | :--- | :--- |
| **Rule Enabled** | `CheckBox` | **Always Visible** | Enables or disables evaluation of this rule during active schedule hours. |
| **Rule Name** | `LineEdit` | **Always Visible** | Human-readable name displayed in tabs, notifications, and `{rule_name}`. |

### Section 2: Monitor
| Control | Type | Visibility Condition | Description |
| :--- | :--- | :--- | :--- |
| **Selection Mode** | `ComboBox` (`["Executables", "Titles", "Urls"]`) | **Always Visible** | Determines whether the rule matches process executables, window titles, or browser URLs. Close Tab and Close Window remain available across all Selection Modes; switching to Titles or Urls automatically sanitizes Kill Process to Close Window, and switching to Executables downgrades Kill Process to Close Window if no specific .exe target is configured. |
| **Monitored Executables** | `ExecutableSelector` | **`cur-monitor == "Executables"`** | Chip list of `.exe` names, `"Browse .exe..."` file dialog, and running apps dropdown. Empty UI list serializes as `["*"]`. |
| **Window Title Regex** | `LineEdit` | **`cur-monitor == "Titles"`** | Regular expression matched against `event.data["title"]` and visible window titles. |
| **Browser URL Regex** | `LineEdit` | **`cur-monitor == "Urls"`** | Regular expression matched against `event.data["url"]` from `aw-watcher-web`. |

### Section 3: Trigger
| Control | Type | Visibility Condition | Description |
| :--- | :--- | :--- | :--- |
| **Trigger:** | `ComboBox` (`["Period", "Limit"]`) | **Always Visible** | Selects continuous active session duration (`Period`) vs. cumulative daily usage quota (`Limit`). |
| **Period:** | `DurationInput` (`Hr`/`Min`/`Sec`) | **`cur-trigger == "Period"`** | Continuous active session time before triggering the configured action. |
| **Reset Session after AFK:** | `DurationInput` (`Hr`/`Min`/`Sec`) | **`cur-trigger == "Period"`** | AFK gap required to reset the continuous session counter to `0`. |
| **Daily Limit (0 = immediate):** | `DurationInput` (`Hr`/`Min`/`Sec`) | **`cur-trigger == "Limit"`** | Maximum cumulative daily active duration allowed (`0` = immediate enforcement during scheduled hours). |

### Section 4: Action
| Control | Type | Visibility Condition | Description |
| :--- | :--- | :--- | :--- |
| **Action:** | `ComboBox` | **Always Visible** | Options adapt dynamically: Close Tab and Close Window are available across all Selection Modes (`Executables`, `Titles`, `Urls`). When Selection Mode == `"Executables"` and at least one specific `.exe` target is configured, all 7 actions (`["Show Overlay", "Show Notification", "Show Statistics", "Lock Workstation", "Close Tab", "Close Window", "Kill Process"]`) are available. Otherwise (Titles, Urls, or Executables with wildcard/all screen time), `Kill Process` is omitted: `["Show Overlay", "Show Notification", "Show Statistics", "Lock Workstation", "Close Tab", "Close Window"]`. |
| **Notification Method:** | Segmented Toggle (`["HUD", "Windows"]`) | **`cur-action == "Show Notification"`** | Selects interactive top-right `HudToast` card (`HUD`) vs. native Windows Action Center toast (`Windows`). Positioned directly after the Action dropdown. |
| **Duration:** | `DurationInput` (`Hr`/`Min`/`Sec`) | **`cur-action == "Show Notification" && notification-method == "HUD"`** | Controls the duration the HUD notification remains visible on screen before auto-dismissing. |
| **Show Time Remaining:** | `CheckBox` | **`cur-action == "Show Notification" && notification-method == "HUD"`** | Displays a real-time countdown timer in the top-right HUD notification header showing remaining display time. |
| **Warn Before Action (0 = disabled):** | `DurationInput` (`Hr`/`Min`/`Sec`) | **Always Visible** | Lead time before threshold to display a pre-action warning (`0` = disabled). |
| **Grace Countdown (0 = disabled):** | `DurationInput` (`Hr`/`Min`/`Sec`) | **Always Visible** | Displays a top-right `HudTimer` final countdown before executing the action (`0` = immediate execution). |
| **Break Duration:** | `DurationInput` (`Hr`/`Min`/`Sec`) | **`cur-action == "Show Overlay"`** | Required physical idle duration to complete the full-screen break overlay. |
| **Click-through Overlay:** | `CheckBox` | **`cur-action == "Show Overlay"`** | Applies `WS_EX_TRANSPARENT \| WS_EX_NOACTIVATE` so mouse/keyboard pass through the overlay. |
| **Strict Mode:** | `CheckBox` | **`cur-action == "Show Overlay"`** | Hides Skip/Snooze buttons on the overlay and locks tray bypasses & settings edits behind a 15s confirmation delay while enforcing. |
| **Message Template & Formatting Toolbar** | `TextEdit` + Gutter + Toolbar + `FormattedText` Preview | **`cur-action == "Show Overlay" \|\| cur-action == "Show Notification"`** | Hosted in a dedicated sub-card inside Section 4 with rich formatted message editor, toolbar, line gutter, and live formatted preview. |

### Section 5: Active Hours Schedule
| Control | Type | Visibility Condition | Description |
| :--- | :--- | :--- | :--- |
| **Active Hours Schedule** | `ScheduleGrid` (7×24 Matrix) | **Always Visible** | Enclosed in a dedicated `SectionCard` with click-and-drag hourly grid with `Work Hours`, `Evenings`, `Select All`, and `Clear All` presets. |

---

## 4. Unified Rule Evaluation Pipeline (`src/engine.rs` & `src/enforcer.rs`)

### A. Event Stream Separation (`poll_tick`)
- `poll_tick()` queries window events (`aw-watcher-window_`) into `all_events` and browser tab events (`aw-watcher-web`) into a separate `web_events` vector.
- Rules with `MonitorSelection::Urls` evaluate exclusively against `web_events`, preventing double-counting of browser screen time in `total_today_secs` or wildcard `"*"` executable rules.

### B. Three-Stage Evaluation (`evaluate_rule`)
For each enabled rule active in the current `(day_index, hour_index)` schedule slot:
1. **Progress Calculation:**
   - **`TriggerType::Period`:** Computes continuous active session duration (`streak`) via `calculate_continuous_streak_for_rule` (resetting to `0` on any AFK gap $\ge$ `reset_after_afk`). Threshold is `rule.period.to_seconds()`.
   - **`TriggerType::Limit`:** Computes cumulative active duration since `logical_day_start` (`day_start_hour`) via `calculate_usage_for_rule`. Threshold is `rule.limit.to_seconds()`.
2. **Stage 1 — Pre-Action Warning (`warn_before > 0`):**
   - When `threshold > 0 && warn_before > 0 && elapsed >= threshold - warn_before && elapsed < threshold` and a matching non-minimized window is open (`find_matching_window_for_rule`):
   - Fires a pre-action warning once per cycle/day (`warned_current_cycle = true`).
3. **Stage 2 — Grace Countdown (`grace_duration > 0`):**
   - When `elapsed >= threshold` and a matching window is open:
   - If `effective_grace > 0`, starts `rt.grace_countdown = Some((effective_grace, effective_grace))` and displays `OverlayMode::HudTimer` counting down every second in `realtime_tick()`.
4. **Stage 3 — Action Execution (`execute_rule_action`):**
   - Executes when `grace_countdown` reaches `0` (or immediately when `grace_duration == 0`):
     - **`Show Overlay`:** Launches full-screen `OverlayMode::BreakEnforcer` across all monitors for `break_duration` (with 1.5s natural idle pause if `grace_duration == 0`, fullscreen D3D downgrade to Windows toast if `suppress_fullscreen_d3d` is enabled, and amber pause banner when physical input is detected).
     - **`Show Notification`:** Dispatches either `OverlayMode::HudToast` (`NotificationMethod::Hud`) or a WinRT system toast (`NotificationMethod::Windows`) using the rule's formatted `message`. HUD notifications remain visible for `notification_duration` before auto-dismissing, and display an optional countdown timer in the header when `show_remaining_time` is enabled.
     - **`Show Statistics`:** Opens the 24-Hour Screen Time Summary (`OverlayMode::HudStatus`).
     - **`Lock Workstation`:** Calls `enforcer::lock_workstation()`.
     - **`Close Tab`:** Sends `Ctrl+W` (`enforcer::close_active_browser_tab`) to the matching foreground window (falling back to `close_target_windows`).
     - **`Close Window`:** Posts `WM_CLOSE` (`enforcer::close_target_windows`) to matching visible non-minimized windows.
     - **`Kill Process`:** Terminates matching processes via `sysinfo` (`enforcer::kill_target_processes`).