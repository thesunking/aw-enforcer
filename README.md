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
    - **`Show Statistics`:** Opens a 24-Hour Screen Time Summary HUD with an hourly activity histogram.
    - **`Lock Workstation`:** Immediately locks the Windows session.
    - **`Close Tab`:** Closes the active tab of the matching foreground window.
    - **`Close Window`:** Closes matching visible, non-minimized windows.
    - **`Kill Process`:** Terminates matching processes.

- **Three-Stage Enforcement Pipeline:**
  1. **Stage 1 — Pre-Action Warning:** Alerts you ahead of time (via HUD toast or native Windows Action Center notification) before a break or quota enforcement triggers. Supports custom formatted message templates across all rules.
  2. **Stage 2 — Grace Countdown:** Displays a non-intrusive top-right `HudTimer` progress card counting down the final seconds so you can save your work.
  3. **Stage 3 — Action Execution:** Executes the configured rule action when the countdown completes.

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
| `ui/schedule_grid.slint` | Interactive weekly schedule matrix with click-and-drag painting and presets |

---

## License

This project is licensed under the [MIT License](https://opensource.org/license/mit).