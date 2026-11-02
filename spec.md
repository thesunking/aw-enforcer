# Technical Specification: "AW-Enforcer" (Unified Rule Engine & Slint UI)

## 1. Project Overview & Orthogonal Trigger/Action Architecture

Build a local, lightweight Windows desktop application in **Rust** (`aw-enforcer`) that runs persistently in the system tray, polls the local ActivityWatch REST API (`http://localhost:5600/api/0/`), and enforces user-configured usage and ergonomic rules.

### Key Architectural Highlights in v1.0.0

1. **Orthogonal `Trigger` and `Action` Rule Model (`Rule`):**
   - Decouples **how a rule is triggered** (`TriggerType`: `Period` vs. `Limit`) from **what happens when it triggers** (`RuleAction`: `Show Overlay`, `Show Notification`, `Show Statistics`, `Lock Workstation`, `Close Tab`, `Close Window`, `Kill Process`).
   - Any trigger (`Period` or `Limit`) can be paired with any of the 7 actions, with universal pre-action warnings (`Warn Before Action`) and pre-execution countdowns (`Grace Countdown`).
2. **Unified Three-Stage Evaluation Pipeline (`src/engine.rs`):**
   - Replaces separate frequency/limit evaluation paths with a single `evaluate_rule()` pipeline:
     - **Stage 1 (Warn Before Action):** Fires when `warn_before > 0` and `remaining <= warn_before`. Supports custom formatted message templates across all rule actions (preserving unambiguous upcoming-break countdowns for `ShowOverlay`).
     - **Stage 2 (Grace Countdown):** Displays a top-right `HudTimer` countdown when `grace_duration > 0` before executing the configured action, rendering the rule's custom message template.
     - **Stage 3 (Action Execution):** Executes `Show Overlay`, `Show Notification` (`HUD` or `Windows`), `Show Statistics`, `Lock Workstation`, `Close Tab`, `Close Window`, or `Kill Process`.
   - Centralizes cycle state transitions on `RuleRuntime` (`reset_active_timers()`, `complete_cycle(now)`, and `snooze(until)` with `prev_completed_at` tracking) so snoozing on any cycle pauses enforcement for `minutes` and then re-evaluates without losing pre-snooze session progress.
3. **Five-Section Rule Editor & Preview Alerts (`ui/settings.slint`):**
   - Each rule's configuration page is organized into five clean sections with headers placed outside their cards: **1. General Settings**, **2. Monitor Target**, **3. Trigger**, **4. Action**, and **5. Active Hours Schedule** (each enclosed in a dedicated `SectionCard`).
   - The Message Template & Formatting Toolbar section is hosted in a dedicated sub-card within the **4. Action** card (visible when `action == ShowOverlay || action == ShowNotification || warn_before > 0 || grace_duration > 0`) and live-updates its preview whenever rule names, targets, monitor modes, triggers, or durations change.
   - Includes a dedicated **"Preview This Rule's Alert"** button that dispatches a preview alert matching the rule's exact custom template, formatting, click-through, and notification parameters (previewing `BreakEnforcer` for `ShowOverlay`, `HudToast` for `ShowNotification` or zero-grace warnings, and `HudTimer` for actions with `grace_duration > 0`).
4. **Configuration Import & Export (`src/config.rs`, `ui/settings.slint`, `src/main.rs`):**
   - Users can export their complete configuration to a `.toml` file (`aw-enforcer-config.toml`) or import an existing configuration file via native `rfd::FileDialog` pickers in General Settings.
   - Import automatically executes full validation (`validate_strftime_format`, opacity clamping, excessive threshold clamping, and `sanitize_rules()`), loads the rules and settings into the UI, synchronizes live badges, and sets `has-unsaved-changes = true` for user review before committing. Both controls are disabled during active strict mode (`ui-locked-by-strict`).
5. **Single-Instance IPC, Window Activation & System Tray UX:**
   - Enforces single-instance execution via a named Win32 mutex (`Local\AW_Enforcer_SingleInstance_Mutex`).
   - Pairs with a named Win32 auto-reset activation event (`Local\AW_Enforcer_Activate_Event`): when a second instance is launched without `--autostart` or `--minimized`, it signals the activation event and exits immediately.
   - The primary instance's 250ms UI timer detects the event, calls `ShowWindow(hwnd, SW_RESTORE)`, and `SetForegroundWindow(hwnd)` to focus the existing `SettingsWindow`.
   - Double-clicking the system tray icon (`TrayIconEvent::DoubleClick`) opens or restores the `SettingsWindow`.
   - The system tray tooltip dynamically displays active snooze countdowns (e.g. `AW-Enforcer (Snoozed 15m) — Today: 2h 15m`), and rule status badges in the overview display `"Snoozed ({rem}m)"` during active global or per-rule snooze intervals. The tray menu contextually toggles between `"Snooze All (15m)"` and `"Resume Snoozed Rules"` based on active snoozes (`OverlayAction::UpdateLiveStatus.any_snoozed`), allowing early resumption of both global and individual rule snoozes via `DaemonCommand::ClearSnoozeAll`.
