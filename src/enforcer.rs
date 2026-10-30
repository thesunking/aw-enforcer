use regex::Regex;
use sysinfo::ProcessesToUpdate;
use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VK_CONTROL,
};
use windows_sys::Win32::UI::Shell::{
    SHQueryUserNotificationState, QUNS_BUSY, QUNS_PRESENTATION_MODE, QUNS_QUIET_TIME,
    QUNS_RUNNING_D3D_FULL_SCREEN,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId, IsIconic,
    IsWindowVisible, PostMessageW, WM_CLOSE,
};

/// Shared helper to check if a `sysinfo::Process` matches any target executable name.
fn process_matches_targets(
    proc: &sysinfo::Process,
    target_executables: &[String],
) -> bool {
    if target_executables.is_empty() {
        return false;
    }

    let proc_name = proc.name().to_string_lossy();
    let exe_filename = proc
        .exe()
        .and_then(|p| p.file_name())
        .map(|f| f.to_string_lossy().to_string());

    for target in target_executables {
        let trimmed = target.trim();
        if trimmed.is_empty() || trimmed == "*" {
            continue;
        }
        let target_with_ext = format!("{trimmed}.exe");
        if proc_name.eq_ignore_ascii_case(trimmed)
            || proc_name.eq_ignore_ascii_case(&target_with_ext)
        {
            return true;
        }
        if let Some(ref fname) = exe_filename {
            if fname.eq_ignore_ascii_case(trimmed)
                || fname.eq_ignore_ascii_case(&target_with_ext)
            {
                return true;
            }
        }
    }

    false
}

/// Returns true if Windows reports a fullscreen D3D app, presentation mode, busy fullscreen app, or Focus Assist quiet time.
#[allow(dead_code)]
pub fn is_fullscreen_or_presentation_active() -> bool {
    unsafe {
        let mut state: i32 = 0;
        if SHQueryUserNotificationState(&mut state) == 0 {
            state == QUNS_RUNNING_D3D_FULL_SCREEN
                || state == QUNS_PRESENTATION_MODE
                || state == QUNS_BUSY
                || state == QUNS_QUIET_TIME
        } else {
            false
        }
    }
}

/// Sends a passive native Windows notification via WinRT.
pub fn send_system_notification(title: &str, message: &str) {
    if let Err(e) = winrt_notification::Toast::new(winrt_notification::Toast::POWERSHELL_APP_ID)
        .title(title)
        .text1(message)
        .duration(winrt_notification::Duration::Short)
        .show()
    {
        tracing::warn!("Failed to send WinRT notification: {e}");
    }
}

/// Locks the Windows workstation immediately.
pub fn lock_workstation() -> bool {
    tracing::info!("Locking workstation via LockWorkStation()");
    unsafe { windows_sys::Win32::System::Shutdown::LockWorkStation() != 0 }
}

/// Context passed into the Win32 `EnumWindows` callback to match and optionally close top-level windows.
struct EnumMatchContext<'a> {
    target_executables: &'a [String],
    compiled_regex: Option<&'a Regex>,
    system: Option<&'a sysinfo::System>,
    close_matched: bool,
    matched_count: usize,
    matched_name: Option<String>,
}

/// Win32 `EnumWindows` callback that inspects visible, non-minimized windows against title regex or process targets.
unsafe extern "system" fn enum_windows_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let ctx = &mut *(lparam as *mut EnumMatchContext);
    if IsWindowVisible(hwnd) == 0 || IsIconic(hwnd) != 0 {
        return 1;
    }

    let mut matches = false;
    let mut title = String::new();

    if ctx.compiled_regex.is_some() || ctx.close_matched {
        let mut title_buf = [0u16; 512];
        let len = GetWindowTextW(hwnd, title_buf.as_mut_ptr(), title_buf.len() as i32);
        if len > 0 {
            title = String::from_utf16_lossy(&title_buf[..len as usize]);
        }
    }

    if let Some(re) = ctx.compiled_regex {
        if !title.is_empty() && re.is_match(&title) {
            matches = true;
            if ctx.matched_name.is_none() {
                ctx.matched_name = Some(title.clone());
            }
        }
    }

    if !matches {
        if let Some(system) = ctx.system {
            let mut pid: u32 = 0;
            GetWindowThreadProcessId(hwnd, &mut pid);
            if let Some(proc) = system.process(sysinfo::Pid::from_u32(pid)) {
                if process_matches_targets(proc, ctx.target_executables) {
                    matches = true;
                    if ctx.matched_name.is_none() {
                        ctx.matched_name = Some(proc.name().to_string_lossy().to_string());
                    }
                }
            }
        }
    }

    if matches {
        ctx.matched_count += 1;
        if ctx.close_matched {
            tracing::info!(
                "Posting WM_CLOSE to matching window (HWND: {:?}, Title: '{}')",
                hwnd,
                title
            );
            PostMessageW(hwnd, WM_CLOSE, 0, 0);
        } else {
            return 0; // Stop enumeration early on first match
        }
    }

    1
}

