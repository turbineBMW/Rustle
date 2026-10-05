//! How an account signs in. Accounts live in Evolution Data Server: an
//! OAuth one gets a fresh token from the registry for every connection, a
//! password one reads the keyring entry EDS keeps for the account's source.
//! IMAP and SMTP sign in separately, since EDS keeps them apart.
//!
//! Called from worker threads: everything here blocks on IPC, and a token
//! can cost a network round trip to refresh.
//!
//! The keyring is the desktop's Secret Service, reached over one connection
//! and one encrypted session shared by the whole process and taken one caller
//! at a time, and a password read once is kept in memory. Every worker
//! opening its own session at launch was enough to abort gnome-keyring-daemon
//! (it loses track of short-lived clients). EDS's entries are only there, so
//! this never uses a sandbox's private keyring file.
//!
//! A sandbox can't reach the keyring. On omarchy-mobile the phone's bridge
//! keeps EDS's entries for it (`dev.omarchy.Accounts`, one source at a time),
//! and when that name is on the bus the EDS passwords go through it instead.
//! The pre-EDS passwords were never there, so a sandbox has none to move.

// oo7's error is large: the calls in here keep it, and the public API boxes
// it (`Result`).
#![allow(clippy::result_large_err)]

use crate::eds;
use crate::models::{Account, Auth};
use crate::net::auth::Credential;
use futures_lite::future::block_on;
use log::warn;
use oo7::dbus::{Collection, Service};
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// Where Rustle kept passwords before accounts moved to EDS; read once to
/// move them.
const LEGACY_SCHEMA: &str = "io.github.turbinebmw.Rustle.Account";
/// What EDS files its source passwords under.
const EDS_SCHEMA: &str = "org.gnome.Evolution.Data.Source";

pub type Result<T> = std::result::Result<T, Box<oo7::dbus::Error>>;

static KEYRING: Mutex<Option<Collection>> = Mutex::new(None);

/// Passwords already read from the keyring, by EDS source UID.
static PASSWORDS: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Run `f` against the default keyring, opening it on first use. A failure
/// may mean the daemon went away and took our session with it, so that gets
/// one fresh connection and a second try.
fn with_keyring<T>(
    f: impl Fn(&Collection) -> std::result::Result<T, oo7::dbus::Error>,
) -> Result<T> {
    let mut keyring = lock(&KEYRING);
    if let Some(existing) = keyring.as_ref() {
        match f(existing) {
            Err(oo7::dbus::Error::ZBus(_) | oo7::dbus::Error::Deleted) => *keyring = None,
            result => return result.map_err(Box::new),
        }
    }
    let collection =
        block_on(async { Service::new().await?.default_collection().await }).map_err(Box::new)?;
    let fresh = keyring.insert(collection);
    f(fresh).map_err(Box::new)
}

fn find(attributes: &HashMap<&str, &str>) -> Result<Option<String>> {
    with_keyring(|keyring| {
        block_on(async {
            if keyring.is_locked().await? {
                keyring.unlock(None).await?;
            }
            let items = keyring.search_items(attributes).await?;
            let Some(item) = items.first() else {
                return Ok(None);
            };
            if item.is_locked().await? {
                item.unlock(None).await?;
            }
            let secret = item.secret().await?;
            Ok(Some(String::from_utf8_lossy(&secret).into_owned()))
        })
    })
}

/// The password EDS keeps for a source.
fn source_password(uid: &str) -> Result<Option<String>> {
    if let Some(password) = lock(&PASSWORDS).as_ref().and_then(|p| p.get(uid)) {
        return Ok(Some(password.clone()));
    }
    let password = if keeper::available() {
        keeper::lookup(uid)?
    } else {
        find(&HashMap::from([("e-source-uid", uid)]))?
    };
    if let Some(password) = &password {
        lock(&PASSWORDS)
            .get_or_insert_with(HashMap::new)
            .insert(uid.to_string(), password.clone());
    }
    Ok(password)
}

/// Store a source's password where EDS reads it.
pub fn store_source_password(uid: &str, label: &str, password: &str) -> Result<()> {
    lock(&PASSWORDS)
        .get_or_insert_with(HashMap::new)
        .remove(uid);
    if keeper::available() {
        return keeper::store(uid, label, password);
    }
    let attributes = HashMap::from([
        ("xdg:schema", EDS_SCHEMA),
        ("e-source-uid", uid),
        ("eds-origin", "evolution-data-server"),
    ]);
    let label = format!("Evolution Data Source “{label}”");
    with_keyring(|keyring| {
        block_on(async {
            if keyring.is_locked().await? {
                keyring.unlock(None).await?;
            }
            keyring
                .create_item(&label, &attributes, password, true, None)
                .await
                .map(|_| ())
        })
    })
}

pub fn clear_source_password(uid: &str) -> Result<()> {
    lock(&PASSWORDS)
        .get_or_insert_with(HashMap::new)
        .remove(uid);
    if keeper::available() {
        return keeper::clear(uid);
    }
    with_keyring(|keyring| {
        block_on(async {
            for item in keyring
                .search_items(&HashMap::from([("e-source-uid", uid)]))
                .await?
            {
                item.delete(None).await?;
            }
            Ok(())
        })
    })
}

/// The password Rustle stored for an account before EDS, if any.
pub fn legacy_password(account_id: i64) -> Result<Option<String>> {
    if keeper::available() {
        return Ok(None);
    }
    let id = account_id.to_string();
    find(&HashMap::from([
        ("xdg:schema", LEGACY_SCHEMA),
        ("account-id", id.as_str()),
    ]))
}

