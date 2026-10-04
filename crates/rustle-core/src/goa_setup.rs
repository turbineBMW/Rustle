//! What Online Accounts needs on this machine, and how to install what's
//! missing. On GNOME accounts are managed in Settings; anywhere else the
//! standalone gnome-online-accounts-gtk window stands in for it; on a phone
//! (the `phone` feature) the phone's own settings do.

use log::debug;
use std::path::Path;

const DBUS_NAME: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";
const SETTINGS_BUS_NAME: &str = "org.gnome.Settings";
const SECRETS_BUS_NAME: &str = "org.freedesktop.secrets";
pub const STANDALONE_SETTINGS: &str = "gnome-online-accounts-gtk";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Desktop {
    Gnome,
    Other,
    /// A phone shell (the `phone` feature): its settings add the accounts,
    /// and an app in its sandbox keeps its keyring through the Secret portal.
    Phone,
}

impl Desktop {
    /// From `XDG_CURRENT_DESKTOP`, a colon-separated list such as
    /// "ubuntu:GNOME".
    pub fn from_current_desktop(value: &str) -> Self {
        if value
            .split(':')
            .any(|name| name.eq_ignore_ascii_case("gnome"))
        {
            Desktop::Gnome
        } else {
            Desktop::Other
        }
    }

    pub fn detect() -> Self {
        if cfg!(feature = "phone") {
            return Desktop::Phone;
        }
        Self::from_current_desktop(&std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default())
    }
}

/// A piece Online Accounts can't work without. Every distribution below
/// packages these under the same names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Component {
    /// goa-daemon, which holds the accounts and hands out tokens.
    Daemon,
    /// GNOME Settings, whose Online Accounts panel adds them on GNOME.
    GnomeSettings,
    /// The standalone window that adds them elsewhere.
    StandaloneSettings,
    /// A Secret Service provider, where goa-daemon keeps the sign-ins.
    Keyring,
}

impl Component {
    pub fn package(self) -> &'static str {
        match self {
            Component::Daemon => "gnome-online-accounts",
            Component::GnomeSettings => "gnome-control-center",
            Component::StandaloneSettings => STANDALONE_SETTINGS,
            Component::Keyring => "gnome-keyring",
        }
    }

    pub fn required_on(desktop: Desktop) -> Vec<Component> {
        match desktop {
            Desktop::Gnome => vec![
                Component::Daemon,
                Component::GnomeSettings,
                Component::Keyring,
            ],
            Desktop::Other => vec![
                Component::Daemon,
                Component::StandaloneSettings,
                Component::Keyring,
            ],
            Desktop::Phone => vec![Component::Daemon],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackageManager {
    Pacman,
    Apt,
    Dnf,
    Zypper,
    Xbps,
    Apk,
    Emerge,
}

impl PackageManager {
    /// From /etc/os-release: `ID` first, then each `ID_LIKE` entry, so
    /// derivatives such as Omarchy (ID_LIKE=arch) resolve to their parent.
    pub fn from_os_release(contents: &str) -> Option<Self> {
        let field = |key: &str| {
            contents.lines().find_map(|line| {
                let value = line.strip_prefix(key)?.strip_prefix('=')?;
                Some(
                    value
                        .trim()
                        .trim_matches('"')
                        .trim_matches('\'')
                        .to_string(),
                )
            })
        };
        let id = field("ID").unwrap_or_default();
        let like = field("ID_LIKE").unwrap_or_default();
        std::iter::once(id.as_str())
            .chain(like.split_whitespace())
            .find_map(Self::for_distribution)
    }

    fn for_distribution(id: &str) -> Option<Self> {
        Some(match id {
            "arch" | "manjaro" | "endeavouros" | "cachyos" => PackageManager::Pacman,
            "debian" | "ubuntu" | "linuxmint" | "pop" => PackageManager::Apt,
            "fedora" | "rhel" | "centos" => PackageManager::Dnf,
            "opensuse" | "suse" | "opensuse-tumbleweed" | "opensuse-leap" => PackageManager::Zypper,
            "void" => PackageManager::Xbps,
            "alpine" => PackageManager::Apk,
            "gentoo" => PackageManager::Emerge,
            _ => return None,
        })
    }

    /// For a distribution os-release doesn't name: whichever tool is on PATH.
    fn from_path() -> Option<Self> {
        [
            ("pacman", PackageManager::Pacman),
            ("apt", PackageManager::Apt),
            ("dnf", PackageManager::Dnf),
            ("zypper", PackageManager::Zypper),
            ("xbps-install", PackageManager::Xbps),
            ("apk", PackageManager::Apk),
            ("emerge", PackageManager::Emerge),
        ]
        .into_iter()
        .find_map(|(program, manager)| is_on_path(program).then_some(manager))
    }

    pub fn detect() -> Option<Self> {
        std::fs::read_to_string("/etc/os-release")
            .ok()
            .and_then(|contents| Self::from_os_release(&contents))
            .or_else(Self::from_path)
    }

    pub fn install_command(self, packages: &[&str]) -> String {
        let prefix = match self {
            PackageManager::Pacman => "sudo pacman -S --needed",
            PackageManager::Apt => "sudo apt install",
            PackageManager::Dnf => "sudo dnf install",
            PackageManager::Zypper => "sudo zypper install",
            PackageManager::Xbps => "sudo xbps-install",
            PackageManager::Apk => "sudo apk add",
            PackageManager::Emerge => "sudo emerge --ask",
        };
        let packages: Vec<String> = packages
            .iter()
            .map(|package| match self {
                // Portage wants the category.
                PackageManager::Emerge => format!("{}/{package}", emerge_category(package)),
                _ => package.to_string(),
            })
            .collect();
        format!("{prefix} {}", packages.join(" "))
    }
}

fn emerge_category(package: &str) -> &'static str {
    match package {
        "gnome-control-center" | "gnome-keyring" => "gnome-base",
        _ => "net-libs",
    }
}

/// The result of looking: what's missing, and how to get it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Setup {
    pub desktop: Desktop,
    pub missing: Vec<Component>,
    pub package_manager: Option<PackageManager>,
}

