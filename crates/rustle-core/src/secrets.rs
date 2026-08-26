//! Where an account's sign-in comes from: the system keyring for an account
//! typed in by hand, GNOME Online Accounts for one imported from Settings.
//!
//! Called from worker threads: both branches block on IPC, and the Online
//! Accounts one can spend a network round trip refreshing an expired token.

use crate::goa;
use crate::models::Account;
use crate::net::auth::Credential;
use log::warn;
use secret_service::blocking::SecretService;
use secret_service::EncryptionType;
use std::collections::HashMap;

/// The schema name the Python app stored under, so existing passwords carry over.
const SCHEMA: &str = "io.github.turbinebmw.Rustle.Account";

pub type Result<T> = std::result::Result<T, secret_service::Error>;

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
    let service = SecretService::connect(EncryptionType::Dh)?;
    let collection = service.get_default_collection()?;
    collection.ensure_unlocked()?;
    let (_, attributes) = attributes(account_id);
    collection.create_item(
        &format!("Rustle account {account_id}"),
        borrow(&attributes),
        password.as_bytes(),
        true,
        "text/plain",
    )?;
    Ok(())
}

pub fn lookup_password(account_id: i64) -> Result<Option<String>> {
    let service = SecretService::connect(EncryptionType::Dh)?;
    let (_, attributes) = attributes(account_id);
    let results = service.search_items(borrow(&attributes))?;
    let Some(item) = results.unlocked.first().or(results.locked.first()) else {
        return Ok(None);
    };
    item.ensure_unlocked()?;
    let secret = item.get_secret()?;
    Ok(Some(String::from_utf8_lossy(&secret).into_owned()))
}

pub fn clear_password(account_id: i64) -> Result<()> {
    let service = SecretService::connect(EncryptionType::Dh)?;
    let (_, attributes) = attributes(account_id);
    let results = service.search_items(borrow(&attributes))?;
    for item in results.unlocked.iter().chain(results.locked.iter()) {
        item.delete()?;
    }
    Ok(())
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