6. **Ergonomic, Shell & Anti-Bypass Safeguards:**
   - **Double-Counting Prevention via Interval Coalescing (`merge_overlapping_intervals`):** Eliminates double-counted screen time from concurrent browser windows, nested activity events, and overlapping ActivityWatch records across continuous session tracking, daily quota usage calculations, and 24-hour histogram binning.
   - **Multi-Browser Web Watcher Integration & Resilient Bucket Selection:** Dynamically discovers all local browser buckets (Chrome, Edge, Firefox, Brave) while strictly excluding non-local synchronized buckets (`-synced` and `_synced`) across window, AFK, and web watchers, querying unioned and `not-afk` intersected web activity. Periodically refreshes web bucket discovery every 60 seconds so newly opened browsers are detected at runtime. When multiple local window or AFK buckets exist (e.g., across hostname migrations), deterministically selects the bucket matching the local machine's hostname or with the latest `last_updated`/`created` timestamp.
   - **URL Tab-Level Target Verification, Full URL Tracking & Wildcard Protection:** In Stage 2/3 and break countdowns, matches open browser windows against the specific tab title regex (`rt.last_matched_title_regex`, falling back to regex-escaped `rt.last_matched_title`), ensuring that closing or navigating away from the target tab during a grace period safely skips destructive closing. `RuleRuntime::last_matched_url` preserves the full URL string (`ev.data.url`) alongside `last_matched_app` (hostname), enabling path-based URL regexes (e.g. `youtube\.com/shorts`) to be matched accurately via `matched_url_or_app_matches_rule`. Navigating between pages matching `rule.url_regex` while a grace period is actively ticking continuously updates `rt.last_matched_app`, `rt.last_matched_url`, `rt.last_matched_title`, and `rt.last_matched_title_regex`. Destructive actions (`CloseTab` and `CloseWindow`) for `MonitorSelection::Urls` strictly require a verified tab title regex (`last_matched_title_regex`); if the tab title is empty or whitespace-only, destructive actions are skipped and the cycle completes safely, preventing unguarded closures from closing all open browser windows. Event selection orders by end-time and start-time (`(event_end_utc(ev), ev.timestamp)`) across active window discovery, rule evaluation, and active app polling so long-running active sessions are never shadowed by shorter finished intervals. The `{app}` message placeholder accurately extracts and retains the URL hostname (e.g. `youtube.com`).
   - **Stage 1 Warning Snooze State Preservation:** Snoozing an alert while a pre-action warning is active preserves `rt.warned_current_cycle = true` so snooze expiration does not fire duplicate pre-action warnings within the same threshold cycle.
   - **Long Process Paths & Keystroke Synthesis Guards:** Win32 executable path resolution employs a 1024-wchar stack buffer to prevent `ERROR_INSUFFICIENT_BUFFER` on deep paths. Tab-closing input synthesis (`enforcer::close_active_browser_tab`) explicitly releases physical `VK_SHIFT` and `VK_MENU` keys prior to sending `Ctrl+W` to prevent unintended `Ctrl+Shift+W` (closing whole browser window).
   - **Dynamic Lifecycle and Off-Schedule Timer Cleanup:** Deleting, disabling, or transitioning off-schedule on any rule immediately resets its active countdown timers and clears or transitions any active overlay if that rule was being displayed.
   - **Logical Day-Start Hour Offset (`day_start_hour`, default `4` = `04:00 AM`):** Prevents midnight rollover from resetting daily quotas during late-night sessions.
   - **1.5s Natural Break Delay & Visual Idle Feedback:** Waits for a 1.5s pause in user input before launching a full-screen `Show Overlay` break (when `grace_duration == 0`), and displays `"⏸ Input detected — timer paused until you step away"` when mouse/keyboard activity occurs during a break.
   - **Fullscreen / Presentation DND (`SHQueryUserNotificationState`):** Automatically downgrades `Show Overlay` breaks to passive Windows notifications during fullscreen D3D games, presentations, or Focus Assist.
   - **Strict Mode Lockout & Overlay Priority Hierarchy:** `OverlayMode::priority()` enforces `BreakEnforcer (4) > HudTimer (3) > HudToast (2) > HudStatus (1) > Hidden (0)`. When `Strict Mode` is enabled on a `Show Overlay` rule, it hides Skip/Snooze buttons on the overlay, disables tray bypass items, blocks `SnoozeBreak`/`SkipBreak`/`CloseOverlay`/`TriggerTestOverlay` commands in the engine, and locks rule/settings edits (including `Pause All Rules` and `Test Overlays` buttons) behind a 15-second confirmation timer while actively enforcing or during the pending break natural pause (`rt.pending_break_since`). Active strict `BreakEnforcer` overlays cannot be clobbered by non-strict, click-through, or shorter breaks from other rules, and countdown transitions prioritize active breaks maximizing `(strict_mode, !click_through, remaining_seconds)`. Test and preview alerts can replace preceding test overlays without priority lockouts.
   - **Protected Window & Shell Guards (`enforcer::is_protected_window`):** Excludes the `aw-enforcer` process (`GetCurrentProcessId()`) and Windows shell classes (`Progman`, `WorkerW`, `Shell_TrayWnd`, `Shell_SecondaryTrayWnd`) from window enumeration, wildcard `WM_CLOSE`, `Ctrl+W` synthesis, and process termination.
   - **Sub-Second Session Interval Accumulation & Clamping:** Continuous session calculation accumulates millisecond intervals before integer second conversion and clamps event boundaries to `now_utc`, preventing up to 999ms per-interval truncation.
   - **Dismissing Pending Breaks Completes Cycle:** Dismissing a break during its natural 1.5s pause marks the cycle completed (`rt.complete_cycle(now)`), preventing it from immediately re-triggering on the next poll tick.
   - **Strict Unlock Reset on Window Close:** When `destroy_windows_on_hide = false`, closing the Settings window immediately resets `strict_unlocked = false` so strict mode lockouts cannot be bypassed across window re-openings.
   - **Action Center Plain-Text Sanitization:** `formatter::format_plain_message` cleanly strips Markdown heading prefixes (`# `, `## `) and valid attribute blocks (`[color=... size=... bold]`) before sending native WinRT toasts to the Windows Action Center.
   - **Off-Screen HUD Clamping & Clean Initial Placement:** `clamp_or_reset_status_pos` detects if the 24h Stats HUD window has drifted off-screen (e.g. after monitor disconnection) and resets it to the primary display's top-right corner. Opening/starting `OverlayMode::HudStatus` computes DPI-scaled bounds on the primary display (`primary_top_right`) without inheriting stale positions from other overlay modes or unscaled dimensions, ensuring the HUD starts fully on-screen within work area boundaries, and resetting stored coordinates on `hide_all()`.
   - **Settings Window Sizing & Multi-Row Toolbar Layouts:** `SettingsWindow` defines `preferred-width: 1240px` (`min-width: 1080px`, `preferred-height: 780px`) so the window opens comfortably displaying all actions and buttons. Test overlay buttons in General Settings and message variable quick-insert chips in Section 4 are organized into dual-row layouts, and rule card overview descriptions elide overflowing text, ensuring all action buttons remain fully visible on screen.
