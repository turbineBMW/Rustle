//! Reading mail accounts out of GNOME Online Accounts, over raw D-Bus with
//! Gio. Tokens are fetched live on every operation rather than copied into
//! the keyring, since they expire hourly.

use crate::models::{parse_port, Security};
use crate::net::auth::Credential;
use crate::net::NET_TIMEOUT;
use gio::prelude::*;
use glib::Variant;
use log::{debug, warn};
use std::collections::HashMap;

pub const BUS_NAME: &str = "org.gnome.OnlineAccounts";
pub const OBJECT_PATH: &str = "/org/gnome/OnlineAccounts";

const OBJECT_MANAGER: &str = "org.freedesktop.DBus.ObjectManager";
const ACCOUNT: &str = "org.gnome.OnlineAccounts.Account";
const MAIL: &str = "org.gnome.OnlineAccounts.Mail";
const OAUTH2: &str = "org.gnome.OnlineAccounts.OAuth2Based";

const IMAP_IMPLICIT_TLS_PORT: u16 = 993;
const IMAP_PORT: u16 = 143;
const SMTP_IMPLICIT_TLS_PORT: u16 = 465;
const SMTP_STARTTLS_PORT: u16 = 587;
const SMTP_PORT: u16 = 25;

pub type Properties = HashMap<String, Variant>;
pub type Interfaces = HashMap<String, Properties>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OnlineAccount {
    pub goa_id: String,
    pub email: String,
    pub display_name: String,
    pub provider_name: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_security: Security,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_security: Security,
    /// Microsoft 365 comes back false: its token only covers the Graph API,
    /// so there is no IMAP server to point at.
    pub is_mail_supported: bool,
    /// Mail switched off for this account in Settings. Listed rather than
    /// dropped, or the dialog says "no accounts" about one the user can see.
    pub is_mail_enabled: bool,
    /// Only OAuth accounts are imported: an IMAP/SMTP one is the Add Account
    /// dialog's job.
    pub is_oauth2: bool,
}

/// GNOME Online Accounts keeps a non-default port inside the host string,
/// as "mail.example.com:1143".
pub fn split_host_port(value: &str, default_port: u16) -> (String, u16) {
    match value.rsplit_once(':') {
        Some((host, port)) => match parse_port(port) {
            Some(port) => (host.to_string(), port),
            None => (value.to_string(), default_port),
        },
        None => (value.to_string(), default_port),
    }
}

fn security(is_implicit_tls: bool, is_starttls: bool) -> Security {
    if is_implicit_tls {
        Security::Tls
    } else if is_starttls {
        Security::StartTls
    } else {
        Security::None
    }
}

fn string(properties: &Properties, key: &str) -> String {
    properties
        .get(key)
        .and_then(|v| v.get::<String>())
        .unwrap_or_default()
}

fn boolean(properties: &Properties, key: &str) -> bool {
    properties
        .get(key)
        .and_then(|v| v.get::<bool>())
        .unwrap_or(false)
}

pub fn imap_server(mail: &Properties) -> (String, u16, Security) {
    let is_implicit_tls = boolean(mail, "ImapUseSsl");
    let default_port = if is_implicit_tls {
        IMAP_IMPLICIT_TLS_PORT
    } else {
        IMAP_PORT
    };
    let (host, port) = split_host_port(&string(mail, "ImapHost"), default_port);
    (
        host,
        port,
        security(is_implicit_tls, boolean(mail, "ImapUseTls")),
    )
}

pub fn smtp_server(mail: &Properties) -> (String, u16, Security) {
    let is_implicit_tls = boolean(mail, "SmtpUseSsl");
    let is_starttls = boolean(mail, "SmtpUseTls");
    let default_port = if is_implicit_tls {
        SMTP_IMPLICIT_TLS_PORT
    } else if is_starttls {
        SMTP_STARTTLS_PORT
    } else {
        SMTP_PORT
    };
    let (host, port) = split_host_port(&string(mail, "SmtpHost"), default_port);
    (host, port, security(is_implicit_tls, is_starttls))
}

/// Build the account record from one object's interfaces. Pure, so it can be
/// tested without a bus.
pub fn account_from_interfaces(interfaces: &Interfaces) -> Option<OnlineAccount> {
    let account = interfaces.get(ACCOUNT)?;
    if boolean(account, "IsTemporary") {
        return None;
    }
    let empty = Properties::new();
    let mail = interfaces.get(MAIL).unwrap_or(&empty);
    let (imap_host, imap_port, imap_security) = imap_server(mail);
    let (smtp_host, smtp_port, smtp_security) = smtp_server(mail);
    let mut email = string(mail, "EmailAddress");
    if email.is_empty() {
        email = string(account, "PresentationIdentity");
    }
    let mut display_name = string(mail, "Name");
    if display_name.is_empty() {
        display_name = email.split('@').next().unwrap_or("").to_string();
    }
    Some(OnlineAccount {
        goa_id: string(account, "Id"),
        email,
        display_name,
        provider_name: string(account, "ProviderName"),
        is_mail_supported: !imap_host.is_empty() && !smtp_host.is_empty(),
        is_mail_enabled: !boolean(account, "MailDisabled"),
        is_oauth2: interfaces.contains_key(OAUTH2),
        imap_host,
        imap_port,
        imap_security,
        smtp_host,
        smtp_port,
        smtp_security,
    })
}

/// Every account in GNOME Online Accounts, mail-capable or not.
pub fn mail_accounts() -> Vec<OnlineAccount> {
    let objects = match managed_objects() {
        Ok(objects) => objects,
        Err(error) => {
            debug!("could not list GNOME Online Accounts: {error}");
            return Vec::new();
        }
    };
    let mut accounts: Vec<OnlineAccount> = objects
        .values()
        .filter_map(account_from_interfaces)
        .collect();
    accounts.sort_by(|a, b| a.email.cmp(&b.email));
    accounts
}

