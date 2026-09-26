//! Where an account's sign-in comes from: the system keyring for an account
//! typed in by hand, GNOME Online Accounts for one imported from Settings.
//!
//! Called from worker threads: both branches block on IPC, and the Online
//! Accounts one can spend a network round trip refreshing an expired token.
//!
//! The keyring is reached over one connection and one encrypted session, shared
//! by the whole process and taken one caller at a time, and a password read once
//! is kept in memory. Every worker opening its own session at launch was enough
//! to abort gnome-keyring-daemon (it loses track of short-lived clients).

use crate::goa;
use crate::models::Account;
use crate::net::auth::Credential;
use log::warn;
use secret_service::blocking::SecretService;
use secret_service::EncryptionType;
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// The schema name the Python app stored under, so existing passwords carry over.
const SCHEMA: &str = "io.github.turbinebmw.Rustle.Account";

pub type Result<T> = std::result::Result<T, secret_service::Error>;

static SERVICE: Mutex<Option<SecretService<'static>>> = Mutex::new(None);

/// Passwords already read from the keyring, by account id.
static PASSWORDS: Mutex<Option<HashMap<i64, String>>> = Mutex::new(None);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Run `f` against the shared connection, opening it on first use. A D-Bus
/// failure means the daemon went away and took our session with it, so that
/// gets one fresh connection and a second try.
fn with_service<T>(f: impl Fn(&SecretService<'static>) -> Result<T>) -> Result<T> {
    let mut service = lock(&SERVICE);
    if let Some(existing) = service.as_ref() {
        match f(existing) {
            Err(secret_service::Error::Zbus(_) | secret_service::Error::ZbusFdo(_)) => {
                *service = None;
            }
            result => return result,
        }
    }
    let fresh = service.insert(SecretService::connect(EncryptionType::Dh)?);
    f(fresh)
}

/// Drop the remembered password, so the next sign-in reads the keyring again.
/// For when the server rejects it: it may have been changed outside the app.
pub fn forget_password(account_id: i64) {
    if let Some(passwords) = lock(&PASSWORDS).as_mut() {
        passwords.remove(&account_id);
    }
}

fn attributes(account_id: i64) -> (String, HashMap<&'static str, String>) {
    let id = account_id.to_string();
    (
        id.clone(),
        HashMap::from([("xdg:schema", SCHEMA.to_string()), ("account-id", id)]),
    )
}

fn borrow<'a>(attributes: &'a HashMap<&'static str, String>) -> HashMap<&'static str, &'a str> {
    attributes.iter().map(|(k, v)| (*k, v.as_str())).collect()
}

pub fn store_password(account_id: i64, password: &str) -> Result<()> {
    forget_password(account_id);
    let (_, attributes) = attributes(account_id);
    with_service(|service| {
        let collection = service.get_default_collection()?;
        collection.ensure_unlocked()?;
        collection.create_item(
            &format!("Rustle account {account_id}"),
            borrow(&attributes),
            password.as_bytes(),
            true,
            "text/plain",
        )?;
        Ok(())
    })
}

pub fn lookup_password(account_id: i64) -> Result<Option<String>> {
    if let Some(password) = lock(&PASSWORDS).as_ref().and_then(|p| p.get(&account_id)) {
        return Ok(Some(password.clone()));
    }
    let (_, attributes) = attributes(account_id);
    let password = with_service(|service| {
        let results = service.search_items(borrow(&attributes))?;
        let Some(item) = results.unlocked.first().or(results.locked.first()) else {
            return Ok(None);
        };
        item.ensure_unlocked()?;
        let secret = item.get_secret()?;
        Ok(Some(String::from_utf8_lossy(&secret).into_owned()))
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
    let (_, attributes) = attributes(account_id);
    with_service(|service| {
        let results = service.search_items(borrow(&attributes))?;
        for item in results.unlocked.iter().chain(results.locked.iter()) {
            item.delete()?;
        }
        Ok(())
    })
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