7. **Performance, Renderer & GPU Memory Management:**
   - **Worker Thread Optimization:** Tokio runtime is configured with `.worker_threads(2)` to minimize idle background thread overhead on high-core CPUs.
   - **Direct JSON Event Stream Deserialization:** `AwClient` executes queries and deserializes directly into `Vec<Vec<AwEvent>>`, eliminating intermediate `serde_json::Value` allocations and cloning.
   - **Multi-Hour 24h Histogram Bin Rollover:** Stats histogram aligns hour boundaries to multiples of `(bin_size_mins / 60)` and computes coalesced overlap for intermediate bins during multi-bin transitions.
   - **Rendering Backend Toggle (`slint_renderer`, default `"femtovg"`):** Allows switching between OpenGL hardware acceleration (`renderer-femtovg`) and CPU software rendering (`renderer-software`) directly from the General Settings **Performance** card.
   - **DPI-Aware Positioning & DWM Frame Reset:** Top-right HUD cards scale logical pixel geometry and screen margins by monitor DPI scale factor (`scale_factor().max(1.0)`). Reused overlay windows cleanly reset DWM client area margins (`DwmExtendFrameIntoClientArea`) with zeroed margins when transitioning from fullscreen mode.
   - **Same-Tick Countdown Decrement Prevention:** Real-time 1s countdown reorders active HUD overlay ticking to the start of `realtime_tick()`, ensuring newly spawned cards from completed grace periods or launched breaks retain their full duration without being decremented in the same tick.
   - **Authoritative Win32 Overlay Placement, Multi-Monitor Hotplug & Slint Rendering:** Overlay styling, window styles, and monitor geometry updates are applied authoritatively in a single pass (`apply_pending_win32_styles()`, returning early if `OverlayMode::Hidden`). `OverlayManager::show_mode` updates placement configurations across all monitors with re-entrancy protection (`is_updating_placements`) before invoking `apply_pending_win32_styles()`, preventing recursive hotplug checks and stack overflows on multi-monitor systems. Active overlay windows explicitly activate Slint rendering via `win.show()` alongside Win32 `SW_SHOW`, ensuring both full-screen break screens and floating HUD cards (`HudToast`, `HudTimer`, `HudStatus`) reliably render on screen. During active break enforcement, changes in connected display count or monitor work areas (e.g., monitor plug/unplug, resolution or dock changes) are automatically detected and trigger overlay respawning to maintain full multi-monitor coverage. Countdown state (`remaining_seconds` and `duration_seconds`) is continuously synchronized in `current_mode` during `update_countdown()`, ensuring display hotplug respawns preserve the active countdown exactly without resetting to full duration. Break overlays accurately track `duration_seconds` and compute real-time progress ratios on launch and reuse, while initializing user idle status via `get_idle_time_millis() >= 3000`.
   - **Cached Window UI State Synchronization (`sync_live_state_to_ui`):** When `destroy_windows_on_hide` is disabled and the existing `SettingsWindow` is re-opened or changes are discarded, active session duration, daily screen time, active app, strict lock states, and live rule status badges are preserved and refreshed immediately.
   - **Full-Screen Overlay Surface Cleanup (`destroy_windows_on_hide`, default `true`):** Calling `OverlayManager::hide_all` clears active `OverlayWindow` instances from `self.windows`, immediately releasing multi-monitor swapchains, framebuffers, and driver memory once breaks or toasts dismiss.
   - **Lazy-Instantiation & Teardown of Settings UI:** `SettingsWindow` is not allocated at startup when launched silently (`--autostart` / `--minimized`). It is instantiated on demand when clicking "Open Settings" in the tray, and dropped on close (`'X'`) via a zero-delay UI timer when `destroy_windows_on_hide` is enabled.
   - **Event-Driven UI Dispatch & Idle-Quiet Connection Polling:** `OverlayAction` updates from the Tokio engine are dispatched directly onto the Slint UI thread via `slint::invoke_from_event_loop` (`OVERLAY_ACTION_HANDLER`), while tray/countdown maintenance runs on a lightweight `250ms` timer that only updates the UI model and polls `/api/0/info` connection health when `SettingsWindow` is visible.
   - **Per-Tick Usage Caching (`RuleRuntime::daily_usage_seconds`):** Caches daily quota calculations during `evaluate_rule` so `poll_tick` reuses the cached value when formatting Live Status badges instead of scanning events twice.
   - **Zero-Allocation Win32 Process Resolution & Shared Snapshot Iterator:** Uses `QueryFullProcessImageNameW` on stack buffers and zero-allocation `.exe` stem matching (`enforcer::exe_name_matches_targets`). Process termination (`kill_target_processes`) and known-app discovery (`scan_and_update_known_apps`) share `enforcer::for_each_process` over Win32 ToolHelp snapshots.
   - **Typed Event Schema (`AwEventData`) & Optimized Release Profile:** Deserializes ActivityWatch event payloads into strongly-typed Rust structs and compiles release binaries with `lto = true`, `codegen-units = 1`, `strip = true`, and `opt-level = "s"`.
   - **Bucket Discovery Deduplication & Query Escaping:** `AwClient::refresh_buckets` immediately timestamps `last_web_bucket_check` upon start of discovery, preventing redundant back-to-back `/api/0/buckets/` calls during poll ticks. Bucket identifiers escape backslashes and double quotes (`escape_bucket_id`) before query string interpolation.
   - **Zero-Allocation Protected Window Class Check:** Window class inspection compares UTF-16 buffers directly against static UTF-16 constants for shell surfaces (`Progman`, `WorkerW`, `Shell_TrayWnd`, `Shell_SecondaryTrayWnd`), eliminating heap `String` allocations during `EnumWindows`.
   - **Immediate Telemetry & Badge Refresh on Reload:** Applying settings or reloading configuration triggers an immediate `self.poll_tick().await`, refreshing live badges and tray tooltip instantly without waiting for the next polling interval.

