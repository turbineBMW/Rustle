//! All SQLite access. One connection, main thread only: the worker threads
//! do network and hand results back here.

use crate::assistant::SearchFilter;
use crate::eds;
use crate::folders;
use crate::models::{
    is_hex_color, Account, Auth, Email, Folder, MessageHeader, NewAccount, Security,
};
use crate::queue::{Change, PendingOp};
use rusqlite::{params, Connection, OptionalExtension, Row};
use std::collections::HashSet;
use std::path::Path;

pub type Result<T> = std::result::Result<T, rusqlite::Error>;

/// Every column `email_from_row` reads.
const EMAIL_COLUMNS: &str = "id, folder_id, server_id, sender, sender_address, recipient, \
    recipient_address, subject, preview, date, unread, starred, message_id, pinned";

/// Schema changes since the first release, applied in order. How many have run
/// is stored in PRAGMA user_version. Only ever append -- editing or reordering
/// these would give databases in the wild a different schema to new ones.
const MIGRATIONS: &[&str] = &[
    "ALTER TABLE folders ADD COLUMN parent_id INTEGER REFERENCES folders(id)",
    "ALTER TABLE folders ADD COLUMN delimiter TEXT NOT NULL DEFAULT '/'",
    "ALTER TABLE emails ADD COLUMN sender_address TEXT NOT NULL DEFAULT '';
     UPDATE emails SET sender_address = COALESCE(
        (SELECT address FROM contacts WHERE contacts.name = emails.sender), '');",
    "ALTER TABLE emails ADD COLUMN recipient TEXT NOT NULL DEFAULT '';
     ALTER TABLE emails ADD COLUMN recipient_address TEXT NOT NULL DEFAULT '';",
    "ALTER TABLE accounts ADD COLUMN goa_id TEXT NOT NULL DEFAULT ''",
    "INSERT INTO emails_fts(emails_fts) VALUES ('rebuild')",
    "ALTER TABLE accounts ADD COLUMN color TEXT NOT NULL DEFAULT ''",
    "ALTER TABLE accounts ADD COLUMN signature TEXT NOT NULL DEFAULT ''",
    "ALTER TABLE accounts ADD COLUMN label TEXT NOT NULL DEFAULT ''",
    "ALTER TABLE accounts ADD COLUMN notification_sound TEXT NOT NULL DEFAULT ''",
    "ALTER TABLE emails ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE emails DROP COLUMN conversation_id;
     ALTER TABLE emails DROP COLUMN in_reply_to;
     ALTER TABLE emails DROP COLUMN reference_ids",
    "ALTER TABLE accounts ADD COLUMN eds_uid TEXT NOT NULL DEFAULT '';
     ALTER TABLE accounts ADD COLUMN eds_smtp_uid TEXT NOT NULL DEFAULT '';
     ALTER TABLE accounts ADD COLUMN eds_root_uid TEXT NOT NULL DEFAULT '';
     ALTER TABLE accounts ADD COLUMN imap_user TEXT NOT NULL DEFAULT '';
     ALTER TABLE accounts ADD COLUMN imap_auth TEXT NOT NULL DEFAULT 'password';
     ALTER TABLE accounts ADD COLUMN smtp_user TEXT NOT NULL DEFAULT '';
     ALTER TABLE accounts ADD COLUMN smtp_auth TEXT NOT NULL DEFAULT 'password';
     ALTER TABLE accounts ADD COLUMN hidden INTEGER NOT NULL DEFAULT 0;
     UPDATE accounts SET imap_auth = 'oauth2', smtp_auth = 'oauth2' WHERE goa_id <> '';",
    "ALTER TABLE accounts ADD COLUMN picture TEXT NOT NULL DEFAULT ''",
    "CREATE TABLE pending_ops (
        id INTEGER PRIMARY KEY,
        account_id INTEGER NOT NULL,
        folder_id INTEGER NOT NULL,
        folder TEXT NOT NULL,
        kind TEXT NOT NULL,
        uids TEXT NOT NULL,
        email_ids TEXT NOT NULL DEFAULT '',
        flag TEXT NOT NULL DEFAULT '',
        is_add INTEGER NOT NULL DEFAULT 0,
        dest_id INTEGER NOT NULL DEFAULT 0,
        dest TEXT NOT NULL DEFAULT ''
     )",
    // Search reaches into bodies (as far as the database has them) and every
    // recipient. FTS5 can't add a column, so the index is rebuilt.
    "ALTER TABLE emails ADD COLUMN body_text TEXT NOT NULL DEFAULT '';
     ALTER TABLE emails ADD COLUMN recipients TEXT NOT NULL DEFAULT '';
     DROP TRIGGER IF EXISTS emails_fts_insert;
     DROP TRIGGER IF EXISTS emails_fts_delete;
     DROP TRIGGER IF EXISTS emails_fts_update;
     DROP TABLE IF EXISTS emails_fts;
     CREATE VIRTUAL TABLE emails_fts USING fts5(
        sender, subject, preview, body_text, content='emails', content_rowid='id'
     );
     CREATE TRIGGER emails_fts_insert AFTER INSERT ON emails BEGIN
        INSERT INTO emails_fts(rowid, sender, subject, preview, body_text)
        VALUES (new.id, new.sender, new.subject, new.preview, new.body_text);
     END;
     CREATE TRIGGER emails_fts_delete AFTER DELETE ON emails BEGIN
        INSERT INTO emails_fts(emails_fts, rowid, sender, subject, preview, body_text)
        VALUES ('delete', old.id, old.sender, old.subject, old.preview, old.body_text);
     END;
     CREATE TRIGGER emails_fts_update AFTER UPDATE OF sender, subject, preview, body_text
     ON emails BEGIN
        INSERT INTO emails_fts(emails_fts, rowid, sender, subject, preview, body_text)
        VALUES ('delete', old.id, old.sender, old.subject, old.preview, old.body_text);
        INSERT INTO emails_fts(rowid, sender, subject, preview, body_text)
        VALUES (new.id, new.sender, new.subject, new.preview, new.body_text);
     END;
     INSERT INTO emails_fts(emails_fts) VALUES ('rebuild');",
    "ALTER TABLE contacts ADD COLUMN sent_count INTEGER NOT NULL DEFAULT 0",
];

/// `accounts.hidden`: shown, removed by the user (EDS still has it), or
/// gone from EDS (kept so its mail returns if the account does).
const VISIBLE: i64 = 0;
const HIDDEN_BY_USER: i64 = 1;
const MISSING_FROM_EDS: i64 = 2;

/// What `reconcile_eds` changed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Reconciled {
    /// Accounts EDS has that Rustle didn't; they need a first sync.
    pub added: Vec<i64>,
    /// Anything about the visible accounts changed.
    pub is_changed: bool,
}

/// "Name <address>", or the bare address without a name.
pub fn contact_label(name: &str, address: &str) -> String {
    if name.is_empty() {
        address.to_string()
    } else {
        format!("{name} <{address}>")
    }
}

