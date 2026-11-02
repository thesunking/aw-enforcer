# AW-Enforcer

**AW-Enforcer** is a lightweight, native Windows desktop application written in Rust that connects to [ActivityWatch](https://activitywatch.net/) to enforce customizable ergonomic breaks, application usage quotas, and screen-time rules. It runs persistently in the Windows system tray and uses hardware-accelerated [Slint](https://slint.dev/) UI components for configuration, HUD notifications, and multi-monitor overlays.

---

## Key Features

- **Configuration Import & Export (Backup & Transfer):**
  - Back up, restore, and share entire rule sets and daemon preferences as clean `.toml` files.
  - The Settings **General Settings** tab provides dedicated **"Export Config..."** and **"Import Config..."** buttons backed by native `rfd::FileDialog` pickers.
  - Imported configurations automatically run through full safety validation (`Config::sanitize()`, including port clamping, minimum 2s poll intervals, opacity clamping, excessive threshold clamping, duration minimums, and `sanitize_rules()`) and are loaded into the UI with an unsaved changes flag so you can review imported rules before saving and applying them.

- **Orthogonal Rule Engine (`Monitor` × `Trigger` × `Action`):**
  - **3 Monitor Modes:** Match all screen time (`*`), specific process executables (`.exe`), window title regular expressions (`Titles`), or active browser tab URL regular expressions (`Urls` dynamically discovered across all installed browsers via `aw-watcher-web*`, strictly excluding non-local synced buckets ending with `-synced` or `_synced`).
  - **2 Trigger Types:**
    - **`Period`:** Triggers after a continuous active session duration (with interval coalescing and configurable AFK reset threshold).
    - **`Limit`:** Triggers when cumulative daily active usage reaches a quota (anchored to a configurable logical day-start hour, e.g., `04:00 AM`).
  - **7 Enforcement Actions:**
    - **`Show Overlay`:** Displays a full-screen break overlay across all connected monitors that counts down only while you are physically idle.
    - **`Show Notification`:** Sends either an interactive top-right `HUD` toast card (with configurable duration, optional countdown timer, and snooze buttons) or a native `Windows` Action Center toast.
    - **`Show Statistics`:** Opens the draggable 24-Hour Screen Time Summary HUD with an hourly activity histogram that live-updates on every poll tick while open without extra ActivityWatch requests (supporting multi-hour bins and intermediate multi-bin transition interpolation).
    - **`Lock Workstation`:** Immediately locks the Windows session (`LockWorkStation`), with idle detection to avoid repeated lock calls while already away.
    - **`Close Tab`:** Sends `Ctrl+W` to close the active tab of the matching foreground window without closing the entire browser window if focus shifts (releasing physical Shift/Alt keys beforehand and verifying browser executable and tab title match for `Urls` rules; strictly requiring a verified tab title regex to avoid closing all browser windows on empty titles).
    - **`Close Window`:** Gracefully closes matching visible, non-minimized windows via `WM_CLOSE` (requiring browser executable and verified tab title match for `Urls` rules without unguarded fallbacks).
    - **`Kill Process`:** Terminates matching background and foreground processes (restricted to rules targeting specific `.exe` names).

- **Three-Stage Enforcement Pipeline:**
  1. **Stage 1 — Pre-Action Warning (`Warn Before Action`):** Alerts you ahead of time (via HUD toast or native Windows Action Center notification) before a break or quota enforcement triggers. Supports custom formatted message templates across all rule actions (preserving unambiguous upcoming-break countdown toasts for `Show Overlay`). Displays an inline UI warning in the Rule Editor whenever `Warn Before Action` is configured greater than or equal to the rule's trigger threshold (`warn-before-exceeds-threshold`). Snoozing a warning preserves cycle state so snooze expiration does not re-emit duplicate warnings for the same cycle while below threshold.
  2. **Stage 2 — Grace Countdown (`Grace Countdown`):** Displays a non-intrusive top-right `HudTimer` progress card counting down the final seconds so you can save your work, rendering custom message templates across quota enforcement actions (`Close Window`, `Close Tab`, `Kill Process`, `Lock Workstation`). For URL and executable rules, closing or navigating away from the target tab or app during the grace period safely aborts destructive actions (continuously refreshing tab titles and full matched URLs during in-site navigation, and clearing cached URL target state if no recent active browser web event exists). Destructive URL actions strictly require a non-empty tab title regex so empty titles safely complete the cycle without unguarded window closures. For `Close Tab` actions, target match verification strictly requires the foreground window to be an active browser (`enforcer::is_foreground_browser()`).
  3. **Stage 3 — Action Execution:** Executes the configured rule action when the countdown completes. Newly spawned HUD cards and break overlays retain their full duration without same-tick countdown decrement, with initial progress bar ratios correctly rendered immediately on launch.

- **Ergonomic, Shell & Anti-Bypass Safeguards:**
  - **Single-Instance IPC, Window Activation & Tray UX:** Enforces a persistent Win32 mutex (`Local\AW_Enforcer_SingleInstance_Mutex`) and signals a named Win32 event (`Local\AW_Enforcer_Activate_Event`) when a second instance is launched, automatically restoring and focusing the running instance's Settings window. Double-clicking the system tray icon immediately opens or restores the Settings window. The tray tooltip displays live snooze countdowns (e.g., `AW-Enforcer (Snoozed 15m) — Today: 2h 15m`), and rule status badges in the overview display `"Snoozed ({rem}m)"` during active snoozes. The tray menu contextually toggles between `"Snooze All (15m)"` and `"Resume Snoozed Rules"` whenever snoozes are active (`any_snoozed`), allowing one-click early resumption of paused enforcement (`DaemonCommand::ClearSnoozeAll`). All tray and UI state-mutating commands (`SnoozeAll`, `SetPauseAll`, `SkipBreak`, `DismissBreakOverlay`, `SnoozeBreak`, `ResetRuleSession`, `ClearSnoozeAll`, and `ReloadConfig`) trigger an immediate telemetry and badge refresh (`self.poll_tick().await`), updating tray tooltips, menu toggles, and UI badges instantly without waiting for the next poll interval.
  - **Dynamic Multi-Browser Web Discovery, Concurrent Queries & Active Window Interval Intersection:** `AwClient` discovers local `aw-watcher-web*` buckets across Chrome, Edge, Firefox, and Brave, periodically refreshing bucket discovery every 60 seconds to detect newly opened browsers on the fly while strictly ignoring synchronized remote buckets (`-synced` and `_synced`). Each discovered local web bucket is flooded individually (`flood(query_bucket(...))`) and combined via nested binary `concat` expressions inside `sort_by_timestamp` (adhering to ActivityWatch's 2-argument `concat` arity). `AwClient` strictly requires verified local bucket IDs without `find_bucket` fallbacks. Before polling, `AwClient::prepare_poll_buckets` verifies required buckets and refreshes web buckets if needed; if offline or unverified, poll ticks bypass queries immediately without redundant `/api/0/buckets/` calls or `RwLock` contention. When URL rules are active and buckets are verified, `Engine::poll_tick` executes window and web queries concurrently via `tokio::join!`, and intersects `web_events` with active browser window intervals (`filter_web_events_by_active_browser_windows`). This ensures background or secondary-monitor browser tabs never accumulate `Urls` quota or continuous session time while the user is actively working in non-browser applications (e.g., VS Code).
  - **Expanded Browser Support, Short-Circuited Window Scans & PID Caching:** `enforcer::BROWSER_EXES` natively supports Chrome, Edge, Firefox, Brave, Opera, Vivaldi, Chromium, Arc, Zen, LibreWolf, Waterfox, Floorp, and Thorium. Win32 window callbacks and tab closures short-circuit immediately when window titles fail regex matching, avoiding process handle queries. During `scan_windows`, a per-enumeration PID process cache (`pid_cache: HashMap<u32, Option<String>>`) prevents redundant `OpenProcess` and executable name queries across multiple windows sharing the same process. Wildcard executable rules (`"*"`) resolve foreground applications via `enforcer::get_foreground_app_name()` to prevent runaway window-closing loops on empty desktops, URL rules track full matched URLs (`last_matched_url`) for path-based regexes (e.g. `youtube\.com/shorts`), and destructive URL actions strictly require verified tab title regexes to avoid closing unrelated browser windows.
  - **Resilient Local Bucket Selection:** When multiple local window or AFK buckets exist (e.g. after computer renames), automatically compares `last_updated` and `created` metadata timestamps and local machine hostname (`COMPUTERNAME`) to select the active bucket deterministically.
  - **Double-Counting Prevention & Event Recency Ordering:** Automatically sorts and merges overlapping or adjacent ActivityWatch events (`merge_overlapping_intervals`) across continuous session calculations, cumulative daily usage quotas, and 24-hour histogram binning. Evaluates event recency based on `(event_end_utc, timestamp)` to ensure active sessions are never masked by shorter, already-ended background intervals.
  - **Idle-Aware Break Timer:** Full-screen breaks wait for a natural 1.5-second pause in typing/mouse movement before appearing and pause their countdown (`⏸ Input detected — timer paused until you step away`) whenever physical input is detected.
  - **Fullscreen / D3D Presentation Suppression:** Optionally detects fullscreen Direct3D games, presentations, or Windows Focus Assist (`SHQueryUserNotificationState`) and downgrades intrusive overlays to passive notifications.
  - **Strict Mode & Top-Priority Break Lockout:** When enabled on a `Show Overlay` rule, gives `BreakEnforcer` top priority, hides Skip/Snooze buttons on the break screen, disables system tray and command bypasses (including `DismissBreakOverlay`, `SnoozeAll`, and `SetPauseAll`), and locks settings edits behind a 15-second confirmation countdown while actively enforcing or during the 1.5s natural break pause (`pending_break_since`). All Child Rule Editor controls across Sections 1 through 5 (including `ExecutableSelector`, `DurationInput`, `ScheduleGrid`, formatting toolbar buttons, and rule Add/Duplicate/Delete/Toggle actions) are disabled in the UI via `enabled: !root.ui-locked-by-strict` and guarded in `src/main.rs` callbacks by `is_ui_locked_by_strict`. Active strict break screens cannot be clobbered by non-strict or lower-priority break overlays, and countdown transitions prioritize rules maximizing `(strict_mode, !click_through, remaining_seconds)`.
  - **Dynamic Rule Lifecycle & Long Process Paths:** Disabling, deleting, or going off-schedule on any rule immediately resets its active countdown timers and clears or transitions any active overlay. Process resolution uses a 1024-character buffer to support deep Windows executable paths.
  - **Protected Process & Windows Shell Guards:** Automatically excludes the `aw-enforcer` process and Windows shell surfaces (`Progman`, `WorkerW`, `Shell_TrayWnd`, `Shell_SecondaryTrayWnd`, `NotifyIconOverflowWindow`, `TopLevelWindowForOverflowXamlIsland`, `MultitaskingViewFrame`) from wildcard window closing, tab closing, and process termination.
  - **Pause All Rules Indefinitely:** Toggleable from both General Settings (`pause_all_rules`) and the system tray context menu (`Pause All Rules` / `Resume All Rules`).
  - **Click-Through Overlays:** Optionally allows mouse clicks and keystrokes to pass through the `Show Overlay` screen (`WS_EX_TRANSPARENT | WS_EX_NOACTIVATE`).
  - **Sub-Second Session Interval Accumulation & Clamping:** Continuous session duration calculation accumulates sub-second intervals in milliseconds before converting to seconds and clamps event boundaries to current UTC time, preventing sub-second event truncation.
  - **Dismissing Pending Breaks Completes Cycle:** Dismissing a break during its natural 1.5-second idle pause marks the cycle completed (`rt.complete_cycle(now)`), preventing it from immediately re-triggering on the next poll tick.
  - **Strict Mode Unlock Reset on Window Close:** When `destroy_windows_on_hide = false`, closing the Settings window immediately resets `strict_unlocked = false` so strict mode lockouts cannot be bypassed across window re-openings.
  - **Action Center Plain-Text Sanitization:** Formats native Windows Action Center toasts as clean plain text via `formatter::format_plain_message`, stripping Markdown heading prefixes (`# `, `## `) and attribute blocks (`[color=... size=... bold]`) while preserving interpolated placeholders.
  - **Off-Screen HUD Clamping & Clean Initial Placement:** Pure positioning helper `clamp_or_reset_status_pos` detects if the 24h Stats HUD window has drifted off-screen (e.g. after disconnecting a secondary display) and resets it to the primary display top-right corner. Starting `OverlayMode::HudStatus` initializes DPI-scaled geometry against the primary display work area without inheriting stale window coordinates from other overlay modes or unscaled metrics, ensuring the HUD starts fully on-screen, with position tracking cleanly reset on overlay close.
  - **Settings Window Sizing & Multi-Row Toolbar Layouts:** `SettingsWindow` specifies `preferred-width: 1240px` (`min-width: 1080px`, `preferred-height: 780px`) so that the window opens with sufficient width to display all buttons and cards. Test overlay controls and message template quick-insert variable chips use multi-row layouts with text elision on overview cards, ensuring all buttons remain visible without clipping.

- **Display, HiDPI, Multi-Monitor & DWM Compositing:**
  - **DPI-Aware Overlay Sizing:** Dynamically reads window scale factors to scale HUD card geometry and screen margins accurately on high-DPI displays.
  - **Multi-Monitor Hotplug & Countdown State Preservation:** Dynamically detects monitor connects, disconnects, and resolution changes during active full-screen breaks (`apply_pending_win32_styles`, returning early if `OverlayMode::Hidden`), automatically re-covering all displays without user intervention. Actively synchronizes `remaining_seconds` and `duration_seconds` in `current_mode` across countdown updates so hotplug respawns retain the exact current countdown without resetting.
  - **Deduplicated Win32 Placement & Slint Window Activation:** Batches window styling, DWM blur-behind, geometry, and visibility into a single authoritative update pipeline with re-entrancy guards (`is_updating_placements`), eliminating recursive hotplug checks and stack overflows during multi-monitor break setup while ensuring Slint component presentation (`win.show()`) is activated alongside Win32 window displays for both fullscreen breaks and floating HUD cards (`HudToast`, `HudTimer`, `HudStatus`).
  - **DWM Client Frame Reset:** Properly resets DWM frame client margins (`DwmExtendFrameIntoClientArea`) on reused overlay windows when transitioning from fullscreen break overlays to HUD cards.

- **Performance & Idle Memory Optimization:**
  - **Capped Tokio Worker Threads:** Builds the Tokio multi-threaded runtime with 2 worker threads, eliminating idle thread overhead on high-core CPUs.
  - **Direct JSON Event Stream Deserialization:** Directly parses ActivityWatch query responses into typed event collections without intermediate `serde_json::Value` allocations or cloning.
  - **Concurrent Window & Web ActivityWatch Queries:** When URL monitoring rules are active, `Engine::poll_tick` runs window (`query_intersected_events`) and web (`query_web_events`) ActivityWatch queries concurrently via `tokio::join!`, halving polling latency and network round-trip delays.
  - **Slint Rendering Backend (`slint_renderer`):** Switch between OpenGL hardware acceleration (`renderer-femtovg`, default) and lightweight CPU software rendering (`renderer-software`) directly from the Settings **Performance** section. CPU rendering significantly reduces GPU VRAM and overall memory footprint in resource-constrained or virtualized environments (requires application restart).
  - **Destroy Windows when Hidden (`destroy_windows_on_hide`):** Frees full-screen OpenGL/GPU surfaces and Slint textures when overlay alerts or the Settings window are closed or hidden (`destroy_windows_on_hide = true`, default); when disabled, windows remain cached in memory for instant display without cold-start initialization.
  - **Event-Driven UI Dispatch & Smart Polling:** `OverlayAction` updates are pushed from the Tokio engine directly onto the Slint event loop via `slint::invoke_from_event_loop` without busy-polling. Daily quota usage is cached per poll tick (`daily_usage_seconds`), and `/api/0/info` connection checks and UI model updates pause automatically when the Settings window is closed.
  - **Zero-Allocation Win32 Process Resolution & Matching:** Uses `QueryFullProcessImageNameW` on stack buffers and zero-allocation `.exe` stem matching (`exe_name_matches_targets`). Process termination and running-app discovery share a single lightweight Win32 ToolHelp snapshot iterator (`for_each_process`) with zero external process-enumeration crates.
  - **Typed Event Schema (`AwEventData`):** Deserializes ActivityWatch event payloads into compact, strongly-typed Rust structs (`app`, `title`, `url`, `status`) rather than dynamic JSON maps, minimizing heap allocation and memory fragmentation over 24-hour periods.
  - **Optimized Release Profile:** Compiled with Link-Time Optimization (`lto = true`), single codegen units, debug symbol stripping, and size-optimized code generation (`opt-level = "s"`) to minimize resident memory page mapping in RAM.
  - **Bucket Discovery Deduplication, Safe Mutex Recovery & Query Escaping:** `AwClient::prepare_poll_buckets` verifies required buckets and throttles web bucket checks to 60-second intervals before polling starts. When ActivityWatch is offline, offline ticks execute at most 1 `/api/0/buckets/` request (down from 3) and bypass queries without `RwLock` write-lock contention or `tokio::join!` query races. All mutex accesses on `last_web_bucket_check` use poisoning recovery (`.unwrap_or_else(|e| e.into_inner())`) to prevent daemon crashes. Bucket identifiers escape backslashes and double quotes (`escape_bucket_id`) before query string interpolation.
  - **AwClient Connection & Bucket Cache Preservation:** `AwClient` tracks configured port and API key (`matches_config`), preserving HTTP connection pools and verified bucket discovery across configuration reloads and saves unless connection settings genuinely change.
  - **Deduplicated Tab Title Regex Compilation:** `RuleRuntime::set_last_matched_title` normalizes whitespace/empty titles to `None` and short-circuits when titles are unchanged, avoiding recompilation of title regular expressions on every poll tick.
  - **Zero-Allocation Protected Window Class Check:** Window class inspection compares UTF-16 buffers directly against static UTF-16 constants for shell surfaces (`Progman`, `WorkerW`, `Shell_TrayWnd`, `Shell_SecondaryTrayWnd`, `NotifyIconOverflowWindow`, `TopLevelWindowForOverflowXamlIsland`, `MultitaskingViewFrame`), eliminating heap `String` allocations during `EnumWindows`.
  - **Immediate Telemetry & Badge Refresh on Reload:** Applying settings or reloading configuration triggers an immediate `self.poll_tick().await`, refreshing live status badges and tray tooltip instantly without waiting for the next polling interval.

- **Rich Message Editor & Live Preview:**
  - Customize break and notification messages with Markdown headings (`#`, `##`) with full inline formatting attribute support (e.g. `# [color=#38bdf8 italic] Heading`), line-level styling (`[bold italic underline color=#38bdf8 size=18]`), and dynamic runtime placeholders: `{rule_name}`, `{app}` / `{app_name}` (retaining URL hostnames for URL rules), `{session_duration}`, `{break_time}`, `{used}`, `{limit}`, and `{remaining}`.
  - **Single-Pass Placeholder Interpolation (`MessageContext::interpolate`):** Scans templates in a single pass with pre-allocated buffer capacity (`template.len() + 32`), substituting known placeholders without re-scanning inserted values. This prevents cross-placeholder injection (preventing window titles containing `{limit}` or other tags from being recursively expanded), supports `{app_name}` as an alias for `{app}`, and leaves unknown tags or unclosed braces intact.
  - **Unified Live Preview Placeholder Calculation (`build_preview_context`):** Message preview placeholder calculation is centralized in `formatter::build_preview_context()`, ensuring identical calculation across live Settings editor previews and dispatched preview alerts (clamping `remaining` to `warn_before.min(threshold)` or fallback `300.min(threshold)`, and computing `used` as `threshold.saturating_sub(remaining)`). Duration placeholders use clean short-string notation (`DurationConfig::to_short_string()`, e.g. `"50m"`, `"10m"`, `"1h"`, `"5m"`) matching runtime overlays 1:1, format realistic `{remaining}` quota durations, and reflect left alignment for notifications and center alignment for full-screen breaks.
  - **Universal Message Templates & Live Formatting Toolbar:** Customize message templates across all rule actions — visible whenever `cur-action == "Show Overlay" || cur-action == "Show Notification" || root.rule-warn-before-val > 0 || root.rule-grace-val > 0`, enabling custom warning and grace countdown templates for quota enforcement rules (like Rule 2, *"Weeknight Gaming Limit"*). If a rule's template message is left empty or whitespace-only, `get_effective_rule_message` automatically provides canonical fallback templates across fullscreen breaks, HUD toasts, and Windows Action Center notifications (`rule_message_fallback`).
  - **"Preview This Rule's Alert" Button:** Test the selected rule's exact custom template, styling, click-through, and countdown duration with one click without having to wait for actual rule thresholds. Automatically previews `HudTimer` for enforcement actions with grace countdowns, `HudToast` for notifications and pre-action warnings, `BreakEnforcer` for full-screen breaks, and `HudStatus` (24h Screen Time Summary) for `Show Statistics` rules without grace or warnings.

- **Interactive Rule Management & Real-Time Validation:**
  - **Manual Executable Input & Wildcard Guidance:** In addition to browsing and selecting from active processes, `ExecutableSelector` provides a manual text input field with Enter/Add button support, automatic `.exe` suffix normalization, and case-insensitive deduplication, alongside an informative wildcard indicator (`"All applications monitored (* wildcard) — add specific .exe names below to restrict."`).
  - **Live State & Telemetry Synchronization:** Reopening a cached Settings window immediately synchronizes current active apps, session duration, screentime, and per-rule live status badges from cached engine state. Saving settings immediately reflects all sanitized and clamped configuration values (such as `aw_port` clamped to `1..=65535`, `poll_interval_seconds` minimum `2`, duration minimums, and `OverlayConfig` hex colors validated via `formatter::parse_hex_color`) back to the UI, while closing or discarding changes restores the last saved configuration and preserves live rule status badges. General Settings aligns `SpinBox` bounds (`aw_port` minimum `1`, `poll_interval_seconds` minimum `2`, `daily-target-minutes` minimum `1`), dynamically clamping `threshold-excessive-minutes` to be at least `daily-target-minutes`.
  - **Multi-Rule Syntax Validation & Disabled Rule Bypass:** Live regex syntax checking on `Titles` and `Urls` rules with real-time error messages in the editor and an aggregate warning banner in the left sidebar above `Save & Apply Changes`. Regex errors on disabled rules are ignored, so unchecking "Rule Enabled" in either the Rule Editor or Overview immediately clears validation errors and unblocks saving.
  - **Per-Rule Snooze & Session Reset Controls:** Each rule card in the Overview tab and the Child Rule Editor header features compact **Snooze 15m** and **Reset Timer** buttons (disabled during active strict mode). Snoozing pauses that specific rule for 15 minutes. Reset Timer calls `RuleRuntime::reset_session(now)`, restarting continuous session accumulation for `Period` rules (`last_completed_at = now`), resetting daily usage quotas for `Limit` rules (`daily_reset_at = now`, `daily_usage_seconds = 0`, `action_executed_today = false`), and isolating rollback flags (`can_rollback_completion = false`) so subsequent snoozes cannot leak and undo completed/skipped breaks or manual session resets. Acknowledging a Stage 3 `HudToast` or letting it auto-expire to 0 immediately clears `can_rollback_completion = false`, ensuring completed notifications cannot be reverted by subsequent snoozes.
  - **Overview Rule Management (Toggle, Duplicate & Delete):** Toggle rule activation directly from the Overview list via two-way bindings, duplicate rules with all properties cloned and view automatically switched to the new rule's editor, and revalidate regexes on rule deletion to instantly clear blocking validation errors.
  - **7×24 Weekly Active Hours Schedule:** Interactive click-and-drag hourly matrix per rule with one-click presets (`Work Hours`, `Evenings`, `Select All`, `Clear All`).

---

## Prerequisites

1. **Windows 10 or Windows 11** (64-bit).
2. **[ActivityWatch](https://activitywatch.net/)** running locally (default port `5600`):
   - `aw-watcher-window` and `aw-watcher-afk` (installed by default with ActivityWatch) are required for `Executables` and `Titles` rules.
   - `aw-watcher-web` browser extension is required if you create rules using the `Urls` selection mode (dynamically discovered even if the browser starts after `aw-enforcer`).
3. **Rust Toolchain** (1.75+ recommended, `x86_64-pc-windows-msvc`) to build from source.

---

## Building & Running

Clone the repository and build with Cargo:

```powershell
# Run in development mode
cargo run

# Build optimized release binary
cargo build --release

```

The compiled executable will be located at `target\release\aw-enforcer.exe`.

### Command-Line Arguments

* `--autostart` / `--minimized`: Launches AW-Enforcer silently in the system tray without opening the Settings window on startup (`--autostart` is automatically configured in `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` when **Start on Windows Login** is enabled).
* `--test-break`: Immediately triggers a sample full-screen break overlay on launch for visual testing.

### Running Tests

```powershell
cargo test

```

---

## Configuration

AW-Enforcer automatically loads and saves its configuration to:

```text
%LOCALAPPDATA%\aw-enforcer\config.toml

```

### Example `config.toml`

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

```

---

## Project Structure

| File | Responsibility |
| --- | --- |
| `src/main.rs` | Single-instance mutex, system tray menu, event-driven Slint UI dispatch, and Tokio runtime setup |
| `src/config.rs` | TOML configuration schema, serde serialization, `strftime` validation, and `Config::sanitize()` / `Rule::sanitize()` safety invariants |
| `src/aw_client.rs` | Asynchronous ActivityWatch REST client (`/api/0/buckets/` dynamic discovery and `filter_period_intersect` queries) |
| `src/engine.rs` | Real-time 1s countdown loop, periodic ActivityWatch polling loop, interval-coalesced session/quota calculation, and 3-stage rule evaluation |
| `src/enforcer.rs` | Protected Win32 window enumeration (`EnumWindows`), `WM_CLOSE` dispatch, `Ctrl+W` keystroke synthesis, shared ToolHelp process iteration, and workstation lock |
| `src/overlay.rs` | Multi-monitor Slint `OverlayWindow` lifecycle, priority hierarchy, Win32 extended styles (`WS_EX_TOPMOST`, `WS_EX_TRANSPARENT`), and DWM composition |
| `src/formatter.rs` | Message template placeholder interpolation and line-level attribute/heading parser |
| `src/autostart.rs` | Windows Registry (`HKCU\...\Run`) startup registration |
| `ui/settings.slint` | Settings window, sidebar navigation, 5-section rule editor, and interactive 7×24 schedule matrix |
| `ui/overlay.slint` | Full-screen break enforcer, HUD toast, HUD grace countdown timer, and 24-hour screen time histogram cards |
| `ui/formatted_text.slint` | Reusable rich-text paragraph renderer component |
| `ui/executable_selector.slint` | Executable chip list, file browser trigger, and running process selector |
| `ui/duration_input.slint` | Reusable numeric value + `Hr`/`Min`/`Sec` unit selector |
| `ui/schedule_grid.slint` | Interactive 7×24 weekly schedule matrix with click-and-drag painting and presets |

---

## License

This project is licensed under the [MIT License](https://opensource.org/license/mit).