---

## 2. Configuration Schema (`src/config.rs`)

Store configuration at `%LOCALAPPDATA%\aw-enforcer\config.toml`. Note that saving an empty `rules = []` list is preserved across restarts; default rules are only generated when creating a new `config.toml` or falling back from a corrupted file.

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

### B. Strongly-Typed `Rule` Enums, Struct & Sanitization

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
    pub message: String,                     // Rich formatted message template (used when action == ShowOverlay || ShowNotification, or as custom warning/grace alert across all actions)

    // 5. Schedule
    #[serde(default)]
    pub schedule: RuleSchedule,              // 7x24 bitmask matrix
}

```

`Rule::sanitize(&mut self)` enforces safety and timing invariants on both config load and UI save (`view_to_rule`), automatically downgrading `RuleAction::KillProcess` to `RuleAction::CloseWindow` unless `monitor_selection == MonitorSelection::Executables` and at least one non-wildcard `.exe` target is configured, while clamping `period.value`, `reset_after_afk.value`, `break_duration.value`, and `notification_duration.value` to at least `1`. `Rule::trigger_threshold_seconds()` returns `period.to_seconds().max(1)` for `TriggerType::Period`.

`Config::sanitize(&mut self)` unifies all application configuration invariants, validating `histogram_time_format` (falling back to `"%H:%M"`), clamping `aw_port` (\ge 1), `poll_interval_seconds` (\ge 2), `day_start_hour` (\le 23), normalizing `slint_renderer` (`"software"` vs `"femtovg"`), clamping `overlay.opacity` to `[0.1, 1.0]`, clamping stats thresholds (`daily_screentime_target_minutes` \ge 1, `threshold_excessive_minutes` \ge target, histogram bin size \ge 1, bin count \ge 1), and invoking `sanitize_rules()`. Both `Config::load_from_path` and `Config::save_to_path` invoke `Config::sanitize()`, ensuring identical invariants across disk persistence and configuration export/import.

### C. Example `config.toml` (v1.0.0)

```toml
[general]
aw_port = 5600
poll_interval_seconds = 10
autostart = true
api_key = ""
day_start_hour = 4
suppress_fullscreen_d3d = true
pause_all_rules = false
destroy_windows_on_hide = true
slint_renderer = "femtovg"

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
notification_duration = { value = 10, unit = "Sec" }
show_remaining_time = false
message = "# Time to stretch!\n[color=#38bdf8 size=18 bold] You have been active for {session_duration}.\nPlease step away for {break_time} to recharge."

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
period = { value = 50, unit = "Min" }
reset_after_afk = { value = 5, unit = "Min" }
limit = { value = 1, unit = "Hr" }
action = "Close Window"
warn_before = { value = 5, unit = "Min" }
grace_duration = { value = 30, unit = "Sec" }
break_duration = { value = 10, unit = "Min" }
click_through = false
strict_mode = false
notification_method = "HUD"
notification_duration = { value = 10, unit = "Sec" }
show_remaining_time = false
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

