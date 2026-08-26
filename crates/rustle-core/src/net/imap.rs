//! One IMAP session: connect, sign in, run a few commands, log out.

use super::auth::{Credential, Mechanism};
use super::errors::NetError;
use super::{is_loopback, NET_TIMEOUT};
use crate::models::Security;
use ::imap::types::Flag;
use ::imap::Session;
use imap_proto::{Capability, NameAttribute};
use log::debug;
use regex::Regex;
use std::collections::HashSet;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::LazyLock;

pub type Result<T> = std::result::Result<T, NetError>;

/// IMAP system flags (RFC 3501 2.3.2). The same spellings are parsed out of a
/// FETCH reply and sent back by `store_flags`.
pub const FLAG_SEEN: &str = "\\Seen";
pub const FLAG_FLAGGED: &str = "\\Flagged";

/// Gmail files its own copy of everything sent through it. This capability is
/// how it identifies itself, so we don't append a second copy on top.
pub const GMAIL_CAPABILITY: &str = "X-GM-EXT-1";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailboxInfo {
    pub name: String,
    /// "" when the server reports NIL: a flat namespace.
    pub delimiter: String,
    /// A container that cannot hold mail (Gmail's "[Gmail]"): shown, never selected.
    pub is_selectable: bool,
}

/// One message's headers exactly as the server sent them. Raw on purpose:
/// addresses are unparsed header text and `date` is the original RFC 5322
/// string. `sync` turns this into a `MessageHeader`, the display-ready form.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FetchedHeader {
    pub uid: String,
    pub from_header: String,
    pub to_header: String,
    pub cc_header: String,
    pub subject: String,
    pub date: String,
    pub message_id: String,
    pub in_reply_to: String,
    pub references: String,
    pub is_seen: bool,
    pub is_flagged: bool,
    /// A snippet of the body, already decoded; empty when the server sent none.
    pub preview: String,
}

struct Xoauth2<'a>(&'a Credential);

impl ::imap::Authenticator for Xoauth2<'_> {
    type Response = String;
    fn process(&self, _challenge: &[u8]) -> String {
        self.0.xoauth2_response()
    }
}

pub struct ImapSession {
    host: String,
    port: u16,
    security: Security,
    session: Option<Session<::imap::Connection>>,
    client: Option<::imap::Client<::imap::Connection>>,
    capabilities: HashSet<String>,
}

impl ImapSession {
    pub fn new(host: &str, port: u16, security: Security) -> Self {
        ImapSession {
            host: host.to_string(),
            port,
            security,
            session: None,
            client: None,
            capabilities: HashSet::new(),
        }
    }

    /// Open the socket and, depending on the security, the TLS layer. Done by
    /// hand rather than through `ClientBuilder` so the socket carries a
    /// timeout: without one a server that stops answering hangs the worker
    /// thread forever.
    pub fn connect(&mut self) -> Result<()> {
        let tcp = connect_tcp(&self.host, self.port)?;
        let mut client: ::imap::Client<::imap::Connection> = match self.security {
            Security::None => {
                let mut client = ::imap::Client::new(Box::new(tcp) as ::imap::Connection);
                client.read_greeting()?;
                client
            }
            Security::Tls => {
                let tls = tls_connector(&self.host)?
                    .connect(&self.host, tcp)
                    .map_err(handshake_error)?;
                let mut client = ::imap::Client::new(Box::new(tls) as ::imap::Connection);
                client.read_greeting()?;
                client
            }
            Security::StartTls => {
                start_tls(&tcp)?;
                let tls = tls_connector(&self.host)?
                    .connect(&self.host, tcp)
                    .map_err(handshake_error)?;
                let mut client = ::imap::Client::new(Box::new(tls) as ::imap::Connection);
                // The greeting arrived on the plaintext half of the connection.
                client.greeting_read = true;
                client
            }
        };
        self.capabilities = client
            .capabilities()?
            .iter()
            .map(|capability| match capability {
                Capability::Imap4rev1 => "IMAP4REV1".to_string(),
                Capability::Auth(name) => format!("AUTH={}", name.to_uppercase()),
                Capability::Atom(name) => name.to_uppercase(),
            })
            .collect();
        self.client = Some(client);
        Ok(())
    }

