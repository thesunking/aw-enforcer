use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::{ComponentHandle, ModelRc, VecModel};
use std::cell::RefCell;
use std::ptr;
use std::rc::Rc;
use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM, POINT, RECT};
use windows_sys::Win32::Graphics::Dwm::{
    DwmEnableBlurBehindWindow, DwmExtendFrameIntoClientArea, DWM_BB_BLURREGION, DWM_BB_ENABLE,
    DWM_BLURBEHIND,
};
use windows_sys::Win32::Graphics::Gdi::{
    CreateRectRgn, DeleteObject, EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO,
    ScreenToClient,
};
use windows_sys::Win32::UI::Controls::MARGINS;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetWindowLongPtrW, GetWindowRect, IsWindowVisible, PostMessageW, SendMessageW,
    SetLayeredWindowAttributes, SetWindowLongPtrW, SetWindowPos, ShowWindow, GWL_EXSTYLE,
    GWL_STYLE, HTCAPTION, HWND_TOPMOST, LWA_ALPHA, SWP_FRAMECHANGED, SWP_NOACTIVATE,
    SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, SW_HIDE, SW_SHOW, SW_SHOWNOACTIVATE, WM_LBUTTONUP, WM_MOUSEMOVE,
    WM_NCLBUTTONDOWN, WS_CAPTION, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_POPUP, WS_SYSMENU,
    WS_THICKFRAME,
};

const MONITORINFOF_PRIMARY: u32 = 1;

use crate::config::OverlayConfig;
use crate::formatter::{self, FormattedParagraph};
use crate::OverlayWindow;

/// Physical and work-area rectangle bounds for a connected display monitor.
#[derive(Clone)]
pub struct MonitorBounds {
    pub monitor_rect: RECT,
    pub work_rect: RECT,
    pub is_primary: bool,
}

impl std::fmt::Debug for MonitorBounds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MonitorBounds")
            .field("monitor_rect", &(self.monitor_rect.left, self.monitor_rect.top, self.monitor_rect.right, self.monitor_rect.bottom))
            .field("work_rect", &(self.work_rect.left, self.work_rect.top, self.work_rect.right, self.work_rect.bottom))
            .field("is_primary", &self.is_primary)
            .finish()
    }
}

/// Active display mode and payload for the overlay window manager.
#[derive(Debug, Clone)]
pub enum OverlayMode {
    Hidden,
    BreakEnforcer {
        rule_id: String,
        title: String,
        paragraphs: Vec<FormattedParagraph>,
        duration_seconds: u32,
        remaining_seconds: u32,
        allow_snooze: bool,
        strict_mode: bool,
        click_through: bool,
    },
    HudToast {
        rule_id: String,
        title: String,
        paragraphs: Vec<FormattedParagraph>,
        snooze_options_minutes: Vec<u32>,
        duration_seconds: u32,
        remaining_seconds: u32,
        show_time_remaining: bool,
    },
    HudTimer {
        rule_id: String,
        title: String,
        paragraphs: Vec<FormattedParagraph>,
        duration_seconds: u32,
        remaining_seconds: u32,
    },
    HudStatus {
        title: String,
        status_level: String,
        total_active_minutes: u32,
        target_minutes: u32,
        bin_values: Vec<f32>,
        bin_labels: Vec<String>,
        is_loading: bool,
        click_through: bool,
    },
}

impl OverlayMode {
    /// Returns the display priority of this overlay mode so lower-priority alerts cannot clobber active enforcement overlays.
    pub fn priority(&self) -> u8 {
        match self {
            OverlayMode::Hidden => 0,
            OverlayMode::HudStatus { .. } => 1,
            OverlayMode::HudToast { .. } => 2,
            OverlayMode::HudTimer { .. } => 3,
            OverlayMode::BreakEnforcer { .. } => 4,
        }
    }

    /// Returns the rule ID associated with this overlay mode, if any.
    pub fn rule_id(&self) -> Option<&str> {
        match self {
            OverlayMode::BreakEnforcer { rule_id, .. }
            | OverlayMode::HudToast { rule_id, .. }
            | OverlayMode::HudTimer { rule_id, .. } => Some(rule_id.as_str()),
            _ => None,
        }
    }
}

/// Target Win32 window geometry and extended style flags for an overlay window instance.
#[derive(Clone, Debug, PartialEq)]
pub struct Win32Placement {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub click_through: bool,
    pub no_activate: bool,
    pub extend_dwm_frame: bool,
}

/// Manages one or more Slint `OverlayWindow` instances across connected monitors and applies Win32 DWM/ex-style attributes.
pub struct OverlayManager {
    windows: Vec<OverlayWindow>,
    current_mode: OverlayMode,
    overlay_config: OverlayConfig,
    command_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::engine::DaemonCommand>>,
    status_pos: Rc<RefCell<Option<(i32, i32)>>>,
    desired_placements: Vec<Option<Win32Placement>>,
    applied_placements: Vec<Option<Win32Placement>>,
    destroy_windows_on_hide: bool,
    is_updating_placements: bool,
}

