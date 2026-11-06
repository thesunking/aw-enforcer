use regex::Regex;
use std::sync::Once;
use winreg::enums::HKEY_CURRENT_USER;
use winreg::RegKey;
use windows_sys::Win32::Foundation::{
    CloseHandle, BOOL, HWND, INVALID_HANDLE_VALUE, LPARAM,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
#[cfg(not(test))]
use windows_sys::Win32::System::Threading::{TerminateProcess, PROCESS_TERMINATE};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VK_CONTROL, VK_LCONTROL,
    VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU, VK_RCONTROL, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT,
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
    "chromium.exe",
    "arc.exe",
    "zen.exe",
    "librewolf.exe",
    "waterfox.exe",
    "floorp.exe",
    "thorium.exe",
];

/// Helper retrieving the process ID owning the given window handle.
#[inline]
unsafe fn get_window_pid(hwnd: HWND) -> u32 {
    let mut pid: u32 = 0;
    GetWindowThreadProcessId(hwnd, &mut pid);
    pid
}

/// Helper retrieving the window title as a UTF-8 string on a 512-word stack buffer.
unsafe fn get_window_title(hwnd: HWND) -> String {
    let mut title_buf = [0u16; 512];
    let len = GetWindowTextW(hwnd, title_buf.as_mut_ptr(), title_buf.len() as i32);
    if len > 0 {
        String::from_utf16_lossy(&title_buf[..len as usize])
    } else {
        String::new()
    }
}

/// Helper retrieving the executable name of the process owning the given window handle.
#[inline]
unsafe fn get_window_process_name(hwnd: HWND) -> Option<String> {
    get_process_name_by_pid(get_window_pid(hwnd))
}

/// Helper to post WM_CLOSE to a window with logging.
unsafe fn post_wm_close(hwnd: HWND, title: Option<&str>) -> bool {
    if let Some(t) = title {
        tracing::info!(
            "Posting WM_CLOSE to matching window (HWND: {:?}, Title: '{}')",
            hwnd,
            t
        );
    }
    if PostMessageW(hwnd, WM_CLOSE, 0, 0) != 0 {
        true
    } else {
        tracing::warn!(
            "PostMessageW(WM_CLOSE) failed or was blocked by UIPI on HWND {:?}",
            hwnd
        );
        false
    }
}

/// Returns the process executable name (or non-empty title fallback) of the current foreground window,
/// returning None if there is no foreground window, or if it is a protected shell / application window.
pub fn get_foreground_app_name() -> Option<String> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if is_protected_foreground_window(hwnd) {
            return None;
        }

        if let Some(exe_name) = get_window_process_name(hwnd) {
            if !exe_name.trim().is_empty() {
                return Some(exe_name);
            }
        }

        let title = get_window_title(hwnd);
        let trimmed = title.trim();
        if !trimmed.is_empty() {
            Some(trimmed.to_string())
        } else {
            None
        }
    }
}

/// Checks if the current foreground window is a non-protected window belonging to an executable in `BROWSER_EXES`.
pub fn is_foreground_browser() -> bool {
    unsafe {
        let hwnd = GetForegroundWindow();
        if is_protected_foreground_window(hwnd) {
            return false;
        }
        get_window_process_name(hwnd)
            .map_or(false, |exe| exe_name_matches_targets(&exe, BROWSER_EXES))
    }
}

/// Trims whitespace, strips any leading directory path, and returns `(basename, stem_without_exe)`
/// handling case-insensitive `.exe` suffixes.
pub fn normalize_exe_stem(name: &str) -> (&str, &str) {
    let trimmed = name.trim();
    let basename = match trimmed.rsplit_once(['\\', '/']) {
        Some((_, base)) => base.trim(),
        None => trimmed,
    };
    let stem = if basename.len() >= 4 && basename[basename.len() - 4..].eq_ignore_ascii_case(".exe") {
        &basename[..basename.len() - 4]
    } else {
        basename
    };
    (basename, stem)
}

#[inline]
fn stem_or_base_matches(
    exe_base: &str,
    exe_stem: &str,
    target_base: &str,
    target_stem: &str,
) -> bool {
    exe_base.eq_ignore_ascii_case(target_base)
        || exe_base.eq_ignore_ascii_case(target_stem)
        || exe_stem.eq_ignore_ascii_case(target_base)
        || exe_stem.eq_ignore_ascii_case(target_stem)
}