    pub fn sign_in(&mut self, credential: &Credential) -> Result<()> {
        let client = self.client.take().ok_or_else(|| {
            NetError::Protocol(format!("not connected to {}:{}", self.host, self.port))
        })?;
        let result = match credential.mechanism {
            Mechanism::Xoauth2 => client.authenticate("XOAUTH2", &Xoauth2(credential)),
            Mechanism::Login => client.login(&credential.user, &credential.secret),
        };
        match result {
            Ok(session) => {
                self.session = Some(session);
                Ok(())
            }
            Err((error, _client)) => Err(error.into()),
        }
    }

    /// Runs from a `finally`-style position on every operation, so it never
    /// raises over the error already on its way out. A server hanging up
    /// first is normal, not a problem.
    pub fn logout(&mut self) {
        if let Some(mut session) = self.session.take() {
            if let Err(error) = session.logout() {
                debug!("IMAP logout from {} failed: {error}", self.host);
            }
        }
        self.client = None;
    }

    fn require(&mut self) -> Result<&mut Session<::imap::Connection>> {
        // Never hand back nothing: a caller that skipped connect() has to
        // fail loudly rather than quietly do nothing.
        let host = &self.host;
        let port = self.port;
        self.session
            .as_mut()
            .ok_or_else(|| NetError::Protocol(format!("not signed in to {host}:{port}")))
    }

    pub fn has_capability(&self, name: &str) -> bool {
        self.capabilities.contains(&name.to_uppercase())
    }

    /// Every listed mailbox, containers included so the caller can rebuild
    /// the hierarchy.
    pub fn list_folders(&mut self) -> Result<Vec<MailboxInfo>> {
        let names = self.require()?.list(None, Some("*"))?;
        Ok(names
            .iter()
            .map(|name| MailboxInfo {
                name: name.name().to_string(),
                delimiter: name.delimiter().unwrap_or("").to_string(),
                is_selectable: !name.attributes().contains(&NameAttribute::NoSelect),
            })
            .collect())
    }

    /// Open a mailbox; return how many messages it holds. Read-only by default
    /// keeps us non-destructive and never marks mail as read.
    pub fn select(&mut self, mailbox: &str, is_writable: bool) -> Result<u32> {
        let session = self.require()?;
        let info = if is_writable {
            session.select(mailbox)?
        } else {
            session.examine(mailbox)?
        };
        Ok(info.exists)
    }

    /// How many unread messages a mailbox holds, without selecting it.
    pub fn unseen_count(&mut self, mailbox: &str) -> Result<u32> {
        let status = self.require()?.status(mailbox, "(UNSEEN)")?;
        status
            .unseen
            .ok_or_else(|| NetError::Protocol(format!("no UNSEEN in the status of {mailbox}")))
    }

    /// Upload a message into a mailbox, stored `\Seen`: this is our own copy
    /// of something we just sent, and arriving as unread would be wrong.
    pub fn append(&mut self, mailbox: &str, raw: &[u8]) -> Result<()> {
        self.require()?
            .append(mailbox, raw)
            .flag(Flag::Seen)
            .finish()?;
        Ok(())
    }

    /// Add or remove flags on a UID set: "7" or "7,9,20".
    pub fn store_flags(&mut self, uids: &str, flag: &str, should_add: bool) -> Result<()> {
        let command = if should_add { "+FLAGS" } else { "-FLAGS" };
        self.require()?
            .uid_store(uids, format!("{command} ({flag})"))?;
        Ok(())
    }

    /// Every UID in the currently selected mailbox.
    pub fn search_all_uids(&mut self) -> Result<HashSet<String>> {
        let uids = self.require()?.uid_search("ALL")?;
        Ok(uids.into_iter().map(|uid| uid.to_string()).collect())
    }

