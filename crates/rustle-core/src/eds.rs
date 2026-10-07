//! Mail accounts from Evolution Data Server, the desktop's account registry.
//! GNOME Online Accounts, graphmail-bridge, Evolution and this app all keep
//! their accounts there, so it is the one list Rustle reads.
//!
//! Spoken over raw D-Bus with Gio, like the rest of the crate. The registry
//! publishes each source as an object whose `Data` property is the source's
//! key file; a mail account is three of them (account, identity, transport),
//! often below a collection that owns the sign-in.
//!
//! Blocking calls: run them on a worker.

use crate::models::{Auth, Security};
use crate::net::NET_TIMEOUT;
use gio::prelude::*;
use glib::{KeyFile, Variant};
use log::{debug, warn};
use std::collections::HashMap;

pub const BUS_NAME: &str = "org.gnome.evolution.dataserver.Sources5";
pub const OBJECT_PATH: &str = "/org/gnome/evolution/dataserver/SourceManager";

const OBJECT_MANAGER: &str = "org.freedesktop.DBus.ObjectManager";
const SOURCE_MANAGER: &str = "org.gnome.evolution.dataserver.SourceManager";
const SOURCE: &str = "org.gnome.evolution.dataserver.Source";
const OAUTH2: &str = "org.gnome.evolution.dataserver.Source.OAuth2Support";
const REMOVABLE: &str = "org.gnome.evolution.dataserver.Source.Removable";

/// Sources this app creates carry this prefix, which is how it knows the
/// accounts it may delete rather than only hide.
pub const OWN_PREFIX: &str = "rustle-";

const IMAP_IMPLICIT_TLS_PORT: u16 = 993;
const IMAP_PORT: u16 = 143;
const SMTP_IMPLICIT_TLS_PORT: u16 = 465;
const SMTP_STARTTLS_PORT: u16 = 587;
const SMTP_PORT: u16 = 25;

/// iCloud's calendar and contacts servers, discovered from by EDS.
pub const ICLOUD_CALDAV: &str = "https://caldav.icloud.com/";
pub const ICLOUD_CARDDAV: &str = "https://contacts.icloud.com/";

/// One registry object: a source's UID, key file and what it supports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    pub uid: String,
    pub path: String,
    pub data: String,
    pub has_oauth2: bool,
    pub is_removable: bool,
}

/// Where and how to connect for one direction of an account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Server {
    /// The source holding these settings; its sign-in is looked up by it.
    pub uid: String,
    pub host: String,
    pub port: u16,
    pub security: Security,
    pub user: String,
    pub auth: Auth,
}

/// A usable IMAP account with its identity and SMTP transport.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailAccount {
    /// The mail account source: the account's identity in Rustle.
    pub uid: String,
    /// The top of the account's source tree (a collection, or the mail
    /// account itself): what is removed with it, and the fallback for a
    /// password stored once for the whole account.
    pub root_uid: String,
    /// GNOME Online Accounts' id, when the account comes from there.
    pub goa_id: String,
    pub email: String,
    /// The person's name for the From header.
    pub name: String,
    /// What the registry calls the account ("Gmail").
    pub label: String,
    pub imap: Server,
    pub smtp: Server,
}

impl MailAccount {
    /// Created by this app, so removing it in Rustle deletes it from EDS.
    pub fn is_own(&self) -> bool {
        self.root_uid.starts_with(OWN_PREFIX)
    }
}