## 3. UI Navigation & Settings Screens (`ui/settings.slint`)

### A. Tab 0: General Settings (`active-tab == 0`)

The General Settings tab organizes application-wide preferences into five structured `SectionCard` panels:

1. **General Settings:** ActivityWatch Port (`aw_port`), Polling Interval in seconds (`poll_interval_seconds`), Start on Windows Login (`autostart`), API Key (`api_key`), Logical Day Start Hour (`day_start_hour`), Fullscreen/Presentation DND (`suppress_fullscreen_d3d`), and Pause All Rules (`pause_all_rules`, disabled when `ui-locked-by-strict` is active).
2. **Performance:**
* **Slint Renderer (`slint-renderer`):** ComboBox selecting between OpenGL (`OpenGL (renderer-femtovg)`) and CPU Software (`CPU Software (renderer-software)`). Includes an interactive `InfoBadge` tooltip on hover/click (requires application restart).
* **Destroy Windows when Hidden (`destroy-windows-on-hide`):** Checkbox controlling whether `OverlayWindow` and `SettingsWindow` instances and their OpenGL/GPU contexts are destroyed when hidden or closed (`true`), or kept resident in memory for instantaneous zero-latency display (`false`).
3. **Overlay Theme & Appearance:** Overlay Opacity slider (`overlay_opacity`), Accent Color hex input (`accent_color_hex`), and test overlay trigger buttons (`Test Break Screen`, `Test HUD Toast`, `Test HUD Timer`, `Test 24h Stats HUD` — all disabled when `ui-locked-by-strict` is active).
4. **Activity Statistics & 24h HUD Configuration:** Daily Target Screen Time (`daily_screentime_target_minutes`), Excessive Threshold (`threshold_excessive_minutes`), and Histogram Strftime Code (`histogram_time_format`) with live preview validation.
5. **Configuration Backup & Transfer:** Dedicated card with **Export Config...** and **Import Config...** buttons (disabled during active strict mode via `ui-locked-by-strict`). Exports the active configuration via `rfd::FileDialog` to `.toml` format; imports external `.toml` files, runs full validation (`load_from_path`), synchronizes the UI and live telemetry, and marks `has-unsaved-changes = true` for user review before committing.

### B. Tab 1: Rule Child Tab Sections & Conditional Visibility Matrix (`active-tab == 1 && selected-rule-idx >= 0`)

Adding a rule (`on_add_rule`) scans existing rules in `rules_model` to generate the lowest unused `rule-{n}` ID so deleting and adding rules never produces ID collisions. When `active-tab == 1 && selected-rule-idx >= 0`, `child_rule_view` binds reactive properties for the selected rule (`cur-monitor`, `cur-trigger`, `cur-action`, `has-target-executables`) and renders five structured `SectionCard` groups:

### Section 1: General

| Control | Type | Visibility Condition | Description |
| --- | --- | --- | --- |
| **Rule Enabled** | `CheckBox` | **Always Visible** | Enables or disables evaluation of this rule during active schedule hours. |
| **Rule Name** | `LineEdit` | **Always Visible** | Human-readable name displayed in tabs, notifications, and `{rule_name}`. |

### Section 2: Monitor

| Control | Type | Visibility Condition | Description |
| --- | --- | --- | --- |
| **Selection Mode** | `ComboBox` (`["Executables", "Titles", "Urls"]`) | **Always Visible** | Determines whether the rule matches process executables, window titles, or browser URLs. Switching modes sanitizes `Kill Process` to `Close Window` if no specific `.exe` target applies and refreshes the live message preview. |
| **Monitored Executables** | `ExecutableSelector` | **`cur-monitor == "Executables"`** | Chip list of `.exe` names, manual `.exe` input field with Add button/Enter key support, `"Browse .exe..."` file dialog, and running apps dropdown (which resets to placeholder after each selection). Empty UI list serializes as `["*"]` and displays wildcard guidance text ("All applications monitored (* wildcard) — add specific .exe names below to restrict."). Input names are automatically normalized with `.exe` extensions and deduplicated case-insensitively. |
| **Window Title Regex** | `LineEdit` | **`cur-monitor == "Titles"`** | Regular expression matched against `event.data.title` and visible window titles (with real-time regex syntax validation). |
| **Browser URL Regex** | `LineEdit` | **`cur-monitor == "Urls"`** | Regular expression matched against `event.data.url` from `aw-watcher-web` (with real-time regex syntax validation). |

### Section 3: Trigger