    /// Move one message and return its destination UID when reported.
    /// COPYUID is the response code used by most servers; MOVEUID by some
    /// implementing RFC 6851. Both arrive on the tagged OK line, which the
    /// crate's own `uid_mv` discards, so the command is run raw.
    pub fn r#move(&mut self, uid: &str, destination: &str) -> Result<Option<String>> {
        let session = self.require()?;
        let command = format!("UID MOVE {uid} {}", quote_mailbox(destination));
        let (data, _done_at) = session.run(&command)?;
        let text = String::from_utf8_lossy(&data);
        Ok(destination_uid(&text))
    }

    /// Fetch UID + flags + a few headers for a window of `limit` messages,
    /// `offset` messages back from the newest. offset=0 is the newest page.
    pub fn fetch_recent_headers(
        &mut self,
        exists: u32,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<FetchedHeader>> {
        if exists == 0 || offset >= exists {
            return Ok(Vec::new());
        }
        let end = exists - offset;
        let start = end.saturating_sub(limit - 1).max(1); // exists=1000,limit=50,offset=50 -> 901:950
        let fetches = self.require()?.fetch(
            format!("{start}:{end}"),
            // BODY.PEEK[...] = look at the header WITHOUT marking it \Seen.
            // The first 4 KiB of the body ride along for the preview line;
            // Content-Type and the transfer encoding are what decode them.
            "(UID FLAGS BODY.PEEK[HEADER.FIELDS (DATE FROM TO CC SUBJECT MESSAGE-ID IN-REPLY-TO REFERENCES CONTENT-TYPE CONTENT-TRANSFER-ENCODING)] BODY.PEEK[TEXT]<0.4096>)",
        )?;
        Ok(fetches
            .iter()
            .filter_map(|fetch| {
                let uid = fetch.uid?;
                let header_bytes = fetch.header().unwrap_or(&[]);
                let flags = fetch.flags();
                let mut header = parse_header(
                    uid.to_string(),
                    header_bytes,
                    flags.contains(&Flag::Seen),
                    flags.contains(&Flag::Flagged),
                );
                header.preview =
                    crate::mime::preview_from_slices(header_bytes, fetch.text().unwrap_or(&[]));
                Some(header)
            })
            .collect())
    }

    /// Fetch one full message (headers + body) by its stable UID.
    pub fn fetch_message(&mut self, uid: &str) -> Result<Vec<u8>> {
        let fetches = self.require()?.uid_fetch(uid, "(BODY.PEEK[])")?;
        fetches
            .iter()
            .find_map(|fetch| fetch.body().map(|body| body.to_vec()))
            .ok_or_else(|| NetError::Protocol(format!("no message body returned for uid {uid}")))
    }
}

impl Drop for ImapSession {
    fn drop(&mut self) {
        self.logout();
    }
}

/// Resolve and connect with the shared timeout on every step.
fn connect_tcp(host: &str, port: u16) -> Result<TcpStream> {
    let mut last_error = None;
    for address in (host, port).to_socket_addrs()? {
        match TcpStream::connect_timeout(&address, NET_TIMEOUT) {
            Ok(stream) => {
                stream.set_read_timeout(Some(NET_TIMEOUT))?;
                stream.set_write_timeout(Some(NET_TIMEOUT))?;
                return Ok(stream);
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error
        .unwrap_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("failed to lookup address information for {host}"),
            )
        })
        .into())
}

/// The STARTTLS exchange, spoken directly on the socket: the crate's own
/// helper for this is private, and it is two lines of protocol.
fn start_tls(tcp: &TcpStream) -> Result<()> {
    use std::io::{BufRead, BufReader, Write};
    let mut reader = BufReader::new(tcp.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?; // the greeting
    if !line.starts_with("* OK") && !line.starts_with("* PREAUTH") {
        return Err(NetError::Protocol(format!(
            "unexpected IMAP greeting: {}",
            line.trim()
        )));
    }
    (&*tcp).write_all(b"P1 STARTTLS\r\n")?;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Err(NetError::Protocol(
                "connection closed during STARTTLS".into(),
            ));
        }
        if let Some(reply) = line.strip_prefix("P1 ") {
            if reply.starts_with("OK") {
                return Ok(());
            }
            return Err(NetError::Protocol(format!(
                "STARTTLS refused: {}",
                reply.trim()
            )));
        }
    }
}

fn tls_connector(host: &str) -> Result<native_tls::TlsConnector> {
    let skip_verify = is_loopback(host);
    Ok(native_tls::TlsConnector::builder()
        .danger_accept_invalid_certs(skip_verify)
        .danger_accept_invalid_hostnames(skip_verify)
        .build()?)
}

fn handshake_error(error: native_tls::HandshakeError<TcpStream>) -> NetError {
    match error {
        native_tls::HandshakeError::Failure(error) => NetError::Tls(error),
        native_tls::HandshakeError::WouldBlock(_) => {
            NetError::Protocol("TLS handshake would block".into())
        }
    }
}

