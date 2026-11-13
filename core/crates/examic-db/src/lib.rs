//! The library database: which recordings exist, their bookmarks and where
//! speech occurs in them.
//!
//! The whole file is encrypted by SQLCipher (AES-256 with a per-page HMAC)
//! under a raw 256-bit key. A raw key skips SQLCipher's password stretching,
//! which would cost a quarter of a second on every app start to protect a key
//! that is already uniformly random.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

// Not called from Rust: naming the crate is what makes the linker pick up the
// SQLCipher library it builds, and the system libraries that needs.
use examic_sqlcipher as _;
use rusqlite::{Connection, OptionalExtension, Row, params};
use zeroize::Zeroizing;

/// One entry per schema version; entry `n` upgrades version `n` to `n + 1`.
/// Shipped entries are never edited, only appended to.
const MIGRATIONS: &[&str] = &["
    CREATE TABLE recordings (
        id            TEXT PRIMARY KEY,
        title         TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL,
        duration_ms   INTEGER NOT NULL DEFAULT 0,
        sample_rate   INTEGER NOT NULL,
        audio_file    TEXT NOT NULL,
        size_bytes    INTEGER NOT NULL DEFAULT 0,
        status        TEXT NOT NULL CHECK (status IN ('RECORDING', 'READY', 'RECOVERED'))
    ) STRICT;
    CREATE INDEX recordings_by_date ON recordings (created_at_ms DESC);

    CREATE TABLE bookmarks (
        id           INTEGER PRIMARY KEY,
        recording_id TEXT NOT NULL REFERENCES recordings (id) ON DELETE CASCADE,
        timestamp_ms INTEGER NOT NULL,
        note         TEXT
    ) STRICT;
    CREATE INDEX bookmarks_by_recording ON bookmarks (recording_id, timestamp_ms);

    CREATE TABLE speech_intervals (
        recording_id TEXT NOT NULL REFERENCES recordings (id) ON DELETE CASCADE,
        start_ms     INTEGER NOT NULL,
        end_ms       INTEGER NOT NULL,
        PRIMARY KEY (recording_id, start_ms)
    ) STRICT, WITHOUT ROWID;
"];

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("the database cannot be opened with this key")]
    WrongKey,
    #[error("the SQLite library in this build cannot encrypt")]
    EncryptionUnavailable,
    #[error("database was written by a newer version of the app (schema {0})")]
    TooNew(i64),
    #[error("recording not found")]
    NotFound,
}

pub type Result<T> = std::result::Result<T, DbError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingStatus {
    /// Capture is in progress, or the app died before it could finish.
    Recording,
    Ready,
    /// Salvaged after an interrupted capture; the end may be missing.
    Recovered,
}

