use std::env;
use winreg::enums::HKEY_CURRENT_USER;
use winreg::RegKey;

const APP_NAME: &str = "AW-Enforcer";
const RUN_KEY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// Formats the command line string to launch the executable at `exe_path` minimized to tray on login.
pub fn format_autostart_command(exe_path: &str) -> String {
    format!("\"{}\" --autostart", exe_path)
}

/// Resolves the command string for the current executable.
fn current_exe_autostart_command() -> Result<String, Box<dyn std::error::Error>> {
    let exe_path = env::current_exe()?;
    let path_str = exe_path.to_str().ok_or("Invalid executable path")?;
    Ok(format_autostart_command(path_str))
}

/// Checks whether the application is registered to autostart in Windows `HKCU\...\Run`
/// matching the current executable command.
pub fn is_autostart_enabled() -> bool {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let Ok(key) = hkcu.open_subkey(RUN_KEY_PATH) else {
        return false;
    };
    let Ok(expected_command) = current_exe_autostart_command() else {
        return false;
    };
    match key.get_value::<String, _>(APP_NAME) {
        Ok(cmd) => cmd == expected_command,
        Err(_) => false,
    }
}

/// Registers or removes the application in the Windows `HKCU\...\Run` registry key
/// so it automatically launches in the background (`--autostart`) on user login.
///
/// When `enable` is `true`, skips rewriting the registry value if it already matches
/// the current executable command.
pub fn set_autostart(enable: bool) -> Result<(), Box<dyn std::error::Error>> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey(RUN_KEY_PATH)?;

    if enable {
        let command = current_exe_autostart_command()?;
        if let Ok(existing) = key.get_value::<String, _>(APP_NAME) {
            if existing == command {
                tracing::debug!("Autostart registry entry already up to date: {}", command);
                return Ok(());
            }
        }
        key.set_value(APP_NAME, &command)?;
        tracing::info!("Registered autostart in registry: {}", command);
    } else {
        match key.delete_value(APP_NAME) {
            Ok(_) => tracing::info!("Removed autostart from registry"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Box::new(e)),
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_autostart_command() {
        let path = r"C:\Program Files\aw-enforcer\aw-enforcer.exe";
        assert_eq!(
            format_autostart_command(path),
            r#""C:\Program Files\aw-enforcer\aw-enforcer.exe" --autostart"#
        );

        let simple = "aw-enforcer.exe";
        assert_eq!(
            format_autostart_command(simple),
            r#""aw-enforcer.exe" --autostart"#
        );
    }

    #[test]
    fn test_set_autostart_and_is_enabled_idempotency() {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let key = hkcu.open_subkey(RUN_KEY_PATH).ok();
        let original_val: Option<String> = key.as_ref().and_then(|k| k.get_value(APP_NAME).ok());

        // 1. Enable autostart
        let res = set_autostart(true);
        assert!(res.is_ok(), "set_autostart(true) failed: {:?}", res);
        assert!(
            is_autostart_enabled(),
            "is_autostart_enabled() should return true after enable"
        );

        // 2. Call set_autostart(true) again - should be a no-op (idempotent)
        let res_repeat = set_autostart(true);
        assert!(
            res_repeat.is_ok(),
            "repeat set_autostart(true) should succeed without error"
        );
        assert!(is_autostart_enabled());

        // 3. Disable autostart
        let res_disable = set_autostart(false);
        assert!(
            res_disable.is_ok(),
            "set_autostart(false) failed: {:?}",
            res_disable
        );
        assert!(
            !is_autostart_enabled(),
            "is_autostart_enabled() should return false after disable"
        );

        // 4. Repeat disable should also succeed gracefully
        let res_disable_repeat = set_autostart(false);
        assert!(res_disable_repeat.is_ok());

        // Restore original state
        if let Some(orig) = original_val {
            let _ = hkcu
                .create_subkey(RUN_KEY_PATH)
                .map(|(k, _)| k.set_value(APP_NAME, &orig));
        }
    }
}