/// The live sign-in for one online account.
pub fn credential(goa_id: &str) -> Option<Credential> {
    let objects = match managed_objects() {
        Ok(objects) => objects,
        Err(error) => {
            warn!("could not reach GNOME Online Accounts: {error}");
            return None;
        }
    };
    let Some((path, interfaces)) = objects.iter().find(|(_, interfaces)| {
        interfaces
            .get(ACCOUNT)
            .is_some_and(|account| string(account, "Id") == goa_id)
    }) else {
        warn!("online account {goa_id} is gone from GNOME Online Accounts");
        return None;
    };
    if !interfaces.contains_key(OAUTH2) {
        warn!("online account {goa_id} no longer signs in with OAuth");
        return None;
    }
    let token = match call(path, OAUTH2, "GetAccessToken", None) {
        Ok(reply) => reply.child_value(0).get::<String>().unwrap_or_default(),
        Err(error) => {
            warn!("could not get an access token for online account {goa_id}: {error}");
            return None;
        }
    };
    let empty = Properties::new();
    let mail = interfaces.get(MAIL).unwrap_or(&empty);
    let mut user = string(mail, "ImapUserName");
    if user.is_empty() {
        user = string(mail, "EmailAddress");
    }
    Some(Credential::token(user, token))
}

fn managed_objects() -> Result<HashMap<String, Interfaces>, glib::Error> {
    let reply = call(OBJECT_PATH, OBJECT_MANAGER, "GetManagedObjects", None)?;
    // a{oa{sa{sv}}}: object paths don't decode as String, so walk it by hand.
    let mut objects = HashMap::new();
    let dictionary = reply.child_value(0);
    for entry in dictionary.iter() {
        let path = entry.child_value(0).str().unwrap_or("").to_string();
        let mut interfaces = Interfaces::new();
        for interface in entry.child_value(1).iter() {
            let name = interface.child_value(0).str().unwrap_or("").to_string();
            let mut properties = Properties::new();
            for property in interface.child_value(1).iter() {
                let key = property.child_value(0).str().unwrap_or("").to_string();
                let value = property
                    .child_value(1)
                    .as_variant()
                    .unwrap_or_else(|| property.child_value(1));
                properties.insert(key, value);
            }
            interfaces.insert(name, properties);
        }
        objects.insert(path, interfaces);
    }
    Ok(objects)
}

fn call(
    object_path: &str,
    interface: &str,
    method: &str,
    parameters: Option<&Variant>,
) -> Result<Variant, glib::Error> {
    let proxy = gio::DBusProxy::for_bus_sync(
        gio::BusType::Session,
        gio::DBusProxyFlags::DO_NOT_LOAD_PROPERTIES | gio::DBusProxyFlags::DO_NOT_CONNECT_SIGNALS,
        None,
        BUS_NAME,
        object_path,
        interface,
        gio::Cancellable::NONE,
    )?;
    proxy.call_sync(
        method,
        parameters,
        gio::DBusCallFlags::NONE,
        NET_TIMEOUT.as_millis() as i32,
        gio::Cancellable::NONE,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn properties(pairs: &[(&str, Variant)]) -> Properties {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn splits_host_and_port() {
        assert_eq!(
            split_host_port("mail.example.com:1143", 143),
            ("mail.example.com".into(), 1143)
        );
        assert_eq!(
            split_host_port("mail.example.com", 143),
            ("mail.example.com".into(), 143)
        );
        assert_eq!(
            split_host_port("mail.example.com:x", 143),
            ("mail.example.com:x".into(), 143)
        );
    }

    #[test]
    fn works_out_servers_and_security() {
        let mail = properties(&[
            ("ImapHost", "imap.gmail.com".to_variant()),
            ("ImapUseSsl", true.to_variant()),
            ("SmtpHost", "smtp.gmail.com".to_variant()),
            ("SmtpUseTls", true.to_variant()),
        ]);
        assert_eq!(
            imap_server(&mail),
            ("imap.gmail.com".into(), 993, Security::Tls)
        );
        assert_eq!(
            smtp_server(&mail),
            ("smtp.gmail.com".into(), 587, Security::StartTls)
        );
        let plain = properties(&[("SmtpHost", "localhost".to_variant())]);
        assert_eq!(
            smtp_server(&plain),
            ("localhost".into(), 25, Security::None)
        );
    }

    #[test]
    fn builds_accounts() {
        let mut interfaces = Interfaces::new();
        interfaces.insert(
            ACCOUNT.into(),
            properties(&[
                ("Id", "account_1".to_variant()),
                ("ProviderName", "Google".to_variant()),
                ("PresentationIdentity", "ada@gmail.com".to_variant()),
            ]),
        );
        interfaces.insert(
            MAIL.into(),
            properties(&[
                ("ImapHost", "imap.gmail.com".to_variant()),
                ("ImapUseSsl", true.to_variant()),
                ("SmtpHost", "smtp.gmail.com".to_variant()),
                ("SmtpUseTls", true.to_variant()),
            ]),
        );
        interfaces.insert(OAUTH2.into(), Properties::new());
        let account = account_from_interfaces(&interfaces).unwrap();
        assert_eq!(account.email, "ada@gmail.com");
        assert_eq!(account.display_name, "ada");
        assert!(account.is_mail_supported && account.is_mail_enabled && account.is_oauth2);

        interfaces
            .get_mut(ACCOUNT)
            .unwrap()
            .insert("IsTemporary".into(), true.to_variant());
        assert!(account_from_interfaces(&interfaces).is_none());
    }
}
