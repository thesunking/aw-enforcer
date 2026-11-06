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
    - **`Show Notification`:** Sends either an interactive top-right `HUD` toast card or a native `Windows` Action Center toast.
    - **`Show Statistics`:** Opens a 24-Hour Screen Time Summary HUD with an hourly activity histogram. Supports optional click-through mode (`click_through`) to interact with underlying windows while monitoring stats.
    - **`Lock Workstation`:** Immediately locks the Windows session. Repeat triggers for `Limit` rules enforce a minimum 15-second grace period so users can save work or snooze after unlocking.
    - **`Close Tab`:** Closes the active tab of the matching foreground window via synthesized `Ctrl+W`. Active tab closure strictly requires a matching foreground window to ensure background windows or unrelated applications are never sent keystrokes.
    - **`Close Window`:** Closes matching visible top-level windows (including minimized taskbar windows).
    - **`Kill Process`:** Terminates matching processes via ToolHelp snapshots, verifying running processes even when minimized or resident in the system tray.

- **Three-Stage Enforcement Pipeline:**
  1. **Stage 1 — Pre-Action Warning:** Alerts you ahead of time (via HUD toast or native Windows Action Center notification) before a break or quota enforcement triggers. Configurable delivery method (HUD vs Windows), duration, and countdown visibility across all rule actions. Supports custom formatted message templates.
  2. **Stage 2 — Grace Countdown:** Displays a non-intrusive top-right `HudTimer` progress card counting down the final seconds so you can save your work. Window minimization does not bypass enforcement: `Close Window` closes minimized taskbar windows, `Kill Process` terminates background/tray targets, and `Lock Workstation` executes reliably regardless of active desktop state. Repeat executions of `Limit` + `Lock Workstation` enforce a minimum 15-second grace countdown to prevent immediate lock-out loops upon unlocking the workstation. Dismissing a destructive grace timer hides the banner without aborting enforcement (requiring `Snooze (5m)` to defer action).
  3. **Stage 3 — Action Execution:** Executes the configured rule action when the countdown completes.
  - **Universal Strict Mode:** Supported across all rule actions to hide Skip/Snooze controls and lock settings/tray bypasses during grace countdowns and break enforcement (while warning/notification toasts can still be acknowledged and dismissed).

- **Customizable Appearance & 24-Hour Statistics HUD:**
  - Full control over overlay opacity, accent color, background color, and text color in **General Settings**, featuring interactive 24×24px color swatches next to Hex inputs for real-time visual feedback.
  - Draggable non-fullscreen HUD overlay cards (`HudToast`, `HudTimer`, `HudStatus`): click and drag any card to position it anywhere across the desktop away from active application controls.
  - Configurable 24-hour histogram bin count (1–96) and bin size (1–1440 minutes) with dynamically resizing bars and intelligent timeline label decimation.
  - One-click rule alert previewing accessible for all rule configurations directly in the Rule Editor, with real-time countdown ticking and immediate dismissal.
  - Rich text formatting with alignment-aware underline rendering sized directly to paragraph text, case-insensitive attribute toggling (e.g., uppercase hex colors), grace-aware preview context interpolation, and an adaptive template editor height with line-selection gutter that dynamically expands to avoid overflow.
  - High-performance binary-searched interval intersection (`partition_point`) for $O(\log N)$ overlap queries and web event window clipping, with single-pass merged interval reuse between telemetry and live statistics overlays.

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

Whenever the configuration is loaded or saved, comprehensive sanitization invariants are enforced:
- **Duration & Quota Clamping:** Polling intervals are clamped to 2–120 seconds, daily screentime targets to 1–1440 minutes, histogram bins to 1–96 bins (1–1440 min/bin), and rule duration inputs to 1–3600 (matching UI SpinBox bounds).
- **Target Normalization:** Executable target entries are trimmed and empty strings dropped; if specific executables exist, wildcards (`"*"`) are dropped and entries are deduplicated case-insensitively while preserving order.
- **Schedule Bitmask Normalization:** Weekday active schedules are canonicalized to standard 24-character `'0'`/`'1'` strings.
- **Rule ID Uniqueness:** Non-empty, unique rule IDs are strictly enforced; duplicate, empty, or `"test_*"` prefixed IDs are automatically assigned the next available `rule-{n}` identifier.
- **Pattern Validation for Titles & URLs:** Rules with `Titles` or `Urls` monitoring require a non-empty, valid regular expression pattern. Empty or whitespace-only patterns are rejected with an inline error in the editor and prevent saving until a valid regex is provided.
- **Global Session & Telemetry Accuracy:** Live status and system tray tooltip session telemetry track continuous active screen time walking backward across merged activity intervals with a 5-minute AFK reset gap, providing continuous session metrics even when only `Limit` rules are configured or `Period` rules are snoozed.

---

## Project Structure

| File | Responsibility |
| --- | --- |
| `src/main.rs` | Single-instance mutex, system tray menu, event-driven Slint UI dispatch, and Tokio runtime setup |
| `src/config.rs` | TOML configuration schema, serde serialization, `strftime` validation, duration/quota bounds clamping, target/schedule normalization, unique rule ID enforcement, and `Config::sanitize()` / `Rule::sanitize()` safety invariants |
| `src/aw_client.rs` | Asynchronous ActivityWatch REST client (`/api/0/buckets/` dynamic discovery, thread-safe hostname ranking, and `filter_period_intersect` queries) |
| `src/engine.rs` | Real-time 1s countdown loop, periodic ActivityWatch polling loop, O(log N) binary-searched interval intersection, single-pass poll interval reuse, and 3-stage rule evaluation |
| `src/enforcer.rs` | Protected Win32 window enumeration (`EnumWindows`), `WM_CLOSE` dispatch, `Ctrl+W` keystroke synthesis, shared ToolHelp process iteration, and workstation lock |
| `src/overlay.rs` | Multi-monitor Slint `OverlayWindow` lifecycle, priority hierarchy, Win32 extended styles (`WS_EX_TOPMOST`, `WS_EX_TRANSPARENT`), and DWM composition |
| `src/formatter.rs` | Message template placeholder interpolation, case-insensitive attribute toggle, grace-aware preview context, and line-level attribute/heading parser |
| `src/autostart.rs` | Windows Registry (`HKCU\...\Run`) startup registration |
| `ui/settings.slint` | Settings window, sidebar navigation, 5-section rule editor, and interactive 7×24 schedule matrix |
| `ui/overlay.slint` | Full-screen break enforcer, HUD toast, HUD grace countdown timer, and 24-hour screen time histogram cards |
| `ui/formatted_text.slint` | Reusable rich-text paragraph renderer component with alignment-aware text underlines |
| `ui/executable_selector.slint` | Executable chip list, file browser trigger, and running process selector |
| `ui/duration_input.slint` | Reusable numeric value + `Hr`/`Min`/`Sec` unit selector |
| `ui/schedule_grid.slint` | Interactive weekly schedule matrix with click-and-drag painting and presets |

---

## License

This project is licensed under the [MIT License](https://opensource.org/license/mit).