use regex::Regex;
use windows_sys::Win32::Foundation::{
    CloseHandle, BOOL, HWND, INVALID_HANDLE_VALUE, LPARAM,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, TerminateProcess,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VK_CONTROL,
};
use windows_sys::Win32::UI::Shell::{
    SHQueryUserNotificationState, QUNS_BUSY, QUNS_PRESENTATION_MODE, QUNS_QUIET_TIME,
    QUNS_RUNNING_D3D_FULL_SCREEN,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId,
    IsIconic, IsWindowVisible, PostMessageW, WM_CLOSE,
};

/// Canonical list of known browser executables used for URL rules and tab-closing actions.
pub const BROWSER_EXES: &[&str] = &[
    "chrome.exe",
    "msedge.exe",
    "firefox.exe",
    "brave.exe",
    "opera.exe",
    "vivaldi.exe",
];

/// Checks if `exe_name` matches any target in `target_executables` (case-insensitive, with or without `.exe`).
pub fn exe_name_matches_targets<S: AsRef<str>>(
    exe_name: &str,
    target_executables: &[S],
) -> bool {
    let exe_trimmed = exe_name.trim();
    if exe_trimmed.is_empty() || target_executables.is_empty() {
        return false;
    }
    let exe_stem = exe_trimmed
        .strip_suffix(".exe")
        .or_else(|| exe_trimmed.strip_suffix(".EXE"))
        .unwrap_or_else(|| {
            if exe_trimmed.len() > 4
                && exe_trimmed[exe_trimmed.len() - 4..].eq_ignore_ascii_case(".exe")
            {
                &exe_trimmed[..exe_trimmed.len() - 4]
            } else {
                exe_trimmed
            }
        });

    for target in target_executables {
        let t = target.as_ref().trim();
        if t.is_empty() || t == "*" {
            continue;
        }
        if exe_trimmed.eq_ignore_ascii_case(t) || exe_stem.eq_ignore_ascii_case(t) {
            return true;
        }
    }
    false
}

/// Iterates over all running processes via a Win32 ToolHelp snapshot, invoking `f(pid, exe_name)`.
pub fn for_each_process(mut f: impl FnMut(u32, &str)) {
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot.is_null() || snapshot == INVALID_HANDLE_VALUE {
            return;
        }

        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let exe_name = String::from_utf16_lossy(&entry.szExeFile[..len]);
                f(entry.th32ProcessID, &exe_name);

                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }

        CloseHandle(snapshot);
    }
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

/// Returns true if the window is invalid, invisible, minimized, owned by current process, or a Windows shell surface.
unsafe fn is_protected_window(hwnd: HWND) -> bool {
    if hwnd.is_null() || IsWindowVisible(hwnd) == 0 || IsIconic(hwnd) != 0 {
        return true;
    }

    let mut pid: u32 = 0;
    GetWindowThreadProcessId(hwnd, &mut pid);
    if pid == GetCurrentProcessId() {
        return true;
    }

    let mut class_buf = [0u16; 256];
    let len = GetClassNameW(hwnd, class_buf.as_mut_ptr(), class_buf.len() as i32);
    if len > 0 {
        let class_name = String::from_utf16_lossy(&class_buf[..len as usize]);
        match class_name.as_str() {
            "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd" => return true,
            _ => {}
        }
    }

    false
}

/// Context passed into the Win32 `EnumWindows` callback to match and optionally close top-level windows.
struct EnumMatchContext<'a, S: AsRef<str>> {
    target_executables: &'a [S],
    compiled_regex: Option<&'a Regex>,
    has_exe_targets: bool,
    close_matched: bool,
    matched_count: usize,
    matched_name: Option<String>,
}

/// Win32 `EnumWindows` callback that inspects visible, non-minimized windows against title regex or process targets.
unsafe extern "system" fn enum_windows_callback<S: AsRef<str>>(hwnd: HWND, lparam: LPARAM) -> BOOL {
    if is_protected_window(hwnd) {
        return 1;
    }
    let ctx = &mut *(lparam as *mut EnumMatchContext<'_, S>);

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
fn scan_windows<S: AsRef<str>>(
    target_executables: &[S],
    compiled_regex: Option<&Regex>,
    close_matched: bool,
) -> (usize, Option<String>) {
    let has_exe_targets = target_executables
        .iter()
        .any(|t| {
            let s = t.as_ref().trim();
            !s.is_empty() && s != "*"
        });

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
            Some(enum_windows_callback::<S>),
            &mut ctx as *mut EnumMatchContext<'_, S> as isize,
        );
    }

    (ctx.matched_count, ctx.matched_name)
}

/// Closes top-level visible windows matching target executables or title regex by sending WM_CLOSE.
pub fn close_target_windows<S: AsRef<str>>(
    target_executables: &[S],
    compiled_regex: Option<&Regex>,
) -> usize {
    let is_wildcard = compiled_regex.is_none()
        && target_executables.iter().any(|t| t.as_ref().trim() == "*");
    if is_wildcard {
        unsafe {
            let hwnd = GetForegroundWindow();
            if !is_protected_window(hwnd) {
                PostMessageW(hwnd, WM_CLOSE, 0, 0);
                return 1;
            }
        }
        return 0;
    }
    scan_windows(target_executables, compiled_regex, true).0
}

/// Finds the first matching visible, non-minimized window and returns the matched title or process name.
pub fn find_matching_window<S: AsRef<str>>(
    target_executables: &[S],
    compiled_regex: Option<&Regex>,
) -> Option<String> {
    scan_windows(target_executables, compiled_regex, false).1
}

/// Terminates processes matching target executables via Win32 ToolHelp snapshot.
pub fn kill_target_processes<S: AsRef<str>>(target_executables: &[S]) -> usize {
    if target_executables.is_empty() {
        return 0;
    }
    let has_specific_target = target_executables
        .iter()
        .any(|t| {
            let s = t.as_ref().trim();
            !s.is_empty() && s != "*"
        });
    if !has_specific_target {
        return 0;
    }

    let my_pid = unsafe { GetCurrentProcessId() };
    let mut killed_count = 0;

    for_each_process(|pid, exe_name| {
        if pid == my_pid || pid == 0 {
            return;
        }

        // Never allow wildcard "*" to terminate all system processes
        if exe_name_matches_targets(exe_name, target_executables) {
            tracing::info!(
                "Terminating target process: {} (PID: {})",
                exe_name,
                pid
            );
            unsafe {
                let h_proc = OpenProcess(PROCESS_TERMINATE, 0, pid);
                if !h_proc.is_null() {
                    if TerminateProcess(h_proc, 1) != 0 {
                        killed_count += 1;
                    }
                    CloseHandle(h_proc);
                }
            }
        }
    });

    killed_count
}

/// Sends Ctrl+W to close the active tab if the foreground window matches the target criteria;
/// returns true if Ctrl+W was sent.
pub fn close_active_browser_tab<S: AsRef<str>>(
    target_executables: &[S],
    compiled_regex: Option<&Regex>,
) -> bool {
    unsafe {
        let hwnd = GetForegroundWindow();
        if is_protected_window(hwnd) {
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
            if target_executables.iter().any(|t| t.as_ref().trim() == "*") {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_close_target_windows_empty_targets_no_wildcard() {
        // Must return 0 and not close foreground window
        let closed = close_target_windows::<&str>(&[], None);
        assert_eq!(closed, 0);
    }
}
