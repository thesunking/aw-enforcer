# AW-Enforcer

**AW-Enforcer** is a lightweight, native Windows desktop application written in Rust that connects to [ActivityWatch](https://activitywatch.net/) to enforce customizable ergonomic breaks, application usage quotas, and screen-time rules. It runs persistently in the Windows system tray and uses hardware-accelerated [Slint](https://slint.dev/) UI components for configuration, HUD notifications, and multi-monitor overlays.

---

## Key Features

- **Configuration Import & Export (Backup & Transfer):**
  - Back up, restore, and share entire rule sets and daemon preferences as clean `.toml` files.
  - The **General Settings** tab provides dedicated **"Export Config..."** and **"Import Config..."** buttons.

- **Orthogonal Rule Engine (`Monitor` × `Trigger` × `Action`):**
  - **3 Monitor Modes:** Match all screen time (`*`), specific process executables (`.exe`), window title regular expressions (`Titles`), or active browser tab URL regular expressions.
  - **2 Trigger Types:**
    - **`Period`:** Triggers after a continuous active session duration.
    - **`Limit`:** Triggers when cumulative daily active usage reaches a quota.
  - **7 Enforcement Actions:**
    - **`Show Overlay`:** Displays a full-screen break overlay across all monitors that counts down only while you are physically idle.
    - **`Show Notification`:** Sends either an interactive top-right `HUD` toast card or a branded native `Windows` Action Center toast (displaying "AW-Enforcer" headers via registered AUMID).
    - **`Show Statistics`:** Opens a 24-Hour Screen Time Summary HUD with an hourly activity histogram. Supports optional click-through mode (`click_through`) to interact with underlying windows while monitoring stats.
    - **`Lock Workstation`:** Immediately locks the Windows session. Repeat triggers for `Limit` rules enforce a minimum 15-second grace period so users can save work or snooze after unlocking.
    - **`Close Tab`:** Closes the active tab of the matching foreground window via synthesized `Ctrl+W` after explicitly releasing modifier and Windows keys (`VK_SHIFT`, `VK_MENU`, `VK_LWIN`, `VK_RWIN`), validating `SendInput` return status for UIPI diagnostics. Active tab closure strictly requires a matching foreground window to ensure background windows or unrelated applications are never sent keystrokes.
    - **`Close Window`:** Closes matching visible top-level windows (including minimized taskbar windows), verifying `PostMessageW(WM_CLOSE)` success to log UIPI permission blocks.
    - **`Kill Process`:** Terminates matching processes via ToolHelp snapshots, verifying running processes even when minimized or resident in the system tray.

- **Three-Stage Enforcement Pipeline:**
  1. **Stage 1 — Pre-Action Warning:** Alerts you ahead of time (via HUD toast or native Windows Action Center notification) before a break or quota enforcement triggers. Configurable delivery method (HUD vs Windows), duration, and countdown visibility across all rule actions. Supports custom formatted message templates.
  2. **Stage 2 — Grace Countdown:** Displays a non-intrusive top-right `HudTimer` progress card counting down the final seconds so you can save your work. Window minimization does not bypass enforcement: `Close Window` closes minimized taskbar windows, `Kill Process` terminates background/tray targets, and `Lock Workstation` executes reliably regardless of active desktop state. Repeat executions of `Limit` + `Lock Workstation` enforce a minimum 15-second grace countdown to prevent immediate lock-out loops upon unlocking the workstation. Dismissing a destructive grace timer hides the banner without aborting enforcement (requiring `Snooze (5m)` to defer action).
  3. **Stage 3 — Action Execution:** Executes the configured rule action when the countdown completes.
  - **Universal Strict Mode:** Supported across all rule actions to hide Skip/Snooze controls and lock settings/tray bypasses during grace countdowns and break enforcement (while warning/notification toasts can still be acknowledged and dismissed).

- **Tri-State System Tray Visual Indicator:**
  - **Sky Blue (`#38bdf8`):** Normal active monitoring with rules active and within schedule.
  - **Slate Gray (`#94a3b8`):** All rules paused (`Pause All Rules`), indicating dormant monitoring.
  - **Alert Red (`#ef4444`):** At least one active rule is currently in a warning period, grace countdown, or active break enforcement.
  - Icon changes are transition-only, eliminating redundant Win32 GDI icon allocation and redraw overhead during live telemetry ticks.

- **Customizable Appearance & 24-Hour Statistics HUD:**
  - Full control over overlay opacity, accent color, background color, and text color in **General Settings**.
  - Draggable non-fullscreen HUD overlay cards (`HudToast`, `HudTimer`, `HudStatus`).
  - Configurable 24-hour histogram bin count (1–96) and bin size (1–1440 minutes).
  - One-click rule alert previewing accessible for all rule configurations directly in the Rule Editor.
  - Rich text formatting.

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

---

## Project Structure

| File | Responsibility |
| --- | --- |
| `src/main.rs` | Single-instance mutex & activation event polling, overlay action dispatch, main Slint timer loop, and Tokio async runtime setup |
| `src/settings_window.rs` | `SettingsWindow` lifecycle management, full callback wiring, live telemetry synchronization, ActivityWatch health polling, and input validation / model conversion helpers |
| `src/tray.rs` | System tray menu construction, `TrayManager` lifecycle, `TrayAction` event polling, tri-state RGBA icon asset caching (`create_default_tray_icon`, `create_paused_tray_icon`, `create_alert_tray_icon`), and dynamic status/tooltip synchronization |
| `src/config.rs` | TOML configuration schema, serde serialization, `strftime` validation, duration/quota bounds clamping, target/schedule normalization, unique rule ID enforcement, and `Config::sanitize()` / `Rule::sanitize()` safety invariants |
| `src/aw_client.rs` | Asynchronous ActivityWatch REST client (`/api/0/buckets/` dynamic discovery, thread-safe hostname ranking, and `filter_period_intersect` queries) |
| `src/engine.rs` | Real-time 1s countdown loop, periodic ActivityWatch polling loop, command dispatch, and 3-stage rule evaluation pipeline |
| `src/engine/intervals.rs` | Stateless interval coalescing (`merge_overlapping_intervals`), $O(\log N)$ binary-searched overlap calculation (`compute_overlap_from_merged_intervals`), browser window web event intersection, and zero-allocation backward continuous session traversal |
| `src/engine/matcher.rs` | URL and executable predicates, URL host extraction (`extract_url_host`), recency cutoff and active window lookup, and rule-filtered session/usage calculation |
| `src/engine/runtime.rs` | Ephemeral per-rule lifecycle state machine (`RuleRuntime`), daily quota rollovers, regex compilation caching, and state-preserving snooze logic |
| `src/engine/stats_hud.rs` | 24-hour screen time status HUD cache (`StatsHudCache`), bin-start alignment, live rollover shifting, and status overlay presentation |
| `src/engine/preview.rs` | Synthetic test overlay triggers (`trigger_test_overlay`) and live rule alert previews (`preview_rule_alert`) for the UI editor |
| `src/enforcer.rs` | Protected Win32 window enumeration (`EnumWindows`), `WM_CLOSE` dispatch with UIPI checks, `Ctrl+W` keystroke synthesis with Win/modifier release, AUMID registry branding, ToolHelp process iteration, workstation lock, and physical input idle tracking (`GetLastInputInfo`, `GetTickCount`) |
| `src/overlay.rs` | Multi-monitor Slint `OverlayWindow` lifecycle, priority hierarchy, Win32 extended styles (`WS_EX_TOPMOST`, `WS_EX_TRANSPARENT`), and DWM composition |
| `src/formatter.rs` | Message template placeholder interpolation, case-insensitive attribute toggle, grace-aware preview context, line-level attribute/heading parser, fallback rule templates, and UI snooze badge formatting (`format_snooze_badge`) |
| `src/autostart.rs` | Windows Registry (`HKCU\...\Run`) startup registration |
| `ui/main.slint` | Slint component root exporting `SettingsWindow`, `OverlayWindow`, `RuleView`, and `FormattedParagraph` |
| `ui/settings.slint` | Settings window shell, sidebar navigation, sub-tab routing, save/discard commit actions, and strict mode lock presentation |
| `ui/settings_general_tab.slint` | General preferences tab component (`GeneralTab`): autostart, theme colors/opacity, Slint renderer, 24h HUD bins/quotas, and backup/transfer |
| `ui/settings_rules_overview.slint` | Rules overview list component (`RulesOverview`): rule cards, ordering, enablement, snooze, reset, duplicate/delete actions, and dynamic Pause/Unpause All Rules button |
| `ui/settings_rule_editor.slint` | 5-section rule editor component (`RuleEditor`): targets, triggers, actions, message template formatting toolbar, preview card, and schedule matrix |
| `ui/settings_live_status_tab.slint` | Live telemetry status tab component (`LiveStatusTab`): ActivityWatch server connection, real-time screen-time metrics, and embedded `ActivityHistogram` |
| `ui/settings_widgets.slint` | Reusable settings widgets: `RuleView` schema, `SectionCard`, `FormRow`, `ColorSettingRow`, `SidebarNavItem`, `StatusBadge`, and format buttons |
| `ui/overlay.slint` | Overlay window shell (`OverlayWindow`) routing between fullscreen breaks, HUD toast, HUD timer, and HUD status modes |
| `ui/break_enforcer.slint` | Full-screen break enforcer overlay card (`BreakEnforcer`) with countdown timer, idle detection banner, and skip/snooze controls |
| `ui/hud_toast.slint` | Interactive HUD toast card (`HudToast`) with notification header, rich-text body, snooze dropdown, and acknowledge action |
| `ui/hud_timer.slint` | HUD timer / grace countdown card (`HudTimer`) with warning header, countdown, progress bar, and snooze/dismiss actions |
| `ui/activity_histogram.slint` | Shared 24-hour activity timeline histogram component (`ActivityHistogram`) displaying status badges, dynamic capacity-scaled timeline bars, decimated time labels, and active/target screentime summary |
| `ui/overlay_widgets.slint` | Shared overlay widgets: draggable window handle (`WindowDragArea`) and styled ratio progress bar (`ProgressBar`) |
| `ui/formatted_text.slint` | Reusable rich-text paragraph renderer component with alignment-aware text underlines |
| `ui/executable_selector.slint` | Executable chip list, file browser trigger, and running process selector |
| `ui/duration_input.slint` | Reusable numeric value + `Hr`/`Min`/`Sec` unit selector |
| `ui/schedule_grid.slint` | Interactive weekly schedule matrix with click-and-drag painting and presets |

---

## License

This project is licensed under the [MIT License](https://opensource.org/license/mit).