pub fn clear_legacy_password(account_id: i64) -> Result<()> {
    if keeper::available() {
        return Ok(());
    }
    let id = account_id.to_string();
    with_keyring(|keyring| {
        block_on(async {
            for item in keyring
                .search_items(&HashMap::from([
                    ("xdg:schema", LEGACY_SCHEMA),
                    ("account-id", id.as_str()),
                ]))
                .await?
            {
                item.delete(None).await?;
            }
            Ok(())
        })
    })
}

/// Drop the remembered passwords, so the next sign-in reads the keyring
/// again. For when the server rejects one: it may have been changed in EDS.
pub fn forget_password(account: &Account) {
    if let Some(passwords) = lock(&PASSWORDS).as_mut() {
        for uid in [
            &account.eds_uid,
            &account.eds_smtp_uid,
            &account.eds_root_uid,
        ] {
            passwords.remove(uid.as_str());
        }
    }
}

/// How to sign this account in to IMAP, or None when we cannot.
pub fn credential_for(account: &Account) -> Option<Credential> {
    if account.eds_uid.is_empty() {
        return unlinked_credential(account, false);
    }
    credential(
        account,
        &account.eds_uid,
        &account.imap_user,
        account.imap_auth,
    )
}

/// How to sign this account in to SMTP, or None when we cannot.
pub fn smtp_credential_for(account: &Account) -> Option<Credential> {
    if account.eds_smtp_uid.is_empty() {
        return unlinked_credential(account, true);
    }
    credential(
        account,
        &account.eds_smtp_uid,
        &account.smtp_user,
        account.smtp_auth,
    )
}

/// An account not linked to EDS yet: on the first start after the move a
/// sync can run before the accounts are re-read. An Online Accounts one is
/// found in EDS by its id; one typed in here still has its old password.
fn unlinked_credential(account: &Account, is_smtp: bool) -> Option<Credential> {
    if account.goa_id.is_empty() {
        return match legacy_password(account.id) {
            Ok(Some(password)) => Some(Credential::password(account.email.clone(), password)),
            Ok(None) => None,
            Err(error) => {
                warn!(
                    "could not read the keyring for account {}: {error}",
                    account.email
                );
                None
            }
        };
    }
    let found = eds::mail_accounts()
        .ok()?
        .into_iter()
        .find(|found| found.goa_id == account.goa_id)?;
    let server = if is_smtp { &found.smtp } else { &found.imap };
    credential(account, &server.uid, &server.user, server.auth)
}

fn credential(account: &Account, uid: &str, user: &str, auth: Auth) -> Option<Credential> {
    let user = if user.is_empty() {
        account.email.clone()
    } else {
        user.to_string()
    };
    match auth {
        Auth::OAuth2 => match eds::access_token(uid) {
            Ok(token) if !token.is_empty() => Some(Credential::token(user, token)),
            Ok(_) => {
                warn!(
                    "Evolution Data Server gave an empty token for {}",
                    account.email
                );
                None
            }
            Err(error) => {
                warn!(
                    "could not get an access token for {} from Evolution Data Server: {error}",
                    account.email
                );
                None
            }
        },
        // EDS keeps a password with the source that asked for it, or once
        // for a whole account at the top of its tree.
        Auth::Password => {
            for candidate in [uid, account.eds_root_uid.as_str()] {
                if candidate.is_empty() {
                    continue;
                }
                match source_password(candidate) {
                    Ok(Some(password)) => return Some(Credential::password(user, password)),
                    Ok(None) => {}
                    Err(error) => {
                        warn!(
                            "could not read the keyring for account {}: {error}",
                            account.email
                        );
                        return None;
                    }
                }
            }
            warn!("no password in the keyring for account {}", account.email);
            None
        }
    }
}

/// omarchy-mobile's keeper of EDS's passwords, for sandboxed apps: the
/// phone's bridge, `dev.omarchy.Accounts`, which reads and writes only the
/// keyring entries EDS keeps per source.
mod keeper {
    use super::Result;
    use crate::net::NET_TIMEOUT;
    use gio::prelude::*;
    use glib::Variant;
    use std::sync::OnceLock;

    const NAME: &str = "dev.omarchy.Accounts";
    const PATH: &str = "/dev/omarchy/Accounts";

    fn failed(error: glib::Error) -> Box<oo7::dbus::Error> {
        Box::new(oo7::dbus::Error::IO(std::io::Error::other(
            error.to_string(),
        )))
    }

    /// Whether the keeper is on the bus; asked once.
    pub fn available() -> bool {
        static AVAILABLE: OnceLock<bool> = OnceLock::new();
        *AVAILABLE.get_or_init(|| {
            let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else {
                return false;
            };
            bus.call_sync(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "NameHasOwner",
                Some(&(NAME,).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                -1,
                gio::Cancellable::NONE,
            )
            .ok()
            .and_then(|reply| reply.child_value(0).get::<bool>())
            .unwrap_or(false)
        })
    }

    fn call(method: &str, parameters: Variant) -> Result<Variant> {
        let bus =
            gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).map_err(failed)?;
        bus.call_sync(
            Some(NAME),
            PATH,
            NAME,
            method,
            Some(&parameters),
            None,
            gio::DBusCallFlags::NONE,
            NET_TIMEOUT.as_millis() as i32,
            gio::Cancellable::NONE,
        )
        .map_err(failed)
    }

    pub fn lookup(uid: &str) -> Result<Option<String>> {
        let reply = call("LookupPassword", (uid,).to_variant())?;
        let password = reply.child_value(0).str().unwrap_or("").to_string();
        Ok(Some(password).filter(|password| !password.is_empty()))
    }

    pub fn store(uid: &str, label: &str, password: &str) -> Result<()> {
        call("StorePassword", (uid, label, password).to_variant()).map(|_| ())
    }

    pub fn clear(uid: &str) -> Result<()> {
        call("ClearPassword", (uid,).to_variant()).map(|_| ())
    }
}