/// Returns true if `exe_name` matches any `!`-prefixed exclusion entry in `target_executables`.
pub fn exe_name_is_excluded<S: AsRef<str>>(exe_name: &str, target_executables: &[S]) -> bool {
    let (exe_base, exe_stem) = normalize_exe_stem(exe_name);
    if exe_base.is_empty() {
        return false;
    }
    for target in target_executables {
        let t = target.as_ref().trim();
        if let Some(rest) = t.strip_prefix('!') {
            let (target_base, target_stem) = normalize_exe_stem(rest);
            if !target_base.is_empty()
                && stem_or_base_matches(exe_base, exe_stem, target_base, target_stem)
            {
                return true;
            }
        }
    }
    false
}

/// Checks if `exe_name` matches any target in `target_executables` (case-insensitive, with or without `.exe`),
/// respecting `!`-prefixed exclusion entries.
pub fn exe_name_matches_targets<S: AsRef<str>>(
    exe_name: &str,
    target_executables: &[S],
) -> bool {
    let (exe_base, exe_stem) = normalize_exe_stem(exe_name);
    if exe_base.is_empty() || target_executables.is_empty() {
        return false;
    }

    if exe_name_is_excluded(exe_name, target_executables) {
        return false;
    }

    let has_exclusions = has_exclusion_targets(target_executables);
    let has_pos_specific = has_specific_targets(target_executables);
    let has_wildcard = has_wildcard_target(target_executables);

    if has_exclusions && (has_wildcard || !has_pos_specific) {
        return true;
    }

    for target in target_executables {
        let t = target.as_ref().trim();
        if t.is_empty() || t == "*" || t.starts_with('!') {
            continue;
        }
        let (t_base, t_stem) = normalize_exe_stem(t);
        if !t_base.is_empty() && stem_or_base_matches(exe_base, exe_stem, t_base, t_stem) {
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

        let mut buf = [0u16; 1024];
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
            matches!(
                state,
                QUNS_RUNNING_D3D_FULL_SCREEN | QUNS_PRESENTATION_MODE | QUNS_BUSY | QUNS_QUIET_TIME
            )
        } else {
            false
        }
    }
}

/// Application User Model ID (AUMID) used for native Windows Action Center notifications.
pub const APP_AUMID: &str = "AW-Enforcer";
/// Display name associated with the AUMID shown in Windows notification headers.
pub const APP_DISPLAY_NAME: &str = "AW-Enforcer";
const AUMID_REG_PATH: &str = r"Software\Classes\AppUserModelId\AW-Enforcer";

static AUMID_REGISTERED: Once = Once::new();

/// Registers the application user model ID (AUMID) in the Windows Registry under
/// `HKCU\Software\Classes\AppUserModelId\AW-Enforcer` so that native Action Center
/// toast notifications display "AW-Enforcer" as the header instead of "Windows PowerShell".
pub fn register_app_user_model_id() -> Result<(), Box<dyn std::error::Error>> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey(AUMID_REG_PATH)?;
    key.set_value("DisplayName", &APP_DISPLAY_NAME)?;
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_str) = exe_path.to_str() {
            let _ = key.set_value("IconUri", &exe_str);
        }
    }
    tracing::info!("Registered AUMID '{}' in Windows Registry", APP_AUMID);
    Ok(())
}

/// Ensures the application user model ID (AUMID) has been registered in the Windows Registry at least once.
pub fn ensure_app_user_model_id_registered() {
    AUMID_REGISTERED.call_once(|| {
        if let Err(e) = register_app_user_model_id() {
            tracing::warn!("Failed to register AUMID '{APP_AUMID}' in registry: {e}");
        }
    });
}

#[cfg(test)]
static TEST_NOTIFICATIONS_SENT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static LAST_TEST_NOTIFICATION: std::sync::Mutex<Option<(String, String)>> = std::sync::Mutex::new(None);

#[cfg(test)]
/// Returns the total count of native system notifications dispatched during tests.
pub fn get_test_notifications_sent() -> usize {
    TEST_NOTIFICATIONS_SENT.load(std::sync::atomic::Ordering::SeqCst)
}

#[cfg(test)]
/// Returns the most recently dispatched native system notification (title, message) in test mode.
pub fn get_last_test_notification() -> Option<(String, String)> {
    LAST_TEST_NOTIFICATION.lock().unwrap().clone()
}

