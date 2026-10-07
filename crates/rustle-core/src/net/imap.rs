//! One IMAP session: connect, sign in, run a few commands, log out.

use super::auth::{Credential, Mechanism};
use super::errors::NetError;
use super::{is_loopback, NET_TIMEOUT};
use crate::models::Security;
use ::imap::extensions::idle::WaitOutcome;
use ::imap::types::{Flag, UnsolicitedResponse};
use ::imap::Session;
use imap_proto::{Capability, NameAttribute};
use log::debug;
use regex::Regex;
use std::collections::HashSet;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::LazyLock;
use std::time::Duration;

pub type Result<T> = std::result::Result<T, NetError>;

/// IMAP system flags (RFC 3501 2.3.2). The same spellings are parsed out of a
/// FETCH reply and sent back by `store_flags`.
pub const FLAG_SEEN: &str = "\\Seen";
pub const FLAG_FLAGGED: &str = "\\Flagged";
/// Outlook's pin-to-top, as the graphmail-bridge exposes it. Any server that
/// permits keywords (`\*` in PERMANENTFLAGS) stores it too, just for us.
pub const FLAG_PINNED: &str = "$Pinned";

/// Gmail files its own copy of everything sent through it. This capability is
/// how it identifies itself, so we don't append a second copy on top.
pub const GMAIL_CAPABILITY: &str = "X-GM-EXT-1";

/// The longest UID set put on one command line. RFC 7162 asks clients to
/// keep lines under 8192 octets and some servers refuse anything longer;
/// this leaves room for the tag, the command and its arguments.
const MAX_UID_SET_LEN: usize = 4000;

/// What a header fetch asks for. BODY.PEEK[...] = look WITHOUT marking the
/// message \Seen. The first 4 KiB of the body ride along for the preview
/// line; Content-Type and the transfer encoding are what decode them.
const HEADER_FETCH_QUERY: &str = "(UID FLAGS BODY.PEEK[HEADER.FIELDS (DATE FROM TO CC SUBJECT MESSAGE-ID REFERENCES IN-REPLY-TO THREAD-INDEX CONTENT-TYPE CONTENT-TRANSFER-ENCODING)] BODY.PEEK[TEXT]<0.4096>)";

