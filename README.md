# AW-Enforcer

**AW-Enforcer** is a lightweight, native Windows desktop application written in Rust that connects to [ActivityWatch](https://activitywatch.net/) to enforce customizable ergonomic breaks, application usage quotas, and screen-time rules. It runs persistently in the Windows system tray and uses hardware-accelerated [Slint](https://slint.dev/) UI components for configuration, HUD notifications, and multi-monitor overlays.

---

## Key Features

- **Orthogonal Rule Engine (`Monitor` × `Trigger` × `Action`):**
  - **3 Monitor Modes:** Match all screen time (`*`), specific process executables (`.exe`), window title regular expressions (`Titles`), or browser tab URL regular expressions (`Urls` via `aw-watcher-web`).
  - **2 Trigger Types:**
    - **`Period`:** Triggers after a continuous active session duration (with configurable AFK reset threshold).
    - **`Limit`:** Triggers when cumulative daily active usage reaches a quota (anchored to a configurable logical day-start hour, e.g., `04:00 AM`).
  - **7 Enforcement Actions:**
    - **`Show Overlay`:** Displays a full-screen break overlay across all connected monitors that counts down only while you are physically idle.
    - **`Show Notification`:** Sends either an interactive top-right `HUD` toast card (with configurable duration, optional countdown timer, and snooze buttons) or a native `Windows` Action Center toast.
    - **`Show Statistics`:** Opens the draggable 24-Hour Screen Time Summary HUD with an hourly activity histogram that live-updates on every poll tick while open without extra ActivityWatch requests.
    - **`Lock Workstation`:** Immediately locks the Windows session (`LockWorkStation`).
    - **`Close Tab`:** Sends `Ctrl+W` to close the active tab of the matching foreground window.
    - **`Close Window`:** Gracefully closes matching visible, non-minimized windows via `WM_CLOSE`.
    - **`Kill Process`:** Terminates matching background and foreground processes (restricted to rules targeting specific `.exe` names).

- **Three-Stage Enforcement Pipeline:**
  1. **Stage 1 — Pre-Action Warning (`Warn Before Action`):** Alerts you ahead of time (via HUD toast or Windows notification) before a break or quota enforcement triggers.
  2. **Stage 2 — Grace Countdown (`Grace Countdown`):** Displays a non-intrusive top-right `HudTimer` progress card counting down the final seconds so you can save your work.
  3. **Stage 3 — Action Execution:** Executes the configured rule action when the countdown completes.

- **Ergonomic & Anti-Bypass Safeguards:**
  - **Idle-Aware Break Timer:** Full-screen breaks wait for a natural 1.5-second pause in typing/mouse movement before appearing and pause their countdown (`⏸ Input detected — timer paused until you step away`) whenever physical input is detected.
  - **Fullscreen / D3D Presentation Suppression:** Optionally detects fullscreen Direct3D games, presentations, or Windows Focus Assist (`SHQueryUserNotificationState`) and downgrades intrusive overlays to passive notifications.
  - **Strict Mode & 15-Second Lockout Delay:** When enabled on a `Show Overlay` rule, hides Skip/Snooze buttons on the break screen, disables system tray bypasses, and requires a 15-second confirmation countdown before unlocking settings edits during an active break.
  - **Pause All Rules Indefinitely:** Toggleable from both General Settings (`pause_all_rules`) and the system tray context menu (`Pause All Rules` / `Resume All Rules`).
  - **Click-Through Overlays:** Optionally allows mouse clicks and keystrokes to pass through the `Show Overlay` screen (`WS_EX_TRANSPARENT | WS_EX_NOACTIVATE`).
  - **Performance & Idle Memory Optimization:**
    - **Slint Rendering Backend (`slint_renderer`):** Switch between OpenGL hardware acceleration (`renderer-femtovg`, default) and lightweight CPU software rendering (`renderer-software`) directly from the Settings **Performance** section. CPU rendering significantly reduces GPU VRAM and overall memory footprint in resource-constrained or virtualized environments (requires application restart).
    - **Destroy Windows when Hidden (`destroy_windows_on_hide`):** Frees full-screen OpenGL/GPU surfaces and Slint textures when overlay alerts or the Settings window are closed or hidden (`destroy_windows_on_hide = true`, default); when disabled, windows remain cached in memory for instant display without cold-start initialization.

- **Rich Message Editor & Live Preview:**
  - Customize break and notification messages with Markdown-style headings (`#`, `##`), line-level styling (`[bold italic underline color=#38bdf8 size=18]`), and dynamic runtime placeholders: `{rule_name}`, `{app}`, `{session_duration}`, `{break_time}`, `{used}`, `{limit}`, and `{remaining}`.

- **7×24 Weekly Active Hours Schedule:**
  - Interactive click-and-drag hourly matrix per rule with one-click presets (`Work Hours`, `Evenings`, `Select All`, `Clear All`).

---

## Prerequisites

1. **Windows 10 or Windows 11** (64-bit).
2. **[ActivityWatch](https://activitywatch.net/)** running locally (default port `5600`):
   - `aw-watcher-window` and `aw-watcher-afk` (installed by default with ActivityWatch) are required for `Executables` and `Titles` rules.
   - `aw-watcher-web` browser extension is required if you create rules using the `Urls` selection mode.
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

- `--autostart`: Launches AW-Enforcer silently in the system tray without opening the Settings window on startup (automatically configured in `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` when **Launch AW-Enforcer on Windows startup** is enabled).

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
| :--- | :--- |
| `src/main.rs` | Single-instance mutex, system tray menu, Slint UI state synchronization, and Tokio runtime setup |
| `src/config.rs` | TOML configuration schema, serde serialization, `strftime` validation, and rule safety sanitization |
| `src/aw_client.rs` | Asynchronous ActivityWatch REST client (`/api/0/buckets/` discovery and `filter_period_intersect` queries) |
| `src/engine.rs` | Real-time 1s countdown loop, periodic ActivityWatch polling loop, session duration/quota calculation, and 3-stage rule evaluation |
| `src/enforcer.rs` | Win32 window enumeration (`EnumWindows`), `WM_CLOSE` dispatch, `Ctrl+W` keystroke synthesis, process termination, and workstation lock |
| `src/overlay.rs` | Multi-monitor Slint `OverlayWindow` lifecycle, Win32 extended styles (`WS_EX_TOPMOST`, `WS_EX_TRANSPARENT`), and DWM composition |
| `src/formatter.rs` | Message template placeholder interpolation and line-level attribute/heading parser |
| `src/autostart.rs` | Windows Registry (`HKCU\...\Run`) startup registration |
| `ui/settings.slint` | Settings window, sidebar navigation, 5-section rule editor, and interactive 7×24 schedule matrix |
| `ui/overlay.slint` | Full-screen break enforcer, HUD toast, HUD grace countdown timer, and 24-hour screen time histogram cards |
| `ui/formatted_text.slint` | Reusable rich-text paragraph renderer component |

---

## License

This project is licensed under the [MIT License](LICENSE).


