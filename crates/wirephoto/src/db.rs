//! The index: SQLite in WAL mode (design.md, index). A cache of the disk plus keys and the change log; deleting
//! it costs a rescan, never data.
//!
//! One writer connection behind a mutex (SQLite allows one writer at a time anyway) and a pool of readers.

use std::path::Path;

use parking_lot::Mutex;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{Connection, OptionalExtension, Row, params};
use wirephoto_api::{ChangeType, Kind};

use crate::kind;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS entries (
    rowid        INTEGER PRIMARY KEY,
    id           TEXT NOT NULL UNIQUE,
    library      TEXT NOT NULL,
    parent       INTEGER,
    name         TEXT NOT NULL,
    path         TEXT NOT NULL,
    kind         INTEGER NOT NULL,
    mime         TEXT,
    size         INTEGER NOT NULL DEFAULT 0,
    mtime_ns     INTEGER NOT NULL DEFAULT 0,
    dev          INTEGER NOT NULL DEFAULT 0,
    ino          INTEGER NOT NULL DEFAULT 0,
    sha256       BLOB,
    width        INTEGER,
    height       INTEGER,
    orientation  INTEGER,
    taken        TEXT,
    sort_taken   INTEGER NOT NULL DEFAULT 0,
    duration     REAL,
    media_state  INTEGER NOT NULL DEFAULT 0,
    previewable  INTEGER NOT NULL DEFAULT 0,
    preview_done INTEGER NOT NULL DEFAULT 0,
    trashed_at   INTEGER,
    trash_root   INTEGER,
    orig_parent  INTEGER,
    orig_name    TEXT,
    missing      INTEGER NOT NULL DEFAULT 0,
    -- hashing failed (unreadable, or changed while read): left until the next start, which retries it
    hash_deferred INTEGER NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX IF NOT EXISTS entries_path ON entries(library, path);
CREATE INDEX IF NOT EXISTS entries_parent ON entries(parent, name);
CREATE INDEX IF NOT EXISTS entries_taken ON entries(library, sort_taken);
CREATE INDEX IF NOT EXISTS entries_sha ON entries(sha256);
CREATE INDEX IF NOT EXISTS entries_ino ON entries(library, dev, ino);
CREATE INDEX IF NOT EXISTS entries_size ON entries(library, size);
CREATE INDEX IF NOT EXISTS entries_media ON entries(media_state, kind);
CREATE INDEX IF NOT EXISTS entries_trash ON entries(trash_root);
CREATE INDEX IF NOT EXISTS entries_missing ON entries(library, missing);

CREATE TABLE IF NOT EXISTS changes (
    seq      INTEGER PRIMARY KEY AUTOINCREMENT,
    time     INTEGER NOT NULL,
    type     INTEGER NOT NULL,
    entry    TEXT NOT NULL,
    library  TEXT NOT NULL,
    parent   TEXT
);
CREATE INDEX IF NOT EXISTS changes_library ON changes(library, seq);

CREATE TABLE IF NOT EXISTS audit (
    id       INTEGER PRIMARY KEY,
    time     INTEGER NOT NULL,
    op       TEXT NOT NULL,
    key      TEXT,
    client   TEXT,
    entry    TEXT,
    path     TEXT,
    detail   TEXT NOT NULL,
    status   INTEGER NOT NULL,
    ms       INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS audit_time ON audit(time);

CREATE TABLE IF NOT EXISTS keys (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    hash       BLOB NOT NULL UNIQUE,
    scope      TEXT NOT NULL,
    libraries  TEXT,
    origin     TEXT,
    created    INTEGER NOT NULL,
    last_used  INTEGER
);

-- Resumable uploads (tus, /v1/tus): the bytes in the library's .wp-uploads/<id>; once stored, the entry it
-- became (created_entry 0: the folder had those bytes already), kept until expires.
DROP TABLE IF EXISTS uploads;
CREATE TABLE IF NOT EXISTS tus (
    id            TEXT PRIMARY KEY,
    key           TEXT NOT NULL,
    library       TEXT NOT NULL,
    parent        TEXT NOT NULL,
    name          TEXT NOT NULL,
    length        INTEGER NOT NULL,
    sha256        BLOB,
    received      INTEGER NOT NULL DEFAULT 0,
    created       INTEGER NOT NULL,
    expires       INTEGER NOT NULL,
    entry         TEXT,
    created_entry INTEGER
);

CREATE TABLE IF NOT EXISTS meta (
    k  TEXT PRIMARY KEY,
    v  BLOB NOT NULL
);
"#;

/// `media_state`: facts not read yet, read, or nothing to read (not media, or unreadable).
pub const MEDIA_PENDING: i64 = 0;
pub const MEDIA_DONE: i64 = 1;
pub const MEDIA_NONE: i64 = 2;

pub struct Db {
    writer: Mutex<Connection>,
    readers: r2d2::Pool<SqliteConnectionManager>,
}

fn configure(c: &Connection) -> rusqlite::Result<()> {
    c.busy_timeout(std::time::Duration::from_secs(10))?;
    c.pragma_update(None, "journal_mode", "WAL")?;
    c.pragma_update(None, "synchronous", "NORMAL")?;
    c.pragma_update(None, "foreign_keys", "OFF")?;
    Ok(())
}

/// The preview queue (indexer::preview_pass), in the inode order it is worked in: only the entries still to do,
/// so looking at an empty queue reads nothing instead of the whole table. (The hash queue has `entries_sha`.)
const TODO_INDEXES: &str = r#"
CREATE INDEX IF NOT EXISTS entries_preview_todo ON entries(dev, ino)
    WHERE previewable = 1 AND preview_done = 0 AND trashed_at IS NULL AND missing = 0;
"#;

/// Columns added to the schema after indexes had been created with it: added to an older index on open.
const ADDED_COLUMNS: &[(&str, &str)] = &[("entries", "hash_deferred INTEGER NOT NULL DEFAULT 0")];

impl Db {
    pub fn open(path: &Path) -> anyhow::Result<Db> {
        let writer = Connection::open(path)?;
        configure(&writer)?;
        writer.execute_batch(SCHEMA)?;
        // Columns added since an index was created (CREATE TABLE IF NOT EXISTS leaves an old table alone).
        for (table, column) in ADDED_COLUMNS {
            let name = column.split(' ').next().unwrap_or(column);
            let has: bool = writer.query_row(
                &format!("SELECT COUNT(*) > 0 FROM pragma_table_info('{table}') WHERE name = ?"),
                [name],
                |r| r.get(0),
            )?;
            if !has {
                writer.execute(&format!("ALTER TABLE {table} ADD COLUMN {column}"), [])?;
            }
        }
        writer.execute_batch(TODO_INDEXES)?;
        let manager = SqliteConnectionManager::file(path).with_init(|c| {
            c.busy_timeout(std::time::Duration::from_secs(10))?;
            c.pragma_update(None, "query_only", "ON")
        });
        let readers = r2d2::Pool::builder().max_size(8).build(manager)?;
        Ok(Db {
            writer: Mutex::new(writer),
            readers,
        })
    }

    /// Runs `f` on the writer inside a transaction.
    pub fn write<T>(
        &self,
        f: impl FnOnce(&rusqlite::Transaction) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let mut c = self.writer.lock();
        let tx = c.transaction()?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    pub fn read<T>(&self, f: impl FnOnce(&Connection) -> anyhow::Result<T>) -> anyhow::Result<T> {
        let c = self.readers.get()?;
        f(&c)
    }

    pub fn meta_get(&self, k: &str) -> anyhow::Result<Option<Vec<u8>>> {
        self.read(|c| {
            Ok(
                c.query_row("SELECT v FROM meta WHERE k = ?", [k], |r| r.get(0))
                    .optional()?,
            )
        })
    }

    pub fn meta_set(&self, k: &str, v: &[u8]) -> anyhow::Result<()> {
        self.write(|t| {
            t.execute(
                "INSERT INTO meta(k, v) VALUES (?, ?) ON CONFLICT(k) DO UPDATE SET v = excluded.v",
                params![k, v],
            )?;
            Ok(())
        })
    }
}

/// An entry as stored, with its parent's ID joined in.
#[derive(Debug, Clone)]
pub struct EntryRow {
    pub rowid: i64,
    pub id: String,
    pub library: String,
    pub parent: Option<i64>,
    pub parent_id: Option<String>,
    pub name: String,
    pub path: String,
    pub kind: Kind,
    pub mime: Option<String>,
    pub size: u64,
    pub mtime_ns: i64,
    pub dev: u64,
    pub ino: u64,
    pub sha256: Option<Vec<u8>>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub orientation: Option<u8>,
    pub taken: Option<String>,
    pub sort_taken: i64,
    pub duration: Option<f64>,
    pub media_state: i64,
    pub previewable: bool,
    pub trashed_at: Option<i64>,
    pub trash_root: Option<i64>,
    pub orig_parent: Option<i64>,
    pub orig_name: Option<String>,
}

pub const ENTRY_COLS: &str = "e.rowid, e.id, e.library, e.parent, p.id, e.name, e.path, e.kind, e.mime, e.size, \
    e.mtime_ns, e.dev, e.ino, e.sha256, e.width, e.height, e.orientation, e.taken, e.sort_taken, e.duration, \
    e.media_state, e.previewable, e.trashed_at, e.trash_root, e.orig_parent, e.orig_name";

pub const ENTRY_FROM: &str = "entries e LEFT JOIN entries p ON p.rowid = e.parent";

impl EntryRow {
    pub fn from_row(r: &Row) -> rusqlite::Result<EntryRow> {
        Ok(EntryRow {
            rowid: r.get(0)?,
            id: r.get(1)?,
            library: r.get(2)?,
            parent: r.get(3)?,
            parent_id: r.get(4)?,
            name: r.get(5)?,
            path: r.get(6)?,
            kind: kind::from_i64(r.get(7)?),
            mime: r.get(8)?,
            size: r.get::<_, i64>(9)? as u64,
            mtime_ns: r.get(10)?,
            dev: r.get::<_, i64>(11)? as u64,
            ino: r.get::<_, i64>(12)? as u64,
            sha256: r.get(13)?,
            width: r.get(14)?,
            height: r.get(15)?,
            orientation: r.get(16)?,
            taken: r.get(17)?,
            sort_taken: r.get(18)?,
            duration: r.get(19)?,
            media_state: r.get(20)?,
            previewable: r.get::<_, i64>(21)? != 0,
            trashed_at: r.get(22)?,
            trash_root: r.get(23)?,
            orig_parent: r.get(24)?,
            orig_name: r.get(25)?,
        })
    }

    pub fn etag(&self) -> String {
        if self.kind == Kind::Folder {
            crate::util::folder_etag(&self.library, &self.path)
        } else {
            crate::util::file_etag(self.size, self.mtime_ns, self.ino)
        }
    }

    pub fn is_trashed(&self) -> bool {
        self.trashed_at.is_some()
    }
}

pub fn entry_by_id(c: &Connection, id: &str) -> rusqlite::Result<Option<EntryRow>> {
    c.query_row(
        &format!("SELECT {ENTRY_COLS} FROM {ENTRY_FROM} WHERE e.id = ?"),
        [id],
        EntryRow::from_row,
    )
    .optional()
}

pub fn entry_by_rowid(c: &Connection, rowid: i64) -> rusqlite::Result<Option<EntryRow>> {
    c.query_row(
        &format!("SELECT {ENTRY_COLS} FROM {ENTRY_FROM} WHERE e.rowid = ?"),
        [rowid],
        EntryRow::from_row,
    )
    .optional()
}

pub fn entry_by_path(
    c: &Connection,
    library: &str,
    path: &str,
) -> rusqlite::Result<Option<EntryRow>> {
    c.query_row(
        &format!("SELECT {ENTRY_COLS} FROM {ENTRY_FROM} WHERE e.library = ? AND e.path = ?"),
        params![library, path],
        EntryRow::from_row,
    )
    .optional()
}

/// Direct children of a folder (live and trashed alike).
pub fn children(c: &Connection, parent: i64) -> rusqlite::Result<Vec<EntryRow>> {
    let mut st = c.prepare_cached(&format!(
        "SELECT {ENTRY_COLS} FROM {ENTRY_FROM} WHERE e.parent = ?"
    ))?;
    let rows = st.query_map([parent], EntryRow::from_row)?;
    rows.collect()
}

/// Every entry strictly below `path` in a library.
pub fn descendants(c: &Connection, library: &str, path: &str) -> rusqlite::Result<Vec<EntryRow>> {
    let prefix = if path.is_empty() {
        String::new()
    } else {
        format!("{path}/")
    };
    let mut st = c.prepare_cached(&format!(
        "SELECT {ENTRY_COLS} FROM {ENTRY_FROM} WHERE e.library = ?1 AND e.path != '' AND substr(e.path, 1, length(?2)) = ?2 AND e.path != ?3"
    ))?;
    let rows = st.query_map(params![library, prefix, path], EntryRow::from_row)?;
    rows.collect()
}

pub fn counts(c: &Connection, folder: i64) -> rusqlite::Result<(u64, u64)> {
    c.query_row(
        "SELECT COALESCE(SUM(kind = 0), 0), COALESCE(SUM(kind != 0), 0) FROM entries WHERE parent = ? AND trashed_at IS NULL AND missing = 0",
        [folder],
        |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)),
    )
}

/// A new entry's columns.
pub struct NewEntry<'a> {
    pub library: &'a str,
    pub parent: Option<i64>,
    pub name: &'a str,
    pub path: &'a str,
    pub kind: Kind,
    pub mime: Option<&'a str>,
    pub size: u64,
    pub mtime_ns: i64,
    pub dev: u64,
    pub ino: u64,
}