impl OverlayManager {
    /// Creates a new `OverlayManager` with the given visual theme configuration, command sender, and performance setting.
    pub fn new(
        overlay_config: OverlayConfig,
        command_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::engine::DaemonCommand>>,
        destroy_windows_on_hide: bool,
    ) -> Self {
        Self {
            windows: Vec::new(),
            current_mode: OverlayMode::Hidden,
            overlay_config,
            command_tx,
            status_pos: Rc::new(RefCell::new(None)),
            desired_placements: Vec::new(),
            applied_placements: Vec::new(),
            destroy_windows_on_hide,
            is_updating_placements: false,
        }
    }

    /// Sets whether overlay windows are destroyed when hidden or kept in memory.
    pub fn set_destroy_windows_on_hide(&mut self, destroy: bool) {
        self.destroy_windows_on_hide = destroy;
        if destroy && matches!(self.current_mode, OverlayMode::Hidden) {
            self.windows.clear();
        }
    }

    /// Ensures at least `active_count` overlay windows exist and hides or destroys any surplus windows.
    fn prepare_windows(&mut self, active_count: usize) {
        self.ensure_window_count(active_count);
        for i in 0..active_count {
            if i < self.windows.len() {
                self.windows[i].show().ok();
            }
        }
        for i in active_count..self.windows.len() {
            if let Some(hwnd) = Self::extract_hwnd(&self.windows[i]) {
                unsafe {
                    ShowWindow(hwnd, SW_HIDE);
                }
            }
            self.windows[i].hide().ok();
            if !self.destroy_windows_on_hide && i < self.desired_placements.len() {
                self.desired_placements[i] = None;
                self.applied_placements[i] = None;
            }
        }
        if self.destroy_windows_on_hide {
            self.windows.truncate(active_count);
            self.desired_placements.truncate(active_count);
            self.applied_placements.truncate(active_count);
        }
    }

    /// Computes top-right coordinates `(x, y)` inset by `margin` inside the primary monitor's work area.
    pub fn primary_top_right(monitors: &[MonitorBounds], card_w: i32, margin: i32) -> (i32, i32) {
        let m = monitors.iter().find(|m| m.is_primary).unwrap_or(&monitors[0]);
        (m.work_rect.right - card_w - margin, m.work_rect.top + margin)
    }

    /// Positions window `idx` onto `(target_x, target_y)` before reading its DPI scale factor,
    /// ensuring HUD overlays are never sized using a secondary monitor's stale scale factor.
    fn prepare_target_scale(&mut self, idx: usize, target_x: i32, target_y: i32) -> f32 {
        if let Some(win) = self.windows.get(idx) {
            win.window()
                .set_position(slint::PhysicalPosition::new(target_x, target_y));
            if let Some(hwnd) = Self::extract_hwnd(win) {
                unsafe {
                    SetWindowPos(
                        hwnd,
                        HWND_TOPMOST,
                        target_x,
                        target_y,
                        0,
                        0,
                        SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER,
                    );
                }
            }
            win.window().scale_factor().max(1.0)
        } else {
            1.0
        }
    }

    /// Updates the stored `OverlayConfig` and immediately applies the new colors/opacity to all managed windows.
    pub fn update_overlay_config(&mut self, config: OverlayConfig) {
        self.overlay_config = config;
        for win in &self.windows {
            Self::apply_theme_to_window(win, &self.overlay_config);
        }
    }

    /// Pushes opacity, accent color, and backdrop color from `OverlayConfig` to a single `OverlayWindow`.
    fn apply_theme_to_window(win: &OverlayWindow, cfg: &OverlayConfig) {
        win.set_overlay_opacity(cfg.opacity.clamp(0.1, 1.0));
        if let Some(accent) = formatter::parse_hex_color(&cfg.accent_color) {
            win.set_accent_color(accent);
        }
        if let Some(backdrop) = formatter::parse_hex_color(&cfg.background_color) {
            win.set_backdrop_color(backdrop);
        }
    }

    /// Records the desired `Win32Placement` for window `idx`, marking it pending if changed.
    pub fn set_desired_placement(&mut self, idx: usize, placement: Win32Placement) {
        if idx >= self.desired_placements.len() {
            self.desired_placements.resize(idx + 1, None);
        }
        if idx >= self.applied_placements.len() {
            self.applied_placements.resize(idx + 1, None);
        }
        if self.desired_placements[idx].as_ref() != Some(&placement) {
            self.desired_placements[idx] = Some(placement);
            self.applied_placements[idx] = None;
        }
    }

    /// Positions and shows overlay window `idx`, then applies its Win32 extended styles and DWM composition attributes.
    fn apply_win32_overlay_styles(
        &mut self,
        idx: usize,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        click_through: bool,
        no_activate: bool,
        extend_dwm_frame: bool,
    ) {
        let safe_w = width.max(1);
        let safe_h = height.max(1);

        self.set_desired_placement(
            idx,
            Win32Placement {
                x,
                y,
                width: safe_w,
                height: safe_h,
                click_through,
                no_activate,
                extend_dwm_frame,
            },
        );
    }