/// Every source in the registry.
pub fn sources() -> Result<Vec<Source>, glib::Error> {
    let reply = call(OBJECT_PATH, OBJECT_MANAGER, "GetManagedObjects", None)?;
    // a{oa{sa{sv}}}: object paths don't decode as String, so walk it by hand.
    let mut sources = Vec::new();
    for entry in reply.child_value(0).iter() {
        let path = entry.child_value(0).str().unwrap_or("").to_string();
        let mut source = Source {
            uid: String::new(),
            path,
            data: String::new(),
            has_oauth2: false,
            is_removable: false,
        };
        for interface in entry.child_value(1).iter() {
            let name = interface.child_value(0).str().unwrap_or("").to_string();
            match name.as_str() {
                OAUTH2 => source.has_oauth2 = true,
                REMOVABLE => source.is_removable = true,
                SOURCE => {
                    for property in interface.child_value(1).iter() {
                        let key = property.child_value(0).str().unwrap_or("").to_string();
                        let value = property
                            .child_value(1)
                            .as_variant()
                            .unwrap_or_else(|| property.child_value(1));
                        let text = value.str().unwrap_or("").to_string();
                        match key.as_str() {
                            "UID" => source.uid = text,
                            "Data" => source.data = text,
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        if !source.uid.is_empty() {
            sources.push(source);
        }
    }
    Ok(sources)
}

/// Every enabled IMAP account in the registry.
pub fn mail_accounts() -> Result<Vec<MailAccount>, glib::Error> {
    Ok(mail_accounts_from(&sources()?))
}

/// The IMAP accounts among `sources`. Pure, so it can be tested without a
/// bus. Accounts missing their identity or transport, switched off, or of a
/// kind Rustle can't read (POP, local folders) are left out.
pub fn mail_accounts_from(sources: &[Source]) -> Vec<MailAccount> {
    let files: HashMap<&str, (KeyFile, &Source)> = sources
        .iter()
        .filter_map(|source| {
            let file = KeyFile::new();
            file.load_from_data(&source.data, glib::KeyFileFlags::NONE)
                .ok()?;
            Some((source.uid.as_str(), (file, source)))
        })
        .collect();
    let mut accounts: Vec<MailAccount> = files
        .iter()
        .filter_map(|(uid, (file, source))| account_from(uid, file, source, &files))
        .collect();
    accounts.sort_by(|a, b| a.email.cmp(&b.email).then(a.uid.cmp(&b.uid)));
    accounts
}

fn account_from(
    uid: &str,
    file: &KeyFile,
    source: &Source,
    files: &HashMap<&str, (KeyFile, &Source)>,
) -> Option<MailAccount> {
    if text(file, "Mail Account", "BackendName") != "imapx" {
        return None;
    }
    let chain = ancestors(uid, files);
    let enabled = |uid: &str| {
        files
            .get(uid)
            .is_some_and(|(file, _)| boolean(file, "Data Source", "Enabled", true))
    };
    if !chain.iter().all(|uid| enabled(uid)) {
        return None;
    }
    // A collection with mail switched off (in Online Accounts, say) still
    // keeps its mail sources around.
    let mail_enabled = chain.iter().skip(1).all(|parent| {
        files
            .get(parent.as_str())
            .is_none_or(|(file, _)| boolean(file, "Collection", "MailEnabled", true))
    });
    if !mail_enabled {
        return None;
    }
    let identity_uid = text(file, "Mail Account", "IdentityUid");
    let (identity, _) = files.get(identity_uid.as_str())?;
    if !enabled(&identity_uid) {
        return None;
    }
    let email = text(identity, "Mail Identity", "Address");
    if email.is_empty() {
        return None;
    }
    let transport_uid = text(identity, "Mail Submission", "TransportUid");
    let (transport, transport_source) = files.get(transport_uid.as_str())?;
    if text(transport, "Mail Transport", "BackendName") != "smtp" || !enabled(&transport_uid) {
        return None;
    }
    let root_uid = chain.last().cloned().unwrap_or_else(|| uid.to_string());
    let goa_id = chain
        .iter()
        .find_map(|uid| {
            let (file, _) = files.get(uid.as_str())?;
            let id = text(file, "GNOME Online Accounts", "AccountId");
            (!id.is_empty()).then_some(id)
        })
        .unwrap_or_default();
    let imap = server(uid, file, source, &email, Direction::Imap);
    let smtp = server(
        &transport_uid,
        transport,
        transport_source,
        &email,
        Direction::Smtp,
    );
    if imap.host.is_empty() || smtp.host.is_empty() {
        debug!("EDS account {uid} has no IMAP or SMTP server; skipped");
        return None;
    }
    Some(MailAccount {
        uid: uid.to_string(),
        root_uid,
        goa_id,
        name: text(identity, "Mail Identity", "Name"),
        label: text(file, "Data Source", "DisplayName"),
        email,
        imap,
        smtp,
    })
}

/// `uid` and its parents, nearest first.
fn ancestors(uid: &str, files: &HashMap<&str, (KeyFile, &Source)>) -> Vec<String> {
    let mut chain = vec![uid.to_string()];
    let mut current = uid.to_string();
    while let Some((file, _)) = files.get(current.as_str()) {
        let parent = text(file, "Data Source", "Parent");
        if parent.is_empty() || chain.contains(&parent) || !files.contains_key(parent.as_str()) {
            break;
        }
        chain.push(parent.clone());
        current = parent;
    }
    chain
}

#[derive(Clone, Copy)]
enum Direction {
    Imap,
    Smtp,
}

fn server(uid: &str, file: &KeyFile, source: &Source, email: &str, direction: Direction) -> Server {
    let port = u16::try_from(integer(file, "Authentication", "Port"))
        .ok()
        .filter(|port| *port > 0);
    let method = text(file, "Security", "Method");
    let security = security_from_method(&method, port, direction);
    if !matches!(
        method.as_str(),
        "ssl-on-alternate-port" | "starttls-on-standard-port" | "none"
    ) {
        warn!("EDS source {uid} has security method {method:?}; using {security:?}");
    }
    let port = port.unwrap_or(match (direction, security) {
        (Direction::Imap, Security::Tls) => IMAP_IMPLICIT_TLS_PORT,
        (Direction::Imap, _) => IMAP_PORT,
        (Direction::Smtp, Security::Tls) => SMTP_IMPLICIT_TLS_PORT,
        (Direction::Smtp, Security::StartTls) => SMTP_STARTTLS_PORT,
        (Direction::Smtp, Security::None) => SMTP_PORT,
    });
    let mut user = text(file, "Authentication", "User");
    if user.is_empty() {
        user = email.to_string();
    }
    Server {
        uid: uid.to_string(),
        host: text(file, "Authentication", "Host"),
        port,
        security,
        user,
        auth: auth(&text(file, "Authentication", "Method"), source.has_oauth2),
    }
}

/// The `[Security] Method` EDS wrote. Only an explicit "none" is plaintext:
/// a missing or unknown method gets TLS, or STARTTLS on a port that speaks
/// plaintext first, so a password never goes out in the clear by default.
fn security_from_method(method: &str, port: Option<u16>, direction: Direction) -> Security {
    match method {
        "ssl-on-alternate-port" => Security::Tls,
        "starttls-on-standard-port" => Security::StartTls,
        "none" => Security::None,
        _ => match (direction, port) {
            (Direction::Imap, Some(IMAP_PORT)) => Security::StartTls,
            (Direction::Smtp, Some(SMTP_STARTTLS_PORT | SMTP_PORT)) => Security::StartTls,
            _ => Security::Tls,
        },
    }
}

/// OAuth when the registry offers a token for the source, or the method
/// names an OAuth mechanism; a password otherwise (EDS writes "none", "",
/// "PLAIN" or "LOGIN" for that).
fn auth(method: &str, has_oauth2: bool) -> Auth {
    let oauth_method = matches!(
        method.to_ascii_lowercase().as_str(),
        "xoauth2" | "oauth2" | "oauthbearer" | "google" | "outlook" | "yahoo"
    );
    if has_oauth2 || oauth_method {
        Auth::OAuth2
    } else {
        Auth::Password
    }
}

fn text(file: &KeyFile, group: &str, key: &str) -> String {
    file.string(group, key)
        .map(|value| value.trim().to_string())
        .unwrap_or_default()
}

fn boolean(file: &KeyFile, group: &str, key: &str, default: bool) -> bool {
    file.boolean(group, key).unwrap_or(default)
}

fn integer(file: &KeyFile, group: &str, key: &str) -> i64 {
    file.int64(group, key).unwrap_or(0)
}

/// A fresh OAuth access token for a source, from the registry (which asks
/// GNOME Online Accounts or EDS's own OAuth services).
pub fn access_token(uid: &str) -> Result<String, glib::Error> {
    let path = path_of(uid)?;
    let reply = call(&path, OAUTH2, "GetAccessToken", None)?;
    Ok(reply.child_value(0).str().unwrap_or("").to_string())
}

/// Add sources to the registry: `(uid, key file)` pairs.
pub fn create_sources(sources: &[(String, String)]) -> Result<(), glib::Error> {
    let array: HashMap<String, String> = sources.iter().cloned().collect();
    let parameters = Variant::tuple_from_iter([array.to_variant()]);
    call(
        OBJECT_PATH,
        SOURCE_MANAGER,
        "CreateSources",
        Some(&parameters),
    )?;
    Ok(())
}

/// Remove a source and everything below it.
pub fn remove_source(uid: &str) -> Result<(), glib::Error> {
    let path = path_of(uid)?;
    call(&path, REMOVABLE, "Remove", None)?;
    Ok(())
}

fn path_of(uid: &str) -> Result<String, glib::Error> {
    sources()?
        .into_iter()
        .find(|source| source.uid == uid)
        .map(|source| source.path)
        .ok_or_else(|| {
            glib::Error::new(
                gio::IOErrorEnum::NotFound,
                &format!("no source {uid} in Evolution Data Server"),
            )
        })
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

/// What a new password account needs: who, where, and optionally the
/// CalDAV/CardDAV servers to register calendars and contacts from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewMailAccount {
    pub email: String,
    pub name: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_security: Security,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_security: Security,
    /// `(calendars URL, contacts URL)`.
    pub dav: Option<(String, String)>,
}

/// The key files for a new password account, as `(uid, data)` with the
/// root first. With calendars and contacts the root is a WebDAV collection
/// and the mail sources sit below it, so EDS reuses one stored password for
/// all of them; without, the root is the mail account and the identity and
/// transport sit below that, as Evolution files them. Removing the root
/// removes the rest. `stem` makes the UIDs unique.
pub fn new_account_sources(account: &NewMailAccount, stem: &str) -> Vec<(String, String)> {
    let root = format!("{OWN_PREFIX}{stem}");
    let (mail_uid, identity_uid, transport_uid) = (
        format!("{root}-mail"),
        format!("{root}-identity"),
        format!("{root}-transport"),
    );
    let email = escape(&account.email);
    let name = escape(&account.name);
    let label = escape(&account.email);
    let mut sources = Vec::new();
    let collection = match &account.dav {
        Some((calendars, contacts)) => {
            sources.push((
                root.clone(),
                format!(
                    "[Data Source]\nDisplayName={label}\nEnabled=true\nParent=\n\n\
                     [Collection]\nBackendName=webdav\nIdentity={email}\n\
                     CalendarEnabled=true\nContactsEnabled=true\nMailEnabled=true\n\
                     CalendarUrl={calendars}\nContactsUrl={contacts}\n\n\
                     [Authentication]\nHost=\nPort=0\nUser={email}\nMethod=plain/password\n\
                     RememberPassword=true\nProxyUid=system-proxy\n\n\
                     [Security]\nMethod=tls\n",
                    calendars = escape(calendars),
                    contacts = escape(contacts),
                ),
            ));
            root.clone()
        }
        None => String::new(),
    };
    // Below the collection, or else below the mail account.
    let parent = if collection.is_empty() {
        mail_uid.clone()
    } else {
        collection.clone()
    };
    sources.push((
        mail_uid.clone(),
        format!(
            "[Data Source]\nDisplayName={label}\nEnabled=true\nParent={collection}\n\n\
             [Mail Account]\nBackendName=imapx\nIdentityUid={identity_uid}\n\n\
             [Authentication]\nHost={host}\nPort={port}\nUser={email}\nMethod=\n\
             RememberPassword=true\nProxyUid=system-proxy\n\n\
             [Security]\nMethod={security}\n",
            host = escape(&account.imap_host),
            port = account.imap_port,
            security = security_method(account.imap_security),
        ),
    ));
    sources.push((
        identity_uid,
        format!(
            "[Data Source]\nDisplayName={label}\nEnabled=true\nParent={parent}\n\n\
             [Mail Identity]\nAddress={email}\nName={name}\n\n\
             [Mail Submission]\nTransportUid={transport_uid}\n"
        ),
    ));
    sources.push((
        transport_uid,
        format!(
            "[Data Source]\nDisplayName={label}\nEnabled=true\nParent={parent}\n\n\
             [Mail Transport]\nBackendName=smtp\n\n\
             [Authentication]\nHost={host}\nPort={port}\nUser={email}\nMethod=PLAIN\n\
             RememberPassword=true\nProxyUid=system-proxy\n\n\
             [Security]\nMethod={security}\n",
            host = escape(&account.smtp_host),
            port = account.smtp_port,
            security = security_method(account.smtp_security),
        ),
    ));
    sources
}

/// Create a password account in EDS and store its password, then wait for
/// the registry to list it. Returns every account the registry then has.
pub fn create_password_account(
    account: &NewMailAccount,
    password: &str,
) -> Result<Vec<MailAccount>, String> {
    let stem = format!(
        "{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis())
            .unwrap_or_default()
    );
    let sources = new_account_sources(account, &stem);
    // The password goes first, so the account never shows up unable to
    // sign in.
    for uid in password_uids(&sources) {
        crate::secrets::store_source_password(&uid, &account.email, password)
            .map_err(|error| format!("could not store the password: {error}"))?;
    }
    create_sources(&sources)
        .map_err(|error| format!("Evolution Data Server refused the account: {error}"))?;
    let mail_uid = sources
        .iter()
        .find(|(_, data)| data.contains("[Mail Account]"))
        .map(|(uid, _)| uid.clone())
        .unwrap_or_default();
    for _ in 0..50 {
        let accounts = mail_accounts()
            .map_err(|error| format!("could not read Evolution Data Server: {error}"))?;
        if accounts.iter().any(|found| found.uid == mail_uid) {
            return Ok(accounts);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Err("Evolution Data Server did not list the new account".into())
}

impl NewMailAccount {
    /// An account Rustle kept itself before EDS, as EDS should hold it.
    pub fn from_account(account: &crate::models::Account) -> Self {
        NewMailAccount {
            email: account.email.clone(),
            name: account.display_name.clone(),
            imap_host: account.imap_host.clone(),
            imap_port: account.imap_port,
            imap_security: account.imap_security,
            smtp_host: account.smtp_host.clone(),
            smtp_port: account.smtp_port,
            smtp_security: account.smtp_security,
            dav: is_icloud(&account.email)
                .then(|| (ICLOUD_CALDAV.to_string(), ICLOUD_CARDDAV.to_string())),
        }
    }
}

/// The UIDs a new account's password is stored under: the collection's,
/// or the mail account's and the transport's when there is none.
pub fn password_uids(sources: &[(String, String)]) -> Vec<String> {
    match sources.first() {
        Some((uid, data)) if data.contains("[Collection]") => vec![uid.clone()],
        _ => sources
            .iter()
            .filter(|(_, data)| {
                data.contains("[Mail Account]") || data.contains("[Mail Transport]")
            })
            .map(|(uid, _)| uid.clone())
            .collect(),
    }
}

fn security_method(security: Security) -> &'static str {
    match security {
        Security::Tls => "ssl-on-alternate-port",
        Security::StartTls => "starttls-on-standard-port",
        Security::None => "none",
    }
}

/// Key file escaping for a value on one line.
fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

/// iCloud mail addresses, whose accounts also bring calendars and contacts.
pub fn is_icloud(email: &str) -> bool {
    let domain = email
        .rsplit_once('@')
        .map(|(_, domain)| domain.to_ascii_lowercase());
    matches!(domain.as_deref(), Some("icloud.com" | "me.com" | "mac.com"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(uid: &str, data: &str, has_oauth2: bool) -> Source {
        Source {
            uid: uid.into(),
            path: format!("/{uid}"),
            data: data.into(),
            has_oauth2,
            is_removable: false,
        }
    }

    /// The shape GNOME Online Accounts leaves in the registry for Gmail.
    fn gmail() -> Vec<Source> {
        vec![
            source(
                "coll",
                "[Data Source]\nDisplayName=Gmail\nEnabled=true\nParent=\n\
                 [Collection]\nBackendName=google\nMailEnabled=true\n\
                 [GNOME Online Accounts]\nAccountId=account_1\n",
                true,
            ),
            source(
                "mail",
                "[Data Source]\nDisplayName=Gmail\nEnabled=true\nParent=coll\n\
                 [Mail Account]\nBackendName=imapx\nIdentityUid=identity\n\
                 [Authentication]\nHost=imap.gmail.com\nMethod=XOAUTH2\nPort=993\n\
                 User=ada@gmail.com\n[Security]\nMethod=ssl-on-alternate-port\n",
                true,
            ),
            source(
                "identity",
                "[Data Source]\nDisplayName=Gmail\nEnabled=true\nParent=coll\n\
                 [Mail Submission]\nTransportUid=transport\n\
                 [Mail Identity]\nAddress=ada@gmail.com\nName=Ada Lovelace\n",
                true,
            ),
            source(
                "transport",
                "[Data Source]\nDisplayName=Gmail\nEnabled=true\nParent=coll\n\
                 [Authentication]\nHost=smtp.gmail.com\nMethod=XOAUTH2\nPort=465\n\
                 User=ada@gmail.com\n[Security]\nMethod=ssl-on-alternate-port\n\
                 [Mail Transport]\nBackendName=smtp\n",
                true,
            ),
            // EDS's built-in local folders and search folders.
            source(
                "local",
                "[Data Source]\nDisplayName=On This Computer\n[Mail Account]\nBackendName=maildir\n",
                false,
            ),
            source(
                "vfolder",
                "[Data Source]\nDisplayName=Search Folders\n[Mail Account]\nBackendName=vfolder\n",
                false,
            ),
        ]
    }

    #[test]
    fn reads_online_accounts_through_their_collection() {
        let accounts = mail_accounts_from(&gmail());
        assert_eq!(accounts.len(), 1);
        let account = &accounts[0];
        assert_eq!(account.uid, "mail");
        assert_eq!(account.root_uid, "coll");
        assert_eq!(account.goa_id, "account_1");
        assert_eq!(account.email, "ada@gmail.com");
        assert_eq!(account.name, "Ada Lovelace");
        assert_eq!(account.label, "Gmail");
        assert_eq!(
            account.imap,
            Server {
                uid: "mail".into(),
                host: "imap.gmail.com".into(),
                port: 993,
                security: Security::Tls,
                user: "ada@gmail.com".into(),
                auth: Auth::OAuth2,
            }
        );
        assert_eq!(account.smtp.uid, "transport");
        assert_eq!(account.smtp.port, 465);
        assert!(!account.is_own());
    }

    #[test]
    fn mail_switched_off_or_disabled_hides_the_account() {
        let mut sources = gmail();
        sources[0].data = sources[0]
            .data
            .replace("MailEnabled=true", "MailEnabled=false");
        assert!(mail_accounts_from(&sources).is_empty());
        let mut sources = gmail();
        sources[1].data = sources[1].data.replace("Enabled=true", "Enabled=false");
        assert!(mail_accounts_from(&sources).is_empty());
        let mut sources = gmail();
        sources.remove(3);
        assert!(mail_accounts_from(&sources).is_empty(), "no transport");
    }

    #[test]
    fn bridge_accounts_keep_plaintext_loopback_and_passwords() {
        let sources = vec![
            source(
                "bridge",
                "[Data Source]\nDisplayName=Work\nParent=\n[Collection]\nBackendName=webdav\nMailEnabled=true\n",
                false,
            ),
            source(
                "bridge-mail",
                "[Data Source]\nDisplayName=Work\nParent=bridge\n\
                 [Mail Account]\nBackendName=imapx\nIdentityUid=bridge-identity\n\
                 [Authentication]\nHost=127.0.0.1\nPort=1143\nUser=me@work.com\nMethod=\n\
                 [Security]\nMethod=none\n",
                false,
            ),
            source(
                "bridge-identity",
                "[Data Source]\nParent=bridge\n[Mail Identity]\nAddress=me@work.com\n\
                 [Mail Submission]\nTransportUid=bridge-transport\n",
                false,
            ),
            source(
                "bridge-transport",
                "[Data Source]\nParent=bridge\n[Authentication]\nHost=127.0.0.1\nPort=1025\n\
                 Method=PLAIN\n[Security]\nMethod=none\n[Mail Transport]\nBackendName=smtp\n",
                false,
            ),
        ];
        let account = &mail_accounts_from(&sources)[0];
        assert_eq!(account.imap.security, Security::None);
        assert_eq!(account.imap.auth, Auth::Password);
        assert_eq!(account.smtp.port, 1025);
        assert_eq!(
            account.smtp.user, "me@work.com",
            "falls back to the address"
        );
        assert_eq!(account.goa_id, "");
    }

    #[test]
    fn default_ports_follow_security() {
        let mut sources = gmail();
        sources[1].data = sources[1].data.replace("Port=993\n", "");
        sources[3].data = sources[3]
            .data
            .replace("Port=465\n", "")
            .replace("ssl-on-alternate-port", "starttls-on-standard-port");
        let account = &mail_accounts_from(&sources)[0];
        assert_eq!((account.imap.port, account.smtp.port), (993, 587));
    }

    #[test]
    fn only_an_explicit_none_is_plaintext() {
        let read = |imap: &str, smtp: &str| {
            let mut sources = gmail();
            // The group goes, and a "Port=" given in its place overrides.
            let swap = |data: &str, with: &str, default: &str| {
                let data = data.replace("[Security]\nMethod=ssl-on-alternate-port\n", "");
                match with.strip_prefix("Port=") {
                    Some(_) => data.replace(default, with),
                    None => format!("{data}{with}"),
                }
            };
            sources[1].data = swap(&sources[1].data, imap, "Port=993\n");
            sources[3].data = swap(&sources[3].data, smtp, "Port=465\n");
            let account = mail_accounts_from(&sources).remove(0);
            (
                (account.imap.security, account.imap.port),
                (account.smtp.security, account.smtp.port),
            )
        };
        // No [Security] group at all.
        assert_eq!(read("", ""), ((Security::Tls, 993), (Security::Tls, 465)));
        assert_eq!(
            read("[Security]\nMethod=bogus\n", "[Security]\nMethod=\n"),
            ((Security::Tls, 993), (Security::Tls, 465))
        );
        // A plaintext-first port gets STARTTLS instead.
        assert_eq!(
            read("Port=143\n", "Port=587\n"),
            ((Security::StartTls, 143), (Security::StartTls, 587))
        );
        assert_eq!(
            security_from_method("", Some(143), Direction::Imap),
            Security::StartTls
        );
        assert_eq!(
            security_from_method("", Some(587), Direction::Smtp),
            Security::StartTls
        );
        assert_eq!(
            security_from_method("", Some(25), Direction::Smtp),
            Security::StartTls
        );
        assert_eq!(
            security_from_method("", Some(1143), Direction::Imap),
            Security::Tls
        );
        assert_eq!(
            read("[Security]\nMethod=none\n", "[Security]\nMethod=none\n"),
            ((Security::None, 993), (Security::None, 465))
        );
    }

    #[test]
    fn new_icloud_accounts_bring_calendars_under_one_collection() {
        let account = NewMailAccount {
            email: "ada@icloud.com".into(),
            name: "Ada".into(),
            imap_host: "imap.mail.me.com".into(),
            imap_port: 993,
            imap_security: Security::Tls,
            smtp_host: "smtp.mail.me.com".into(),
            smtp_port: 587,
            smtp_security: Security::StartTls,
            dav: Some((ICLOUD_CALDAV.into(), ICLOUD_CARDDAV.into())),
        };
        let created = new_account_sources(&account, "1");
        let uids: Vec<&str> = created.iter().map(|(uid, _)| uid.as_str()).collect();
        assert_eq!(
            uids,
            [
                "rustle-1",
                "rustle-1-mail",
                "rustle-1-identity",
                "rustle-1-transport"
            ]
        );
        assert_eq!(password_uids(&created), ["rustle-1"]);
        // Read back through the same parser the app uses.
        let sources: Vec<Source> = created
            .iter()
            .map(|(uid, data)| source(uid, data, false))
            .collect();
        let read = &mail_accounts_from(&sources)[0];
        assert_eq!(read.root_uid, "rustle-1");
        assert!(read.is_own());
        assert_eq!(read.smtp.security, Security::StartTls);
        assert_eq!(read.imap.auth, Auth::Password);
        assert!(created[0]
            .1
            .contains("CalendarUrl=https://caldav.icloud.com/"));
    }

    #[test]
    fn plain_accounts_are_rooted_at_the_mail_account() {
        let account = NewMailAccount {
            email: "me@example.com".into(),
            name: "Me".into(),
            imap_host: "imap.example.com".into(),
            imap_port: 993,
            imap_security: Security::Tls,
            smtp_host: "smtp.example.com".into(),
            smtp_port: 465,
            smtp_security: Security::Tls,
            dav: None,
        };
        let created = new_account_sources(&account, "2");
        assert_eq!(created[0].0, "rustle-2-mail");
        assert!(created[0].1.contains("Parent=\n"));
        for (_, data) in &created[1..] {
            assert!(data.contains("Parent=rustle-2-mail\n"), "{data}");
        }
        assert_eq!(
            password_uids(&created),
            ["rustle-2-mail", "rustle-2-transport"]
        );
        let sources: Vec<Source> = created
            .iter()
            .map(|(uid, data)| source(uid, data, false))
            .collect();
        let read = &mail_accounts_from(&sources)[0];
        assert_eq!(read.root_uid, "rustle-2-mail");
        assert!(read.is_own());
    }

    #[test]
    fn recognises_icloud_addresses() {
        assert!(is_icloud("Ada@Me.com"));
        assert!(!is_icloud("ada@gmail.com"));
    }
}