/// One message's conversation headers, raw.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ThreadHeaders {
    pub uid: String,
    pub message_id: String,
    pub references: String,
    pub in_reply_to: String,
    pub thread_index: String,
}

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
    pub is_seen: bool,
    pub is_flagged: bool,
    pub is_pinned: bool,
    /// A snippet of the body, already decoded; empty when the server sent none.
    pub preview: String,
    /// The start of the body as text, for the search index: the same 4 KiB
    /// the preview comes from, not cut down to a line.
    pub body_text: String,
    /// Conversation headers, raw: References, In-Reply-To, Thread-Index.
    pub references: String,
    pub in_reply_to: String,
    pub thread_index: String,
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
    /// A second handle on the socket under the TLS layer, so a wait can be
    /// cut short from another thread and the read timeout put back.
    socket: Option<TcpStream>,
    /// A command failed part-way through its reply (a timeout, a dropped or
    /// garbled response): what the server sends next may answer the command
    /// before, so nothing more is sent on this connection.
    is_broken: bool,
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
            socket: None,
            is_broken: false,
        }
    }

    /// Open the socket and, depending on the security, the TLS layer. Done by
    /// hand rather than through `ClientBuilder` so the socket carries a
    /// timeout: without one a server that stops answering hangs the worker
    /// thread forever.
    pub fn connect(&mut self) -> Result<()> {
        let tcp = connect_tcp(&self.host, self.port)?;
        self.socket = tcp.try_clone().ok();
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
        self.capabilities = capability_names(&client.capabilities()?);
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
                // What a server lists before sign-in is what an anonymous
                // client may use; Dovecot and Gmail name UIDPLUS, MOVE and
                // IDLE only after. Ask again, or those look missing.
                let capabilities = self.command(|session| session.capabilities())?;
                self.capabilities = capability_names(&capabilities);
                Ok(())
            }
            Err((error, _client)) => Err(error.into()),
        }
    }

    /// Runs from a `finally`-style position on every operation, so it never
    /// raises over the error already on its way out. A server hanging up
    /// first is normal, not a problem.
    pub fn logout(&mut self) {
        if self.is_broken {
            // LOGOUT's reply could not be told from a late one: just hang up.
            if let Some(socket) = self.socket.take() {
                let _ = socket.shutdown(std::net::Shutdown::Both);
            }
            self.session = None;
        } else if let Some(mut session) = self.session.take() {
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

    /// Run one command on the signed-in session. Every command goes through
    /// here, so a failure that leaves the reply half read marks the session
    /// broken no matter who sent it; a clean NO or BAD from the server does
    /// not, the exchange is over.
    fn command<T>(
        &mut self,
        run: impl FnOnce(&mut Session<::imap::Connection>) -> ::imap::Result<T>,
    ) -> Result<T> {
        if self.is_broken {
            return Err(NetError::Io(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                format!("the connection to {} is out of step", self.host),
            )));
        }
        let result = run(self.require()?);
        if let Err(error) = &result {
            self.is_broken |= leaves_stream_dirty(error);
        }
        Ok(result?)
    }

    /// Signed in, and nothing has left the connection out of step: safe to
    /// hand to the next job.
    pub fn is_usable(&self) -> bool {
        self.session.is_some() && !self.is_broken
    }

    pub fn has_capability(&self, name: &str) -> bool {
        self.capabilities.contains(&name.to_uppercase())
    }

    /// Another handle on the connection's socket. Shutting it down from any
    /// thread makes a blocked read on the session return at once, which is
    /// how a long `idle` wait is cancelled.
    pub fn socket(&self) -> Option<TcpStream> {
        self.socket
            .as_ref()
            .and_then(|socket| socket.try_clone().ok())
    }

    /// Sit in IDLE (RFC 2177) until the selected mailbox changes or
    /// `timeout` passes. True when it changed: a message arrived, went, or
    /// had its flags touched. Servers may drop a client idle for 30 minutes,
    /// so callers re-issue this inside that.
    pub fn idle(&mut self, timeout: Duration) -> Result<bool> {
        let socket = self.socket();
        let session = self.require()?;
        let mut server_left = false;
        let outcome = {
            let mut handle = session.idle();
            handle.timeout(timeout).keepalive(false);
            let outcome = handle.wait_while(|response| match response {
                UnsolicitedResponse::Bye { .. } => {
                    server_left = true;
                    false
                }
                UnsolicitedResponse::Exists(_)
                | UnsolicitedResponse::Expunge(_)
                | UnsolicitedResponse::Recent(_)
                | UnsolicitedResponse::Fetch { .. } => false,
                _ => true,
            });
            // The crate clears the read timeout once the wait ends, and the
            // DONE it sends on drop reads a reply: put the timeout back first
            // or a server that went quiet holds the thread forever.
            if let Some(socket) = &socket {
                let _ = socket.set_read_timeout(Some(NET_TIMEOUT));
            }
            outcome
        };
        self.is_broken |= server_left || outcome.is_err();
        if server_left {
            return Err(NetError::Protocol(format!(
                "{} closed the connection",
                self.host
            )));
        }
        let changed = outcome? == WaitOutcome::MailboxChanged;
        // What arrived between the end of the wait and DONE's reply lands in
        // the unsolicited queue, where the next IDLE would never see it.
        Ok(self.drain_changes() || changed)
    }

    /// Empty the queue of responses the server sent unasked, and say whether
    /// any of them reported a change to the selected mailbox.
    pub fn drain_changes(&mut self) -> bool {
        let Some(session) = self.session.as_mut() else {
            return false;
        };
        // The whole queue is taken up front, so stopping at the first change
        // still empties it.
        session.take_all_unsolicited().any(|response| {
            matches!(
                response,
                UnsolicitedResponse::Exists(_)
                    | UnsolicitedResponse::Expunge(_)
                    | UnsolicitedResponse::Recent(_)
                    | UnsolicitedResponse::Fetch { .. }
            )
        })
    }

    /// A round trip that does nothing: proof the connection still works.
    pub fn noop(&mut self) -> Result<()> {
        self.command(|session| session.noop())
    }

    /// Drop the connection without a goodbye: for one that may be dead,
    /// where LOGOUT would only wait out the timeout.
    pub fn discard(mut self) {
        if let Some(socket) = self.socket.take() {
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
        self.session = None;
        self.client = None;
    }

    /// Every listed mailbox, containers included so the caller can rebuild
    /// the hierarchy.
    pub fn list_folders(&mut self) -> Result<Vec<MailboxInfo>> {
        let names = self.command(|session| session.list(None, Some("*")))?;
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
        let info = self.command(|session| {
            if is_writable {
                session.select(mailbox)
            } else {
                session.examine(mailbox)
            }
        })?;
        Ok(info.exists)
    }

    /// How many unread messages a mailbox holds, without selecting it.
    pub fn unseen_count(&mut self, mailbox: &str) -> Result<u32> {
        let status = self.command(|session| session.status(mailbox, "(UNSEEN)"))?;
        status
            .unseen
            .ok_or_else(|| NetError::Protocol(format!("no UNSEEN in the status of {mailbox}")))
    }

    /// Upload a message into a mailbox, stored `\Seen`: this is our own copy
    /// of something we just sent, and arriving as unread would be wrong.
    pub fn append(&mut self, mailbox: &str, raw: &[u8]) -> Result<()> {
        self.command(|session| session.append(mailbox, raw).flag(Flag::Seen).finish())?;
        Ok(())
    }

    /// Upload a draft: `\Draft` so any client offers to finish it, `\Seen`
    /// so it never counts as unread.
    pub fn append_draft(&mut self, mailbox: &str, raw: &[u8]) -> Result<()> {
        self.command(|session| {
            session
                .append(mailbox, raw)
                .flags([Flag::Draft, Flag::Seen])
                .finish()
        })?;
        Ok(())
    }

    /// CREATE, RENAME and DELETE a mailbox, by its name on the wire.
    pub fn create_mailbox(&mut self, name: &str) -> Result<()> {
        self.command(|session| session.create(name))
    }

    pub fn rename_mailbox(&mut self, from: &str, to: &str) -> Result<()> {
        self.command(|session| session.rename(from, to))
    }

    pub fn delete_mailbox(&mut self, name: &str) -> Result<()> {
        self.command(|session| session.delete(name))
    }

    /// Remove messages from the selected (writable) mailbox for good, or as
    /// near as the server allows without touching anything else: see
    /// `expunge_uids`.
    pub fn delete_uids(&mut self, uids: &[String]) -> Result<()> {
        for set in uid_sets(uids) {
            self.command(|session| session.uid_store(&set, "+FLAGS (\\Deleted)"))?;
        }
        self.expunge_uids(uids)
    }

    /// Expunge `uids`, already marked \Deleted, and nothing else. UID
    /// EXPUNGE (UIDPLUS) does exactly that. Without it there is only a
    /// plain EXPUNGE, which also purges whatever else is marked \Deleted in
    /// the mailbox -- mail another client marked and can still undelete. So
    /// it is sent only when a search finds nothing marked but ours;
    /// otherwise ours stay marked for a later expunge to take. A leftover
    /// is a nuisance; another client's mail purged is gone for good. (One
    /// marked in the round trip between search and expunge still goes.)
    fn expunge_uids(&mut self, uids: &[String]) -> Result<()> {
        if self.has_capability("UIDPLUS") {
            for set in uid_sets(uids) {
                self.command(|session| session.uid_expunge(&set))?;
            }
            return Ok(());
        }
        let ours: HashSet<&str> = uids.iter().map(String::as_str).collect();
        let marked = match self.command(|session| session.uid_search("DELETED")) {
            Ok(marked) => marked,
            // Refused: no way to be sure, so leave them marked.
            Err(error) if self.is_usable() => {
                debug!(
                    "left {} message(s) marked deleted on {}: {error}",
                    uids.len(),
                    self.host
                );
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if marked
            .iter()
            .all(|uid| ours.contains(uid.to_string().as_str()))
        {
            self.command(|session| session.expunge())?;
        } else {
            debug!(
                "left {} message(s) marked deleted on {}: others are marked too and it has no UIDPLUS",
                uids.len(),
                self.host
            );
        }
        Ok(())
    }

    /// Add or remove a flag on some messages, in as few STOREs as keep each
    /// command line short. Adding or removing twice is harmless, so a
    /// retry after a failure part-way only repeats what already landed.
    pub fn store_flags(&mut self, uids: &[String], flag: &str, should_add: bool) -> Result<()> {
        let command = if should_add { "+FLAGS" } else { "-FLAGS" };
        for set in uid_sets(uids) {
            self.command(|session| session.uid_store(&set, format!("{command} ({flag})")))?;
        }
        Ok(())
    }

    /// Every UID in the currently selected mailbox.
    pub fn search_all_uids(&mut self) -> Result<HashSet<String>> {
        let uids = self.command(|session| session.uid_search("ALL"))?;
        Ok(uids.into_iter().map(|uid| uid.to_string()).collect())
    }

    /// The UIDs in the selected mailbox matching a SEARCH `criteria`.
    pub fn search_uids(&mut self, criteria: &str) -> Result<Vec<String>> {
        let uids = self.command(|session| session.uid_search(criteria))?;
        Ok(uids.into_iter().map(|uid| uid.to_string()).collect())
    }

    /// Move one message and return its destination UID when reported.
    /// COPYUID is the response code used by most servers; MOVEUID by some
    /// implementing RFC 6851. Both arrive in the reply, which the crate's
    /// own `uid_mv` and `uid_copy` discard, so the commands are run raw.
    /// A server without MOVE gets the long way round: COPY, mark the
    /// original \Deleted, expunge it (see `expunge_uids` for when that
    /// waits). Should a step after the copy fail, the original stays put
    /// and the error goes back: at worst a second copy, never no copy.
    pub fn r#move(&mut self, uid: &str, destination: &str) -> Result<Option<String>> {
        let mailbox = quote_mailbox(destination);
        if self.has_capability("MOVE") {
            let command = format!("UID MOVE {uid} {mailbox}");
            let (data, _done_at) = self.command(|session| session.run(&command))?;
            return Ok(destination_uid(&String::from_utf8_lossy(&data)));
        }
        let command = format!("UID COPY {uid} {mailbox}");
        let (data, _done_at) = self.command(|session| session.run(&command))?;
        let destination_uid = destination_uid(&String::from_utf8_lossy(&data));
        self.command(|session| session.uid_store(uid, "+FLAGS (\\Deleted)"))?;
        self.expunge_uids(&[uid.to_string()])?;
        Ok(destination_uid)
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
        self.fetch_header_set(&format!("{start}:{end}"), false)
    }

    /// Fetch UID + flags + a few headers for an explicit UID set
    /// ("1:5,8,10:20"): the backfill's way of asking for exactly the
    /// messages it lacks, whatever their sequence numbers are by now.
    /// Just the conversation headers of some messages, for filling in the
    /// keys of mail fetched before they were kept.
    pub fn fetch_thread_headers(&mut self, uid_set: &str) -> Result<Vec<ThreadHeaders>> {
        let fetches = self.command(|session| {
            session.uid_fetch(
                uid_set,
                "(UID BODY.PEEK[HEADER.FIELDS (MESSAGE-ID REFERENCES IN-REPLY-TO THREAD-INDEX)])",
            )
        })?;
        Ok(fetches
            .iter()
            .filter_map(|fetch| {
                let uid = fetch.uid?.to_string();
                // See `fetch_header_set`: no header section, not an answer.
                let parsed = mail_parser::MessageParser::default().parse_headers(fetch.header()?);
                let raw = |name: &str| -> String {
                    parsed
                        .as_ref()
                        .and_then(|message| message.header_raw(name))
                        .map(|text| text.trim().to_string())
                        .unwrap_or_default()
                };
                Some(ThreadHeaders {
                    uid,
                    message_id: raw("Message-ID"),
                    references: raw("References"),
                    in_reply_to: raw("In-Reply-To"),
                    thread_index: raw("Thread-Index"),
                })
            })
            .collect())
    }

    pub fn fetch_headers_by_uid(&mut self, uid_set: &str) -> Result<Vec<FetchedHeader>> {
        if uid_set.is_empty() {
            return Ok(Vec::new());
        }
        self.fetch_header_set(uid_set, true)
    }

    fn fetch_header_set(&mut self, set: &str, by_uid: bool) -> Result<Vec<FetchedHeader>> {
        let fetches = self.command(|session| {
            if by_uid {
                session.uid_fetch(set, HEADER_FETCH_QUERY)
            } else {
                session.fetch(set, HEADER_FETCH_QUERY)
            }
        })?;
        Ok(fetches
            .iter()
            .filter_map(|fetch| {
                let uid = fetch.uid?;
                // The crate hands back every FETCH the server sent, including
                // unsolicited ones ("* 7 FETCH (UID 345 FLAGS (\Seen))" when
                // another client changes a flag mid-command). Those carry no
                // header section; taking them for an answer would save a
                // blank message over the real one.
                let header_bytes = fetch.header()?;
                let flags = fetch.flags();
                let mut header = parse_header(
                    uid.to_string(),
                    header_bytes,
                    flags.contains(&Flag::Seen),
                    flags.contains(&Flag::Flagged),
                    flags.iter().any(is_pinned_flag),
                );
                (header.preview, header.body_text) =
                    crate::mime::texts_from_slices(header_bytes, fetch.text().unwrap_or(&[]));
                Some(header)
            })
            .collect())
    }

    /// Fetch one full message (headers + body) by its stable UID.
    pub fn fetch_message(&mut self, uid: &str) -> Result<Vec<u8>> {
        let fetches = self.command(|session| session.uid_fetch(uid, "(BODY.PEEK[])"))?;
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

/// Capability names as `has_capability` looks them up: upper case, with
/// AUTH= mechanisms spelled out.
fn capability_names(capabilities: &::imap::types::Capabilities) -> HashSet<String> {
    capabilities
        .iter()
        .map(|capability| match capability {
            Capability::Imap4rev1 => "IMAP4REV1".to_string(),
            Capability::Auth(name) => format!("AUTH={}", name.to_uppercase()),
            Capability::Atom(name) => name.to_uppercase(),
        })
        .collect()
}

/// Whether a failed command may have left part of its reply unread. Only a
/// tagged NO or BAD ends the exchange cleanly (and a command refused before
/// it was sent never started one); anything else -- a timeout, a dropped
/// connection, a reply the parser choked on, another command's tag -- leaves
/// the stream where the next command would read the wrong answer.
fn leaves_stream_dirty(error: &::imap::Error) -> bool {
    !matches!(
        error,
        ::imap::Error::No(_) | ::imap::Error::Bad(_) | ::imap::Error::Validate(_)
    )
}

/// An IMAP sequence set for a run of UIDs, with consecutive values folded
/// into ranges so a 200-message batch stays a short command line.
pub fn uid_set(uids: &[u32]) -> String {
    uid_ranges(uids).join(",")
}

/// UIDs as sequence sets for as many commands as it takes: folded into
/// ranges like `uid_set`, then split so no set is longer than
/// MAX_UID_SET_LEN. Thousands of scattered UIDs would otherwise make one
/// line a server refuses outright. Empty for no UIDs.
pub fn uid_sets(uids: &[String]) -> Vec<String> {
    let numbers: Vec<u32> = uids.iter().filter_map(|uid| uid.parse().ok()).collect();
    split_uid_ranges(uid_ranges(&numbers), MAX_UID_SET_LEN)
}

/// The UIDs sorted, deduplicated and folded: "1:3", "7", "9:12".
fn uid_ranges(uids: &[u32]) -> Vec<String> {
    let mut sorted = uids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut ranges = Vec::new();
    let mut run: Option<(u32, u32)> = None;
    for uid in sorted {
        match run {
            Some((start, end)) if uid == end + 1 => run = Some((start, uid)),
            Some((start, end)) => {
                ranges.push(range_text(start, end));
                run = Some((uid, uid));
            }
            None => run = Some((uid, uid)),
        }
    }
    if let Some((start, end)) = run {
        ranges.push(range_text(start, end));
    }
    ranges
}

fn range_text(start: u32, end: u32) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}:{end}")
    }
}

/// Join ranges with commas into sets no longer than `max_len` (one range
/// is never split, and none comes near the limit).
fn split_uid_ranges(ranges: Vec<String>, max_len: usize) -> Vec<String> {
    let mut sets: Vec<String> = Vec::new();
    for range in ranges {
        match sets.last_mut() {
            Some(set) if set.len() + 1 + range.len() <= max_len => {
                set.push(',');
                set.push_str(&range);
            }
            _ => sets.push(range),
        }
    }
    sets
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

/// Keywords are atoms the server echoes back however the client cased them.
fn is_pinned_flag(flag: &Flag) -> bool {
    matches!(flag, Flag::Custom(name) if name.eq_ignore_ascii_case(FLAG_PINNED))
}

/// Let mail-parser decode the header block: it handles line folding and the
/// =?utf-8?...?= encoding you'd otherwise see as gibberish.
pub fn parse_header(
    uid: String,
    header_bytes: &[u8],
    is_seen: bool,
    is_flagged: bool,
    is_pinned: bool,
) -> FetchedHeader {
    let parsed = mail_parser::MessageParser::default().parse_headers(header_bytes);
    let header = |name: &str| -> String {
        parsed
            .as_ref()
            .and_then(|message| decoded_header(message, name))
            .unwrap_or_default()
    };
    let raw = |name: &str| -> String {
        parsed
            .as_ref()
            .and_then(|message| message.header_raw(name))
            .map(|text| text.trim().to_string())
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
        is_seen,
        is_flagged,
        is_pinned,
        preview: String::new(),
        body_text: String::new(),
        references: raw("References"),
        in_reply_to: raw("In-Reply-To"),
        thread_index: raw("Thread-Index"),
    }
}

/// A header as display text: addresses re-joined as `Name <addr>` so the
/// address parser sees decoded names, ids wrapped back in angle brackets,
/// everything else as its decoded text.
fn decoded_header(message: &mail_parser::Message, name: &str) -> Option<String> {
    use mail_parser::HeaderValue;
    let value = message.header(name)?;
    // Restore the angle brackets stripped by mail-parser.
    let is_id_header = name == "Message-ID";
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
    fn uid_sets_fold_runs_and_split_long_lines() {
        assert_eq!(uid_set(&[]), "");
        assert_eq!(uid_set(&[5]), "5");
        assert_eq!(uid_set(&[9, 8, 7, 3, 1, 2, 7]), "1:3,7:9");
        assert_eq!(uid_set(&[4, 2]), "2,4");
        let strings = |uids: &[u32]| -> Vec<String> { uids.iter().map(u32::to_string).collect() };
        assert!(uid_sets(&[]).is_empty());
        assert_eq!(uid_sets(&strings(&[3, 1, 2, 10])), ["1:3,10"]);
        assert_eq!(
            split_uid_ranges(uid_ranges(&[1, 2, 3, 10, 12, 20, 21]), 8),
            ["1:3,10", "12,20:21"]
        );
        // Every other UID up to 20000: nothing folds, so it has to split.
        let sparse: Vec<u32> = (1..20_000).step_by(2).collect();
        let sets = uid_sets(&strings(&sparse));
        assert!(sets.len() > 1);
        assert!(sets.iter().all(|set| set.len() <= MAX_UID_SET_LEN));
        let rejoined: Vec<u32> = sets
            .iter()
            .flat_map(|set| set.split(','))
            .map(|uid| uid.parse().unwrap())
            .collect();
        assert_eq!(rejoined, sparse);
        // A contiguous block folds to one short range.
        let block: Vec<u32> = (1..=20_000).collect();
        assert_eq!(uid_sets(&strings(&block)), ["1:20000"]);
    }

    #[test]
    fn parses_fetched_headers() {
        let raw = b"From: =?utf-8?q?Ada_Lovelace?= <ada@example.com>\r\nTo: Bob <bob@x.y>, c@x.y\r\nSubject: =?utf-8?q?Gr=C3=BC=C3=9Fe?=\r\nDate: Wed, 16 Jul 2026 10:00:00 +0000\r\nMessage-ID: <m1@x>\r\nReferences: <a@x>\r\n <b@x>\r\n\r\n";
        let header = parse_header("9".into(), raw, true, false, true);
        assert!(header.is_pinned);
        assert_eq!(header.from_header, "\"Ada Lovelace\" <ada@example.com>");
        assert_eq!(header.to_header, "\"Bob\" <bob@x.y>, c@x.y");
        assert_eq!(header.subject, "Grüße");
        assert_eq!(header.message_id, "<m1@x>");
        assert!(header.date.contains("2026"));
        assert!(header.is_seen);
    }
}