    /// Applies any unapplied `Win32Placement` styles (`WS_POPUP`, `WS_EX_TOPMOST`, click-through, and DWM blur-behind) to native HWNDs.
    pub fn apply_pending_win32_styles(&mut self) {
        if matches!(self.current_mode, OverlayMode::Hidden) {
            return;
        }
        if !self.is_updating_placements && matches!(self.current_mode, OverlayMode::BreakEnforcer { .. }) {
            let current_monitors = Self::enumerate_monitors();
            let active_placements_count = self.desired_placements.iter().filter(|p| p.is_some()).count();
            let mut monitors_changed = current_monitors.len() != active_placements_count;
            if !monitors_changed {
                for (idx, mon) in current_monitors.iter().enumerate() {
                    let is_full_monitor = mon.work_rect.left == mon.monitor_rect.left
                        && mon.work_rect.top == mon.monitor_rect.top
                        && mon.work_rect.right == mon.monitor_rect.right
                        && mon.work_rect.bottom == mon.monitor_rect.bottom;
                    let width = (mon.work_rect.right - mon.work_rect.left).max(1);
                    let height = ((mon.work_rect.bottom - mon.work_rect.top) - if is_full_monitor { 1 } else { 0 }).max(1);

                    match self.desired_placements.get(idx).and_then(|p| p.as_ref()) {
                        Some(p) => {
                            if p.x != mon.work_rect.left
                                || p.y != mon.work_rect.top
                                || p.width != width
                                || p.height != height
                            {
                                monitors_changed = true;
                                break;
                            }
                        }
                        None => {
                            monitors_changed = true;
                            break;
                        }
                    }
                }
            }

            if monitors_changed {
                let mode = self.current_mode.clone();
                self.show_mode(mode);
                return;
            }
        }

        for idx in 0..self.windows.len() {
            let desired = match self.desired_placements.get(idx).and_then(|p| p.as_ref()) {
                Some(d) => d.clone(),
                None => continue,
            };

            if self.applied_placements.get(idx).and_then(|p| p.as_ref()) == Some(&desired) {
                continue;
            }

            let win = &self.windows[idx];
            win.show().ok();
            if let Some(hwnd) = Self::extract_hwnd(win) {
                let safe_w = desired.width.max(1);
                let safe_h = desired.height.max(1);

                unsafe {
                    // Strip standard caption/borders while keeping winit's frameless style intact
                    let mut style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
                    style &= !(WS_CAPTION | WS_SYSMENU | WS_THICKFRAME | WS_MINIMIZEBOX | WS_MAXIMIZEBOX);
                    style |= WS_POPUP;
                    SetWindowLongPtrW(hwnd, GWL_STYLE, style as isize);

                    let mut ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
                    ex_style |= WS_EX_TOOLWINDOW | WS_EX_TOPMOST;

                    if desired.click_through {
                        // Win32 requires WS_EX_LAYERED | WS_EX_TRANSPARENT for mouse click-through
                        ex_style |= WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE;
                    } else {
                        // CRITICAL: Clear WS_EX_LAYERED when not click-through so Windows DWM
                        // composites the OpenGL alpha channel directly on secondary monitors
                        // instead of falling back to an opaque GDI redirection bitmap.
                        ex_style &= !(WS_EX_LAYERED | WS_EX_TRANSPARENT);
                        if desired.no_activate {
                            ex_style |= WS_EX_NOACTIVATE;
                        } else {
                            ex_style &= !WS_EX_NOACTIVATE;
                        }
                    }

                    SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex_style as isize);

                    if desired.click_through {
                        SetLayeredWindowAttributes(hwnd, 0, 255, LWA_ALPHA);
                    }

                    // Re-enable DWM per-pixel alpha composition for OpenGL (winit trick:
                    // DwmEnableBlurBehindWindow with an empty 0,0,-1,-1 region).
                    let hrgn = CreateRectRgn(0, 0, -1, -1);
                    let bb = DWM_BLURBEHIND {
                        dwFlags: DWM_BB_ENABLE | DWM_BB_BLURREGION,
                        fEnable: 1,
                        hRgnBlur: hrgn,
                        fTransitionOnMaximized: 0,
                    };
                    DwmEnableBlurBehindWindow(hwnd, &bb);
                    if !hrgn.is_null() {
                        DeleteObject(hrgn as _);
                    }

                    if desired.extend_dwm_frame {
                        let margins = MARGINS {
                            cxLeftWidth: -1,
                            cxRightWidth: -1,
                            cyTopHeight: -1,
                            cyBottomHeight: -1,
                        };
                        DwmExtendFrameIntoClientArea(hwnd, &margins);
                    } else {
                        let margins = MARGINS {
                            cxLeftWidth: 0,
                            cxRightWidth: 0,
                            cyTopHeight: 0,
                            cyBottomHeight: 0,
                        };
                        DwmExtendFrameIntoClientArea(hwnd, &margins);
                    }

                    let swp_flags = if desired.click_through || desired.no_activate {
                        SWP_FRAMECHANGED | SWP_NOACTIVATE | SWP_SHOWWINDOW
                    } else {
                        SWP_FRAMECHANGED | SWP_SHOWWINDOW
                    };
                    SetWindowPos(hwnd, HWND_TOPMOST, desired.x, desired.y, safe_w, safe_h, swp_flags);
                    ShowWindow(
                        hwnd,
                        if desired.click_through || desired.no_activate {
                            SW_SHOWNOACTIVATE
                        } else {
                            SW_SHOW
                        },
                    );
                    win.window()
                        .set_position(slint::PhysicalPosition::new(desired.x, desired.y));
                    win.window()
                        .set_size(slint::PhysicalSize::new(safe_w as u32, safe_h as u32));
                    let mut r_final: RECT = std::mem::zeroed();
                    GetWindowRect(hwnd, &mut r_final);
                    tracing::debug!(
                        "Overlay window {idx} applied: requested ({}, {}, {}x{}), actual ({}, {}) to ({}, {}) [{}x{}]",
                        desired.x, desired.y, safe_w, safe_h,
                        r_final.left, r_final.top, r_final.right, r_final.bottom,
                        r_final.right - r_final.left, r_final.bottom - r_final.top
                    );
                }

                if idx < self.applied_placements.len() {
                    self.applied_placements[idx] = Some(desired);
                }
            }
        }
    }

    /// Enumerates all connected display monitors via Win32 `EnumDisplayMonitors`, falling back to a 1080p primary bounds rect.
    pub fn enumerate_monitors() -> Vec<MonitorBounds> {
        let mut monitors: Vec<MonitorBounds> = Vec::new();

        unsafe extern "system" fn monitor_enum_proc(
            hmonitor: HMONITOR,
            _hdc: HDC,
            _rect: *mut RECT,
            lparam: LPARAM,
        ) -> BOOL {
            let list = &mut *(lparam as *mut Vec<MonitorBounds>);
            let mut info: MONITORINFO = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;

            if GetMonitorInfoW(hmonitor, &mut info) != 0 {
                list.push(MonitorBounds {
                    monitor_rect: info.rcMonitor,
                    work_rect: info.rcWork,
                    is_primary: (info.dwFlags & MONITORINFOF_PRIMARY) != 0,
                });
            }
            1
        }

        unsafe {
            EnumDisplayMonitors(
                0 as HDC,
                ptr::null(),
                Some(monitor_enum_proc),
                &mut monitors as *mut _ as LPARAM,
            );
        }

        if monitors.is_empty() {
            monitors.push(MonitorBounds {
                monitor_rect: RECT {
                    left: 0,
                    top: 0,
                    right: 1920,
                    bottom: 1080,
                },
                work_rect: RECT {
                    left: 0,
                    top: 0,
                    right: 1920,
                    bottom: 1040,
                },
                is_primary: true,
            });
        }

        monitors
    }

    /// Extracts the underlying Win32 `HWND` from any Slint window.
    pub fn extract_window_hwnd(window: &slint::Window) -> Option<HWND> {
        let handle = window.window_handle();
        if let Ok(raw_handle) = handle.window_handle() {
            if let RawWindowHandle::Win32(win32) = raw_handle.as_raw() {
                return Some(win32.hwnd.get() as HWND);
            }
        }
        None
    }

    /// Extracts the underlying Win32 `HWND` from a Slint `OverlayWindow`.
    pub fn extract_hwnd(window: &OverlayWindow) -> Option<HWND> {
        Self::extract_window_hwnd(window.window())
    }

    /// Lazily instantiates Slint `OverlayWindow` instances and wires UI callbacks until `self.windows.len() >= required`.
    pub fn ensure_window_count(&mut self, required: usize) {
        while self.windows.len() < required {
            if let Ok(win) = OverlayWindow::new() {
                win.hide().ok();
                Self::apply_theme_to_window(&win, &self.overlay_config);

                let win_weak = win.as_weak();
                let pos_ref = self.status_pos.clone();
                win.on_drag_status_window(move || {
                    if let Some(w) = win_weak.upgrade() {
                        if let Some(hwnd) = Self::extract_hwnd(&w) {
                            unsafe {
                                ReleaseCapture();
                                SendMessageW(hwnd, WM_NCLBUTTONDOWN, HTCAPTION as usize, 0);
                                let mut r: RECT = std::mem::zeroed();
                                if GetWindowRect(hwnd, &mut r) != 0 {
                                    *pos_ref.borrow_mut() = Some((r.left, r.top));
                                }

                                // Modal Win32 drag loop swallows WM_LBUTTONUP. Synthesize mouse release and move
                                // to release Slint's internal pointer capture and immediately restore cursor and clickability.
                                let mut pt: POINT = std::mem::zeroed();
                                GetCursorPos(&mut pt);
                                ScreenToClient(hwnd, &mut pt);
                                let lparam = ((pt.x as u32) & 0xFFFF) | (((pt.y as u32) & 0xFFFF) << 16);
                                PostMessageW(hwnd, WM_LBUTTONUP, 0, lparam as isize);
                                PostMessageW(hwnd, WM_MOUSEMOVE, 0, lparam as isize);
                            }
                        }
                    }
                });

                if let Some(ref tx) = self.command_tx {
                    let t1 = tx.clone();
                    win.on_snooze_clicked(move |rid, mins| {
                        // Engine handles both BreakRule and AppRule IDs on SnoozeBreak
                        t1.send(crate::engine::DaemonCommand::SnoozeBreak {
                            rule_id: rid.to_string(),
                            minutes: mins as u32,
                        })
                        .ok();
                    });
                    let t2 = tx.clone();
                    win.on_skip_break_clicked(move |rid| {
                        t2.send(crate::engine::DaemonCommand::SkipBreak {
                            rule_id: rid.to_string(),
                        })
                        .ok();
                    });
                    let t3 = tx.clone();
                    win.on_acknowledge_toast_clicked(move |rid| {
                        t3.send(crate::engine::DaemonCommand::AcknowledgeAppWarning {
                            rule_id: rid.to_string(),
                        })
                        .ok();
                    });
                    let t4 = tx.clone();
                    win.on_close_status_clicked(move || {
                        t4.send(crate::engine::DaemonCommand::CloseOverlay).ok();
                    });
                }
                self.windows.push(win);
            } else {
                break;
            }
        }
    }

    /// Hides all managed overlay windows and resets placement state.
    /// If `destroy_windows_on_hide` is true, window instances are cleared from memory.
    pub fn hide_all(&mut self) {
        self.current_mode = OverlayMode::Hidden;
        self.desired_placements.clear();
        self.applied_placements.clear();
        *self.status_pos.borrow_mut() = None;
        for win in &self.windows {
            if let Some(hwnd) = Self::extract_hwnd(win) {
                unsafe {
                    ShowWindow(hwnd, SW_HIDE);
                }
            }
            win.hide().ok();
        }
        if self.destroy_windows_on_hide {
            self.windows.clear();
        }
    }

    /// Configures and displays the overlay windows according to the given `OverlayMode`.
    pub fn show_mode(&mut self, mode: OverlayMode) {
        let monitors = Self::enumerate_monitors();
        let was_hud_status = matches!(self.current_mode, OverlayMode::HudStatus { .. });
        self.current_mode = mode.clone();
        self.is_updating_placements = true;

        match &mode {
            OverlayMode::Hidden => {
                self.hide_all();
            }
            OverlayMode::BreakEnforcer {
                rule_id,
                title,
                paragraphs,
                duration_seconds,
                remaining_seconds,
                allow_snooze,
                strict_mode,
                click_through,
            } => {
                self.prepare_windows(monitors.len());

                let countdown_str = format_countdown(*remaining_seconds, false);

                for (idx, mon) in monitors.iter().enumerate() {
                    if idx >= self.windows.len() {
                        break;
                    }
                    {
                        let win = &self.windows[idx];
                        Self::apply_theme_to_window(win, &self.overlay_config);

                        win.set_mode("break".into());
                        win.set_rule_id(rule_id.as_str().into());
                        win.set_title_text(title.as_str().into());
                        win.set_countdown_text(countdown_str.as_str().into());
                        win.set_progress_ratio(compute_progress_ratio(*remaining_seconds, *duration_seconds));
                        win.set_allow_snooze(*allow_snooze);
                        win.set_strict_mode(*strict_mode);
                        win.set_click_through(*click_through);
                        win.set_is_user_idle(crate::engine::get_idle_time_millis() >= 3000);

                        set_window_paragraphs(win, paragraphs);
                    }

                    let width = mon.work_rect.right - mon.work_rect.left;
                    let is_full_monitor = mon.work_rect.left == mon.monitor_rect.left
                        && mon.work_rect.top == mon.monitor_rect.top
                        && mon.work_rect.right == mon.monitor_rect.right
                        && mon.work_rect.bottom == mon.monitor_rect.bottom;
                    // Position within work_rect so the break overlay does not cover the Windows taskbar.
                    // If work_rect matches monitor_rect 1:1 (e.g. secondary monitor with no taskbar),
                    // subtract 1px from height so DWM does not promote it to an opaque Fullscreen Direct Flip surface.
                    let height = (mon.work_rect.bottom - mon.work_rect.top) - if is_full_monitor { 1 } else { 0 };
                    tracing::debug!(
                        "BreakEnforcer monitor {idx}: monitor_rect={:?}, work_rect={:?}, target_w={width}, target_h={height}",
                        (mon.monitor_rect.left, mon.monitor_rect.top, mon.monitor_rect.right, mon.monitor_rect.bottom),
                        (mon.work_rect.left, mon.work_rect.top, mon.work_rect.right, mon.work_rect.bottom)
                    );
                    self.apply_win32_overlay_styles(
                        idx,
                        mon.work_rect.left,
                        mon.work_rect.top,
                        width,
                        height,
                        *click_through,
                        true, // Keep no_activate = true across all monitors so DWM never enters fullscreen exclusive mode
                        true,
                    );
                }
            }
            OverlayMode::HudToast {
                rule_id,
                title,
                paragraphs,
                snooze_options_minutes,
                duration_seconds,
                remaining_seconds,
                show_time_remaining,
            } => {
                self.prepare_windows(1);

                {
                    let win = &self.windows[0];
                    Self::apply_theme_to_window(win, &self.overlay_config);

                    win.set_mode("hud_toast".into());
                    win.set_rule_id(rule_id.as_str().into());
                    win.set_title_text(title.as_str().into());
                    win.set_show_time_remaining(*show_time_remaining);
                    win.set_countdown_text(format_countdown(*remaining_seconds, true).into());
                    win.set_progress_ratio(compute_progress_ratio(*remaining_seconds, *duration_seconds));
                    win.set_click_through(false);
                    win.set_allow_snooze(!snooze_options_minutes.is_empty());
                    win.set_strict_mode(snooze_options_minutes.is_empty());

                    set_window_paragraphs(win, paragraphs);

                    let snooze_labels: Vec<slint::SharedString> = snooze_options_minutes
                        .iter()
                        .map(|&m| format!("{} min", m).into())
                        .collect();
                    win.set_snooze_labels(ModelRc::from(Rc::new(VecModel::from(snooze_labels))));

                    let snooze_i32: Vec<i32> =
                        snooze_options_minutes.iter().map(|&m| m as i32).collect();
                    win.set_snooze_options(ModelRc::from(Rc::new(VecModel::from(snooze_i32))));
                    win.set_selected_snooze_index(0);
                }

                let primary_m = monitors.iter().find(|m| m.is_primary).unwrap_or(&monitors[0]);
                let scale = self.prepare_target_scale(0, primary_m.work_rect.left, primary_m.work_rect.top);
                let (card_w, card_h, margin) = scale_card_bounds(scale, 480.0, 280.0, 20.0);
                let (x, y) = Self::primary_top_right(&monitors, card_w, margin);

                self.apply_win32_overlay_styles(0, x, y, card_w, card_h, false, true, false);
            }
            OverlayMode::HudTimer {
                rule_id,
                title,
                paragraphs,
                duration_seconds,
                remaining_seconds,
            } => {
                self.prepare_windows(1);

                {
                    let win = &self.windows[0];
                    Self::apply_theme_to_window(win, &self.overlay_config);

                    win.set_mode("hud_timer".into());
                    win.set_rule_id(rule_id.as_str().into());
                    win.set_title_text(title.as_str().into());
                    win.set_countdown_text(format_countdown(*remaining_seconds, true).into());
                    win.set_progress_ratio(compute_progress_ratio(*remaining_seconds, *duration_seconds));
                    win.set_click_through(true);

                    set_window_paragraphs(win, paragraphs);
                }

                let primary_m = monitors.iter().find(|m| m.is_primary).unwrap_or(&monitors[0]);
                let scale = self.prepare_target_scale(0, primary_m.work_rect.left, primary_m.work_rect.top);
                let (card_w, card_h, margin) = scale_card_bounds(scale, 420.0, 180.0, 20.0);
                let (x, y) = Self::primary_top_right(&monitors, card_w, margin);

                self.apply_win32_overlay_styles(0, x, y, card_w, card_h, true, true, false);
            }
            OverlayMode::HudStatus {
                title,
                status_level,
                total_active_minutes,
                target_minutes,
                bin_values,
                bin_labels,
                is_loading,
                click_through,
            } => {
                self.prepare_windows(1);

                let (card_w, card_h, _margin, x, y) = if was_hud_status {
                    let visible_pos = self.windows.first().and_then(Self::extract_hwnd).and_then(|hwnd| unsafe {
                        let mut r: RECT = std::mem::zeroed();
                        (IsWindowVisible(hwnd) != 0 && GetWindowRect(hwnd, &mut r) != 0 && r.right > r.left)
                            .then_some((r.left, r.top))
                    });
                    if let Some((target_x, target_y)) = visible_pos.or_else(|| *self.status_pos.borrow()) {
                        let scale = self.prepare_target_scale(0, target_x, target_y);
                        let (card_w, card_h, margin) = scale_card_bounds(scale, 540.0, 270.0, 20.0);
                        let (x, y) = clamp_or_reset_status_pos((target_x, target_y), card_w, card_h, margin, &monitors);
                        (card_w, card_h, margin, x, y)
                    } else {
                        let primary_m = monitors.iter().find(|m| m.is_primary).unwrap_or(&monitors[0]);
                        let scale = self.prepare_target_scale(0, primary_m.work_rect.left, primary_m.work_rect.top);
                        let (card_w, card_h, margin) = scale_card_bounds(scale, 540.0, 270.0, 20.0);
                        let (x, y) = Self::primary_top_right(&monitors, card_w, margin);
                        (card_w, card_h, margin, x, y)
                    }
                } else {
                    let primary_m = monitors.iter().find(|m| m.is_primary).unwrap_or(&monitors[0]);
                    let scale = self.prepare_target_scale(0, primary_m.work_rect.left, primary_m.work_rect.top);
                    let (card_w, card_h, margin) = scale_card_bounds(scale, 540.0, 270.0, 20.0);
                    let (x, y) = Self::primary_top_right(&monitors, card_w, margin);
                    (card_w, card_h, margin, x, y)
                };

                *self.status_pos.borrow_mut() = Some((x, y));

                let win = &self.windows[0];
                Self::apply_theme_to_window(win, &self.overlay_config);

                win.set_mode("hud_status".into());
                win.set_title_text(title.as_str().into());
                win.set_status_level(status_level.as_str().into());
                win.set_total_active_minutes(*total_active_minutes as i32);
                win.set_target_minutes(*target_minutes as i32);
                win.set_is_loading(*is_loading);
                win.set_click_through(*click_through);

                win.set_histogram_values(ModelRc::from(Rc::new(VecModel::from(
                    bin_values.clone(),
                ))));
                let labels: Vec<slint::SharedString> =
                    bin_labels.iter().map(|s| s.as_str().into()).collect();
                win.set_histogram_labels(ModelRc::from(Rc::new(VecModel::from(labels))));

                self.apply_win32_overlay_styles(0, x, y, card_w, card_h, *click_through, true, false);
            }
        }

        self.is_updating_placements = false;
        self.apply_pending_win32_styles();
    }

    /// Updates the countdown text, progress bar ratio, and user-idle indicator across all active overlay windows.
    pub fn update_countdown(&mut self, remaining_seconds: u32, total_seconds: u32, is_user_idle: bool) {
        match &mut self.current_mode {
            OverlayMode::BreakEnforcer { remaining_seconds: ref mut rem, duration_seconds: ref mut dur, .. }
            | OverlayMode::HudToast { remaining_seconds: ref mut rem, duration_seconds: ref mut dur, .. }
            | OverlayMode::HudTimer { remaining_seconds: ref mut rem, duration_seconds: ref mut dur, .. } => {
                *rem = remaining_seconds;
                *dur = total_seconds;
            }
            _ => {}
        }

        let is_hud = matches!(self.current_mode, OverlayMode::HudTimer { .. } | OverlayMode::HudToast { .. });
        let countdown_str = format_countdown(remaining_seconds, is_hud);
        let ratio = compute_progress_ratio(remaining_seconds, total_seconds);

        for win in &self.windows {
            win.set_countdown_text(countdown_str.as_str().into());
            win.set_progress_ratio(ratio);
            win.set_is_user_idle(is_user_idle);
        }
    }
}