| Control | Type | Visibility Condition | Description |
| --- | --- | --- | --- |
| **Trigger:** | `ComboBox` (`["Period", "Limit"]`) | **Always Visible** | Selects continuous active session duration (`Period`) vs. cumulative daily usage quota (`Limit`). |
| **Period:** | `DurationInput` (`Hr`/`Min`/`Sec`) | **`cur-trigger == "Period"`** | Continuous active session time before triggering the configured action. |
| **Reset Session after AFK:** | `DurationInput` (`Hr`/`Min`/`Sec`) | **`cur-trigger == "Period"`** | AFK gap required to reset the continuous session counter to `0`. |
| **Daily Limit (0 = immediate):** | `DurationInput` (`Hr`/`Min`/`Sec`) | **`cur-trigger == "Limit"`** | Maximum cumulative daily active duration allowed (`0` = immediate enforcement during scheduled hours). |

### Section 4: Action

| Control | Type | Visibility Condition | Description |
| --- | --- | --- | --- |
| **Action:** | `ComboBox` | **Always Visible** | When `cur-monitor == "Executables"` and `has-target-executables` is true, all 7 actions (`["Show Overlay", "Show Notification", "Show Statistics", "Lock Workstation", "Close Tab", "Close Window", "Kill Process"]`) are selectable; otherwise `Kill Process` is omitted. |
| **Notification Method:** | Segmented Toggle (`["HUD", "Windows"]`) | **`cur-action == "Show Notification"`** | Selects interactive top-right `HudToast` card (`HUD`) vs. native Windows Action Center toast (`Windows`). |
| **Duration:** | `DurationInput` (`Hr`/`Min`/`Sec`) | **`cur-action == "Show Notification" && notification-method == "HUD"`** | Controls the duration the HUD notification remains visible on screen before auto-dismissing. |
| **Show Time Remaining:** | `CheckBox` | **`cur-action == "Show Notification" && notification-method == "HUD"`** | Displays a real-time countdown timer in the top-right HUD notification header showing remaining display time. |
| **Warn Before Action (0 = disabled):** | `DurationInput` (`Hr`/`Min`/`Sec`) | **Always Visible** | Lead time before threshold to display a pre-action warning (`0` = disabled). |
| **Grace Countdown (0 = disabled):** | `DurationInput` (`Hr`/`Min`/`Sec`) | **Always Visible** | Displays a top-right `HudTimer` final countdown before executing the action (`0` = immediate execution). |
| **Break Duration:** | `DurationInput` (`Hr`/`Min`/`Sec`) | **`cur-action == "Show Overlay"`** | Required physical idle duration to complete the full-screen break overlay. |
| **Click-through Overlay:** | `CheckBox` | **`cur-action == "Show Overlay"`** | Applies `WS_EX_TRANSPARENT | WS_EX_NOACTIVATE` so mouse/keyboard pass through the overlay. |
| **Strict Mode:** | `CheckBox` | **`cur-action == "Show Overlay"`** | Hides Skip/Snooze buttons on the overlay and locks tray bypasses & settings edits behind a 15s confirmation delay while enforcing. |
| **Message Template & Formatting Toolbar** | `TextEdit` + Gutter + Toolbar + `FormattedText` Preview | **`cur-action == "Show Overlay" || cur-action == "Show Notification" || root.rule-warn-before-val > 0 || root.rule-grace-val > 0`** | Hosted in a dedicated sub-card inside Section 4 with rich formatted message editor, toolbar, line gutter, and live formatted preview. Allows configuring custom alert templates for quota enforcement actions (`Close Window`, `Close Tab`, `Kill Process`, `Lock Workstation`) in addition to Break and Notification actions. Dynamically formats preview placeholders (`{session_duration}`, `{break_time}`, `{limit}`, `{remaining}`) with concise units matching runtime overlay formatting via `formatter::build_preview_context()`, ensuring identical calculation across live Settings editor previews and dispatched preview alerts (clamping `remaining` to `warn_before.min(threshold)` or fallback `300.min(threshold)`, and computing `used` as `threshold.saturating_sub(remaining)`). |

### Section 5: Active Hours Schedule

| Control | Type | Visibility Condition | Description |
| --- | --- | --- | --- |
| **Active Hours Schedule** | `ScheduleGrid` (7×24 Matrix) | **Always Visible** | Enclosed in a dedicated `SectionCard` with click-and-drag hourly grid with `Work Hours`, `Evenings`, `Select All`, and `Clear All` presets. |

### C. Overview Tab & Real-Time Validation Safeguards (`ui/settings.slint`, `src/main.rs`)

1. **Overview Pane (`selected-rule-idx == -1`):**
   - Renders summary cards for all configured rules with reactive two-way bound checkboxes (`checked: r.enabled; toggled => root.toggle-rule-enabled(i, self.checked)`). Toggling updates `rules_model` in Rust without breaking two-way property bindings.
   - Includes **Snooze 15m**, **Reset Timer**, **Configure**, **Duplicate**, and **Delete** actions for each rule (with Snooze, Reset, Duplicate, and Delete disabled during active strict mode via `ui-locked-by-strict`). The Rule Editor header in the Child Rule tab provides matching **Snooze 15m** and **Reset Timer** buttons alongside the rule title and status badge.
   - Clicking **Snooze 15m** dispatches `DaemonCommand::SnoozeBreak { rule_id, minutes: 15 }` followed by an immediate telemetry refresh. Clicking **Reset Timer** dispatches `DaemonCommand::ResetRuleSession { rule_id }`, which calls `rt.reset_session(now)` to reset active timers, clear `snoozed_until`, set `daily_reset_at = Some(now)`, reset `daily_usage_seconds = 0`, clear `action_executed_today = false`, restart `Period` continuous session accumulation from 0s (`last_completed_at = Some(now)`), and isolate rollback flags (`can_rollback_completion = false`) so subsequent snoozes cannot leak and undo resets or finished/skipped breaks. Limit rule usage queries evaluate against `effective_start_utc = daily_reset_at.unwrap_or(start_of_day_utc)`, ignoring earlier events from the logical day.
   - Clicking **Duplicate** clones the target rule (`Rule::duplicate`), assigns a unique `id`, appends `" (Copy)"` to its name, appends it to `rules_model`, and immediately switches the view to the duplicated rule's configuration editor (`selected-rule-idx`).
   - Deleting a rule via `on_delete_rule` un-conditionally re-validates all remaining rules (`refresh_rule_regex_validation`), so deleting an invalid rule from the Overview tab immediately clears regex errors and re-enables saving.
