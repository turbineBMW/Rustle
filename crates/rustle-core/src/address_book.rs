//! The desktop's address books, for recipient suggestions. Evolution Data
//! Server keeps each book -- the local one, and its cache of every synced
//! one (Google, CardDAV, Exchange) -- as a SQLite file. They're read here,
//! read-only, rather than over EDS's D-Bus views: a suggestion list needs
//! names and addresses once, not a live query.

use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};

/// Every (name, address) in the user's EDS address books.
pub fn eds_contacts() -> Vec<(String, String)> {
    contacts_from(&book_files())
}

/// The books' database files: the local data dir holds the user's own
/// books, the cache dir the copies of synced ones. Trash isn't a book.
fn book_files() -> Vec<PathBuf> {
    let roots = [
        glib::user_data_dir().join("evolution/addressbook"),
        glib::user_cache_dir().join("evolution/addressbook"),
    ];
    let mut files = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name == "trash" || name.to_string_lossy().starts_with('.') {
                continue;
            }
            let file = entry.path().join("contacts.db");
            if file.is_file() {
                files.push(file);
            }
        }
    }
    files
}

/// The contacts in these book files, each address once. A book that won't
/// open or has another layout is skipped: suggestions are a convenience.
pub fn contacts_from(files: &[PathBuf]) -> Vec<(String, String)> {
    let mut seen = std::collections::HashSet::new();
    let mut contacts = Vec::new();
    for file in files {
        match read_book(file) {
            Ok(found) => {
                for (name, address) in found {
                    if seen.insert(address.to_lowercase()) {
                        contacts.push((name, address));
                    }
                }
            }
            Err(error) => log::debug!("skipping address book {}: {error}", file.display()),
        }
    }
    contacts
}

fn read_book(file: &Path) -> rusqlite::Result<Vec<(String, String)>> {
    let conn = Connection::open_with_flags(
        file,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    // EDS's own layout: one row per contact in `folder_id`, its addresses in
    // `folder_id_email_list`. Contact lists have addresses of their own
    // members, not one to send to.
    let mut statement = conn.prepare(
        "SELECT COALESCE(NULLIF(c.full_name, ''), NULLIF(c.file_as, ''), ''), e.value
         FROM folder_id c JOIN folder_id_email_list e ON e.uid = c.uid
         WHERE COALESCE(c.is_list, 0) = 0 AND e.value LIKE '%@%'",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    rows.map(|row| row.map(|(name, address)| (name.trim().to_string(), address.trim().to_string())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_an_eds_book() {
        let dir = std::env::temp_dir().join(format!("rustle-book-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("contacts.db");
        let _ = std::fs::remove_file(&file);
        let conn = Connection::open(&file).unwrap();
        conn.execute_batch(
            "CREATE TABLE folder_id (uid TEXT PRIMARY KEY, file_as TEXT, full_name TEXT,
                is_list INTEGER);
             CREATE TABLE folder_id_email_list (uid TEXT NOT NULL, value TEXT);
             INSERT INTO folder_id VALUES ('a', 'Lovelace, Ada', 'Ada Lovelace', 0),
                ('b', 'Bob', '', 0), ('l', 'Team', 'Team', 1);
             INSERT INTO folder_id_email_list VALUES ('a', 'ada@x.y'), ('a', 'ADA@x.y'),
                ('b', 'bob@x.y'), ('b', 'not an address'), ('l', 'team@x.y');",
        )
        .unwrap();
        drop(conn);
        let missing = dir.join("missing.db");
        let contacts = contacts_from(&[missing, file]);
        assert_eq!(
            contacts,
            [
                ("Ada Lovelace".to_string(), "ada@x.y".to_string()),
                ("Bob".to_string(), "bob@x.y".to_string()),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