/// Formats remaining seconds as either `"15s"` (for HUDs under 60s) or `"MM:SS"`.
fn format_countdown(remaining_seconds: u32, is_hud: bool) -> String {
    if is_hud && remaining_seconds < 60 {
        format!("{}s", remaining_seconds)
    } else {
        let mins = remaining_seconds / 60;
        let secs = remaining_seconds % 60;
        format!("{:02}:{:02}", mins, secs)
    }
}

/// Computes the ratio of remaining to total duration safely clamped between 0.0 and 1.0.
fn compute_progress_ratio(remaining_seconds: u32, total_seconds: u32) -> f32 {
    if total_seconds > 0 {
        remaining_seconds as f32 / total_seconds as f32
    } else {
        0.0
    }
}

/// Computes scaled DPI-aware card dimensions `(width, height, margin)` from logical measurements.
fn scale_card_bounds(scale: f32, w: f32, h: f32, margin: f32) -> (i32, i32, i32) {
    (
        (w * scale).round() as i32,
        (h * scale).round() as i32,
        (margin * scale).round() as i32,
    )
}

/// Checks whether a card rectangle at `pos` has a visible intersection of at least 100x60 pixels
/// with at least one monitor's work area. If not (e.g. after a monitor was disconnected or moved offscreen),
/// resets the coordinates to `OverlayManager::primary_top_right`.
pub fn clamp_or_reset_status_pos(
    pos: (i32, i32),
    card_w: i32,
    card_h: i32,
    margin: i32,
    monitors: &[MonitorBounds],
) -> (i32, i32) {
    if monitors.is_empty() {
        return pos;
    }

    let card_left = pos.0;
    let card_top = pos.1;
    let card_right = pos.0 + card_w;
    let card_bottom = pos.1 + card_h;

    let has_visible_intersection = monitors.iter().any(|mon| {
        let inter_left = card_left.max(mon.work_rect.left);
        let inter_right = card_right.min(mon.work_rect.right);
        let inter_top = card_top.max(mon.work_rect.top);
        let inter_bottom = card_bottom.min(mon.work_rect.bottom);

        let inter_w = inter_right - inter_left;
        let inter_h = inter_bottom - inter_top;

        inter_w >= 100 && inter_h >= 60
    });

    if has_visible_intersection {
        pos
    } else {
        OverlayManager::primary_top_right(monitors, card_w, margin)
    }
}

