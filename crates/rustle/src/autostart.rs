//! "Start at Login" where the desktop has no Background portal (Hyprland,
//! Sway and other compositors whose portal backends don't implement it).
//! With a systemd user session that reaches graphical-session.target, the
//! user unit install.sh puts in place is enabled, so a crash restarts Rustle
//! (and the built-in bridge with it). Otherwise an XDG autostart entry is
//! written, which every desktop that runs autostart entries starts once.

use crate::config::APP_ID;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use std::path::{Path, PathBuf};

const SYSTEMD_NAME: &str = "org.freedesktop.systemd1";
const SYSTEMD_PATH: &str = "/org/freedesktop/systemd1";
const MANAGER_INTERFACE: &str = "org.freedesktop.systemd1.Manager";
const UNIT_INTERFACE: &str = "org.freedesktop.systemd1.Unit";
const SESSION_TARGET: &str = "graphical-session.target";

fn unit_name() -> String {
    format!("{APP_ID}.service")
}

/// How "Start at Login" was set, for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Systemd,
    AutostartEntry,
}

/// Turn starting at login on or off without the portal.
pub async fn set(bus: &gio::DBusConnection, is_wanted: bool) -> Result<Method, String> {
    if is_systemd_usable(bus).await {
        set_unit_enabled(bus, is_wanted)
            .await
            .map_err(|error| format!("systemd: {error}"))?;
        return Ok(Method::Systemd);
    }
    set_autostart_entry(is_wanted).map_err(|error| format!("autostart entry: {error}"))?;
    Ok(Method::AutostartEntry)
}

/// The user unit is installed and the session reaches the target it hangs
/// off; without either, enabling it would never start anything.
async fn is_systemd_usable(bus: &gio::DBusConnection) -> bool {
    let installed = call(
        bus,
        SYSTEMD_PATH,
        MANAGER_INTERFACE,
        "GetUnitFileState",
        (unit_name(),).to_variant(),
    )
    .await;
    if installed.is_err() {
        return false;
    }
    let Ok(target) = call(
        bus,
        SYSTEMD_PATH,
        MANAGER_INTERFACE,
        "GetUnit",
        (SESSION_TARGET,).to_variant(),
    )
    .await
    else {
        return false;
    };
    let Some(path) = target.child_value(0).str().map(str::to_string) else {
        return false;
    };
    let Ok(state) = call(
        bus,
        &path,
        "org.freedesktop.DBus.Properties",
        "Get",
        (UNIT_INTERFACE, "ActiveState").to_variant(),
    )
    .await
    else {
        return false;
    };
    // Get answers (v): the variant holds the string.
    state
        .child_value(0)
        .as_variant()
        .and_then(|value| value.str().map(|state| state == "active"))
        .unwrap_or(false)
}

async fn set_unit_enabled(bus: &gio::DBusConnection, is_wanted: bool) -> Result<(), glib::Error> {
    let units = vec![unit_name()];
    if is_wanted {
        // (files, runtime, force): persistent, and not over someone else's link.
        call(
            bus,
            SYSTEMD_PATH,
            MANAGER_INTERFACE,
            "EnableUnitFiles",
            (units, false, false).to_variant(),
        )
        .await?;
    } else {
        call(
            bus,
            SYSTEMD_PATH,
            MANAGER_INTERFACE,
            "DisableUnitFiles",
            (units, false).to_variant(),
        )
        .await?;
    }
    // What `systemctl enable` does after linking, so the target sees it.
    call(
        bus,
        SYSTEMD_PATH,
        MANAGER_INTERFACE,
        "Reload",
        ().to_variant(),
    )
    .await?;
    Ok(())
}

async fn call(
    bus: &gio::DBusConnection,
    path: &str,
    interface: &str,
    method: &str,
    parameters: glib::Variant,
) -> Result<glib::Variant, glib::Error> {
    bus.call_future(
        Some(SYSTEMD_NAME),
        path,
        interface,
        method,
        Some(&parameters),
        None,
        gio::DBusCallFlags::NONE,
        -1,
    )
    .await
}

/// Where the desktop looks for autostart entries; uninstall.sh removes it
/// by the same name.
pub fn autostart_entry_path() -> PathBuf {
    glib::user_config_dir()
        .join("autostart")
        .join(format!("{APP_ID}.desktop"))
}

fn set_autostart_entry(is_wanted: bool) -> std::io::Result<()> {
    let path = autostart_entry_path();
    if !is_wanted {
        return match std::fs::remove_file(&path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        };
    }
    let exe = std::env::current_exe()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, autostart_entry(&exe))
}

/// The entry for `exe`, quoted as the Desktop Entry spec asks when the path
/// has a character the Exec line would otherwise split or expand on.
fn autostart_entry(exe: &Path) -> String {
    let exe = exe.to_string_lossy();
    let is_plain = exe
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-+".contains(c));
    let exec = if is_plain {
        exe.into_owned()
    } else {
        let mut quoted = String::from('"');
        for c in exe.chars() {
            if matches!(c, '"' | '`' | '$' | '\\') {
                // Escaped once for the Exec quoting, then once more as a
                // desktop-file string value.
                quoted.push_str("\\\\");
            }
            quoted.push(c);
        }
        quoted.push('"');
        quoted
    };
    format!(
        "[Desktop Entry]\nType=Application\nName=Rustle\nIcon={APP_ID}\n\
         Exec={exec} --hidden\nNoDisplay=true\nX-GNOME-Autostart-enabled=true\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autostart_entry_quotes_only_what_needs_it() {
        let plain = autostart_entry(Path::new("/home/ada/.local/bin/rustle"));
        assert!(plain.contains("\nExec=/home/ada/.local/bin/rustle --hidden\n"));
        let spaced = autostart_entry(Path::new("/opt/my apps/rustle"));
        assert!(spaced.contains("\nExec=\"/opt/my apps/rustle\" --hidden\n"));
        let dollar = autostart_entry(Path::new("/opt/$x/rustle"));
        assert!(dollar.contains("\nExec=\"/opt/\\\\$x/rustle\" --hidden\n"));
    }
}