/// Search terms as a safe FTS5 query: each term a prefix match, a term with
/// spaces in it a phrase.
fn fts_terms(terms: &[String]) -> String {
    terms
        .iter()
        .map(|term| term.trim())
        .filter(|term| !term.is_empty())
        .map(|term| format!("\"{}\"*", term.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        // A sync commits once per message it stores, and the default journal
        // fsyncs on every one of those. Under WAL a commit is an append, and
        // NORMAL leaves the fsync to the checkpoint.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let db = Database { conn };
        db.create_tables()?;
        db.migrate_schema()?;
        Ok(db)
    }

    fn create_tables(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS accounts (
                id INTEGER PRIMARY KEY,
                email TEXT NOT NULL,
                display_name TEXT NOT NULL,
                imap_host TEXT NOT NULL,
                imap_port INTEGER NOT NULL,
                smtp_host TEXT NOT NULL,
                smtp_port INTEGER NOT NULL,
                imap_security TEXT NOT NULL DEFAULT 'tls',
                smtp_security TEXT NOT NULL DEFAULT 'starttls'
            );
            CREATE TABLE IF NOT EXISTS folders (
                id INTEGER PRIMARY KEY,
                account_id INTEGER NOT NULL REFERENCES accounts(id),
                name TEXT NOT NULL,
                icon_name TEXT NOT NULL DEFAULT 'folder-symbolic'
            );
            CREATE TABLE IF NOT EXISTS emails (
                id INTEGER PRIMARY KEY,
                folder_id INTEGER NOT NULL REFERENCES folders(id),
                server_id TEXT,
                sender TEXT NOT NULL,
                subject TEXT NOT NULL,
                preview TEXT NOT NULL,
                date TEXT NOT NULL,
                unread INTEGER NOT NULL DEFAULT 1,
                starred INTEGER NOT NULL DEFAULT 0,
                raw_message BLOB,
                message_id TEXT,
                in_reply_to TEXT,
                reference_ids TEXT,
                conversation_id INTEGER
            );
            CREATE UNIQUE INDEX IF NOT EXISTS idx_emails_uid ON emails (folder_id, server_id);
            CREATE INDEX IF NOT EXISTS idx_emails_unread ON emails (folder_id, unread);
            CREATE TABLE IF NOT EXISTS contacts (
                address TEXT PRIMARY KEY,
                name TEXT NOT NULL DEFAULT ''
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS emails_fts USING fts5(
                sender, subject, preview, content='emails', content_rowid='id'
            );
            CREATE TRIGGER IF NOT EXISTS emails_fts_insert AFTER INSERT ON emails BEGIN
                INSERT INTO emails_fts(rowid, sender, subject, preview)
                VALUES (new.id, new.sender, new.subject, new.preview);
            END;
            CREATE TRIGGER IF NOT EXISTS emails_fts_delete AFTER DELETE ON emails BEGIN
                INSERT INTO emails_fts(emails_fts, rowid, sender, subject, preview)
                VALUES ('delete', old.id, old.sender, old.subject, old.preview);
            END;
            CREATE TRIGGER IF NOT EXISTS emails_fts_update AFTER UPDATE OF sender, subject, preview
            ON emails BEGIN
                INSERT INTO emails_fts(emails_fts, rowid, sender, subject, preview)
                VALUES ('delete', old.id, old.sender, old.subject, old.preview);
                INSERT INTO emails_fts(rowid, sender, subject, preview)
                VALUES (new.id, new.sender, new.subject, new.preview);
            END;",
        )
    }

    fn migrate_schema(&self) -> Result<()> {
        let version: i64 = self
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        let version = version.max(0) as usize;
        let adds_body_text = MIGRATIONS
            .iter()
            .position(|sql| sql.contains("ADD COLUMN body_text"))
            .is_some_and(|index| index >= version);
        for (index, sql) in MIGRATIONS.iter().enumerate().skip(version) {
            // Keep multi-column removals and their version marker atomic.
            let transaction = self.conn.unchecked_transaction()?;
            transaction.execute_batch(sql)?;
            transaction.pragma_update(None, "user_version", (index + 1) as i64)?;
            transaction.commit()?;
        }
        if adds_body_text {
            self.fill_body_text()?;
        }
        Ok(())
    }

    /// Index the bodies already downloaded, once, when `body_text` arrives.
    fn fill_body_text(&self) -> Result<()> {
        let rows: Vec<(i64, Vec<u8>)> = {
            let mut statement = self
                .conn
                .prepare("SELECT id, raw_message FROM emails WHERE raw_message IS NOT NULL")?;
            let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
            rows.collect::<Result<_>>()?
        };
        let transaction = self.conn.unchecked_transaction()?;
        for (id, raw) in rows {
            let text = crate::mime::search_text(&crate::mime::parse_message(&raw));
            transaction.execute(
                "UPDATE emails SET body_text = ?1 WHERE id = ?2",
                params![text, id],
            )?;
        }
        transaction.commit()
    }

    // --- accounts ---------------------------------------------------------

    fn account_from_row(row: &Row) -> Result<Account> {
        Ok(Account {
            id: row.get("id")?,
            email: row.get("email")?,
            display_name: row.get("display_name")?,
            imap_host: row.get("imap_host")?,
            imap_port: row.get::<_, i64>("imap_port")? as u16,
            imap_security: Security::parse(&row.get::<_, String>("imap_security")?),
            smtp_host: row.get("smtp_host")?,
            smtp_port: row.get::<_, i64>("smtp_port")? as u16,
            smtp_security: Security::parse(&row.get::<_, String>("smtp_security")?),
            goa_id: row.get("goa_id")?,
            eds_uid: row.get("eds_uid")?,
            eds_smtp_uid: row.get("eds_smtp_uid")?,
            eds_root_uid: row.get("eds_root_uid")?,
            imap_user: row.get("imap_user")?,
            imap_auth: Auth::parse(&row.get::<_, String>("imap_auth")?),
            smtp_user: row.get("smtp_user")?,
            smtp_auth: Auth::parse(&row.get::<_, String>("smtp_auth")?),
            color: row.get("color")?,
            signature: row.get("signature")?,
            label: row.get("label")?,
            notification_sound: row.get("notification_sound")?,
            picture: row.get("picture")?,
        })
    }

    pub fn accounts(&self) -> Result<Vec<Account>> {
        self.accounts_where(VISIBLE)
    }

    /// Accounts the user removed from Rustle that EDS still has, so they
    /// can be shown again.
    pub fn hidden_accounts(&self) -> Result<Vec<Account>> {
        self.accounts_where(HIDDEN_BY_USER)
    }

    fn accounts_where(&self, hidden: i64) -> Result<Vec<Account>> {
        let mut statement = self
            .conn
            .prepare("SELECT * FROM accounts WHERE hidden = ?1 ORDER BY id")?;
        let rows = statement.query_map([hidden], Self::account_from_row)?;
        rows.collect()
    }

    /// Accounts still set up the way Rustle kept them before EDS; they are
    /// moved there once and then read from it like any other.
    pub fn accounts_outside_eds(&self) -> Result<Vec<Account>> {
        Ok(self
            .accounts()?
            .into_iter()
            .filter(|account| account.eds_uid.is_empty())
            .collect())
    }

    pub fn account(&self, account_id: i64) -> Result<Option<Account>> {
        self.conn
            .query_row(
                "SELECT * FROM accounts WHERE id = ?1",
                [account_id],
                Self::account_from_row,
            )
            .optional()
    }

    pub fn save_account(&self, account: &NewAccount) -> Result<Account> {
        self.conn.execute(
            "INSERT INTO accounts (email, display_name, imap_host, imap_port, smtp_host, smtp_port,
                imap_security, smtp_security, goa_id, eds_uid, eds_smtp_uid, eds_root_uid,
                imap_user, imap_auth, smtp_user, smtp_auth)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                account.email,
                account.display_name,
                account.imap_host,
                account.imap_port,
                account.smtp_host,
                account.smtp_port,
                account.imap_security.as_str(),
                account.smtp_security.as_str(),
                account.goa_id,
                account.eds_uid,
                account.eds_smtp_uid,
                account.eds_root_uid,
                account.imap_user,
                account.imap_auth.as_str(),
                account.smtp_user,
                account.smtp_auth.as_str(),
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        Ok(self.account(id)?.expect("the row was just inserted"))
    }

    /// Bring the accounts in line with Evolution Data Server's. An EDS account
    /// is matched to a row by its source, else (once, when moving to EDS) by
    /// its Online Accounts id, else by address and IMAP server; a match keeps
    /// its mail, colour, label and signature and takes EDS's servers and
    /// sign-in. Unmatched EDS accounts are added. Rows whose source is gone
    /// are set aside with their mail rather than deleted, so a registry that
    /// is briefly away costs nothing. Only call with a list EDS returned.
    pub fn reconcile_eds(&mut self, accounts: &[eds::MailAccount]) -> Result<Reconciled> {
        let transaction = self.conn.transaction()?;
        let rows: Vec<(Account, i64)> = {
            let mut statement = transaction.prepare("SELECT * FROM accounts ORDER BY id")?;
            let rows = statement.query_map([], |row| {
                Ok((Self::account_from_row(row)?, row.get::<_, i64>("hidden")?))
            })?;
            rows.collect::<Result<_>>()?
        };
        let mut used: HashSet<i64> = HashSet::new();
        let mut reconciled = Reconciled::default();
        for found in accounts {
            let unlinked =
                |row: &&(Account, i64)| row.0.eds_uid.is_empty() && !used.contains(&row.0.id);
            let matched = rows
                .iter()
                .find(|(row, _)| row.eds_uid == found.uid)
                .or_else(|| {
                    rows.iter()
                        .filter(unlinked)
                        .find(|(row, _)| !found.goa_id.is_empty() && row.goa_id == found.goa_id)
                })
                .or_else(|| {
                    rows.iter().filter(unlinked).find(|(row, _)| {
                        row.email.eq_ignore_ascii_case(&found.email)
                            && row.imap_host.eq_ignore_ascii_case(&found.imap.host)
                    })
                });
            let wanted = NewAccount::from(found);
            match matched {
                Some((row, hidden)) => {
                    used.insert(row.id);
                    let was_visible = *hidden == VISIBLE;
                    let hidden = if *hidden == MISSING_FROM_EDS {
                        VISIBLE
                    } else {
                        *hidden
                    };
                    let is_same =
                        NewAccount::from_row(row) == wanted && hidden == VISIBLE && was_visible;
                    transaction.execute(
                        "UPDATE accounts SET email = ?2, display_name = ?3, imap_host = ?4,
                            imap_port = ?5, smtp_host = ?6, smtp_port = ?7, imap_security = ?8,
                            smtp_security = ?9, goa_id = ?10, eds_uid = ?11, eds_smtp_uid = ?12,
                            eds_root_uid = ?13, imap_user = ?14, imap_auth = ?15,
                            smtp_user = ?16, smtp_auth = ?17, hidden = ?18
                         WHERE id = ?1",
                        params![
                            row.id,
                            wanted.email,
                            wanted.display_name,
                            wanted.imap_host,
                            wanted.imap_port,
                            wanted.smtp_host,
                            wanted.smtp_port,
                            wanted.imap_security.as_str(),
                            wanted.smtp_security.as_str(),
                            wanted.goa_id,
                            wanted.eds_uid,
                            wanted.eds_smtp_uid,
                            wanted.eds_root_uid,
                            wanted.imap_user,
                            wanted.imap_auth.as_str(),
                            wanted.smtp_user,
                            wanted.smtp_auth.as_str(),
                            hidden,
                        ],
                    )?;
                    if hidden == VISIBLE && !is_same {
                        reconciled.is_changed = true;
                        if !was_visible {
                            reconciled.added.push(row.id);
                        }
                    }
                }
                None => {
                    transaction.execute(
                        "INSERT INTO accounts (email, display_name, imap_host, imap_port,
                            smtp_host, smtp_port, imap_security, smtp_security, goa_id, eds_uid,
                            eds_smtp_uid, eds_root_uid, imap_user, imap_auth, smtp_user,
                            smtp_auth, label)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                            ?15, ?16, ?17)",
                        params![
                            wanted.email,
                            wanted.display_name,
                            wanted.imap_host,
                            wanted.imap_port,
                            wanted.smtp_host,
                            wanted.smtp_port,
                            wanted.imap_security.as_str(),
                            wanted.smtp_security.as_str(),
                            wanted.goa_id,
                            wanted.eds_uid,
                            wanted.eds_smtp_uid,
                            wanted.eds_root_uid,
                            wanted.imap_user,
                            wanted.imap_auth.as_str(),
                            wanted.smtp_user,
                            wanted.smtp_auth.as_str(),
                            // EDS names Online Accounts ones ("Gmail"); its
                            // default is the address, which needs no label.
                            if found.label.eq_ignore_ascii_case(&found.email) {
                                ""
                            } else {
                                found.label.as_str()
                            },
                        ],
                    )?;
                    reconciled.added.push(transaction.last_insert_rowid());
                    reconciled.is_changed = true;
                }
            }
        }
        for (row, hidden) in &rows {
            // An Online Accounts row EDS has no mail account for is gone (or
            // has mail off); a password row not moved to EDS yet is left be.
            let is_known_to_eds = !row.eds_uid.is_empty() || !row.goa_id.is_empty();
            if is_known_to_eds && !used.contains(&row.id) && *hidden == VISIBLE {
                transaction.execute(
                    "UPDATE accounts SET hidden = ?2 WHERE id = ?1",
                    params![row.id, MISSING_FROM_EDS],
                )?;
                reconciled.is_changed = true;
            }
        }
        transaction.commit()?;
        Ok(reconciled)
    }

    /// Take an account out of Rustle while EDS keeps it: its mail goes, the
    /// row stays so the account isn't added straight back.
    pub fn hide_account(&mut self, account_id: i64) -> Result<()> {
        let transaction = self.conn.transaction()?;
        Self::delete_mail(&transaction, account_id)?;
        transaction.execute(
            "UPDATE accounts SET hidden = ?2 WHERE id = ?1",
            params![account_id, HIDDEN_BY_USER],
        )?;
        transaction.commit()
    }

    /// Bring back an account the user hid; it syncs from scratch.
    pub fn show_account(&self, account_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE accounts SET hidden = ?2 WHERE id = ?1",
            params![account_id, VISIBLE],
        )?;
        Ok(())
    }

    /// Stores the colour that marks this account's mail; anything that isn't
    /// `#rrggbb` clears it back to the palette default.
    pub fn set_account_color(&self, account_id: i64, color: &str) -> Result<()> {
        let color = if is_hex_color(color) { color } else { "" };
        self.conn.execute(
            "UPDATE accounts SET color = ?1 WHERE id = ?2",
            params![color, account_id],
        )?;
        Ok(())
    }

    /// Stores the file name of the account's picture; "" clears it. The
    /// file itself is the caller's to write and remove.
    pub fn set_account_picture(&self, account_id: i64, picture: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE accounts SET picture = ?1 WHERE id = ?2",
            params![picture, account_id],
        )?;
        Ok(())
    }

    pub fn set_account_signature(&self, account_id: i64, signature: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE accounts SET signature = ?1 WHERE id = ?2",
            params![signature, account_id],
        )?;
        Ok(())
    }

    pub fn set_account_label(&self, account_id: i64, label: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE accounts SET label = ?1 WHERE id = ?2",
            params![label.trim(), account_id],
        )?;
        Ok(())
    }

    /// Stores the new-mail sound choice in its setting form (see
    /// `sounds::NotificationSound::as_setting`); "" inherits the app default.
    pub fn set_account_notification_sound(&self, account_id: i64, sound: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE accounts SET notification_sound = ?1 WHERE id = ?2",
            params![sound.trim(), account_id],
        )?;
        Ok(())
    }

    pub fn delete_account(&mut self, account_id: i64) -> Result<()> {
        let transaction = self.conn.transaction()?;
        Self::delete_mail(&transaction, account_id)?;
        transaction.execute("DELETE FROM accounts WHERE id = ?1", [account_id])?;
        transaction.commit()
    }

    fn delete_mail(transaction: &rusqlite::Transaction, account_id: i64) -> Result<()> {
        // Flatten the tree first: the parent_id FK rejects deleting a parent
        // while a child still points at it.
        transaction.execute(
            "UPDATE folders SET parent_id = NULL WHERE account_id = ?1",
            [account_id],
        )?;
        transaction.execute(
            "DELETE FROM emails WHERE folder_id IN (SELECT id FROM folders WHERE account_id = ?1)",
            [account_id],
        )?;
        transaction.execute("DELETE FROM folders WHERE account_id = ?1", [account_id])?;
        transaction.execute(
            "DELETE FROM pending_ops WHERE account_id = ?1",
            [account_id],
        )?;
        Ok(())
    }

    // --- folders ----------------------------------------------------------

    fn folder_from_row(row: &Row) -> Result<Folder> {
        Ok(Folder {
            id: row.get("id")?,
            account_id: row.get("account_id")?,
            name: row.get("name")?,
            icon_name: row.get("icon_name")?,
            parent_id: row.get("parent_id")?,
            delimiter: row.get("delimiter")?,
        })
    }

    pub fn folders_for_account(&self, account_id: i64) -> Result<Vec<Folder>> {
        let mut statement = self
            .conn
            .prepare("SELECT * FROM folders WHERE account_id = ?1 ORDER BY id")?;
        let rows = statement.query_map([account_id], Self::folder_from_row)?;
        rows.collect()
    }

    pub fn all_folders(&self) -> Result<Vec<Folder>> {
        let mut statement = self
            .conn
            .prepare("SELECT * FROM folders ORDER BY account_id, id")?;
        let rows = statement.query_map([], Self::folder_from_row)?;
        rows.collect()
    }

    pub fn folder(&self, folder_id: i64) -> Result<Option<Folder>> {
        self.conn
            .query_row(
                "SELECT * FROM folders WHERE id = ?1",
                [folder_id],
                Self::folder_from_row,
            )
            .optional()
    }

    pub fn folder_by_name(&self, account_id: i64, name: &str) -> Result<Option<Folder>> {
        self.conn
            .query_row(
                "SELECT * FROM folders WHERE account_id = ?1 AND name = ?2",
                params![account_id, name],
                Self::folder_from_row,
            )
            .optional()
    }

    pub fn get_or_create_folder(
        &self,
        account_id: i64,
        name: &str,
        icon_name: &str,
    ) -> Result<Folder> {
        if let Some(existing) = self.folder_by_name(account_id, name)? {
            return Ok(existing);
        }
        self.conn.execute(
            "INSERT INTO folders (account_id, name, icon_name) VALUES (?1, ?2, ?3)",
            params![account_id, name, icon_name],
        )?;
        Ok(Folder {
            id: self.conn.last_insert_rowid(),
            account_id,
            name: name.to_string(),
            icon_name: icon_name.to_string(),
            parent_id: None,
            delimiter: "/".to_string(),
        })
    }

    /// Only the sync knows a folder's real place in the server's hierarchy.
    pub fn set_folder_parent(
        &self,
        folder_id: i64,
        parent_id: Option<i64>,
        delimiter: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE folders SET parent_id = ?1, delimiter = ?2 WHERE id = ?3",
            params![parent_id, delimiter, folder_id],
        )?;
        Ok(())
    }

    fn delete_folder_tree(&self, folder_id: i64) -> Result<()> {
        // Deepest first, for the same FK reason as delete_account.
        let children: Vec<i64> = {
            let mut statement = self
                .conn
                .prepare("SELECT id FROM folders WHERE parent_id = ?1")?;
            let rows = statement.query_map([folder_id], |row| row.get(0))?;
            rows.collect::<Result<_>>()?
        };
        for child in children {
            self.delete_folder_tree(child)?;
        }
        self.conn
            .execute("DELETE FROM emails WHERE folder_id = ?1", [folder_id])?;
        self.conn
            .execute("DELETE FROM folders WHERE id = ?1", [folder_id])?;
        Ok(())
    }

    /// Delete an account's folders (and their emails) whose names aren't in
    /// `keep_names`, mirroring the server's folder list.
    pub fn prune_folders(&self, account_id: i64, keep_names: &HashSet<String>) -> Result<()> {
        let stale: Vec<i64> = {
            let mut statement = self
                .conn
                .prepare("SELECT id, name FROM folders WHERE account_id = ?1")?;
            let rows = statement.query_map([account_id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<Result<Vec<_>>>()?
                .into_iter()
                .filter(|(_, name)| !keep_names.contains(name))
                .map(|(id, _)| id)
                .collect()
        };
        for folder_id in stale {
            // Already gone as a descendant of an earlier pruned folder.
            if self.folder(folder_id)?.is_some() {
                self.delete_folder_tree(folder_id)?;
            }
        }
        Ok(())
    }

    // --- emails -----------------------------------------------------------

    fn email_from_row(row: &Row) -> Result<Email> {
        Ok(Email {
            id: row.get("id")?,
            folder_id: row.get("folder_id")?,
            server_id: row.get("server_id")?,
            sender: row.get("sender")?,
            sender_address: row
                .get::<_, Option<String>>("sender_address")?
                .unwrap_or_default(),
            recipient: row
                .get::<_, Option<String>>("recipient")?
                .unwrap_or_default(),
            recipient_address: row
                .get::<_, Option<String>>("recipient_address")?
                .unwrap_or_default(),
            subject: row.get("subject")?,
            preview: row.get("preview")?,
            date: row.get("date")?,
            is_unread: row.get::<_, i64>("unread")? != 0,
            is_starred: row.get::<_, i64>("starred")? != 0,
            is_pinned: row.get::<_, i64>("pinned")? != 0,
            message_id: row
                .get::<_, Option<String>>("message_id")?
                .unwrap_or_default(),
        })
    }

    pub fn emails_in_folder(&self, folder_id: i64) -> Result<Vec<Email>> {
        let sql =
            format!("SELECT {EMAIL_COLUMNS} FROM emails WHERE folder_id = ?1 ORDER BY id DESC");
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map([folder_id], Self::email_from_row)?;
        rows.collect()
    }

    /// The server UIDs stored for a folder: what a backfill compares against
    /// the server's own list.
    pub fn uids_in_folder(&self, folder_id: i64) -> Result<HashSet<String>> {
        let mut statement = self.conn.prepare(
            "SELECT server_id FROM emails WHERE folder_id = ?1 AND server_id IS NOT NULL AND server_id != ''",
        )?;
        let rows = statement.query_map([folder_id], |row| row.get::<_, String>(0))?;
        rows.collect()
    }

    pub fn email(&self, email_id: i64) -> Result<Option<Email>> {
        let sql = format!("SELECT {EMAIL_COLUMNS} FROM emails WHERE id = ?1");
        self.conn
            .query_row(&sql, [email_id], Self::email_from_row)
            .optional()
    }

    /// Remove server-backed emails no longer present in the authoritative set.
    pub fn prune_stale_emails(
        &mut self,
        folder_id: i64,
        active_uids: &HashSet<String>,
    ) -> Result<usize> {
        let transaction = self.conn.transaction()?;
        transaction.execute_batch("CREATE TEMP TABLE prune_active_uids (uid TEXT PRIMARY KEY)")?;
        let removed = (|| {
            {
                let mut insert =
                    transaction.prepare("INSERT INTO prune_active_uids (uid) VALUES (?1)")?;
                for uid in active_uids {
                    insert.execute([uid])?;
                }
            }
            transaction.execute(
                "DELETE FROM emails WHERE folder_id = ?1 AND server_id IS NOT NULL AND server_id != ''
                 AND NOT EXISTS (SELECT 1 FROM prune_active_uids WHERE prune_active_uids.uid = emails.server_id)",
                [folder_id],
            )
        })();
        transaction.execute_batch("DROP TABLE prune_active_uids")?;
        let removed = removed?;
        transaction.commit()?;
        Ok(removed)
    }

    /// Individual emails across folders, pinned first and then newest first.
    pub fn emails_in_folders(&self, folder_ids: &[i64]) -> Result<Vec<Email>> {
        if folder_ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; folder_ids.len()].join(", ");
        let sql = format!("SELECT {EMAIL_COLUMNS} FROM emails WHERE folder_id IN ({placeholders})");
        let mut statement = self.conn.prepare(&sql)?;
        let rows =
            statement.query_map(rusqlite::params_from_iter(folder_ids), Self::email_from_row)?;
        let emails = rows.collect::<Result<Vec<_>>>()?;
        Ok(Self::sort_emails(emails))
    }

    /// Full-text search returns only the individual messages that match.
    /// What's typed in the search box (`query::parse`), over `folder_ids`.
    pub fn search_emails(&self, folder_ids: &[i64], query: &str) -> Result<Vec<Email>> {
        self.search_emails_with(folder_ids, query, &HashSet::new())
    }

    /// `search_emails`, plus `server_ids`: emails a server-side search
    /// matched, whose bodies this database mostly lacks.
    pub fn search_emails_with(
        &self,
        folder_ids: &[i64],
        query: &str,
        server_ids: &HashSet<i64>,
    ) -> Result<Vec<Email>> {
        let filter = crate::query::parse(query);
        if filter.is_empty() {
            return self.emails_in_folders(folder_ids);
        }
        self.filter_emails(folder_ids, &filter, server_ids)
    }

    /// The emails in `folder_ids` that pass `filter`. Its words match the
    /// sender, subject, preview and indexed body text here; `server_ids` are emails
    /// the server found them in, whose bodies this database mostly lacks.
    pub fn filter_emails(
        &self,
        folder_ids: &[i64],
        filter: &SearchFilter,
        server_ids: &HashSet<i64>,
    ) -> Result<Vec<Email>> {
        if folder_ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; folder_ids.len()].join(", ");
        let mut sql =
            format!("SELECT {EMAIL_COLUMNS} FROM emails WHERE folder_id IN ({placeholders})");
        let mut values: Vec<rusqlite::types::Value> =
            folder_ids.iter().map(|id| (*id).into()).collect();
        // instr rather than LIKE: nothing in the text is a wildcard.
        for (columns, text) in [
            (&["sender", "sender_address"][..], &filter.from),
            (
                &["recipient", "recipient_address", "recipients"][..],
                &filter.to,
            ),
            (&["subject"][..], &filter.subject),
        ] {
            let Some(text) = text else { continue };
            let tests: Vec<String> = columns
                .iter()
                .map(|column| format!("instr(lower({column}), lower(?)) > 0"))
                .collect();
            sql.push_str(&format!(" AND ({})", tests.join(" OR ")));
            values.extend(columns.iter().map(|_| text.trim().to_string().into()));
        }
        for (column, wanted) in [("unread", filter.unread), ("starred", filter.starred)] {
            if let Some(wanted) = wanted {
                sql.push_str(&format!(" AND {column} = ?"));
                values.push((wanted as i64).into());
            }
        }
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map(rusqlite::params_from_iter(values), Self::email_from_row)?;
        let mut emails = rows.collect::<Result<Vec<_>>>()?;

        // Stored dates carry their own zone offsets, so they are compared
        // as instants rather than as text.
        let (after, before) = filter.time_bounds();
        emails.retain(|email| {
            let moment = crate::dates::sort_key(&email.date);
            after.is_none_or(|after| moment >= after) && before.is_none_or(|before| moment < before)
        });
        let matcher = fts_terms(&filter.words);
        if !matcher.is_empty() {
            let mut statement = self
                .conn
                .prepare("SELECT rowid FROM emails_fts WHERE emails_fts MATCH ?")?;
            let found = statement
                .query_map([matcher], |row| row.get::<_, i64>(0))?
                .collect::<Result<HashSet<i64>>>()?;
            emails.retain(|email| found.contains(&email.id) || server_ids.contains(&email.id));
        }
        Ok(Self::sort_emails(emails))
    }

    /// The local ids of a folder's messages, by their server UIDs.
    pub fn email_ids_for_uids(&self, folder_id: i64, uids: &[String]) -> Result<Vec<i64>> {
        let mut statement = self
            .conn
            .prepare("SELECT id FROM emails WHERE folder_id = ?1 AND server_id = ?2")?;
        let mut ids = Vec::new();
        for uid in uids {
            if let Some(id) = statement
                .query_row(params![folder_id, uid], |row| row.get(0))
                .optional()?
            {
                ids.push(id);
            }
        }
        Ok(ids)
    }

    fn sort_emails(mut emails: Vec<Email>) -> Vec<Email> {
        // Sent time is comparable across accounts and defines the day sections.
        // Arrival order breaks ties; the local ID makes the ordering stable.
        emails.sort_by_key(|email| {
            std::cmp::Reverse((
                email.is_pinned,
                crate::dates::sort_key(&email.date),
                email.arrival_key(),
                email.id,
            ))
        });
        emails
    }

    pub fn unread_count_in_folder(&self, folder_id: i64) -> Result<i64> {
        self.conn.query_row(
            "SELECT COUNT(*) FROM emails WHERE folder_id = ?1 AND unread = 1",
            [folder_id],
            |row| row.get(0),
        )
    }

    pub fn set_email_unread(&self, email_id: i64, is_unread: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE emails SET unread = ?1 WHERE id = ?2",
            params![is_unread as i64, email_id],
        )?;
        Ok(())
    }

    pub fn set_email_starred(&self, email_id: i64, is_starred: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE emails SET starred = ?1 WHERE id = ?2",
            params![is_starred as i64, email_id],
        )?;
        Ok(())
    }

    pub fn set_email_pinned(&self, email_id: i64, is_pinned: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE emails SET pinned = ?1 WHERE id = ?2",
            params![is_pinned as i64, email_id],
        )?;
        Ok(())
    }

    /// Atomically move emails as UID-less destination placeholders. UIDs are
    /// only unique within an IMAP mailbox, so the UID is cleared while the
    /// move is pending.
    pub fn move_emails(&mut self, email_ids: &[i64], folder_id: i64) -> Result<()> {
        let transaction = self.conn.transaction()?;
        {
            let mut update = transaction
                .prepare("UPDATE emails SET folder_id = ?1, server_id = NULL WHERE id = ?2")?;
            for email_id in email_ids {
                update.execute(params![folder_id, email_id])?;
            }
        }
        transaction.commit()
    }

    /// Atomically place each moved email at a (folder, UID) the server gave.
    /// A missing UID means the server accepted the move but did not say how
    /// to identify the new row, so the placeholder is removed for the next
    /// sync to fill in.
    pub fn reconcile_moved_emails(&mut self, moves: &[(i64, i64, Option<String>)]) -> Result<()> {
        let transaction = self.conn.transaction()?;
        for (email_id, folder_id, server_id) in moves {
            let Some(server_id) = server_id else {
                transaction.execute("DELETE FROM emails WHERE id = ?1", [email_id])?;
                continue;
            };
            let existing: Option<i64> = transaction
                .query_row(
                    "SELECT id FROM emails WHERE folder_id = ?1 AND server_id = ?2 AND id != ?3",
                    params![folder_id, server_id, email_id],
                    |row| row.get(0),
                )
                .optional()?;
            if existing.is_some() {
                // The destination sync already has the authoritative row.
                transaction.execute("DELETE FROM emails WHERE id = ?1", [email_id])?;
            } else {
                transaction.execute(
                    "UPDATE emails SET folder_id = ?1, server_id = ?2 WHERE id = ?3",
                    params![folder_id, server_id, email_id],
                )?;
            }
        }
        transaction.commit()
    }

    pub fn raw_message(&self, email_id: i64) -> Result<Option<Vec<u8>>> {
        self.conn
            .query_row(
                "SELECT raw_message FROM emails WHERE id = ?1",
                [email_id],
                |row| row.get::<_, Option<Vec<u8>>>(0),
            )
            .optional()
            .map(Option::flatten)
    }

    /// Cache a downloaded message. A row synced before previews existed gets
    /// its preview filled in from the body at the same time.
    pub fn save_raw_message(&self, email_id: i64, raw: &[u8]) -> Result<()> {
        let parsed = crate::mime::parse_message(raw);
        let preview = crate::mime::preview(&parsed);
        let body_text = crate::mime::search_text(&parsed);
        self.conn.execute(
            "UPDATE emails SET raw_message = ?1, body_text = ?4,
                preview = CASE WHEN preview = '' THEN ?3 ELSE preview END
             WHERE id = ?2",
            params![raw, email_id, preview, body_text],
        )?;
        Ok(())
    }

    /// Insert a locally created row (a Sent copy, a draft, an Outbox entry).
    pub fn save_email(&self, folder_id: i64, header: &MessageHeader) -> Result<Email> {
        self.conn.execute(
            "INSERT INTO emails (folder_id, server_id, sender, subject, preview, date, unread,
                sender_address, recipient, recipient_address, message_id)
             VALUES (?1, NULL, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULLIF(?10, ''))",
            params![
                folder_id,
                header.sender,
                header.subject,
                header.preview,
                header.date,
                header.is_unread as i64,
                header.sender_address,
                header.recipient,
                header.recipient_address,
                header.message_id,
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        Ok(self.email(id)?.expect("the row was just inserted"))
    }

    /// Insert one fetched email, or update the flags of one we already have.
    /// Returns true when the row was new, which is how a sync tells which
    /// messages to notify about.
    pub fn save_incoming_email(&self, folder_id: i64, header: &MessageHeader) -> Result<bool> {
        let existing: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT message_id FROM emails WHERE folder_id = ?1 AND server_id = ?2",
                params![folder_id, header.uid],
                |row| row.get(0),
            )
            .optional()?;
        let mut is_new = existing.is_none();
        if let Some(message_id) = &existing {
            if message_id.as_deref().unwrap_or("") != header.message_id {
                // Another message at the same UID (a mailbox that reset its
                // UIDs). Replace the row, which also drops its cached body.
                self.conn.execute(
                    "DELETE FROM emails WHERE folder_id = ?1 AND server_id = ?2",
                    params![folder_id, header.uid],
                )?;
                is_new = true;
            }
        }
        self.conn.execute(
            "INSERT INTO emails (folder_id, server_id, sender, subject, preview, date, unread, starred,
                message_id, sender_address, recipient, recipient_address,
                pinned, recipients, body_text)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
             ON CONFLICT (folder_id, server_id) DO UPDATE SET
                unread = excluded.unread, starred = excluded.starred, pinned = excluded.pinned,
                recipient = excluded.recipient, recipient_address = excluded.recipient_address,
                recipients = excluded.recipients,
                preview = CASE WHEN excluded.preview = '' THEN preview ELSE excluded.preview END,
                body_text = CASE WHEN length(excluded.body_text) > length(body_text)
                    THEN excluded.body_text ELSE body_text END",
            params![
                folder_id,
                header.uid,
                header.sender,
                header.subject,
                header.preview,
                header.date,
                header.is_unread as i64,
                header.is_starred as i64,
                header.message_id,
                header.sender_address,
                header.recipient,
                header.recipient_address,
                header.is_pinned as i64,
                header.recipients,
                header.body_text,
            ],
        )?;
        Ok(is_new)
    }

    /// A locally saved row the server now holds as `uid`: the row takes the
    /// UID, so the next sync updates it rather than adding a second copy.
    /// If a sync got there first, the local row is the duplicate and goes.
    pub fn adopt_server_uid(&self, email_id: i64, uid: &str) -> Result<()> {
        let updated = self.conn.execute(
            "UPDATE OR IGNORE emails SET server_id = ?2 WHERE id = ?1",
            params![email_id, uid],
        )?;
        if updated == 0 {
            self.delete_email(email_id)?;
        }
        Ok(())
    }

    pub fn delete_email(&self, email_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM emails WHERE id = ?1", [email_id])?;
        Ok(())
    }

    // --- the change queue -------------------------------------------------

    fn pending_op_from_row(row: &Row) -> Result<PendingOp> {
        let list = |text: String| -> Vec<String> {
            text.split(',')
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect()
        };
        let uids = list(row.get("uids")?);
        let change = if row.get::<_, String>("kind")? == "move" {
            Change::Move {
                email_ids: list(row.get("email_ids")?)
                    .iter()
                    .filter_map(|id| id.parse().ok())
                    .collect(),
                uids,
                dest_id: row.get("dest_id")?,
                dest: row.get("dest")?,
            }
        } else {
            Change::Flag {
                uids,
                flag: row.get("flag")?,
                add: row.get("is_add")?,
            }
        };
        Ok(PendingOp {
            id: row.get("id")?,
            account_id: row.get("account_id")?,
            folder_id: row.get("folder_id")?,
            folder: row.get("folder")?,
            change,
        })
    }

    /// Queue a change for the server; `op.id` is ignored. Returns the new id.
    pub fn enqueue_op(&self, op: &PendingOp) -> Result<i64> {
        let join = |items: &[String]| items.join(",");
        let (kind, uids, email_ids, flag, is_add, dest_id, dest) = match &op.change {
            Change::Flag { uids, flag, add } => (
                "flag",
                join(uids),
                String::new(),
                flag.as_str(),
                *add,
                0,
                "",
            ),
            Change::Move {
                email_ids,
                uids,
                dest_id,
                dest,
            } => (
                "move",
                join(uids),
                email_ids
                    .iter()
                    .map(i64::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
                "",
                false,
                *dest_id,
                dest.as_str(),
            ),
        };
        self.conn.execute(
            "INSERT INTO pending_ops (account_id, folder_id, folder, kind, uids, email_ids, flag,
                is_add, dest_id, dest)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                op.account_id,
                op.folder_id,
                op.folder,
                kind,
                uids,
                email_ids,
                flag,
                is_add,
                dest_id,
                dest
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// An account's queued changes, oldest first: the order they replay in.
    pub fn pending_ops(&self, account_id: i64) -> Result<Vec<PendingOp>> {
        let mut statement = self
            .conn
            .prepare("SELECT * FROM pending_ops WHERE account_id = ?1 ORDER BY id")?;
        let rows = statement.query_map([account_id], Self::pending_op_from_row)?;
        rows.collect()
    }

    /// The queued changes to one mailbox, oldest first.
    pub fn pending_ops_for_folder(&self, folder_id: i64) -> Result<Vec<PendingOp>> {
        let mut statement = self
            .conn
            .prepare("SELECT * FROM pending_ops WHERE folder_id = ?1 ORDER BY id")?;
        let rows = statement.query_map([folder_id], Self::pending_op_from_row)?;
        rows.collect()
    }

    pub fn finish_op(&self, op_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM pending_ops WHERE id = ?1", [op_id])?;
        Ok(())
    }

    /// A move the connection cut short: the first `done` messages reached the
    /// destination, so only the rest stay queued.
    pub fn trim_move_op(&self, op_id: i64, done: usize) -> Result<()> {
        let op = self
            .conn
            .query_row(
                "SELECT * FROM pending_ops WHERE id = ?1",
                [op_id],
                Self::pending_op_from_row,
            )
            .optional()?;
        let Some(PendingOp {
            change: Change::Move {
                email_ids, uids, ..
            },
            ..
        }) = op
        else {
            return Ok(());
        };
        if done >= uids.len() {
            return self.finish_op(op_id);
        }
        let rest_ids: Vec<String> = email_ids.iter().skip(done).map(i64::to_string).collect();
        self.conn.execute(
            "UPDATE pending_ops SET uids = ?2, email_ids = ?3 WHERE id = ?1",
            params![op_id, uids[done..].join(","), rest_ids.join(",")],
        )?;
        Ok(())
    }

    // --- contacts ---------------------------------------------------------

    /// Remember (name, address) pairs. A later sighting fills in a missing
    /// display name, but an anonymous one never wipes a name we already have.
    pub fn save_contacts(&mut self, addresses: &[(String, String)]) -> Result<()> {
        let transaction = self.conn.transaction()?;
        {
            let mut upsert = transaction.prepare(
                "INSERT INTO contacts (address, name) VALUES (?1, ?2)
                 ON CONFLICT(address) DO UPDATE SET name = excluded.name WHERE excluded.name != ''",
            )?;
            for (name, address) in addresses {
                if !address.is_empty() {
                    upsert.execute(params![address.to_lowercase(), name])?;
                }
            }
        }
        transaction.commit()
    }

    /// Remember the people a message was sent to, and that it was: who
    /// you write to comes first among suggestions.
    pub fn record_sent_contacts(&mut self, addresses: &[(String, String)]) -> Result<()> {
        self.save_contacts(addresses)?;
        let mut bump = self
            .conn
            .prepare("UPDATE contacts SET sent_count = sent_count + 1 WHERE address = ?1")?;
        for (_, address) in addresses {
            if !address.is_empty() {
                bump.execute([address.to_lowercase()])?;
            }
        }
        Ok(())
    }

    /// Every known address as "Name <address>", the ones written to most
    /// first: `sent` is how many messages went to each.
    pub fn ranked_contacts(&self) -> Result<Vec<(String, u32)>> {
        let mut statement = self.conn.prepare(
            "SELECT name, address, sent_count FROM contacts
             ORDER BY sent_count DESC, name, address",
        )?;
        let rows = statement.query_map([], |row| {
            let name: String = row.get(0)?;
            let address: String = row.get(1)?;
            let sent: u32 = row.get(2)?;
            Ok((contact_label(&name, &address), sent))
        })?;
        rows.collect()
    }

    pub fn contact_addresses(&self) -> Result<Vec<String>> {
        let mut statement = self
            .conn
            .prepare("SELECT name, address FROM contacts ORDER BY name, address")?;
        let rows = statement.query_map([], |row| {
            let name: String = row.get(0)?;
            let address: String = row.get(1)?;
            Ok(if name.is_empty() {
                address
            } else {
                format!("{name} <{address}>")
            })
        })?;
        rows.collect()
    }

    // --- roles ------------------------------------------------------------

    /// The local folder that mirrors this account's sent mailbox on the server.
    /// Falls back to creating "Sent" for an account whose folder list hasn't
    /// synced yet.
    pub fn sent_folder(&self, account_id: i64) -> Result<Folder> {
        let folders = self.folders_for_account(account_id)?;
        let name = folders::mailbox_with_role(
            folders.iter().map(|f| f.name.as_str()),
            folders::FolderRole::Sent,
        )
        .unwrap_or(folders::SENT_FOLDER)
        .to_string();
        self.get_or_create_folder(account_id, &name, folders::icon_for_folder(&name))
    }

    /// The local folder that mirrors this account's drafts mailbox, created
    /// as "Drafts" when the folder list hasn't synced yet.
    pub fn drafts_folder(&self, account_id: i64) -> Result<Folder> {
        let folders = self.folders_for_account(account_id)?;
        let name = folders::mailbox_with_role(
            folders.iter().map(|f| f.name.as_str()),
            folders::FolderRole::Drafts,
        )
        .unwrap_or(folders::DRAFTS_FOLDER)
        .to_string();
        self.get_or_create_folder(account_id, &name, folders::icon_for_folder(&name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account() -> NewAccount {
        NewAccount {
            email: "me@example.com".into(),
            display_name: "Me".into(),
            imap_host: "imap.example.com".into(),
            imap_port: 993,
            imap_security: Security::Tls,
            smtp_host: "smtp.example.com".into(),
            smtp_port: 587,
            smtp_security: Security::StartTls,
            goa_id: String::new(),
            eds_uid: String::new(),
            eds_smtp_uid: String::new(),
            eds_root_uid: String::new(),
            imap_user: String::new(),
            imap_auth: Auth::Password,
            smtp_user: String::new(),
            smtp_auth: Auth::Password,
        }
    }

    fn header(uid: &str, subject: &str, date: &str, sender: &str) -> MessageHeader {
        MessageHeader {
            uid: uid.into(),
            sender: sender.into(),
            sender_address: format!("{}@x.y", sender.to_lowercase()),
            subject: subject.into(),
            date: date.into(),
            is_unread: true,
            message_id: format!("<{uid}@x>"),
            ..MessageHeader::default()
        }
    }

    #[test]
    fn accounts_and_folders() {
        let mut db = Database::open_in_memory().unwrap();
        let saved = db.save_account(&account()).unwrap();
        assert_eq!(db.accounts().unwrap(), vec![saved.clone()]);
        assert_eq!(saved.color, "");
        assert_eq!(saved.color_hex(), "#3584e4");
        db.set_account_color(saved.id, "#e62d42").unwrap();
        assert_eq!(
            db.account(saved.id).unwrap().unwrap().color_hex(),
            "#e62d42"
        );
        db.set_account_color(saved.id, "red").unwrap();
        assert_eq!(db.account(saved.id).unwrap().unwrap().color, "");
        assert_eq!(saved.picture_file(), None);
        db.set_account_picture(saved.id, "1-42.png").unwrap();
        assert_eq!(
            db.account(saved.id).unwrap().unwrap().picture_file(),
            Some("1-42.png")
        );
        for unsafe_name in ["../rustle.db", "/etc/passwd", "a\\b.png", ".hidden"] {
            db.set_account_picture(saved.id, unsafe_name).unwrap();
            let stored = db.account(saved.id).unwrap().unwrap();
            assert_eq!(stored.picture_file(), None, "{unsafe_name}");
        }
        db.set_account_picture(saved.id, "").unwrap();
        assert_eq!(db.account(saved.id).unwrap().unwrap().picture, "");
        assert_eq!(saved.signature_html(), "");
        db.set_account_signature(saved.id, "Cheers,\nMe\n").unwrap();
        assert_eq!(
            db.account(saved.id).unwrap().unwrap().signature_html(),
            "Cheers,<br>Me"
        );
        assert_eq!(saved.notification_sound, "");
        db.set_account_notification_sound(saved.id, "none").unwrap();
        assert_eq!(
            db.account(saved.id).unwrap().unwrap().notification_sound,
            "none"
        );
        db.set_account_signature(saved.id, "<div><b>Me</b></div>")
            .unwrap();
        assert_eq!(
            db.account(saved.id).unwrap().unwrap().signature_html(),
            "<div><b>Me</b></div>"
        );
        assert_eq!(saved.name(), saved.email);
        db.set_account_label(saved.id, "  Work ").unwrap();
        let labelled = db.account(saved.id).unwrap().unwrap();
        assert_eq!(labelled.name(), "Work");
        assert_eq!(labelled.short_label(), "Work");
        let inbox = db
            .get_or_create_folder(saved.id, "INBOX", "mail-unread-symbolic")
            .unwrap();
        let child = db
            .get_or_create_folder(saved.id, "INBOX/Sub", "folder-symbolic")
            .unwrap();
        db.set_folder_parent(child.id, Some(inbox.id), "/").unwrap();
        assert_eq!(db.folders_for_account(saved.id).unwrap().len(), 2);
        db.prune_folders(saved.id, &HashSet::from(["Outbox".to_string()]))
            .unwrap();
        assert!(db.folders_for_account(saved.id).unwrap().is_empty());
        db.delete_account(saved.id).unwrap();
        assert!(db.accounts().unwrap().is_empty());
    }

    fn eds_account(uid: &str, email: &str, imap_host: &str, goa_id: &str) -> eds::MailAccount {
        let server = |uid: &str, host: &str, port| eds::Server {
            uid: uid.into(),
            host: host.into(),
            port,
            security: Security::Tls,
            user: email.into(),
            auth: if goa_id.is_empty() {
                Auth::Password
            } else {
                Auth::OAuth2
            },
        };
        eds::MailAccount {
            uid: uid.into(),
            root_uid: format!("{uid}-root"),
            goa_id: goa_id.into(),
            email: email.into(),
            name: "Ada".into(),
            label: "Gmail".into(),
            imap: server(uid, imap_host, 993),
            smtp: server(&format!("{uid}-smtp"), "smtp.example.com", 465),
        }
    }

    #[test]
    fn moving_to_eds_keeps_accounts_and_their_mail() {
        let mut db = Database::open_in_memory().unwrap();
        // Before EDS: one Online Accounts row and one typed-in bridge row.
        let online = db
            .save_account(&NewAccount {
                email: "ada@gmail.com".into(),
                imap_host: "imap.gmail.com".into(),
                goa_id: "account_1".into(),
                imap_auth: Auth::OAuth2,
                ..account()
            })
            .unwrap();
        let bridge = db
            .save_account(&NewAccount {
                email: "ada@work.com".into(),
                imap_host: "127.0.0.1".into(),
                ..account()
            })
            .unwrap();
        db.set_account_color(online.id, "#e62d42").unwrap();
        db.set_account_picture(online.id, "2-1.png").unwrap();
        let inbox = db
            .get_or_create_folder(bridge.id, "INBOX", "mail-unread-symbolic")
            .unwrap();
        let from_eds = [
            eds_account("gmail-mail", "ada@gmail.com", "imap.gmail.com", "account_1"),
            eds_account("bridge-mail", "Ada@Work.com", "127.0.0.1", ""),
        ];
        let reconciled = db.reconcile_eds(&from_eds).unwrap();
        assert!(reconciled.added.is_empty());
        assert!(reconciled.is_changed);
        let accounts = db.accounts().unwrap();
        assert_eq!(accounts.len(), 2);
        let online = db.account(online.id).unwrap().unwrap();
        assert_eq!(online.eds_uid, "gmail-mail");
        assert_eq!(online.eds_smtp_uid, "gmail-mail-smtp");
        assert_eq!(online.smtp_auth, Auth::OAuth2);
        assert_eq!(online.color, "#e62d42", "local settings stay");
        assert_eq!(online.picture, "2-1.png");
        let bridge = db.account(bridge.id).unwrap().unwrap();
        assert_eq!(bridge.eds_uid, "bridge-mail");
        assert_eq!(db.folders_for_account(bridge.id).unwrap(), vec![inbox]);
        assert!(db.accounts_outside_eds().unwrap().is_empty());
        // Nothing to do the second time.
        assert_eq!(db.reconcile_eds(&from_eds).unwrap(), Reconciled::default());
    }

    #[test]
    fn eds_accounts_come_go_and_stay_hidden_when_removed_here() {
        let mut db = Database::open_in_memory().unwrap();
        let gmail = eds_account("gmail-mail", "ada@gmail.com", "imap.gmail.com", "account_1");
        let reconciled = db.reconcile_eds(std::slice::from_ref(&gmail)).unwrap();
        assert_eq!(reconciled.added.len(), 1);
        let id = reconciled.added[0];
        let added = db.account(id).unwrap().unwrap();
        assert_eq!(added.label, "Gmail");
        assert_eq!(added.display_name, "Ada");

        // Gone from EDS: set aside, and back with its id when it returns.
        db.reconcile_eds(&[]).unwrap();
        assert!(db.accounts().unwrap().is_empty());
        let back = db.reconcile_eds(std::slice::from_ref(&gmail)).unwrap();
        assert_eq!(back.added, vec![id]);
        assert_eq!(db.accounts().unwrap().len(), 1);

        // Removed in Rustle: hidden until shown again, whatever EDS says.
        db.hide_account(id).unwrap();
        let reconciled = db.reconcile_eds(std::slice::from_ref(&gmail)).unwrap();
        assert!(!reconciled.is_changed);
        assert!(db.accounts().unwrap().is_empty());
        assert_eq!(db.hidden_accounts().unwrap().len(), 1);
        db.show_account(id).unwrap();
        assert_eq!(db.accounts().unwrap().len(), 1);
    }

    #[test]
    fn online_accounts_rows_without_eds_mail_are_set_aside() {
        let mut db = Database::open_in_memory().unwrap();
        db.save_account(&NewAccount {
            goa_id: "account_9".into(),
            ..account()
        })
        .unwrap();
        let typed = db.save_account(&account()).unwrap();
        db.reconcile_eds(&[]).unwrap();
        // The typed-in one waits to be moved to EDS rather than vanishing.
        assert_eq!(db.accounts().unwrap(), vec![typed]);
    }

    #[test]
    fn existing_mail_survives_removal_of_grouping_columns() {
        let old = Database {
            conn: Connection::open_in_memory().unwrap(),
        };
        old.create_tables().unwrap();
        let removal = MIGRATIONS
            .iter()
            .position(|sql| sql.contains("DROP COLUMN conversation_id"))
            .unwrap();
        for sql in &MIGRATIONS[..removal] {
            old.conn.execute_batch(sql).unwrap();
        }
        old.conn
            .pragma_update(None, "user_version", removal as i64)
            .unwrap();
        // That schema predates columns save_account writes today.
        old.conn
            .execute(
                "INSERT INTO accounts (email, display_name, imap_host, imap_port, smtp_host, smtp_port)
                 VALUES ('me@example.com', 'Me', 'imap.example.com', 993, 'smtp.example.com', 587)",
                [],
            )
            .unwrap();
        let account_id = old.conn.last_insert_rowid();
        let inbox = old.get_or_create_folder(account_id, "INBOX", "i").unwrap();
        // Written as that schema had it: today's writers set later columns.
        for (uid, subject, date, sender) in [
            ("1", "Topic", "2026-01-01T00:00:00Z", "Ada"),
            ("2", "Re: Topic", "2026-01-02T00:00:00Z", "Bob"),
        ] {
            old.conn
                .execute(
                    "INSERT INTO emails (folder_id, server_id, sender, subject, preview, date,
                        unread, message_id, sender_address)
                     VALUES (?1, ?2, ?3, ?4, '', ?5, 1, ?6, ?7)",
                    params![
                        inbox.id,
                        uid,
                        sender,
                        subject,
                        date,
                        format!("<{uid}@x>"),
                        format!("{}@x.y", sender.to_lowercase())
                    ],
                )
                .unwrap();
        }
        old.conn.execute_batch("UPDATE emails SET conversation_id = 1, in_reply_to = '<1@x>', reference_ids = '<1@x>'").unwrap();
        let before = old.emails_in_folders(&[inbox.id]).unwrap();
        let raw = b"Subject: Topic\r\n\r\nOriginal body";
        old.conn
            .execute(
                "UPDATE emails SET raw_message = ?1 WHERE id = ?2",
                params![raw.as_slice(), before[1].id],
            )
            .unwrap();
        let before = old.emails_in_folders(&[inbox.id]).unwrap();

        let db = Database::init(old.conn).unwrap();
        assert_eq!(db.emails_in_folders(&[inbox.id]).unwrap(), before);
        assert_eq!(
            db.raw_message(before[1].id).unwrap().as_deref(),
            Some(raw.as_slice())
        );
        assert_eq!(db.search_emails(&[inbox.id], "Topic").unwrap().len(), 2);
        // Bodies downloaded before the index reached them are indexed once.
        assert_eq!(db.search_emails(&[inbox.id], "original").unwrap().len(), 1);
        let removed_columns: i64 = db.conn.query_row(
            "SELECT count(*) FROM pragma_table_info('emails') WHERE name IN ('conversation_id', 'in_reply_to', 'reference_ids')",
            [], |row| row.get(0),
        ).unwrap();
        assert_eq!(removed_columns, 0);
        // Starting the app again does not replay the column removal.
        let reopened = Database::init(db.conn).unwrap();
        assert_eq!(reopened.emails_in_folders(&[inbox.id]).unwrap(), before);
    }

    #[test]
    fn related_emails_keep_their_positions_and_individual_state() {
        let mut db = Database::open_in_memory().unwrap();
        let account = db.save_account(&account()).unwrap();
        let inbox = db.get_or_create_folder(account.id, "INBOX", "i").unwrap();
        let archive = db.get_or_create_folder(account.id, "Archive", "a").unwrap();
        // Backfill the oldest message last, so local IDs disagree with dates.
        for (uid, subject, date, sender) in [
            ("2", "Unrelated", "2026-01-02T00:00:00Z", "Cy"),
            ("3", "Re: Topic", "2026-01-03T00:00:00Z", "Bob"),
            ("1", "Topic", "2026-01-01T00:00:00Z", "Ada"),
        ] {
            db.save_incoming_email(inbox.id, &header(uid, subject, date, sender))
                .unwrap();
        }
        let emails = db.emails_in_folders(&[inbox.id]).unwrap();
        assert_eq!(
            emails
                .iter()
                .map(|e| e.subject.as_str())
                .collect::<Vec<_>>(),
            ["Re: Topic", "Unrelated", "Topic"]
        );
        let reply = emails[0].id;
        let original = emails[2].id;
        db.set_email_unread(reply, false).unwrap();
        db.set_email_starred(reply, true).unwrap();
        assert!(db.email(original).unwrap().unwrap().is_unread);
        assert!(!db.email(original).unwrap().unwrap().is_starred);
        assert_eq!(
            db.emails_in_folders(&[inbox.id])
                .unwrap()
                .iter()
                .map(|e| e.id)
                .collect::<Vec<_>>(),
            emails.iter().map(|e| e.id).collect::<Vec<_>>()
        );
        db.set_email_pinned(original, true).unwrap();
        assert_eq!(db.emails_in_folders(&[inbox.id]).unwrap()[0].id, original);
        assert!(!db.email(reply).unwrap().unwrap().is_pinned);

        let topic = db.search_emails(&[inbox.id], "Topic").unwrap();
        assert_eq!(topic.len(), 2);
        assert_eq!(topic[0].id, original);
        let sender = db.search_emails(&[inbox.id], "Bob").unwrap();
        assert_eq!(sender.len(), 1);
        assert_eq!(sender[0].id, reply);
        db.move_emails(&[reply], archive.id).unwrap();
        assert_eq!(db.email(original).unwrap().unwrap().folder_id, inbox.id);
        assert_eq!(db.search_emails(&[inbox.id], "Bob").unwrap().len(), 0);
        assert_eq!(
            db.search_emails(&[inbox.id, archive.id], "Bob")
                .unwrap()
                .len(),
            1
        );
        db.delete_email(reply).unwrap();
        assert!(db.email(original).unwrap().is_some());
        assert_eq!(db.search_emails(&[inbox.id], "Topic").unwrap().len(), 1);
        assert!(db.emails_in_folders(&[]).unwrap().is_empty());
        assert!(db.search_emails(&[], "Topic").unwrap().is_empty());
        assert_eq!(
            db.search_emails(&[inbox.id], "  ").unwrap(),
            db.emails_in_folders(&[inbox.id]).unwrap()
        );
    }

    #[test]
    fn pinned_emails_sort_first() {
        let db = Database::open_in_memory().unwrap();
        let account = db.save_account(&account()).unwrap();
        let inbox = db.get_or_create_folder(account.id, "INBOX", "i").unwrap();
        let mut old = header("1", "Old", "2026-01-01T00:00:00Z", "Ada");
        old.is_pinned = true;
        db.save_incoming_email(inbox.id, &old).unwrap();
        db.save_incoming_email(inbox.id, &header("2", "New", "2026-02-01T00:00:00Z", "Bob"))
            .unwrap();
        let subjects = |db: &Database| -> Vec<String> {
            db.emails_in_folders(&[inbox.id])
                .unwrap()
                .iter()
                .map(|c| c.subject.to_string())
                .collect()
        };
        assert_eq!(subjects(&db), vec!["Old", "New"]);
        // Outlook unpinned it: the next sync's header carries no keyword.
        old.is_pinned = false;
        db.save_incoming_email(inbox.id, &old).unwrap();
        assert_eq!(subjects(&db), vec!["New", "Old"]);
        let id = db.emails_in_folders(&[inbox.id]).unwrap()[1].id;
        db.set_email_pinned(id, true).unwrap();
        assert_eq!(subjects(&db), vec!["Old", "New"]);
    }

    #[test]
    fn smart_search_filters_and_unions_server_matches() {
        let db = Database::open_in_memory().unwrap();
        let account = db.save_account(&account()).unwrap();
        let inbox = db.get_or_create_folder(account.id, "INBOX", "i").unwrap();
        for (uid, subject, date, sender) in [
            (
                "1",
                "Invoice 12",
                "2026-09-02T10:00:00+02:00",
                "Ada Lovelace",
            ),
            ("2", "Lunch", "2026-09-03T10:00:00Z", "Ada Lovelace"),
            ("3", "Invoice 9", "2026-08-01T10:00:00Z", "Bob"),
        ] {
            db.save_incoming_email(inbox.id, &header(uid, subject, date, sender))
                .unwrap();
        }
        let subjects = |filter: &SearchFilter, server: &HashSet<i64>| -> Vec<String> {
            db.filter_emails(&[inbox.id], filter, server)
                .unwrap()
                .into_iter()
                .map(|email| email.subject)
                .collect()
        };
        let none = HashSet::new();
        let from_ada = SearchFilter {
            from: Some("ADA".into()),
            ..SearchFilter::default()
        };
        assert_eq!(subjects(&from_ada, &none), ["Lunch", "Invoice 12"]);
        let invoices = SearchFilter {
            words: vec!["invoice".into()],
            after: Some("2026-09-01".into()),
            ..SearchFilter::default()
        };
        assert_eq!(subjects(&invoices, &none), ["Invoice 12"]);
        // "Lunch" mentions an invoice only in its body, which the server saw.
        let lunch = db
            .email_ids_for_uids(inbox.id, &["2".into(), "9".into()])
            .unwrap();
        assert_eq!(lunch.len(), 1);
        let server: HashSet<i64> = lunch.into_iter().collect();
        assert_eq!(subjects(&invoices, &server), ["Lunch", "Invoice 12"]);
        let unread_bob = SearchFilter {
            from: Some("bob".into()),
            unread: Some(false),
            ..SearchFilter::default()
        };
        assert!(subjects(&unread_bob, &none).is_empty());
    }

    #[test]
    fn sync_search_and_unified_inbox() {
        let mut db = Database::open_in_memory().unwrap();
        let one = db.save_account(&account()).unwrap();
        let two = db.save_account(&account()).unwrap();
        let inbox_one = db.get_or_create_folder(one.id, "INBOX", "i").unwrap();
        let inbox_two = db.get_or_create_folder(two.id, "INBOX", "i").unwrap();
        assert!(db
            .save_incoming_email(
                inbox_one.id,
                &header("1", "Alpha", "2026-01-01T00:00:00Z", "Ada")
            )
            .unwrap());
        assert!(db
            .save_incoming_email(
                inbox_one.id,
                &header("2", "Re: Alpha", "2026-01-02T00:00:00Z", "Bob")
            )
            .unwrap());
        assert!(db
            .save_incoming_email(
                inbox_two.id,
                &header("1", "Beta", "2026-01-03T00:00:00Z", "Cy")
            )
            .unwrap());
        assert!(!db
            .save_incoming_email(
                inbox_two.id,
                &header("1", "Beta", "2026-01-03T00:00:00Z", "Cy")
            )
            .unwrap());

        let emails = db.emails_in_folders(&[inbox_one.id]).unwrap();
        assert_eq!(emails.len(), 2);
        assert_eq!(emails[0].subject, "Re: Alpha");
        assert_eq!(emails[1].subject, "Alpha");

        let unified = db.emails_in_folders(&[inbox_one.id, inbox_two.id]).unwrap();
        assert_eq!(unified.len(), 3);
        assert_eq!(unified[0].subject, "Beta");
        assert_eq!(unified[1].folder_id, inbox_one.id);

        let found = db
            .search_emails(&[inbox_one.id, inbox_two.id], "bet")
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].subject, "Beta");
        assert_eq!(db.unread_count_in_folder(inbox_one.id).unwrap(), 2);

        assert_eq!(
            db.prune_stale_emails(inbox_one.id, &HashSet::from(["2".to_string()]))
                .unwrap(),
            1
        );
        assert_eq!(db.emails_in_folder(inbox_one.id).unwrap().len(), 1);
        assert_eq!(
            db.uids_in_folder(inbox_one.id).unwrap(),
            HashSet::from(["2".to_string()])
        );
    }

    #[test]
    fn moves_and_contacts() {
        let mut db = Database::open_in_memory().unwrap();
        let account = db.save_account(&account()).unwrap();
        let inbox = db.get_or_create_folder(account.id, "INBOX", "i").unwrap();
        let archive = db.get_or_create_folder(account.id, "Archive", "a").unwrap();
        db.save_incoming_email(inbox.id, &header("7", "S", "2026-01-01T00:00:00Z", "Ada"))
            .unwrap();
        let mail = &db.emails_in_folder(inbox.id).unwrap()[0];
        db.move_emails(&[mail.id], archive.id).unwrap();
        assert!(db.emails_in_folder(inbox.id).unwrap().is_empty());
        db.reconcile_moved_emails(&[(mail.id, archive.id, Some("3".into()))])
            .unwrap();
        assert_eq!(
            db.emails_in_folder(archive.id).unwrap()[0]
                .server_id
                .as_deref(),
            Some("3")
        );
        db.reconcile_moved_emails(&[(mail.id, archive.id, None)])
            .unwrap();
        assert!(db.emails_in_folder(archive.id).unwrap().is_empty());

        db.save_contacts(&[
            ("Ada".into(), "ADA@x.y".into()),
            (String::new(), "ada@x.y".into()),
        ])
        .unwrap();
        assert_eq!(db.contact_addresses().unwrap(), vec!["Ada <ada@x.y>"]);
        assert_eq!(db.sent_folder(account.id).unwrap().name, "Sent");
    }

    #[test]
    fn a_saved_draft_adopts_its_server_copy() {
        let db = Database::open_in_memory().unwrap();
        let account = db.save_account(&account()).unwrap();
        assert_eq!(db.drafts_folder(account.id).unwrap().name, "Drafts");
        let gmail = db
            .get_or_create_folder(account.id, "[Gmail]/Drafts", "d")
            .unwrap();
        let drafts = db.drafts_folder(account.id).unwrap();
        assert_eq!(drafts.name, "Drafts", "the first folder with the role");
        db.delete_folder_tree(drafts.id).unwrap();
        assert_eq!(db.drafts_folder(account.id).unwrap().id, gmail.id);

        let local = MessageHeader {
            subject: "Later".into(),
            message_id: "<d@x>".into(),
            ..MessageHeader::default()
        };
        let row = db.save_email(gmail.id, &local).unwrap();
        assert_eq!(row.message_id, "<d@x>");
        db.adopt_server_uid(row.id, "9").unwrap();
        // The next sync finds the row it already has.
        let synced = MessageHeader {
            uid: "9".into(),
            ..local.clone()
        };
        assert!(!db.save_incoming_email(gmail.id, &synced).unwrap());
        assert_eq!(db.emails_in_folder(gmail.id).unwrap().len(), 1);

        // A sync that got there first: the local row is the duplicate.
        let again = db.save_email(gmail.id, &local).unwrap();
        db.adopt_server_uid(again.id, "9").unwrap();
        let rows = db.emails_in_folder(gmail.id).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].server_id.as_deref(), Some("9"));
    }

    #[test]
    fn queued_changes_round_trip_in_order() {
        let db = Database::open_in_memory().unwrap();
        let account = db.save_account(&account()).unwrap();
        let flag = PendingOp {
            id: 0,
            account_id: account.id,
            folder_id: 4,
            folder: "INBOX".into(),
            change: Change::Flag {
                uids: vec!["1".into(), "2".into()],
                flag: "\\Seen".into(),
                add: true,
            },
        };
        let mv = PendingOp {
            change: Change::Move {
                email_ids: vec![10, 11, 12],
                uids: vec!["5".into(), "6".into(), "7".into()],
                dest_id: 9,
                dest: "Archive".into(),
            },
            ..flag.clone()
        };
        let flag_id = db.enqueue_op(&flag).unwrap();
        let move_id = db.enqueue_op(&mv).unwrap();
        let ops = db.pending_ops(account.id).unwrap();
        assert_eq!(
            ops,
            [
                PendingOp {
                    id: flag_id,
                    ..flag
                },
                PendingOp { id: move_id, ..mv }
            ]
        );
        assert_eq!(db.pending_ops_for_folder(4).unwrap().len(), 2);

        db.trim_move_op(move_id, 2).unwrap();
        let trimmed = db.pending_ops_for_folder(4).unwrap().pop().unwrap();
        assert_eq!(
            trimmed.change,
            Change::Move {
                email_ids: vec![12],
                uids: vec!["7".into()],
                dest_id: 9,
                dest: "Archive".into(),
            }
        );
        db.finish_op(flag_id).unwrap();
        db.trim_move_op(move_id, 1).unwrap();
        assert!(db.pending_ops(account.id).unwrap().is_empty());

        db.enqueue_op(&ops[0]).unwrap();
        let mut db = db;
        db.delete_account(account.id).unwrap();
        assert!(db.pending_ops(account.id).unwrap().is_empty());
    }

    #[test]
    fn typed_search_reaches_bodies_recipients_and_flags() {
        let db = Database::open_in_memory().unwrap();
        let account = db.save_account(&account()).unwrap();
        let inbox = db.get_or_create_folder(account.id, "INBOX", "i").unwrap();
        let mut budget = header("1", "Numbers", "2026-01-01T00:00:00Z", "Ada");
        budget.body_text = "the quarterly forecast is attached".into();
        budget.recipients = "Bob <bob@x.y>, Carol <carol@x.y>".into();
        db.save_incoming_email(inbox.id, &budget).unwrap();
        let mut lunch = header("2", "Lunch", "2026-01-02T00:00:00Z", "Dan");
        lunch.is_unread = false;
        db.save_incoming_email(inbox.id, &lunch).unwrap();

        let subjects = |query: &str| -> Vec<String> {
            db.search_emails(&[inbox.id], query)
                .unwrap()
                .into_iter()
                .map(|email| email.subject)
                .collect()
        };
        assert_eq!(subjects("forecast"), ["Numbers"]);
        assert_eq!(subjects("to:carol"), ["Numbers"]);
        assert_eq!(subjects("is:read"), ["Lunch"]);
        assert_eq!(subjects("from:dan lunch"), ["Lunch"]);
        assert!(
            subjects("\"forecast attached\"").is_empty(),
            "a phrase is a phrase"
        );

        // A later header fetch with a shorter text doesn't shrink the index.
        budget.body_text = "the".into();
        db.save_incoming_email(inbox.id, &budget).unwrap();
        assert_eq!(subjects("quarterly"), ["Numbers"]);
    }

    #[test]
    fn people_written_to_rank_first() {
        let mut db = Database::open_in_memory().unwrap();
        db.save_contacts(&[
            ("Ada".into(), "ada@x.y".into()),
            ("Bob".into(), "bob@x.y".into()),
        ])
        .unwrap();
        db.record_sent_contacts(&[("".into(), "BOB@x.y".into())])
            .unwrap();
        db.record_sent_contacts(&[("".into(), "bob@x.y".into())])
            .unwrap();
        assert_eq!(
            db.ranked_contacts().unwrap(),
            [
                ("Bob <bob@x.y>".to_string(), 2),
                ("Ada <ada@x.y>".to_string(), 0)
            ]
        );
    }
}