/// Enumerates top-level windows, optionally sending `WM_CLOSE` to matches, and returns `(matched_count, first_matched_name)`.
fn scan_windows(
    target_executables: &[String],
    compiled_regex: Option<&Regex>,
    close_matched: bool,
) -> (usize, Option<String>) {
    let has_exe_targets = target_executables
        .iter()
        .any(|t| !t.trim().is_empty() && t.trim() != "*");

    let system = has_exe_targets.then(|| {
        let mut s = sysinfo::System::new();
        s.refresh_processes(ProcessesToUpdate::All);
        s
    });

    let mut ctx = EnumMatchContext {
        target_executables,
        compiled_regex,
        system: system.as_ref(),
        close_matched,
        matched_count: 0,
        matched_name: None,
    };

    unsafe {
        EnumWindows(
            Some(enum_windows_callback),
            &mut ctx as *mut EnumMatchContext as isize,
        );
    }

    (ctx.matched_count, ctx.matched_name)
}

/// Closes top-level visible windows matching target executables or title regex by sending WM_CLOSE.
pub fn close_target_windows(
    target_executables: &[String],
    compiled_regex: Option<&Regex>,
) -> usize {
    let is_wildcard = compiled_regex.is_none()
        && (target_executables.is_empty()
            || target_executables.iter().any(|t| t.trim() == "*"));
    if is_wildcard {
        unsafe {
            let hwnd = GetForegroundWindow();
            if !hwnd.is_null() && IsWindowVisible(hwnd) != 0 && IsIconic(hwnd) == 0 {
                PostMessageW(hwnd, WM_CLOSE, 0, 0);
                return 1;
            }
        }
        return 0;
    }
    scan_windows(target_executables, compiled_regex, true).0
}

/// Finds the first matching visible, non-minimized window and returns the matched title or process name.
#[allow(dead_code)]
pub fn find_matching_window(
    target_executables: &[String],
    compiled_regex: Option<&Regex>,
) -> Option<String> {
    scan_windows(target_executables, compiled_regex, false).1
}

/// Terminates processes matching target executables via sysinfo.
pub fn kill_target_processes(target_executables: &[String]) -> usize {
    if target_executables.is_empty() {
        return 0;
    }
    let mut system = sysinfo::System::new();
    system.refresh_processes(ProcessesToUpdate::All);

    let mut killed_count = 0;
    for (_pid, process) in system.processes() {
        // Never allow wildcard "*" to terminate all system processes
        if process_matches_targets(process, target_executables) {
            tracing::info!(
                "Terminating target process: {} (PID: {})",
                process.name().to_string_lossy(),
                process.pid()
            );
            if process.kill() {
                killed_count += 1;
            }
        }
    }
    killed_count
}

/// Sends Ctrl+W to close the active tab if the foreground window matches the target criteria;
/// returns true if Ctrl+W was sent.
#[allow(dead_code)]
pub fn close_active_browser_tab(
    target_executables: &[String],
    compiled_regex: Option<&Regex>,
) -> bool {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() || IsWindowVisible(hwnd) == 0 || IsIconic(hwnd) != 0 {
            return false;
        }

        let mut matches = false;
        if let Some(re) = compiled_regex {
            let mut title_buf = [0u16; 512];
            let len = GetWindowTextW(hwnd, title_buf.as_mut_ptr(), title_buf.len() as i32);
            if len > 0 {
                let title = String::from_utf16_lossy(&title_buf[..len as usize]);
                if re.is_match(&title) {
                    matches = true;
                }
            }
        }

        if !matches && !target_executables.is_empty() {
            if target_executables.iter().any(|t| t.trim() == "*") {
                matches = true;
            } else {
                let mut sys = sysinfo::System::new();
                sys.refresh_processes(ProcessesToUpdate::All);
                let mut pid: u32 = 0;
                GetWindowThreadProcessId(hwnd, &mut pid);
                if let Some(proc) = sys.process(sysinfo::Pid::from_u32(pid)) {
                    matches = process_matches_targets(proc, target_executables);
                }
            }
        }

        if matches {
            let make_key = |vk: u16, flags: u32| INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: vk,
                        wScan: 0,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };
            let inputs = [
                make_key(VK_CONTROL, 0),
                make_key(b'W' as u16, 0),
                make_key(b'W' as u16, KEYEVENTF_KEYUP),
                make_key(VK_CONTROL, KEYEVENTF_KEYUP),
            ];
            SendInput(
                inputs.len() as u32,
                inputs.as_ptr(),
                std::mem::size_of::<INPUT>() as i32,
            );
            tracing::info!("Sent Ctrl+W to close active tab on HWND {:?}", hwnd);
            return true;
        }
    }
    false
}