pub fn insert_entry(t: &Connection, e: &NewEntry) -> rusqlite::Result<(i64, String)> {
    let id = crate::util::random_id("ent_");
    let media_state = match e.kind {
        Kind::Image | Kind::Raw | Kind::Video => MEDIA_PENDING,
        _ => MEDIA_NONE,
    };
    t.execute(
        "INSERT INTO entries (id, library, parent, name, path, kind, mime, size, mtime_ns, dev, ino, sort_taken, media_state)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            id,
            e.library,
            e.parent,
            e.name,
            e.path,
            kind::to_i64(e.kind),
            e.mime,
            e.size as i64,
            e.mtime_ns,
            e.dev as i64,
            e.ino as i64,
            e.mtime_ns.div_euclid(1_000_000_000),
            media_state
        ],
    )?;
    Ok((t.last_insert_rowid(), id))
}

/// Records new bytes for a file: stats change, facts and hash must be read again.
pub fn update_file_stat(
    t: &Connection,
    rowid: i64,
    size: u64,
    mtime_ns: i64,
    dev: u64,
    ino: u64,
) -> rusqlite::Result<()> {
    t.execute(
        "UPDATE entries SET size = ?, mtime_ns = ?, dev = ?, ino = ?, sha256 = NULL, hash_deferred = 0, sort_taken = ?,
             media_state = CASE WHEN kind IN (1, 2, 3) THEN 0 ELSE 2 END, previewable = 0, preview_done = 0,
             width = NULL, height = NULL, orientation = NULL, taken = NULL, duration = NULL
         WHERE rowid = ?",
        params![size as i64, mtime_ns, dev as i64, ino as i64, mtime_ns.div_euclid(1_000_000_000), rowid],
    )?;
    Ok(())
}