impl RecordingStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Recording => "RECORDING",
            Self::Ready => "READY",
            Self::Recovered => "RECOVERED",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "READY" => Self::Ready,
            "RECOVERED" => Self::Recovered,
            _ => Self::Recording,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recording {
    pub id: String,
    pub title: String,
    pub created_at_ms: i64,
    pub duration_ms: i64,
    pub sample_rate: u32,
    /// File name inside the recordings directory. Relative, so the library
    /// survives the app's data directory moving.
    pub audio_file: String,
    pub size_bytes: i64,
    pub status: RecordingStatus,
    pub bookmark_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bookmark {
    pub id: i64,
    pub recording_id: String,
    pub timestamp_ms: i64,
    pub note: Option<String>,
}

const RECORDING_COLUMNS: &str = "
    id, title, created_at_ms, duration_ms, sample_rate, audio_file, size_bytes, status,
    (SELECT COUNT(*) FROM bookmarks WHERE recording_id = recordings.id)";

fn recording_from_row(row: &Row) -> rusqlite::Result<Recording> {
    Ok(Recording {
        id: row.get(0)?,
        title: row.get(1)?,
        created_at_ms: row.get(2)?,
        duration_ms: row.get(3)?,
        sample_rate: row.get(4)?,
        audio_file: row.get(5)?,
        size_bytes: row.get(6)?,
        status: RecordingStatus::parse(&row.get::<_, String>(7)?),
        bookmark_count: row.get(8)?,
    })
}

pub struct Database {
    conn: Mutex<Connection>,
}

impl Database {
    /// Opens (creating if needed) the database at `path`, encrypted with `key`.
    pub fn open(path: &Path, key: &[u8; 32]) -> Result<Self> {
        let conn = Connection::open(path)?;
        apply_key(&conn, key)?;
        Self::initialise(conn)
    }

    /// A private, unencrypted in-memory database, for tests.
    pub fn open_in_memory() -> Result<Self> {
        Self::initialise(Connection::open_in_memory()?)
    }

    fn initialise(mut conn: Connection) -> Result<Self> {
        // The first real read is where a wrong key shows up.
        conn.query_row("SELECT COUNT(*) FROM sqlite_master", [], |_| Ok(()))
            .map_err(|e| match e {
                rusqlite::Error::SqliteFailure(f, _)
                    if f.code == rusqlite::ErrorCode::NotADatabase =>
                {
                    DbError::WrongKey
                }
                other => DbError::Sqlite(other),
            })?;

        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        migrate(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        // A panic while holding the lock cannot leave SQLite inconsistent:
        // every multi-statement change runs in a transaction.
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn create_recording(
        &self,
        id: &str,
        title: &str,
        created_at_ms: i64,
        sample_rate: u32,
        audio_file: &str,
    ) -> Result<()> {
        self.conn().execute(
            "INSERT INTO recordings (id, title, created_at_ms, sample_rate, audio_file, status)
             VALUES (?1, ?2, ?3, ?4, ?5, 'RECORDING')",
            params![id, title, created_at_ms, sample_rate, audio_file],
        )?;
        Ok(())
    }

    /// Records the outcome of a capture and where speech was found in it.
    pub fn finish_recording(
        &self,
        id: &str,
        status: RecordingStatus,
        duration_ms: i64,
        size_bytes: i64,
        speech: &[(i64, i64)],
    ) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let updated = tx.execute(
            "UPDATE recordings SET status = ?2, duration_ms = ?3, size_bytes = ?4 WHERE id = ?1",
            params![id, status.as_str(), duration_ms, size_bytes],
        )?;
        if updated == 0 {
            return Err(DbError::NotFound);
        }
        tx.execute("DELETE FROM speech_intervals WHERE recording_id = ?1", [id])?;
        {
            let mut insert = tx.prepare(
                "INSERT OR REPLACE INTO speech_intervals (recording_id, start_ms, end_ms)
                 VALUES (?1, ?2, ?3)",
            )?;
            for &(start_ms, end_ms) in speech {
                insert.execute(params![id, start_ms, end_ms])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn recording(&self, id: &str) -> Result<Recording> {
        self.conn()
            .query_row(
                &format!("SELECT {RECORDING_COLUMNS} FROM recordings WHERE id = ?1"),
                [id],
                recording_from_row,
            )
            .optional()?
            .ok_or(DbError::NotFound)
    }

    /// Every recording, newest first.
    pub fn recordings(&self) -> Result<Vec<Recording>> {
        self.select_recordings("ORDER BY created_at_ms DESC, id DESC")
    }

    /// Recordings whose capture never finished.
    pub fn unfinished_recordings(&self) -> Result<Vec<Recording>> {
        self.select_recordings("WHERE status = 'RECORDING'")
    }

    fn select_recordings(&self, clause: &str) -> Result<Vec<Recording>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {RECORDING_COLUMNS} FROM recordings {clause}"
        ))?;
        let rows = stmt.query_map([], recording_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn rename_recording(&self, id: &str, title: &str) -> Result<()> {
        let updated = self.conn().execute(
            "UPDATE recordings SET title = ?2 WHERE id = ?1",
            params![id, title],
        )?;
        if updated == 0 {
            return Err(DbError::NotFound);
        }
        Ok(())
    }

    /// Deletes a recording with its bookmarks and intervals and returns the
    /// name of its audio file, which the caller removes from disk.
    pub fn delete_recording(&self, id: &str) -> Result<String> {
        self.conn()
            .query_row(
                "DELETE FROM recordings WHERE id = ?1 RETURNING audio_file",
                [id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(DbError::NotFound)
    }

    pub fn add_bookmark(
        &self,
        recording_id: &str,
        timestamp_ms: i64,
        note: Option<&str>,
    ) -> Result<i64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO bookmarks (recording_id, timestamp_ms, note) VALUES (?1, ?2, ?3)",
            params![recording_id, timestamp_ms, note],
        )
        .map_err(|e| match e {
            rusqlite::Error::SqliteFailure(f, _)
                if f.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                DbError::NotFound
            }
            other => DbError::Sqlite(other),
        })?;
        Ok(conn.last_insert_rowid())
    }

    /// Bookmarks of a recording in timeline order.
    pub fn bookmarks(&self, recording_id: &str) -> Result<Vec<Bookmark>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, recording_id, timestamp_ms, note FROM bookmarks
             WHERE recording_id = ?1 ORDER BY timestamp_ms, id",
        )?;
        let rows = stmt.query_map([recording_id], |row| {
            Ok(Bookmark {
                id: row.get(0)?,
                recording_id: row.get(1)?,
                timestamp_ms: row.get(2)?,
                note: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn delete_bookmark(&self, id: i64) -> Result<()> {
        self.conn()
            .execute("DELETE FROM bookmarks WHERE id = ?1", [id])?;
        Ok(())
    }

    /// `(start_ms, end_ms)` of each stretch of speech, in timeline order.
    pub fn speech_intervals(&self, recording_id: &str) -> Result<Vec<(i64, i64)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT start_ms, end_ms FROM speech_intervals
             WHERE recording_id = ?1 ORDER BY start_ms",
        )?;
        let rows = stmt.query_map([recording_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

fn apply_key(conn: &Connection, key: &[u8; 32]) -> Result<()> {
    let mut pragma = Zeroizing::new(String::with_capacity(80));
    pragma.push_str("PRAGMA key = \"x'");
    for byte in key {
        pragma.push_str(&format!("{byte:02x}"));
    }
    pragma.push_str("'\"");
    conn.execute_batch(&pragma)?;

    // Plain SQLite silently ignores `PRAGMA key`. Refuse to continue unless
    // the library really is SQLCipher, or the file would be written in clear.
    let cipher_version: Option<String> = conn
        .query_row("PRAGMA cipher_version", [], |row| row.get(0))
        .optional()?;
    if cipher_version.is_none() {
        return Err(DbError::EncryptionUnavailable);
    }
    Ok(())
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > MIGRATIONS.len() as i64 {
        return Err(DbError::TooNew(version));
    }
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", index as i64 + 1)?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Database {
        Database::open_in_memory().unwrap()
    }

    fn add(db: &Database, id: &str, created_at_ms: i64) {
        db.create_recording(
            id,
            &format!("Title {id}"),
            created_at_ms,
            48_000,
            &format!("{id}.exa"),
        )
        .unwrap();
    }

    #[test]
    fn a_new_recording_starts_unfinished() {
        let db = db();
        add(&db, "a", 1_000);
        let rec = db.recording("a").unwrap();
        assert_eq!(rec.title, "Title a");
        assert_eq!(rec.status, RecordingStatus::Recording);
        assert_eq!(
            (rec.duration_ms, rec.size_bytes, rec.bookmark_count),
            (0, 0, 0)
        );
        assert_eq!(rec.sample_rate, 48_000);
        assert_eq!(db.unfinished_recordings().unwrap(), [rec]);
    }

    #[test]
    fn finishing_stores_totals_and_speech() {
        let db = db();
        add(&db, "a", 1_000);
        db.finish_recording(
            "a",
            RecordingStatus::Ready,
            60_000,
            240_000,
            &[(500, 900), (100, 400)],
        )
        .unwrap();
        let rec = db.recording("a").unwrap();
        assert_eq!(rec.status, RecordingStatus::Ready);
        assert_eq!((rec.duration_ms, rec.size_bytes), (60_000, 240_000));
        assert_eq!(db.speech_intervals("a").unwrap(), [(100, 400), (500, 900)]);
        assert!(db.unfinished_recordings().unwrap().is_empty());

        // Finishing again (a recovery re-run) replaces, not duplicates.
        db.finish_recording("a", RecordingStatus::Recovered, 50_000, 1, &[(0, 10)])
            .unwrap();
        assert_eq!(db.speech_intervals("a").unwrap(), [(0, 10)]);
        assert_eq!(
            db.recording("a").unwrap().status,
            RecordingStatus::Recovered
        );
    }

    #[test]
    fn lists_newest_first() {
        let db = db();
        add(&db, "old", 1_000);
        add(&db, "new", 3_000);
        add(&db, "mid", 2_000);
        let ids: Vec<_> = db.recordings().unwrap().into_iter().map(|r| r.id).collect();
        assert_eq!(ids, ["new", "mid", "old"]);
    }

    #[test]
    fn bookmarks_are_ordered_and_counted() {
        let db = db();
        add(&db, "a", 1_000);
        let late = db.add_bookmark("a", 9_000, Some("decision")).unwrap();
        let early = db.add_bookmark("a", 2_000, None).unwrap();
        let marks = db.bookmarks("a").unwrap();
        assert_eq!(
            marks.iter().map(|b| b.id).collect::<Vec<_>>(),
            [early, late]
        );
        assert_eq!(marks[0].note, None);
        assert_eq!(marks[1].note.as_deref(), Some("decision"));
        assert_eq!(db.recording("a").unwrap().bookmark_count, 2);

        db.delete_bookmark(early).unwrap();
        assert_eq!(db.recording("a").unwrap().bookmark_count, 1);
    }

    #[test]
    fn deleting_a_recording_removes_everything_that_hangs_off_it() {
        let db = db();
        add(&db, "a", 1_000);
        add(&db, "b", 2_000);
        db.add_bookmark("a", 1, None).unwrap();
        db.add_bookmark("b", 1, None).unwrap();
        db.finish_recording("a", RecordingStatus::Ready, 1, 1, &[(0, 1)])
            .unwrap();

        assert_eq!(db.delete_recording("a").unwrap(), "a.exa");
        assert!(matches!(db.recording("a"), Err(DbError::NotFound)));
        assert!(db.bookmarks("a").unwrap().is_empty());
        assert!(db.speech_intervals("a").unwrap().is_empty());
        assert_eq!(db.bookmarks("b").unwrap().len(), 1);
    }

    #[test]
    fn missing_recordings_are_reported_not_ignored() {
        let db = db();
        assert!(matches!(db.recording("x"), Err(DbError::NotFound)));
        assert!(matches!(
            db.rename_recording("x", "t"),
            Err(DbError::NotFound)
        ));
        assert!(matches!(db.delete_recording("x"), Err(DbError::NotFound)));
        assert!(matches!(
            db.add_bookmark("x", 0, None),
            Err(DbError::NotFound)
        ));
        assert!(matches!(
            db.finish_recording("x", RecordingStatus::Ready, 0, 0, &[]),
            Err(DbError::NotFound)
        ));
    }

    #[test]
    fn rename_changes_only_the_title() {
        let db = db();
        add(&db, "a", 1_000);
        db.rename_recording("a", "Board meeting").unwrap();
        let rec = db.recording("a").unwrap();
        assert_eq!(
            (rec.title.as_str(), rec.audio_file.as_str()),
            ("Board meeting", "a.exa")
        );
    }

    #[test]
    fn the_file_on_disk_is_encrypted_and_needs_its_key() {
        let dir = std::env::temp_dir().join(format!("examic-db-key-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("keyed.db");
        let _ = std::fs::remove_file(&path);
        {
            let db = Database::open(&path, &[1; 32]).unwrap();
            db.create_recording("a", "Quarterly budget review", 1, 48_000, "a.exa")
                .unwrap();
        }
        let raw = std::fs::read(&path).unwrap();
        assert!(!raw.starts_with(b"SQLite format 3"));
        assert!(
            !raw.windows(6).any(|w| w == b"budget"),
            "title stored in clear"
        );
        assert!(matches!(
            Database::open(&path, &[2; 32]),
            Err(DbError::WrongKey)
        ));
        let reopened = Database::open(&path, &[1; 32]).unwrap();
        assert_eq!(reopened.recordings().unwrap().len(), 1);
        drop(reopened);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn data_survives_reopening_and_migrations_run_once() {
        let dir = std::env::temp_dir().join(format!("examic-db-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("reopen.db");
        let _ = std::fs::remove_file(&path);
        {
            let db = Database::open(&path, &[1; 32]).unwrap();
            add(&db, "a", 1_000);
        }
        let db = Database::open(&path, &[1; 32]).unwrap();
        assert_eq!(db.recordings().unwrap().len(), 1);
        drop(db);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn refuses_a_database_from_a_newer_app() {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
        assert!(matches!(
            Database::initialise(conn),
            Err(DbError::TooNew(99))
        ));
    }
}
