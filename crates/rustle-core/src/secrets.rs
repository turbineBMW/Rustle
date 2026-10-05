//! Where an account's sign-in comes from: the system keyring for an account
//! typed in by hand, GNOME Online Accounts for one imported from Settings.
//!
//! Called from worker threads: both branches block on IPC, and the Online
//! Accounts one can spend a network round trip refreshing an expired token.
//!
//! The keyring is reached over one connection and one encrypted session, shared
//! by the whole process and taken one caller at a time, and a password read once
//! is kept in memory. Every worker opening its own session at launch was enough
//! to abort gnome-keyring-daemon (it loses track of short-lived clients). `oo7`
//! uses the Secret Service on a desktop and, inside a sandbox, a keyring file in
//! the app's data keyed through the Secret portal.

// oo7's error is large: the calls in here keep it, and the public API boxes
// it (`Result`).
#![allow(clippy::result_large_err)]

use crate::goa;
use crate::models::Account;
use crate::net::auth::Credential;
use futures_lite::future::block_on;
use log::warn;
use oo7::Keyring;
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// The schema name the Python app stored under, so existing passwords carry over.
const SCHEMA: &str = "io.github.turbinebmw.Rustle.Account";

pub type Result<T> = std::result::Result<T, Box<oo7::Error>>;

/// The keyring: the Secret Service on a desktop; inside a sandbox (Flatpak,
/// omarchy-mobile) a file in the app's own data, keyed through the Secret
/// portal. `oo7` picks.
static KEYRING: Mutex<Option<Keyring>> = Mutex::new(None);

/// Passwords already read from the keyring, by account id.
static PASSWORDS: Mutex<Option<HashMap<i64, String>>> = Mutex::new(None);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Run `f` against the shared keyring, opening it on first use. A D-Bus
/// failure means the daemon went away and took our session with it, so that
/// gets one fresh connection and a second try.
fn with_keyring<T>(f: impl Fn(&Keyring) -> oo7::Result<T>) -> Result<T> {
    let mut keyring = lock(&KEYRING);
    if let Some(existing) = keyring.as_ref() {
        match f(existing) {
            Err(oo7::Error::DBus(_)) => {
                *keyring = None;
            }
            result => return result.map_err(Box::new),
        }
    }
    let fresh = keyring.insert(block_on(Keyring::new()).map_err(Box::new)?);
    f(fresh).map_err(Box::new)
}

/// Drop the remembered password, so the next sign-in reads the keyring again.
/// For when the server rejects it: it may have been changed outside the app.
pub fn forget_password(account_id: i64) {
    if let Some(passwords) = lock(&PASSWORDS).as_mut() {
        passwords.remove(&account_id);
    }
}

fn attributes(account_id: i64) -> HashMap<&'static str, String> {
    HashMap::from([
        ("xdg:schema", SCHEMA.to_string()),
        ("account-id", account_id.to_string()),
    ])
}

fn borrow<'a>(attributes: &'a HashMap<&'static str, String>) -> HashMap<&'static str, &'a str> {
    attributes.iter().map(|(k, v)| (*k, v.as_str())).collect()
}

pub fn store_password(account_id: i64, password: &str) -> Result<()> {
    forget_password(account_id);
    let attributes = attributes(account_id);
    with_keyring(|keyring| {
        block_on(async {
            keyring.unlock().await?;
            keyring
                .create_item(
                    &format!("Rustle account {account_id}"),
                    &borrow(&attributes),
                    password,
                    true,
                )
                .await
        })
    })
}

pub fn lookup_password(account_id: i64) -> Result<Option<String>> {
    if let Some(password) = lock(&PASSWORDS).as_ref().and_then(|p| p.get(&account_id)) {
        return Ok(Some(password.clone()));
    }
    let attributes = attributes(account_id);
    let password = with_keyring(|keyring| {
        block_on(async {
            let items = keyring.search_items(&borrow(&attributes)).await?;
            let Some(item) = items.first() else {
                return Ok(None);
            };
            item.unlock().await?;
            let secret = item.secret().await?;
            Ok(Some(String::from_utf8_lossy(&secret).into_owned()))
        })
    })?;
    if let Some(password) = &password {
        lock(&PASSWORDS)
            .get_or_insert_with(HashMap::new)
            .insert(account_id, password.clone());
    }
    Ok(password)
}

pub fn clear_password(account_id: i64) -> Result<()> {
    forget_password(account_id);
    let attributes = attributes(account_id);
    with_keyring(|keyring| block_on(keyring.delete(&borrow(&attributes))))
}

/// How to sign this account in, or None when we cannot.
pub fn credential_for(account: &Account) -> Option<Credential> {
    if account.is_online_account() {
        return goa::credential(&account.goa_id);
    }
    match lookup_password(account.id) {
        Ok(Some(password)) => Some(Credential::password(account.email.clone(), password)),
        Ok(None) => None,
        Err(error) => {
            warn!(
                "could not read the keyring for account {}: {error}",
                account.email
            );
            None
        }
    }
}
