use regex::Regex;
use windows_sys::Win32::Foundation::{
    CloseHandle, BOOL, HWND, INVALID_HANDLE_VALUE, LPARAM,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, TerminateProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_TERMINATE,
};
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

/// Helper to check if an executable name matches any target executable name.
fn exe_name_matches_targets(
    exe_name: &str,
    target_executables: &[String],
) -> bool {
    if target_executables.is_empty() {
        return false;
    }

    for target in target_executables {
        let trimmed = target.trim();
        if trimmed.is_empty() || trimmed == "*" {
            continue;
        }
        let target_with_ext = format!("{trimmed}.exe");
        if exe_name.eq_ignore_ascii_case(trimmed)
            || exe_name.eq_ignore_ascii_case(&target_with_ext)
        {
            return true;
        }
    }

    false
}

/// Retrieves the executable file name for a process ID using `QueryFullProcessImageNameW` on a stack buffer.
pub fn get_process_name_by_pid(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }

        let mut buf = [0u16; 260];
        let mut size = buf.len() as u32;
        let success = QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut size);
        CloseHandle(handle);

        if success != 0 && size > 0 {
            let full_path = String::from_utf16_lossy(&buf[..size as usize]);
            let exe_name = full_path
                .rsplit_once('\\')
                .map(|(_, name)| name)
                .unwrap_or(&full_path);
            Some(exe_name.to_string())
        } else {
            None
        }
    }
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
    has_exe_targets: bool,
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

    if !matches && ctx.has_exe_targets {
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if let Some(exe_name) = get_process_name_by_pid(pid) {
            if exe_name_matches_targets(&exe_name, ctx.target_executables) {
                matches = true;
                if ctx.matched_name.is_none() {
                    ctx.matched_name = Some(exe_name);
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

    let mut ctx = EnumMatchContext {
        target_executables,
        compiled_regex,
        has_exe_targets,
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

/// Terminates processes matching target executables via Win32 ToolHelp snapshot.
pub fn kill_target_processes(target_executables: &[String]) -> usize {
    if target_executables.is_empty() {
        return 0;
    }
    let has_specific_target = target_executables
        .iter()
        .any(|t| !t.trim().is_empty() && t.trim() != "*");
    if !has_specific_target {
        return 0;
    }

    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot.is_null() || snapshot == INVALID_HANDLE_VALUE {
            return 0;
        }

        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

        let mut killed_count = 0;
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let exe_name = String::from_utf16_lossy(&entry.szExeFile[..len]);

                // Never allow wildcard "*" to terminate all system processes
                if exe_name_matches_targets(&exe_name, target_executables) {
                    tracing::info!(
                        "Terminating target process: {} (PID: {})",
                        exe_name,
                        entry.th32ProcessID
                    );
                    let h_proc = OpenProcess(PROCESS_TERMINATE, 0, entry.th32ProcessID);
                    if !h_proc.is_null() {
                        if TerminateProcess(h_proc, 1) != 0 {
                            killed_count += 1;
                        }
                        CloseHandle(h_proc);
                    }
                }

                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }

        CloseHandle(snapshot);
        killed_count
    }
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
                let mut pid: u32 = 0;
                GetWindowThreadProcessId(hwnd, &mut pid);
                if let Some(exe_name) = get_process_name_by_pid(pid) {
                    matches = exe_name_matches_targets(&exe_name, target_executables);
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