/// Moves an entry (and everything below it) to a new parent, name and path.
pub fn move_entry(
    t: &Connection,
    row: &EntryRow,
    new_parent: Option<i64>,
    new_name: &str,
    new_path: &str,
) -> rusqlite::Result<()> {
    t.execute(
        "UPDATE entries SET parent = ?, name = ?, path = ? WHERE rowid = ?",
        params![new_parent, new_name, new_path, row.rowid],
    )?;
    if row.kind == Kind::Folder {
        let old_prefix = format!("{}/", row.path);
        let new_prefix = format!("{new_path}/");
        t.execute(
            "UPDATE entries SET path = ?1 || substr(path, length(?2) + 1)
             WHERE library = ?3 AND substr(path, 1, length(?2)) = ?2",
            params![new_prefix, old_prefix, row.library],
        )?;
    }
    Ok(())
}

/// Deletes an entry and everything below it, recording a `deleted` change for each.
pub fn delete_tree(t: &Connection, row: &EntryRow) -> rusqlite::Result<Vec<String>> {
    let mut gone = descendants(t, &row.library, &row.path)?;
    gone.push(row.clone());
    for e in &gone {
        t.execute("DELETE FROM entries WHERE rowid = ?", [e.rowid])?;
        record_change(
            t,
            ChangeType::Deleted,
            &e.id,
            &e.library,
            e.parent_id.as_deref(),
        )?;
    }
    Ok(gone.into_iter().map(|e| e.id).collect())
}

