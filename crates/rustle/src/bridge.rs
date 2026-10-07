//! graphmail-bridge, built in: a Microsoft 365 mailbox served as local IMAP
//! and SMTP (and CalDAV, for GNOME Calendar) from inside Rustle, for an
//! organisation whose tenant blocks other mail apps -- or GNOME Online
//! Accounts' own. The account itself is an ordinary loopback IMAP account
//! in Evolution Data Server, so the rest of Rustle sees nothing special.
//!
//! The bridge keeps its own configuration and keyring entries, shared with
//! the standalone `graphmail-bridge` program. When that runs as a service
//! it is already serving the port, and the built-in one stays out of its
//! way; otherwise it runs on a thread of its own here, for as long as
//! Rustle does.

use anyhow::{Context, Result};
use graphmail_bridge::config::{AppPaths, Config};
use graphmail_bridge::service::{self, Runtime};
use std::net::{SocketAddr, TcpStream};
use std::sync::{LazyLock, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

struct Running {
    stop: tokio::sync::oneshot::Sender<()>,
    thread: JoinHandle<()>,
}

static RUNNING: LazyLock<Mutex<Option<Running>>> = LazyLock::new(|| Mutex::new(None));

/// The bridge's configuration, when it has accounts to serve.
fn configured() -> Option<(AppPaths, Config)> {
    let paths = AppPaths::discover().ok()?;
    if !paths.config_file.exists() {
        return None;
    }
    let config = Config::load(&paths)
        .map_err(|error| log::warn!("could not read the graphmail-bridge configuration: {error:#}"))
        .ok()?;
    (!config.accounts.is_empty()).then_some((paths, config))
}

/// Whether something answers on the bridge's IMAP port.
fn is_served(config: &Config) -> bool {
    let address = SocketAddr::new(config.server.bind, config.server.imap_port);
    TcpStream::connect_timeout(&address, Duration::from_millis(300)).is_ok()
}

/// Start the built-in bridge, unless there's nothing to serve, it already
/// runs, or a standalone bridge serves the port.
pub fn start_if_needed() {
    let mut running = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
    if running.is_some() {
        return;
    }
    let Some((paths, config)) = configured() else {
        return;
    };
    if is_served(&config) {
        log::info!(
            "graphmail-bridge is already serving port {}; the built-in bridge stays off",
            config.server.imap_port
        );
        return;
    }
    match spawn(paths, config) {
        Ok(started) => *running = Some(started),
        Err(error) => log::error!("could not start the built-in graphmail-bridge: {error:#}"),
    }
}

/// Stop and start again, to pick up a newly added account. A standalone
/// bridge has to be restarted by whoever runs it.
pub fn restart() {
    stop();
    start_if_needed();
}

/// Stop the built-in bridge, waiting for its sockets to close.
pub fn stop() {
    let running = RUNNING.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(running) = running {
        let _ = running.stop.send(());
        let _ = running.thread.join();
    }
}

fn spawn(paths: AppPaths, config: Config) -> Result<Running> {
    let runtime = Runtime::load(config, &paths)?;
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let thread = std::thread::Builder::new()
        .name("graphmail-bridge".into())
        .spawn(move || {
            let tokio = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("graphmail-bridge-worker")
                .enable_all()
                .build()
            {
                Ok(tokio) => tokio,
                Err(error) => {
                    log::error!("could not start the bridge's async runtime: {error}");
                    return;
                }
            };
            tokio.block_on(async move {
                tokio::select! {
                    served = service::serve(runtime) => {
                        if let Err(error) = served {
                            log::error!("the built-in graphmail-bridge stopped: {error:#}");
                        }
                    }
                    _ = stopped => log::debug!("stopping the built-in graphmail-bridge"),
                }
            });
            // Dropping the runtime ends the bridge's sync tasks with it.
        })
        .context("could not start the bridge thread")?;
    log::info!("started the built-in graphmail-bridge");
    Ok(Running { stop, thread })
}

/// One Microsoft 365 account known to GNOME Online Accounts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoaAccount {
    pub id: String,
    pub email: String,
}

/// The Microsoft 365 accounts in GNOME Online Accounts: their sign-in can
/// stand in for the bridge's own where the tenant allows GOA's app.
pub fn goa_microsoft_accounts() -> Vec<GoaAccount> {
    use gtk::gio;
    use gtk::glib;
    let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else {
        return Vec::new();
    };
    let reply = bus.call_sync(
        Some("org.gnome.OnlineAccounts"),
        "/org/gnome/OnlineAccounts",
        "org.freedesktop.DBus.ObjectManager",
        "GetManagedObjects",
        None,
        None,
        gio::DBusCallFlags::NONE,
        2000,
        gio::Cancellable::NONE,
    );
    let Ok(reply) = reply else {
        return Vec::new();
    };
    let objects = reply.child_value(0);
    let mut accounts = Vec::new();
    for index in 0..objects.n_children() {
        let interfaces = objects.child_value(index).child_value(1);
        for slot in 0..interfaces.n_children() {
            let entry = interfaces.child_value(slot);
            if entry.child_value(0).str() != Some("org.gnome.OnlineAccounts.Account") {
                continue;
            }
            let properties = glib::VariantDict::new(Some(&entry.child_value(1)));
            let text = |name: &str| properties.lookup::<String>(name).ok().flatten();
            if text("ProviderType").as_deref() != Some("ms365") {
                continue;
            }
            if let (Some(id), Some(email)) = (text("Id"), text("Identity")) {
                accounts.push(GoaAccount { id, email });
            }
        }
    }
    accounts
}

/// A short account name for the bridge's configuration (no whitespace,
/// not already taken): the address's domain, e.g. "liftwerx".
pub fn account_name_for(email: &str) -> String {
    let taken: Vec<String> = configured()
        .map(|(_, config)| {
            config
                .accounts
                .iter()
                .filter(|account| !account.email.eq_ignore_ascii_case(email))
                .map(|account| account.name.to_ascii_lowercase())
                .collect()
        })
        .unwrap_or_default();
    name_for(email, &taken)
}

/// The account name for `email` that isn't among `taken`.
fn name_for(email: &str, taken: &[String]) -> String {
    let base: String = email
        .rsplit('@')
        .next()
        .and_then(|domain| domain.split('.').next())
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect::<String>()
        .to_ascii_lowercase();
    let base = if base.is_empty() {
        "work".to_string()
    } else {
        base
    };
    let mut name = base.clone();
    let mut n = 2;
    while taken.contains(&name) {
        name = format!("{base}{n}");
        n += 1;
    }
    name
}

#[cfg(test)]
mod tests {
    use super::name_for;

    #[test]
    fn names_come_from_the_domain_and_stay_unique() {
        assert_eq!(name_for("me@LiftWerx.com", &[]), "liftwerx");
        assert_eq!(
            name_for("me@liftwerx.com", &["liftwerx".into(), "liftwerx2".into()]),
            "liftwerx3"
        );
        assert_eq!(name_for("odd@", &[]), "work");
    }
}