#[cfg(test)]
#[allow(dead_code)]
/// Resets test notification tracking counters and records.
pub fn reset_test_notifications() {
    TEST_NOTIFICATIONS_SENT.store(0, std::sync::atomic::Ordering::SeqCst);
    *LAST_TEST_NOTIFICATION.lock().unwrap() = None;
}

/// Sends a passive native Windows notification via WinRT using the branded AUMID.
pub fn send_system_notification(title: &str, message: &str) {
    #[cfg(test)]
    {
        tracing::debug!("[TEST] send_system_notification suppressed: title='{title}', message='{message}'");
        TEST_NOTIFICATIONS_SENT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut last = LAST_TEST_NOTIFICATION.lock().unwrap();
        *last = Some((title.to_string(), message.to_string()));
        return;
    }
    #[cfg(not(test))]
    {
        ensure_app_user_model_id_registered();

        let res = winrt_notification::Toast::new(APP_AUMID)
            .title(title)
            .text1(message)
            .duration(winrt_notification::Duration::Short)
            .show();

        if let Err(e) = res {
            tracing::warn!(
                "Failed to send WinRT notification with custom AUMID '{APP_AUMID}': {e}. Falling back to PowerShell AUMID."
            );
            if let Err(e2) = winrt_notification::Toast::new(winrt_notification::Toast::POWERSHELL_APP_ID)
                .title(title)
                .text1(message)
                .duration(winrt_notification::Duration::Short)
                .show()
            {
                tracing::warn!("Failed to send WinRT notification with fallback AUMID: {e2}");
            }
        }
    }
}

/// Locks the Windows workstation immediately.
pub fn lock_workstation() -> bool {
    tracing::info!("Locking workstation via LockWorkStation()");
    #[cfg(test)]
    {
        true
    }
    #[cfg(not(test))]
    unsafe { windows_sys::Win32::System::Shutdown::LockWorkStation() != 0 }
}

const PROTECTED_CLASSES: &[&str] = &[
    "Progman",
    "WorkerW",
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
    "NotifyIconOverflowWindow",
    "TopLevelWindowForOverflowXamlIsland",
    "MultitaskingViewFrame",
    "Windows.UI.Core.CoreWindow",
    "XamlExplorerHostIslandWindow",
    "ForegroundStaging",
];

#[inline]
fn utf16_matches_ascii(utf16: &[u16], ascii: &str) -> bool {
    utf16.len() == ascii.len()
        && utf16
            .iter()
            .zip(ascii.bytes())
            .all(|(&u, b)| u == b as u16)
}

/// Returns true if the UTF-16 class name corresponds to a protected Windows shell class without heap allocation.
pub fn is_protected_window_class(class_name: &[u16]) -> bool {
    PROTECTED_CLASSES
        .iter()
        .any(|&cls| utf16_matches_ascii(class_name, cls))
}

/// Returns true if the window is invalid, invisible, owned by current process, or a Windows shell surface.
/// Does not reject minimized (iconic) windows.
unsafe fn is_protected_window_base(hwnd: HWND) -> bool {
    if hwnd.is_null() || IsWindowVisible(hwnd) == 0 {
        return true;
    }

    if get_window_pid(hwnd) == GetCurrentProcessId() {
        return true;
    }

    let mut class_buf = [0u16; 256];
    let len = GetClassNameW(hwnd, class_buf.as_mut_ptr(), class_buf.len() as i32);
    if len > 0 && is_protected_window_class(&class_buf[..len as usize]) {
        return true;
    }

    false
}

/// Returns true if the window is protected, invisible, owned by current process, a shell surface, or minimized.
unsafe fn is_protected_foreground_window(hwnd: HWND) -> bool {
    is_protected_window_base(hwnd) || IsIconic(hwnd) != 0
}

/// Context passed into the Win32 `EnumWindows` callback to match and optionally close top-level windows.
struct EnumMatchContext<'a, S: AsRef<str>> {
    target_executables: &'a [S],
    compiled_regex: Option<&'a Regex>,
    has_exe_targets: bool,
    close_matched: bool,
    matched_count: usize,
    matched_name: Option<String>,
    pid_cache: std::collections::HashMap<u32, Option<String>>,
}

