//! All SQLite access. One connection, main thread only: the worker threads
//! do network and hand results back here.

use crate::folders;
use crate::models::{Account, Conversation, Email, Folder, MessageHeader, NewAccount, Security};
use crate::threader;
use rusqlite::{params, Connection, OptionalExtension, Row};
use std::collections::{HashMap, HashSet};
use std::path::Path;

pub type Result<T> = std::result::Result<T, rusqlite::Error>;

/// Every column `email_from_row` reads.
const EMAIL_COLUMNS: &str = "id, folder_id, server_id, sender, sender_address, recipient, \
    recipient_address, subject, preview, date, unread, starred, message_id, in_reply_to, \
    reference_ids, conversation_id";

/// Schema changes since the first release, applied in order. How many have run
/// is stored in PRAGMA user_version. Only ever append -- editing or reordering
/// these would give databases in the wild a different schema to new ones.
/// These mirror the Python app's migrations exactly, so an existing
/// the schema stays compatible with Postcard.
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
];

/// Turn free text into a safe FTS5 query: each word matched as a prefix.
fn fts_query(text: &str) -> String {
    text.split_whitespace()
        .map(|word| format!("\"{}\"*", word.replace('"', "\"\"")))
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
            END;",
        )
    }

    fn migrate_schema(&self) -> Result<()> {
        let version: i64 = self
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        let version = version.max(0) as usize;
        for (index, sql) in MIGRATIONS.iter().enumerate().skip(version) {
            self.conn.execute_batch(sql)?;
            self.conn
                .pragma_update(None, "user_version", (index + 1) as i64)?;
        }
        Ok(())
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
        })
    }

    pub fn accounts(&self) -> Result<Vec<Account>> {
        let mut statement = self.conn.prepare("SELECT * FROM accounts ORDER BY id")?;
        let rows = statement.query_map([], Self::account_from_row)?;
        rows.collect()
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
                imap_security, smtp_security, goa_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
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
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        Ok(self.account(id)?.expect("the row was just inserted"))
    }

    pub fn delete_account(&mut self, account_id: i64) -> Result<()> {
        let transaction = self.conn.transaction()?;
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
        transaction.execute("DELETE FROM accounts WHERE id = ?1", [account_id])?;
        transaction.commit()
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
            message_id: row
                .get::<_, Option<String>>("message_id")?
                .unwrap_or_default(),
            in_reply_to: row
                .get::<_, Option<String>>("in_reply_to")?
                .unwrap_or_default(),
            references: row
                .get::<_, Option<String>>("reference_ids")?
                .unwrap_or_default(),
            conversation_id: row.get("conversation_id")?,
        })
    }

    pub fn emails_in_folder(&self, folder_id: i64) -> Result<Vec<Email>> {
        let sql =
            format!("SELECT {EMAIL_COLUMNS} FROM emails WHERE folder_id = ?1 ORDER BY id DESC");
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map([folder_id], Self::email_from_row)?;
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

    /// Recompute the thread grouping for a folder and store it on each row.
    /// Rows that already hold the right id are left alone.
    pub fn reassign_conversations(&mut self, folder_id: i64) -> Result<()> {
        let emails = self.emails_in_folder(folder_id)?;
        let groups = threader::group(&emails);
        let transaction = self.conn.transaction()?;
        {
            let mut update =
                transaction.prepare("UPDATE emails SET conversation_id = ?1 WHERE id = ?2")?;
            for mail in &emails {
                let group = groups[&mail.id];
                if Some(group) != mail.conversation_id {
                    update.execute(params![group, mail.id])?;
                }
            }
        }
        transaction.commit()
    }

    /// Group a folder's emails into threads, newest thread first.
    pub fn conversations_in_folder(&self, folder_id: i64) -> Result<Vec<Conversation>> {
        self.conversations_in_folders(&[folder_id])
    }

    /// Threads across several folders at once -- the unified inbox. Threads
    /// never span folders (grouping is per folder), so this is a union sorted
    /// by date, which is comparable across accounts where UIDs are not.
    pub fn conversations_in_folders(&self, folder_ids: &[i64]) -> Result<Vec<Conversation>> {
        if folder_ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; folder_ids.len()].join(", ");
        let sql = format!("SELECT {EMAIL_COLUMNS} FROM emails WHERE folder_id IN ({placeholders})");
        let mut statement = self.conn.prepare(&sql)?;
        let rows =
            statement.query_map(rusqlite::params_from_iter(folder_ids), Self::email_from_row)?;
        let emails = rows.collect::<Result<Vec<_>>>()?;
        Ok(Self::conversations_from_emails(
            emails,
            folder_ids.len() > 1,
        ))
    }

    /// Full-text search; return each matching conversation whole. The subquery
    /// narrows to the threads a message matched in, so a search builds only
    /// those.
    pub fn search_conversations(
        &self,
        folder_ids: &[i64],
        query: &str,
    ) -> Result<Vec<Conversation>> {
        let matcher = fts_query(query);
        if matcher.is_empty() {
            return self.conversations_in_folders(folder_ids);
        }
        if folder_ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; folder_ids.len()].join(", ");
        let sql = format!(
            "SELECT {EMAIL_COLUMNS} FROM emails
             WHERE folder_id IN ({placeholders}) AND COALESCE(conversation_id, id) IN (
                SELECT COALESCE(e.conversation_id, e.id) FROM emails_fts f
                JOIN emails e ON e.id = f.rowid
                WHERE e.folder_id IN ({placeholders}) AND emails_fts MATCH ?
             )"
        );
        let mut statement = self.conn.prepare(&sql)?;
        let mut values: Vec<rusqlite::types::Value> = Vec::new();
        for _ in 0..2 {
            values.extend(
                folder_ids
                    .iter()
                    .map(|id| rusqlite::types::Value::from(*id)),
            );
        }
        values.push(rusqlite::types::Value::from(matcher));
        let rows = statement.query_map(rusqlite::params_from_iter(values), Self::email_from_row)?;
        let emails = rows.collect::<Result<Vec<_>>>()?;
        Ok(Self::conversations_from_emails(
            emails,
            folder_ids.len() > 1,
        ))
    }

    fn conversations_from_emails(emails: Vec<Email>, is_cross_folder: bool) -> Vec<Conversation> {
        let mut groups: HashMap<(i64, i64), Vec<Email>> = HashMap::new();
        let mut order: Vec<(i64, i64)> = Vec::new();
        for email in emails {
            let key = (email.folder_id, email.conversation_id.unwrap_or(email.id));
            let entry = groups.entry(key).or_default();
            if entry.is_empty() {
                order.push(key);
            }
            entry.push(email);
        }
        let mut conversations: Vec<Conversation> = order
            .into_iter()
            .map(|key| {
                let mut mails = groups.remove(&key).expect("every key was inserted");
                mails.sort_by_key(Email::arrival_key); // oldest first, so latest() is right
                Conversation::new(mails)
            })
            .collect();
        if is_cross_folder {
            conversations.sort_by(|a, b| b.date().cmp(a.date()));
        } else {
            conversations.sort_by_key(|c| std::cmp::Reverse(c.latest().arrival_key()));
        }
        conversations
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

    pub fn save_raw_message(&self, email_id: i64, raw: &[u8]) -> Result<()> {
        self.conn.execute(
            "UPDATE emails SET raw_message = ?1 WHERE id = ?2",
            params![raw, email_id],
        )?;
        Ok(())
    }

    /// Insert a locally created row (a Sent copy, a draft, an Outbox entry).
    pub fn save_email(&self, folder_id: i64, header: &MessageHeader) -> Result<Email> {
        self.conn.execute(
            "INSERT INTO emails (folder_id, server_id, sender, subject, preview, date, unread,
                sender_address, recipient, recipient_address)
             VALUES (?1, NULL, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
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
                message_id, in_reply_to, reference_ids, sender_address, recipient, recipient_address)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
             ON CONFLICT (folder_id, server_id) DO UPDATE SET
                unread = excluded.unread, starred = excluded.starred,
                recipient = excluded.recipient, recipient_address = excluded.recipient_address",
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
                header.in_reply_to,
                header.references,
                header.sender_address,
                header.recipient,
                header.recipient_address,
            ],
        )?;
        Ok(is_new)
    }

    pub fn delete_email(&self, email_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM emails WHERE id = ?1", [email_id])?;
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
        db.reassign_conversations(inbox_one.id).unwrap();
        db.reassign_conversations(inbox_two.id).unwrap();

        let threads = db.conversations_in_folder(inbox_one.id).unwrap();
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].count(), 2);
        assert_eq!(threads[0].subject(), "Re: Alpha");

        let unified = db
            .conversations_in_folders(&[inbox_one.id, inbox_two.id])
            .unwrap();
        assert_eq!(unified.len(), 2);
        assert_eq!(unified[0].subject(), "Beta");
        assert_eq!(unified[1].folder_id(), inbox_one.id);

        let found = db
            .search_conversations(&[inbox_one.id, inbox_two.id], "bet")
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].subject(), "Beta");
        assert_eq!(db.unread_count_in_folder(inbox_one.id).unwrap(), 2);

        assert_eq!(
            db.prune_stale_emails(inbox_one.id, &HashSet::from(["2".to_string()]))
                .unwrap(),
            1
        );
        assert_eq!(db.emails_in_folder(inbox_one.id).unwrap().len(), 1);
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
}