pub fn record_change(
    t: &Connection,
    kind: ChangeType,
    entry: &str,
    library: &str,
    parent: Option<&str>,
) -> rusqlite::Result<i64> {
    let ty = match kind {
        ChangeType::Created => 0,
        ChangeType::Updated => 1,
        ChangeType::Deleted => 2,
    };
    t.execute(
        "INSERT INTO changes (time, type, entry, library, parent) VALUES (?, ?, ?, ?, ?)",
        params![crate::util::now_secs(), ty, entry, library, parent],
    )?;
    Ok(t.last_insert_rowid())
}

pub fn change_type(v: i64) -> ChangeType {
    match v {
        0 => ChangeType::Created,
        1 => ChangeType::Updated,
        _ => ChangeType::Deleted,
    }
}

/// The last sequence number handed out (kept by SQLite even after old changes are pruned).
pub fn current_seq(c: &Connection) -> rusqlite::Result<i64> {
    // sqlite_sequence only exists once a change has been recorded.
    match c.query_row(
        "SELECT seq FROM sqlite_sequence WHERE name = 'changes'",
        [],
        |r| r.get(0),
    ) {
        Ok(v) => Ok(v),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(0),
        Err(rusqlite::Error::SqliteFailure(_, Some(m))) if m.contains("no such table") => Ok(0),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod migration_tests {
    #[test]
    fn an_older_index_gains_new_columns() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("index.db");
        // The schema as it was before `hash_deferred`.
        let old = super::SCHEMA.replace(
            ",\n    -- hashing failed (unreadable, or changed while read): left until the next start, which retries it\n    hash_deferred INTEGER NOT NULL DEFAULT 0",
            "",
        );
        assert_ne!(old, super::SCHEMA);
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch(&old)
            .unwrap();
        let db = super::Db::open(&path).unwrap();
        let has: bool = db
            .read(|c| {
                Ok(c.query_row(
                    "SELECT COUNT(*) > 0 FROM pragma_table_info('entries') WHERE name = 'hash_deferred'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert!(has);
        drop(db);
        super::Db::open(&path).unwrap(); // and opening again is fine
    }
}