2. **Global Sidebar Validation Error Indicator:**
   - Displays a red warning text block (`#f87171`) above the `Save & Apply Changes` button in the sidebar footer whenever `!root.time-format-valid || !root.rule-regex-valid`:
     - Highlights invalid strftime syntax in General Settings (`"⚠ Invalid time format in General Settings"`).
     - Highlights specific rule regex errors across any enabled rule (`"⚠ Rule '{name}': {error}"`), ensuring users understand why saving is disabled even while navigating other tabs. When editing a rule, regex errors are only evaluated if the rule is enabled (`win.get_rule_enabled() == true`), matching the Overview tab's enabled check so disabling an invalid rule in the editor immediately clears validation errors and unblocks saving.
3. **Two-Way Binding Synchronization:**
   - ComboBox components for Slint renderer display (`slint-renderer-display`), line color selection (`line-color-display`), and line size selection (`line-size-display`) are bound using two-way syntax (`current-value <=> property;`), preventing user selections from severing UI synchronization.

---

## 4. Unified Rule Evaluation Pipeline (`src/engine.rs` & `src/enforcer.rs`)

### A. Event Stream Separation & Multi-Browser Web Discovery (`poll_tick`, `src/aw_client.rs`)

* `poll_tick()` queries window events (`aw-watcher-window_`) into `all_events` and browser tab events (`aw-watcher-web*`) into a separate `web_events` vector. When `has_url_rules` is active, `Engine::poll_tick` confirms verified required local buckets once and executes `query_intersected_events` and `query_web_events` concurrently via `tokio::join!`, halving ActivityWatch polling latency.
* `AwClient` discovers all local bucket keys starting with `aw-watcher-web` (supporting simultaneous use of Chrome, Edge, Firefox, and Brave) while strictly ignoring synchronized remote buckets (`-synced` and `_synced`) across window, AFK, and web watchers. `AwClient` strictly requires verified local bucket IDs without `find_bucket` fallbacks, eliminating the risk of querying unverified or synced remote buckets. Bucket discovery automatically re-checks every 60 seconds to detect newly launched browsers, and local window/AFK bucket selection deterministically prefers matching hostnames and latest update timestamps.
* Construct queries that flood each discovered local web bucket individually (`flood(query_bucket(...))`), fold multiple web buckets via nested binary `concat` expressions (respecting ActivityWatch's 2-argument `concat(events1, events2)` constraint), wrap in `sort_by_timestamp(...)`, and intersect with `not-afk`. Direct deserialization into `Vec<Vec<AwEvent>>` avoids intermediate JSON tree allocations.
* Rules with `MonitorSelection::Urls` evaluate exclusively against `web_events`, preventing double-counting of browser screen time in `total_today_secs` or wildcard `"*"` executable rules.

### B. Three-Stage Evaluation (`evaluate_rule`)

For each enabled rule active in the current `(day_index, hour_index)` schedule slot:

1. **Progress Calculation & Interval Coalescing:**
* **`TriggerType::Period`:** Computes continuous active session duration (`session_duration`) via `calculate_continuous_session_for_rule` by clipping matching events against `last_completed_at`, coalescing overlapping or nested intervals via `merge_overlapping_intervals`, and walking backward until an AFK gap $\ge$ `reset_after_afk` is encountered. Threshold is `rule.period.to_seconds()`.
* **`TriggerType::Limit`:** Computes cumulative active duration since `logical_day_start` (`day_start_hour`) via `calculate_usage_for_rule`, coalescing matching intervals via `merge_overlapping_intervals`, and caches the result in `rt.daily_usage_seconds` for reuse in Live Status badges. Threshold is `rule.limit.to_seconds()`.
* **Active Window / URL Verification (`find_matching_window_for_rule`):** Confirms a matching visible, non-minimized, non-protected window is currently open. For `MonitorSelection::Urls`, requires a visible browser window (`enforcer::BROWSER_EXES`, covering Chrome, Edge, Firefox, Brave, Opera, Vivaldi, Chromium, Arc, Zen, LibreWolf, Waterfox, Floorp, and Thorium) and selects the most recent matching `aw-watcher-web` event ordered by `(event_end_utc(ev), ev.timestamp)` within the active recency window (`2 * poll_interval + 5s`), returning the extracted URL hostname. For `MonitorSelection::Executables` with empty or wildcard `"*"` targets, dynamically resolves the foreground process via `enforcer::get_foreground_app_name()` (returning `None` for destructive `CloseWindow` / `CloseTab` actions if no non-protected foreground window is active, preventing infinite closure loops on empty desktops). Enumeration in `enum_windows_callback` and `close_active_browser_tab` short-circuits Win32 process handle lookups before querying process names whenever `compiled_regex.is_some() && !title_matches`.


2. **Stage 1 — Pre-Action Warning (`warn_before > 0`):**
* When `threshold > 0 && warn_secs > 0 && warn_secs < threshold && elapsed >= threshold - warn_secs && elapsed < threshold`:
* Fires a pre-action warning once per cycle/day (`warned_current_cycle = true`). For `ShowNotification`, `CloseWindow`, `CloseTab`, `KillProcess`, and `LockWorkstation`, uses `&rule.message` whenever configured (falling back to default warning text if empty). For `ShowOverlay`, dispatches a clear upcoming-break countdown toast to keep pre-break warnings and fullscreen break screens distinct.


3. **Stage 2 — Grace Countdown (`grace_duration > 0`):**
* When `elapsed >= threshold`:
* If `effective_grace > 0`, starts `rt.grace_countdown = Some((effective_grace, effective_grace))` and displays `OverlayMode::HudTimer` counting down every second in `realtime_tick()`.
* When the countdown reaches `0`:
  * For `MonitorSelection::Urls`, builds `title_re` from `rt.last_matched_title_regex` and verifies against open browser windows. In-site tab navigation during an active grace countdown continuously refreshes `rt.last_matched_title`, `rt.last_matched_url`, and `rt.last_matched_title_regex`, and cleans up stale tab titles (`rt.set_last_matched_title(None)`) whenever the latest matching URL event contains an empty or whitespace-only title (during both normal evaluation and active grace countdowns). If the offending tab was closed or switched away from during the grace period, or if `last_matched_title_regex` is `None` (empty tab title), destructive actions (`CloseTab`, `CloseWindow`, `KillProcess`, `LockWorkstation`) are safely skipped, logging the closure and completing the cycle.


4. **Stage 3 — Action Execution (`execute_rule_action`):**
* Executes when `grace_countdown` reaches `0` (or immediately when `grace_duration == 0`):
* Overlay updates are reordered in `realtime_tick` so active test/HUD countdowns tick at the start of each second, ensuring newly spawned alerts retain their full duration without same-tick decrement.
* **`Show Overlay`:** Launches full-screen `OverlayMode::BreakEnforcer` across all monitors for `break_duration` (with 1.5s natural idle pause if `grace_duration == 0`, fullscreen D3D downgrade to Windows toast if `suppress_fullscreen_d3d` is enabled, and amber pause banner when physical input is detected). Records `prev_completed_at = last_completed_at` so snoozing restores the pre-break completion timestamp. While a `BreakEnforcer` overlay is displayed, incoming break overlays for different rules cannot replace it if the active break is strict and incoming is non-strict, if active is non-click-through and incoming is click-through, or if equal in strictness/click-through and active remaining duration $\ge$ incoming remaining duration. During countdown transitions (`transition_to_active_countdown_or_hide`), the engine prioritizes the active break rule that maximizes `(strict_mode, !click_through, remaining_seconds)`. Strict mode locks settings, tray actions, and command bypasses during both `active_break` and the 1.5s–15s `pending_break_since` window.
* **`Show Notification`:** Marks the cycle complete (`rt.complete_cycle(now)`) and dispatches either `OverlayMode::HudToast` (`NotificationMethod::Hud`) or a WinRT system toast (`NotificationMethod::Windows`) using the rule's formatted `message`. If the user clicks **Snooze** on the HUD toast, `rt.snooze(until)` restores `last_completed_at = prev_completed_at` so the notification re-triggers after the snooze expires.
* **`Show Statistics`:** Marks the cycle complete and opens the 24-Hour Screen Time Summary (`OverlayMode::HudStatus`), which caches historical hourly bins in `StatsHudCache` and live-updates on subsequent `poll_tick`s without extra ActivityWatch HTTP queries.
* **`Lock Workstation`:** Marks the cycle complete and calls `enforcer::lock_workstation()` (suppressing repeated lock calls on `Limit` rules when user idle time exceeds the poll interval).
* **`Close Tab`:** Marks the cycle complete and sends `Ctrl+W` (`enforcer::close_active_browser_tab`, synthesizing `VK_SHIFT` and `VK_MENU` key-ups beforehand to avoid `Ctrl+Shift+W`) to the matching foreground window (requiring browser executable and verified tab title match for `Urls`, strictly requiring `last_matched_title_regex` and never falling back to closing entire windows or all browser tabs).
* **`Close Window`:** Marks the cycle complete and posts `WM_CLOSE` (`enforcer::close_target_windows`) to matching visible non-minimized windows (requiring browser executable and verified tab title match for `Urls`, strictly requiring `last_matched_title_regex` and excluding protected shell windows and the `aw-enforcer` process).
* **`Kill Process`:** Marks the cycle complete and terminates matching processes via `enforcer::kill_target_processes` (excluding the `aw-enforcer` process).