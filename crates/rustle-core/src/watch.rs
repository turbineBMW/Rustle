//! Push, over IMAP IDLE (RFC 2177): one long-lived session per account sits
//! on the inbox and reports every time the server says it changed. The poll
//! timer stays as the fallback for servers without IDLE and for every other
//! folder.

use crate::folders;
use crate::models::Account;
use crate::net::auth::Credential;
use crate::net::errors::NetError;
use crate::net::imap::ImapSession;
use crate::sync::open_imap;
use log::debug;
use std::net::{Shutdown, TcpStream};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

pub type Result<T> = std::result::Result<T, NetError>;

/// Re-issue IDLE well inside the 29 minutes RFC 2177 allows before a server
/// may drop a silent client.
const IDLE_CYCLE: Duration = Duration::from_secs(20 * 60);

/// The main thread's handle on one watch. Cancelling sets a flag, wakes a
/// sleeping retry, and shuts the socket under a wait that is blocked in a
/// read, so the thread notices at once instead of at the next timeout.
#[derive(Default)]
pub struct InboxWatch {
    cancelled: Mutex<bool>,
    wake: Condvar,
    socket: Mutex<Option<TcpStream>>,
}

/// Why a watch returned without an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchEnd {
    Cancelled,
    /// The server has no IDLE: nothing to wait on, the timer covers it.
    Unsupported,
}

impl InboxWatch {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn cancel(&self) {
        *self.cancelled.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.wake.notify_all();
        if let Some(socket) = self.socket.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }

    pub fn is_cancelled(&self) -> bool {
        *self.cancelled.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Sleep for `duration` unless cancelled first. True when cancelled.
    pub fn wait_cancelled(&self, duration: Duration) -> bool {
        let guard = self.cancelled.lock().unwrap_or_else(|e| e.into_inner());
        let (guard, _) = self
            .wake
            .wait_timeout_while(guard, duration, |cancelled| !*cancelled)
            .unwrap_or_else(|e| e.into_inner());
        *guard
    }

    fn attach(&self, session: Option<&ImapSession>) {
        let socket = session.and_then(ImapSession::socket);
        *self.socket.lock().unwrap_or_else(|e| e.into_inner()) = socket;
        // A cancel that landed between connect and attach must still cut
        // the wait short.
        if self.is_cancelled() {
            self.cancel();
        }
    }
}

/// Hold one session on the account's inbox and call `on_change` whenever
/// the server reports a change, until cancelled or the connection fails.
/// Runs on a blocking thread; the caller reconnects on `Err`.
pub fn watch_inbox(
    account: &Account,
    credential: &Credential,
    watch: &InboxWatch,
    on_change: &mut dyn FnMut(),
) -> Result<WatchEnd> {
    let mut session = open_imap(account, credential)?;
    if !session.has_capability("IDLE") {
        return Ok(WatchEnd::Unsupported);
    }
    watch.attach(Some(&session));
    let result = idle_loop(&mut session, watch, on_change);
    watch.attach(None);
    result
}

fn idle_loop(
    session: &mut ImapSession,
    watch: &InboxWatch,
    on_change: &mut dyn FnMut(),
) -> Result<WatchEnd> {
    let mailboxes = session.list_folders()?;
    let inbox = folders::inbox_name(mailboxes.iter().map(|m| m.name.as_str()));
    session.select(&inbox, false)?;
    while !watch.is_cancelled() {
        match session.idle(IDLE_CYCLE) {
            Ok(true) => on_change(),
            Ok(false) => debug!("re-issuing IDLE on {inbox}"),
            // The read that failed was cut short on purpose.
            Err(_) if watch.is_cancelled() => break,
            Err(error) => return Err(error),
        }
    }
    Ok(WatchEnd::Cancelled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Instant;

    #[test]
    fn cancel_wakes_a_sleeping_wait() {
        let watch = InboxWatch::new();
        let sleeper = watch.clone();
        let handle = thread::spawn(move || {
            let started = Instant::now();
            let cancelled = sleeper.wait_cancelled(Duration::from_secs(30));
            (cancelled, started.elapsed())
        });
        thread::sleep(Duration::from_millis(50));
        watch.cancel();
        let (cancelled, elapsed) = handle.join().unwrap();
        assert!(cancelled);
        assert!(elapsed < Duration::from_secs(5));
        assert!(watch.is_cancelled());
    }

    #[test]
    fn an_uncancelled_wait_runs_out() {
        let watch = InboxWatch::new();
        assert!(!watch.wait_cancelled(Duration::from_millis(20)));
        assert!(!watch.is_cancelled());
    }

    #[test]
    fn cancelling_shuts_the_attached_socket() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        let watch = InboxWatch::new();
        *watch.socket.lock().unwrap() = Some(client.try_clone().unwrap());
        watch.cancel();
        // The peer sees the shutdown as end of stream.
        let mut buffer = [0u8; 1];
        assert_eq!(std::io::Read::read(&mut &server, &mut buffer).unwrap(), 0);
    }
}
