//! Signed-in IMAP sessions kept for a few minutes after a job finishes, so
//! the next job on the same account (opening a message, a flag change, the
//! poll) skips the TCP, TLS and sign-in round trips. A session that sat for a
//! while is checked with NOOP before reuse; one that sat too long, or any
//! session after a suspend, is dropped without a word to the server.

use super::auth::Credential;
use super::imap::ImapSession;
use crate::models::Account;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Older than this, a parked session is not worth trusting: NAT tables and
/// servers forget quiet connections, and a dead one costs a full timeout.
pub const MAX_IDLE: Duration = Duration::from_secs(4 * 60);
/// Parked longer than this, a session is pinged before it's handed out.
pub const CHECK_AFTER: Duration = Duration::from_secs(20);
/// Sessions kept per account. Jobs run in parallel, but rarely many at once.
const MAX_PER_KEY: usize = 3;

/// Parked items by key, each with when it was parked.
pub struct Pool<T> {
    parked: Mutex<HashMap<String, Vec<(T, Instant)>>>,
}

impl<T> Default for Pool<T> {
    fn default() -> Self {
        Self {
            parked: Mutex::new(HashMap::new()),
        }
    }
}

impl<T> Pool<T> {
    /// The most recently parked item for `key` and how long it sat, plus
    /// every item for `key` too old to use, which the caller disposes of
    /// outside the lock.
    pub fn take(&self, key: &str, now: Instant) -> (Option<(T, Duration)>, Vec<T>) {
        let mut parked = self.parked.lock().unwrap_or_else(|e| e.into_inner());
        let Some(items) = parked.get_mut(key) else {
            return (None, Vec::new());
        };
        let mut expired = Vec::new();
        let mut fresh = Vec::new();
        for (item, since) in items.drain(..) {
            if now.saturating_duration_since(since) > MAX_IDLE {
                expired.push(item);
            } else {
                fresh.push((item, since));
            }
        }
        let taken = fresh
            .pop()
            .map(|(item, since)| (item, now.saturating_duration_since(since)));
        *items = fresh;
        (taken, expired)
    }

    /// Park an item. Returns the one pushed out when the key is full.
    pub fn put(&self, key: &str, item: T, now: Instant) -> Option<T> {
        let mut parked = self.parked.lock().unwrap_or_else(|e| e.into_inner());
        let items = parked.entry(key.to_string()).or_default();
        items.push((item, now));
        if items.len() > MAX_PER_KEY {
            return Some(items.remove(0).0);
        }
        None
    }

    /// Everything parked, for the caller to dispose of.
    pub fn drain(&self) -> Vec<T> {
        let mut parked = self.parked.lock().unwrap_or_else(|e| e.into_inner());
        parked
            .drain()
            .flat_map(|(_, items)| items.into_iter().map(|(item, _)| item))
            .collect()
    }
}

static SESSIONS: LazyLock<Pool<ImapSession>> = LazyLock::new(Pool::default);

/// Sessions are only shared between jobs that would sign in the same way.
fn key(account: &Account, credential: &Credential) -> String {
    format!(
        "{}|{}|{}|{}|{:?}",
        account.id, credential.user, account.imap_host, account.imap_port, account.imap_security
    )
}

/// A parked session for this account that still answers, if there is one.
pub(crate) fn checkout(account: &Account, credential: &Credential) -> Option<ImapSession> {
    let key = key(account, credential);
    loop {
        let (taken, expired) = SESSIONS.take(&key, Instant::now());
        expired.into_iter().for_each(ImapSession::discard);
        let (mut session, idle) = taken?;
        if idle < CHECK_AFTER || session.noop().is_ok() {
            log::debug!(
                "reusing a session to {} parked {}s ago",
                account.imap_host,
                idle.as_secs()
            );
            return Some(session);
        }
        session.discard();
    }
}

/// Park a session that finished its job cleanly, for the next one.
pub(crate) fn checkin(account: &Account, credential: &Credential, mut session: ImapSession) {
    session.drain_changes();
    if let Some(mut surplus) = SESSIONS.put(&key(account, credential), session, Instant::now()) {
        surplus.logout();
    }
}

/// Drop every parked session: after a suspend or a network change their
/// sockets are dead, and finding out one at a time costs a timeout each.
pub fn forget_all() {
    SESSIONS.drain().into_iter().for_each(ImapSession::discard);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hands_back_the_newest_and_drops_the_stale() {
        let pool = Pool::default();
        let start = Instant::now();
        pool.put("a", 1, start);
        pool.put("a", 2, start + MAX_IDLE);
        pool.put("b", 3, start);
        let later = start + MAX_IDLE + Duration::from_secs(1);
        let (taken, expired) = pool.take("a", later);
        assert_eq!(taken, Some((2, Duration::from_secs(1))));
        assert_eq!(expired, vec![1]);
        assert_eq!(pool.take("a", later), (None, Vec::new()));
        assert_eq!(pool.drain(), vec![3]);
    }

    #[test]
    fn a_full_key_pushes_out_the_oldest() {
        let pool = Pool::default();
        let now = Instant::now();
        for item in 0..MAX_PER_KEY {
            assert_eq!(pool.put("a", item, now), None);
        }
        assert_eq!(pool.put("a", 99, now), Some(0));
    }
}