/// Win32 `EnumWindows` callback that inspects visible top-level windows (including minimized taskbar windows) against title regex or process targets.
unsafe extern "system" fn enum_windows_callback<S: AsRef<str>>(hwnd: HWND, lparam: LPARAM) -> BOOL {
    if is_protected_window_base(hwnd) {
        return 1;
    }
    let ctx = &mut *(lparam as *mut EnumMatchContext<'_, S>);

    let pid = get_window_pid(hwnd);
    let cached_exe = ctx
        .pid_cache
        .entry(pid)
        .or_insert_with(|| get_process_name_by_pid(pid))
        .clone();

    if let Some(ref exe_name) = cached_exe {
        if exe_name_is_excluded(exe_name, ctx.target_executables) {
            return 1;
        }
    }

    let title = if ctx.compiled_regex.is_some() || ctx.close_matched {
        get_window_title(hwnd)
    } else {
        String::new()
    };

    let title_matches = match ctx.compiled_regex {
        Some(re) => !title.is_empty() && re.is_match(&title),
        None => false,
    };

    if ctx.compiled_regex.is_some() && !title_matches {
        return 1;
    }

    let mut exe_name_opt = cached_exe.clone();
    let exe_matches = if ctx.has_exe_targets {
        if let Some(ref exe_name) = cached_exe {
            let matched = exe_name_matches_targets(exe_name, ctx.target_executables);
            exe_name_opt = Some(exe_name.clone());
            matched
        } else {
            false
        }
    } else {
        false
    };

    let matches = match (ctx.compiled_regex.is_some(), ctx.has_exe_targets) {
        (true, true) => title_matches && exe_matches,
        (true, false) => title_matches,
        (false, true) => exe_matches,
        (false, false) => false,
    };

    if matches {
        if ctx.matched_name.is_none() {
            ctx.matched_name = if !title.is_empty() {
                Some(title.clone())
            } else {
                exe_name_opt
            };
        }
        if ctx.close_matched {
            if post_wm_close(hwnd, Some(&title)) {
                ctx.matched_count += 1;
            }
        } else {
            ctx.matched_count += 1;
            return 0; // Stop enumeration early on first match
        }
    }

    1
}

/// Returns true if `targets` contains at least one non-empty, non-wildcard, non-exclusion executable entry.
pub fn has_specific_targets<S: AsRef<str>>(targets: &[S]) -> bool {
    targets.iter().any(|t| {
        let s = t.as_ref().trim();
        !s.is_empty() && s != "*" && !s.starts_with('!')
    })
}

/// Returns true if `targets` contains at least one non-empty `!`-prefixed exclusion entry.
pub fn has_exclusion_targets<S: AsRef<str>>(targets: &[S]) -> bool {
    targets.iter().any(|t| {
        let s = t.as_ref().trim();
        s.starts_with('!') && !s[1..].trim().is_empty()
    })
}

/// Returns true if `targets` contains a wildcard `"*"` entry.
pub fn has_wildcard_target<S: AsRef<str>>(targets: &[S]) -> bool {
    targets.iter().any(|t| t.as_ref().trim() == "*")
}

/// Enumerates top-level windows, optionally sending `WM_CLOSE` to matches, and returns `(matched_count, first_matched_name)`.
fn scan_windows<S: AsRef<str>>(
    target_executables: &[S],
    compiled_regex: Option<&Regex>,
    close_matched: bool,
) -> (usize, Option<String>) {
    let has_exe_targets = has_specific_targets(target_executables);

    let mut ctx = EnumMatchContext {
        target_executables,
        compiled_regex,
        has_exe_targets,
        close_matched,
        matched_count: 0,
        matched_name: None,
        pid_cache: std::collections::HashMap::new(),
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
    let is_wildcard = compiled_regex.is_none() && has_wildcard_target(target_executables);
    if is_wildcard {
        unsafe {
            let hwnd = GetForegroundWindow();
            if !is_protected_foreground_window(hwnd) {
                if let Some(exe_name) = get_window_process_name(hwnd) {
                    if exe_name_is_excluded(&exe_name, target_executables) {
                        return 0;
                    }
                }
                if post_wm_close(hwnd, None) {
                    return 1;
                }
            }
        }
        return 0;
    }
    scan_windows(target_executables, compiled_regex, true).0
}

/// Finds the first matching visible top-level window (including minimized taskbar windows) and returns the matched title or process name.
pub fn find_matching_window<S: AsRef<str>>(
    target_executables: &[S],
    compiled_regex: Option<&Regex>,
) -> Option<String> {
    scan_windows(target_executables, compiled_regex, false).1
}

/// Finds the first running process matching `target_executables` via ToolHelp snapshot,
/// skipping PID 0 and the current process ID. Returns `None` if targets are empty or wildcard.
pub fn find_matching_process<S: AsRef<str>>(target_executables: &[S]) -> Option<String> {
    if target_executables.is_empty() || !has_specific_targets(target_executables) {
        return None;
    }

    let my_pid = unsafe { GetCurrentProcessId() };
    let mut matched_exe = None;

    for_each_process(|pid, exe_name| {
        if matched_exe.is_some() || pid == my_pid || pid == 0 {
            return;
        }

        if exe_name_matches_targets(exe_name, target_executables) {
            matched_exe = Some(exe_name.to_string());
        }
    });

    matched_exe
}

/// Terminates processes matching target executables via Win32 ToolHelp snapshot.
pub fn kill_target_processes<S: AsRef<str>>(target_executables: &[S]) -> usize {
    if target_executables.is_empty() || !has_specific_targets(target_executables) {
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
            #[cfg(test)]
            {
                killed_count += 1;
            }
            #[cfg(not(test))]
            {
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
        }
    });

    killed_count
}

/// Inspects the current foreground window (`GetForegroundWindow`).
/// Returns `None` if the foreground window is protected, minimized, or does not match the target executables and/or compiled regex.
/// Otherwise, returns `Some(window_title)` or `Some(process_name)`.
pub fn find_matching_foreground_window<S: AsRef<str>>(
    target_executables: &[S],
    compiled_regex: Option<&Regex>,
) -> Option<String> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if is_protected_foreground_window(hwnd) {
            return None;
        }

        let fg_exe = get_window_process_name(hwnd);
        if let Some(ref exe) = fg_exe {
            if exe_name_is_excluded(exe, target_executables) {
                return None;
            }
        }

        let has_wildcard = has_wildcard_target(target_executables);
        let has_exe_targets = has_specific_targets(target_executables);

        let window_title = get_window_title(hwnd);

        let title_matches = match compiled_regex {
            Some(re) => !window_title.is_empty() && re.is_match(&window_title),
            None => false,
        };

        if compiled_regex.is_some() && !title_matches {
            return None;
        }

        let mut matched_exe_name = None;
        let exe_matches = if has_exe_targets {
            if let Some(exe_name) = fg_exe {
                let matches = exe_name_matches_targets(&exe_name, target_executables);
                if matches {
                    matched_exe_name = Some(exe_name);
                }
                matches
            } else {
                false
            }
        } else {
            has_wildcard
        };

        let matches = match (compiled_regex.is_some(), has_exe_targets) {
            (true, true) => title_matches && exe_matches,
            (true, false) => title_matches && (exe_matches || target_executables.is_empty()),
            (false, true) => exe_matches,
            (false, false) => has_wildcard,
        };

        if matches {
            if !window_title.is_empty() {
                Some(window_title)
            } else if let Some(exe) = matched_exe_name {
                Some(exe)
            } else {
                get_window_process_name(hwnd).or_else(|| Some("foreground_window".to_string()))
            }
        } else {
            None
        }
    }
}