/// Sets formatted message paragraphs on an `OverlayWindow`.
fn set_window_paragraphs(win: &OverlayWindow, paragraphs: &[FormattedParagraph]) {
    let para_models = Rc::new(VecModel::from(paragraphs.to_vec()));
    win.set_message_paragraphs(ModelRc::from(para_models));
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Foundation::RECT;

    fn mock_monitor(left: i32, top: i32, right: i32, bottom: i32, is_primary: bool) -> MonitorBounds {
        MonitorBounds {
            monitor_rect: RECT { left, top, right, bottom },
            work_rect: RECT { left, top, right, bottom },
            is_primary,
        }
    }

    #[test]
    fn test_clamp_or_reset_status_pos_single_monitor() {
        let monitors = vec![mock_monitor(0, 0, 1920, 1080, true)];
        let card_w = 540;
        let card_h = 270;
        let margin = 20;

        // 1. Fully visible inside monitor -> preserves position
        let pos_inside = (100, 100);
        assert_eq!(
            clamp_or_reset_status_pos(pos_inside, card_w, card_h, margin, &monitors),
            pos_inside
        );

        // 2. Sufficient intersection (e.g. 150px wide, 100px high visible) -> preserves position
        let pos_edge = (1920 - 150, 100);
        assert_eq!(
            clamp_or_reset_status_pos(pos_edge, card_w, card_h, margin, &monitors),
            pos_edge
        );

        // 3. Insufficient intersection (<100px wide visible) -> resets to primary top right
        let pos_too_far_right = (1920 - 50, 100);
        let expected_top_right = (1920 - card_w - margin, margin);
        assert_eq!(
            clamp_or_reset_status_pos(pos_too_far_right, card_w, card_h, margin, &monitors),
            expected_top_right
        );

        // 4. Insufficient intersection (<60px high visible) -> resets to primary top right
        let pos_too_far_bottom = (100, 1080 - 30);
        assert_eq!(
            clamp_or_reset_status_pos(pos_too_far_bottom, card_w, card_h, margin, &monitors),
            expected_top_right
        );
    }

    #[test]
    fn test_clamp_or_reset_status_pos_multi_monitor() {
        let monitors = vec![
            mock_monitor(0, 0, 1920, 1080, true),
            mock_monitor(1920, 0, 3840, 1080, false),
        ];
        let card_w = 540;
        let card_h = 270;
        let margin = 20;

        // Valid on secondary monitor -> preserves position
        let pos_secondary = (2200, 150);
        assert_eq!(
            clamp_or_reset_status_pos(pos_secondary, card_w, card_h, margin, &monitors),
            pos_secondary
        );
    }

    #[test]
    fn test_clamp_or_reset_status_pos_disconnected_monitor() {
        // Was on secondary monitor (2500, 200), but secondary monitor disconnected, only primary remains
        let monitors = vec![mock_monitor(0, 0, 1920, 1080, true)];
        let card_w = 540;
        let card_h = 270;
        let margin = 20;

        let pos_disconnected = (2500, 200);
        let expected_top_right = (1920 - card_w - margin, margin);
        assert_eq!(
            clamp_or_reset_status_pos(pos_disconnected, card_w, card_h, margin, &monitors),
            expected_top_right
        );
    }

    #[test]
    fn test_show_mode_multi_monitor_break_and_hud_no_recursion() {
        let mut mgr = OverlayManager::new(OverlayConfig::default(), None, false);

        // 1. BreakEnforcer should not overflow stack or recurse on multi-monitor
        let break_mode = OverlayMode::BreakEnforcer {
            rule_id: "test_break".into(),
            title: "Test Break".into(),
            paragraphs: Vec::new(),
            duration_seconds: 300,
            remaining_seconds: 300,
            allow_snooze: true,
            strict_mode: false,
            click_through: false,
        };
        mgr.show_mode(break_mode);
        assert!(matches!(mgr.current_mode, OverlayMode::BreakEnforcer { .. }));
        assert!(!mgr.is_updating_placements);

        // 2. HudToast should configure cleanly
        let toast_mode = OverlayMode::HudToast {
            rule_id: "test_toast".into(),
            title: "Test Toast".into(),
            paragraphs: Vec::new(),
            snooze_options_minutes: vec![5, 10, 15],
            duration_seconds: 10,
            remaining_seconds: 10,
            show_time_remaining: true,
        };
        mgr.show_mode(toast_mode);
        assert!(matches!(mgr.current_mode, OverlayMode::HudToast { .. }));

        // 3. HudTimer should configure cleanly
        let timer_mode = OverlayMode::HudTimer {
            rule_id: "test_timer".into(),
            title: "Test Timer".into(),
            paragraphs: Vec::new(),
            duration_seconds: 30,
            remaining_seconds: 30,
        };
        mgr.show_mode(timer_mode);
        assert!(matches!(mgr.current_mode, OverlayMode::HudTimer { .. }));

        // 4. HudStatus should configure cleanly
        let status_mode = OverlayMode::HudStatus {
            title: "24h Summary".into(),
            status_level: "Great".into(),
            total_active_minutes: 120,
            target_minutes: 480,
            bin_values: vec![10.0, 20.0],
            bin_labels: vec!["01:00".into(), "02:00".into()],
            is_loading: false,
            click_through: false,
        };
        mgr.show_mode(status_mode);
        assert!(matches!(mgr.current_mode, OverlayMode::HudStatus { .. }));
        let placement = mgr.desired_placements[0].as_ref().expect("status placement");
        let monitors = OverlayManager::enumerate_monitors();
        let primary_m = monitors.iter().find(|m| m.is_primary).unwrap_or(&monitors[0]);
        assert!(placement.x >= primary_m.work_rect.left, "HUD status x must be within work_rect left");
        assert!(placement.x + placement.width <= primary_m.work_rect.right, "HUD status right edge must not exceed work_rect right");
        assert!(placement.y >= primary_m.work_rect.top, "HUD status y must be within work_rect top");
        assert!(placement.y + placement.height <= primary_m.work_rect.bottom, "HUD status bottom edge must not exceed work_rect bottom");

        // 5. Hide all cleans up
        mgr.hide_all();
        assert!(matches!(mgr.current_mode, OverlayMode::Hidden));
    }
}