impl Setup {
    pub fn is_ready(&self) -> bool {
        self.missing.is_empty()
    }

    pub fn packages(&self) -> Vec<&'static str> {
        self.missing.iter().map(|c| c.package()).collect()
    }

    /// The one line to paste into a terminal, when the distribution is known.
    pub fn install_command(&self) -> Option<String> {
        let manager = self.package_manager?;
        Some(manager.install_command(&self.packages()))
    }
}

/// Look over the session bus and PATH. Blocks on D-Bus, so run it off the
/// main thread.
pub fn check() -> Setup {
    let desktop = Desktop::detect();
    let names = bus_names();
    let has_name = |name: &str| names.iter().any(|n| n == name);
    let missing = Component::required_on(desktop)
        .into_iter()
        .filter(|component| match component {
            Component::Daemon => !has_name(crate::goa::BUS_NAME),
            Component::GnomeSettings => {
                !has_name(SETTINGS_BUS_NAME) && !is_on_path("gnome-control-center")
            }
            Component::StandaloneSettings => !is_on_path(STANDALONE_SETTINGS),
            Component::Keyring => !has_name(SECRETS_BUS_NAME),
        })
        .collect();
    Setup {
        desktop,
        missing,
        package_manager: PackageManager::detect(),
    }
}

/// Names that are running or can be started on demand. The bus is asked to
/// reload first, or a service installed since login stays invisible.
fn bus_names() -> Vec<String> {
    let bus = match gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) {
        Ok(bus) => bus,
        Err(error) => {
            debug!("no session bus to look for Online Accounts on: {error}");
            return Vec::new();
        }
    };
    let mut names = Vec::new();
    for method in ["ReloadConfig", "ListNames", "ListActivatableNames"] {
        match bus.call_sync(
            Some(DBUS_NAME),
            DBUS_PATH,
            DBUS_NAME,
            method,
            None,
            None,
            gio::DBusCallFlags::NONE,
            crate::net::NET_TIMEOUT.as_millis() as i32,
            gio::Cancellable::NONE,
        ) {
            Ok(reply) if reply.n_children() > 0 => names.extend(
                reply
                    .child_value(0)
                    .get::<Vec<String>>()
                    .unwrap_or_default(),
            ),
            Ok(_) => {}
            Err(error) => debug!("could not {method} on the session bus: {error}"),
        }
    }
    names
}

fn is_on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| Path::new(&dir).join(program).is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_gnome_in_a_desktop_list() {
        assert_eq!(Desktop::from_current_desktop("GNOME"), Desktop::Gnome);
        assert_eq!(
            Desktop::from_current_desktop("ubuntu:GNOME"),
            Desktop::Gnome
        );
        assert_eq!(Desktop::from_current_desktop("Hyprland"), Desktop::Other);
        assert_eq!(
            Desktop::from_current_desktop("GNOME-Flashback"),
            Desktop::Other
        );
        assert_eq!(Desktop::from_current_desktop(""), Desktop::Other);
    }

    #[test]
    fn needs_the_settings_app_of_the_desktop() {
        assert!(Component::required_on(Desktop::Gnome).contains(&Component::GnomeSettings));
        assert_eq!(
            Component::required_on(Desktop::Phone),
            vec![Component::Daemon]
        );
        assert!(Component::required_on(Desktop::Other).contains(&Component::StandaloneSettings));
    }

    #[test]
    fn reads_the_package_manager_from_os_release() {
        let omarchy = "NAME=\"Omarchy\"\nID=omarchy\nID_LIKE=arch\n";
        assert_eq!(
            PackageManager::from_os_release(omarchy),
            Some(PackageManager::Pacman)
        );
        let mint = "ID=linuxmint\nID_LIKE=\"ubuntu debian\"\n";
        assert_eq!(
            PackageManager::from_os_release(mint),
            Some(PackageManager::Apt)
        );
        let tumbleweed = "ID=\"opensuse-tumbleweed\"\nID_LIKE=\"opensuse suse\"\n";
        assert_eq!(
            PackageManager::from_os_release(tumbleweed),
            Some(PackageManager::Zypper)
        );
        // VERSION_ID must not be mistaken for ID.
        assert_eq!(
            PackageManager::from_os_release("VERSION_ID=40\nID=fedora\n"),
            Some(PackageManager::Dnf)
        );
        assert_eq!(PackageManager::from_os_release("ID=nixos\n"), None);
    }

    #[test]
    fn builds_install_commands() {
        let setup = Setup {
            desktop: Desktop::Other,
            missing: vec![Component::Daemon, Component::StandaloneSettings],
            package_manager: Some(PackageManager::Pacman),
        };
        assert!(!setup.is_ready());
        assert_eq!(
            setup.install_command().as_deref(),
            Some("sudo pacman -S --needed gnome-online-accounts gnome-online-accounts-gtk")
        );
        assert_eq!(
            PackageManager::Emerge.install_command(&["gnome-online-accounts"]),
            "sudo emerge --ask net-libs/gnome-online-accounts"
        );
        let unknown = Setup {
            package_manager: None,
            ..setup
        };
        assert_eq!(unknown.install_command(), None);
    }
}