/// Quote a mailbox name (escaping \ and ") so a space stays inside one astring.
pub fn quote_mailbox(name: &str) -> String {
    format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Extract a single destination UID from a COPYUID/MOVEUID response.
pub fn destination_uid(text: &str) -> Option<String> {
    static CODE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?:COPYUID|MOVEUID)\s+\d+\s+\d+(?::\d+)?\s+(\d+)(?::\d+)?").unwrap()
    });
    CODE.captures(text).map(|captures| captures[1].to_string())
}

/// Let mail-parser decode the header block: it handles line folding and the
/// =?utf-8?...?= encoding you'd otherwise see as gibberish.
pub fn parse_header(
    uid: String,
    header_bytes: &[u8],
    is_seen: bool,
    is_flagged: bool,
) -> FetchedHeader {
    let parsed = mail_parser::MessageParser::default().parse_headers(header_bytes);
    let header = |name: &str| -> String {
        parsed
            .as_ref()
            .and_then(|message| decoded_header(message, name))
            .unwrap_or_default()
    };
    FetchedHeader {
        uid,
        from_header: header("From"),
        to_header: header("To"),
        cc_header: header("Cc"),
        subject: header("Subject"),
        date: header("Date"),
        message_id: header("Message-ID"),
        in_reply_to: header("In-Reply-To"),
        references: header("References"),
        is_seen,
        is_flagged,
        preview: String::new(),
    }
}

/// A header as display text: addresses re-joined as `Name <addr>` so the
/// address parser sees decoded names, ids wrapped back in angle brackets,
/// everything else as its decoded text.
fn decoded_header(message: &mail_parser::Message, name: &str) -> Option<String> {
    use mail_parser::HeaderValue;
    let value = message.header(name)?;
    // mail-parser strips the angle brackets off message ids; the threader
    // matches tokens verbatim, so put them back on every id header alike.
    let is_id_header = matches!(name, "Message-ID" | "In-Reply-To" | "References");
    let text = match value {
        HeaderValue::Text(text) if is_id_header => format!("<{text}>"),
        HeaderValue::Address(address) => address
            .iter()
            .map(|mailbox| {
                let name = mailbox.name().unwrap_or("").trim();
                let addr = mailbox.address().unwrap_or("").trim();
                if name.is_empty() {
                    addr.to_string()
                } else {
                    format!("\"{}\" <{addr}>", name.replace('"', ""))
                }
            })
            .collect::<Vec<_>>()
            .join(", "),
        HeaderValue::Text(text) => text.to_string(),
        HeaderValue::TextList(list) => list
            .iter()
            .map(|t| format!("<{t}>"))
            .collect::<Vec<_>>()
            .join(" "),
        HeaderValue::DateTime(date) => date.to_rfc822(),
        _ => message.header_raw(name).unwrap_or("").trim().to_string(),
    };
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_and_response_codes() {
        assert_eq!(
            quote_mailbox("[Gmail]/Sent \"Mail\""),
            "\"[Gmail]/Sent \\\"Mail\\\"\""
        );
        assert_eq!(
            destination_uid("A3 OK [COPYUID 1511554416 142 41] Moved."),
            Some("41".into())
        );
        assert_eq!(
            destination_uid("A3 OK [MOVEUID 1 142:143 41:42] Moved."),
            Some("41".into())
        );
        assert_eq!(destination_uid("A3 OK Done."), None);
    }

    #[test]
    fn parses_fetched_headers() {
        let raw = b"From: =?utf-8?q?Ada_Lovelace?= <ada@example.com>\r\nTo: Bob <bob@x.y>, c@x.y\r\nSubject: =?utf-8?q?Gr=C3=BC=C3=9Fe?=\r\nDate: Wed, 16 Jul 2026 10:00:00 +0000\r\nMessage-ID: <m1@x>\r\nReferences: <a@x>\r\n <b@x>\r\n\r\n";
        let header = parse_header("9".into(), raw, true, false);
        assert_eq!(header.from_header, "\"Ada Lovelace\" <ada@example.com>");
        assert_eq!(header.to_header, "\"Bob\" <bob@x.y>, c@x.y");
        assert_eq!(header.subject, "Grüße");
        assert_eq!(header.message_id, "<m1@x>");
        assert_eq!(header.references, "<a@x> <b@x>");
        assert!(header.date.contains("2026"));
        assert!(header.is_seen);
    }
}
