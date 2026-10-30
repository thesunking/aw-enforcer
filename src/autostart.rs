use std::env;
use winreg::enums::HKEY_CURRENT_USER;
use winreg::RegKey;

const APP_NAME: &str = "AW-Enforcer";
const RUN_KEY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// Registers or removes the application in the Windows `HKCU\...\Run` registry key
/// so it automatically launches in the background (`--autostart`) on user login.
pub fn set_autostart(enable: bool) -> Result<(), Box<dyn std::error::Error>> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey(RUN_KEY_PATH)?;

    if enable {
        let exe_path = env::current_exe()?;
        let path_str = exe_path.to_str().ok_or("Invalid executable path")?;
        // Pass --autostart so the daemon launches minimized in the system tray on login
        let command = format!("\"{}\" --autostart", path_str);
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