/// Sends Ctrl+W to close the active tab if the foreground window matches the target criteria;
/// returns true if Ctrl+W was sent.
pub fn close_active_browser_tab<S: AsRef<str>>(
    target_executables: &[S],
    compiled_regex: Option<&Regex>,
) -> bool {
    if find_matching_foreground_window(target_executables, compiled_regex).is_some() {
        unsafe {
            let hwnd = GetForegroundWindow();
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
                make_key(VK_SHIFT, KEYEVENTF_KEYUP),
                make_key(VK_LSHIFT, KEYEVENTF_KEYUP),
                make_key(VK_RSHIFT, KEYEVENTF_KEYUP),
                make_key(VK_MENU, KEYEVENTF_KEYUP),
                make_key(VK_LMENU, KEYEVENTF_KEYUP),
                make_key(VK_RMENU, KEYEVENTF_KEYUP),
                make_key(VK_LWIN, KEYEVENTF_KEYUP),
                make_key(VK_RWIN, KEYEVENTF_KEYUP),
                make_key(VK_CONTROL, 0),
                make_key(b'W' as u16, 0),
                make_key(b'W' as u16, KEYEVENTF_KEYUP),
                make_key(VK_CONTROL, KEYEVENTF_KEYUP),
                make_key(VK_LCONTROL, KEYEVENTF_KEYUP),
                make_key(VK_RCONTROL, KEYEVENTF_KEYUP),
            ];
            let sent = SendInput(
                inputs.len() as u32,
                inputs.as_ptr(),
                std::mem::size_of::<INPUT>() as i32,
            );
            if sent == 0 {
                tracing::warn!(
                    "SendInput(Ctrl+W) failed or was blocked by UIPI on HWND {:?}",
                    hwnd
                );
                return false;
            }
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
    fn test_safety_guards_empty_and_wildcard_targets() {
        // Empty targets or empty title regex must safely return 0 without actions
        assert_eq!(close_target_windows::<&str>(&[], None), 0);
        assert!(!close_active_browser_tab::<&str>(&[], None));
        assert!(find_matching_foreground_window::<&str>(&[], None).is_none());
        assert!(find_matching_foreground_window::<&str>(&[""], None).is_none());
        assert!(find_matching_foreground_window::<&str>(&["   "], None).is_none());

        // kill_target_processes must refuse to terminate on empty or wildcard targets
        assert_eq!(kill_target_processes::<&str>(&[]), 0);
        assert_eq!(kill_target_processes(&["*"]), 0);
        assert_eq!(kill_target_processes(&["", "   ", "*"]), 0);
    }

    #[test]
    fn test_browser_exes_matching() {
        assert!(exe_name_matches_targets("chrome.exe", BROWSER_EXES));
        assert!(exe_name_matches_targets("CHROME.EXE", BROWSER_EXES));
        assert!(exe_name_matches_targets("msedge.exe", BROWSER_EXES));
        assert!(exe_name_matches_targets("firefox.exe", BROWSER_EXES));
        assert!(exe_name_matches_targets("chromium.exe", BROWSER_EXES));
        assert!(exe_name_matches_targets("arc.exe", BROWSER_EXES));
        assert!(exe_name_matches_targets("zen.exe", BROWSER_EXES));
        assert!(exe_name_matches_targets("librewolf.exe", BROWSER_EXES));
        assert!(exe_name_matches_targets("waterfox.exe", BROWSER_EXES));
        assert!(exe_name_matches_targets("floorp.exe", BROWSER_EXES));
        assert!(exe_name_matches_targets("thorium.exe", BROWSER_EXES));
        assert!(!exe_name_matches_targets("notepad.exe", BROWSER_EXES));
        assert!(!exe_name_matches_targets("code.exe", BROWSER_EXES));

        // Test stem matching without .exe extension
        assert!(exe_name_matches_targets("chrome.exe", &["chrome"]));
        assert!(exe_name_matches_targets("code.exe", &["CODE"]));
        assert!(exe_name_matches_targets("discord.exe", &["discord.exe"]));
        assert!(!exe_name_matches_targets("notepad.exe", &["*", ""]));

        // Test symmetric stem and path matching
        assert!(exe_name_matches_targets("STEAM", &["steam.exe"]));
        assert!(exe_name_matches_targets(r"C:\Games\steam.exe", &["steam"]));
        assert!(exe_name_matches_targets("C:/Program Files/App/app.exe", &["app.exe"]));

        // Test !-prefixed exclusions
        assert!(exe_name_is_excluded("zoom.exe", &["*", "!zoom.exe"]));
        assert!(exe_name_is_excluded(r"C:\Program Files\Zoom\bin\zoom.exe", &["!zoom"]));
        assert!(!exe_name_is_excluded("notepad.exe", &["*", "!zoom.exe"]));
        assert!(!exe_name_matches_targets("zoom.exe", &["*", "!zoom.exe"]));
        assert!(exe_name_matches_targets("notepad.exe", &["*", "!zoom.exe"]));
        assert!(!exe_name_matches_targets("zoom.exe", &["!zoom.exe"]));
        assert!(exe_name_matches_targets("notepad.exe", &["!zoom.exe"]));
    }

    #[test]
    fn test_target_predicates() {
        assert!(has_wildcard_target(&["*"]));
        assert!(has_wildcard_target(&["notepad.exe", " * "]));
        assert!(!has_wildcard_target(&["notepad.exe"]));
        assert!(!has_wildcard_target::<&str>(&[]));

        assert!(has_specific_targets(&["notepad.exe"]));
        assert!(has_specific_targets(&["*", "chrome.exe"]));
        assert!(!has_specific_targets(&["*"]));
        assert!(!has_specific_targets(&["", "   "]));
        assert!(!has_specific_targets::<&str>(&[]));
        assert!(!has_specific_targets(&["*", "!zoom.exe"]));
        assert!(!has_specific_targets(&["!zoom.exe"]));
        assert!(has_specific_targets(&["code.exe", "!zoom.exe"]));

        assert!(has_exclusion_targets(&["!zoom.exe"]));
        assert!(has_exclusion_targets(&["*", "!zoom.exe"]));
        assert!(!has_exclusion_targets(&["*"]));
        assert!(!has_exclusion_targets(&["notepad.exe"]));
        assert!(!has_exclusion_targets(&["!"]));
        assert!(!has_exclusion_targets::<&str>(&[]));
    }

    #[test]
    fn test_is_protected_window_class() {
        let to_u16 = |s: &str| s.encode_utf16().collect::<Vec<u16>>();

        assert!(is_protected_window_class(&to_u16("Progman")));
        assert!(is_protected_window_class(&to_u16("WorkerW")));
        assert!(is_protected_window_class(&to_u16("Shell_TrayWnd")));
        assert!(is_protected_window_class(&to_u16("Shell_SecondaryTrayWnd")));
        assert!(is_protected_window_class(&to_u16("NotifyIconOverflowWindow")));
        assert!(is_protected_window_class(&to_u16("TopLevelWindowForOverflowXamlIsland")));
        assert!(is_protected_window_class(&to_u16("MultitaskingViewFrame")));
        assert!(is_protected_window_class(&to_u16("Windows.UI.Core.CoreWindow")));
        assert!(is_protected_window_class(&to_u16("XamlExplorerHostIslandWindow")));
        assert!(is_protected_window_class(&to_u16("ForegroundStaging")));

        assert!(!is_protected_window_class(&to_u16("Notepad")));
        assert!(!is_protected_window_class(&to_u16("Chrome_WidgetWin_1")));
        assert!(!is_protected_window_class(&to_u16("ProgmanExtra")));
        assert!(!is_protected_window_class(&[]));
    }

    #[test]
    fn test_find_matching_process_safety_guards() {
        assert_eq!(find_matching_process::<&str>(&[]), None);
        assert_eq!(find_matching_process(&["*"]), None);
        assert_eq!(find_matching_process(&["", "   "]), None);
        assert_eq!(find_matching_process(&["*", ""]), None);

        // Nonexistent process returns None
        assert_eq!(find_matching_process(&["nonexistent_process_99999.exe"]), None);

        // Standard Windows system process should match if running
        let explorer = find_matching_process(&["explorer.exe"]);
        if let Some(name) = explorer {
            assert!(name.eq_ignore_ascii_case("explorer.exe") || name.eq_ignore_ascii_case("explorer"));
        }
    }

    #[test]
    fn test_register_app_user_model_id() {
        let res = register_app_user_model_id();
        assert!(res.is_ok(), "register_app_user_model_id should succeed: {:?}", res);

        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let key = hkcu.open_subkey(AUMID_REG_PATH).expect("AUMID registry key should exist");
        let display_name: String = key.get_value("DisplayName").expect("DisplayName should exist");
        assert_eq!(display_name, APP_DISPLAY_NAME);

        // Ensure repeated calls succeed idempotently
        let res2 = register_app_user_model_id();
        assert!(res2.is_ok(), "Repeated register_app_user_model_id calls should succeed");
    }

    #[test]
    fn test_send_system_notification_mocked_in_tests() {
        let before_count = get_test_notifications_sent();
        send_system_notification("Test Alert", "Testing notification suppression");
        let after_count = get_test_notifications_sent();
        assert_eq!(after_count, before_count + 1);

        let last = get_last_test_notification();
        assert_eq!(
            last,
            Some(("Test Alert".to_string(), "Testing notification suppression".to_string()))
        );
    }
}
