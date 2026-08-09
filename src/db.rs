//! The catalog: a rebuildable cache, never the only copy of anything.
//!
//! Everything here can be regenerated from the drive with `photo scan --full`.
//! That is the whole stability argument: losing this file costs time, not work.
//! Tags, ratings and titles live in sidecars; this is an index over them.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: i64 = 4;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS volume (
    id        INTEGER PRIMARY KEY,
    vol_uuid  TEXT NOT NULL UNIQUE,
    label     TEXT,
    last_root TEXT,
    last_seen INTEGER
);

-- One event per day of shooting, derived from capture times. `name` is the
-- user's and survives re-derivation; everything else is regenerated.
CREATE TABLE IF NOT EXISTS event (
    id         INTEGER PRIMARY KEY,
    name       TEXT,
    start_time INTEGER NOT NULL UNIQUE,
    end_time   INTEGER,
    auto       INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE IF NOT EXISTS stack (
    id            INTEGER PRIMARY KEY,
    dir           TEXT NOT NULL,
    base          TEXT NOT NULL,
    event_id      INTEGER REFERENCES event(id),
    sidecar_mtime INTEGER,
    rating        INTEGER,
    flag          INTEGER NOT NULL DEFAULT 0,
    reject        INTEGER NOT NULL DEFAULT 0,
    title         TEXT,
    description   TEXT,
    orientation   INTEGER,
    dirty         INTEGER NOT NULL DEFAULT 0,
    UNIQUE(dir, base)
);

CREATE TABLE IF NOT EXISTS file (
    id               INTEGER PRIMARY KEY,
    stack_id         INTEGER NOT NULL REFERENCES stack(id) ON DELETE CASCADE,
    name             TEXT NOT NULL,
    ext              TEXT,
    role             TEXT NOT NULL,
    size             INTEGER,
    mtime_ns         INTEGER,
    capture_time     INTEGER,
    camera           TEXT,
    lens             TEXT,
    iso              INTEGER,
    fnumber          REAL,
    exposure         REAL,
    focal            REAL,
    exposure_bias    REAL,
    width            INTEGER,
    height           INTEGER,
    exif_orientation INTEGER,
    serial           TEXT,
    shutter_count    INTEGER,
    content_hash     TEXT,
    present          INTEGER NOT NULL DEFAULT 1,
    UNIQUE(stack_id, name)
);

CREATE TABLE IF NOT EXISTS tag (
    id   INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE COLLATE NOCASE
);

CREATE TABLE IF NOT EXISTS stack_tag (
    stack_id INTEGER NOT NULL REFERENCES stack(id) ON DELETE CASCADE,
    tag_id   INTEGER NOT NULL REFERENCES tag(id)   ON DELETE CASCADE,
    PRIMARY KEY (stack_id, tag_id)
);

CREATE TABLE IF NOT EXISTS pending (
    stack_id  INTEGER PRIMARY KEY REFERENCES stack(id) ON DELETE CASCADE,
    queued_at INTEGER
);

CREATE TABLE IF NOT EXISTS trashed (
    stack_id   INTEGER PRIMARY KEY REFERENCES stack(id) ON DELETE CASCADE,
    orig_dir   TEXT,
    trashed_at INTEGER
);

-- Orphans: a profile or sidecar whose image members are all gone. Recorded
-- rather than ignored so `photo doctor` can report them.
CREATE TABLE IF NOT EXISTS orphan (
    path   TEXT PRIMARY KEY,
    kind   TEXT NOT NULL,
    seen_at INTEGER
);

-- The index that makes select-by-tag instant: given a tag, find its stacks.
CREATE INDEX IF NOT EXISTS stack_tag_rev ON stack_tag(tag_id, stack_id);
CREATE INDEX IF NOT EXISTS file_capture  ON file(capture_time);
CREATE INDEX IF NOT EXISTS file_dedupe   ON file(serial, shutter_count);
CREATE INDEX IF NOT EXISTS file_stack    ON file(stack_id);
CREATE INDEX IF NOT EXISTS stack_dir     ON stack(dir);
"#;

pub struct Catalog {
    pub conn: Connection,
    pub path: PathBuf,
}

impl Catalog {
    /// Open (or create) the catalog for one library.
    ///
    /// Keyed by volume UUID **and** library root. Keying on the volume alone
    /// was wrong: two libraries on one filesystem then shared a catalog, and
    /// import saw every incoming file as a duplicate of the other library's.
    /// Both components are known without the drive mounted, so offline queries
    /// still work.
    pub fn open_for(vol_uuid: &str, root: &Path) -> Result<Self> {
        let dir = directories::ProjectDirs::from("", "", "photo-catalog")
            .map(|d| d.data_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from(".photo-catalog"));
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating catalog directory {}", dir.display()))?;

        let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let digest = blake3::hash(canonical.to_string_lossy().as_bytes());
        let short = &digest.to_hex()[..8];

        // A readable fragment of the path keeps the directory browsable.
        let label: String = canonical
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "root".into())
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
            .take(24)
            .collect();

        Self::open_at(dir.join(format!("{vol_uuid}-{label}-{short}.sqlite")))
    }

    pub fn open_at(path: PathBuf) -> Result<Self> {
        let conn = Connection::open(&path)
            .with_context(|| format!("opening catalog {}", path.display()))?;

        // WAL plus NORMAL: a crash can lose the last transaction, never the
        // database. Since the catalog is rebuildable that trade is free.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        conn.execute_batch(SCHEMA)?;

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap_or(0);
        migrate(&conn, version, &path)?;

        Ok(Self { conn, path })
    }

    /// In-memory catalog, for tests.
    pub fn open_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(Self {
            conn,
            path: PathBuf::from(":memory:"),
        })
    }

    pub fn register_volume(&self, uuid: &str, label: Option<&str>, root: &Path) -> Result<i64> {
        let now = now_secs();
        self.conn.execute(
            "INSERT INTO volume (vol_uuid, label, last_root, last_seen)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(vol_uuid) DO UPDATE SET
                 label = excluded.label,
                 last_root = excluded.last_root,
                 last_seen = excluded.last_seen",
            rusqlite::params![uuid, label, root.to_string_lossy(), now],
        )?;
        let id: i64 = self.conn.query_row(
            "SELECT id FROM volume WHERE vol_uuid = ?1",
            [uuid],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    pub fn stack_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM stack", [], |r| r.get(0))?)
    }

    pub fn file_count(&self) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM file WHERE present = 1",
            [],
            |r| r.get(0),
        )?)
    }

    pub fn orphan_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM orphan", [], |r| r.get(0))?)
    }

    /// Existing `(size, mtime_ns)` for every known file, keyed by full path.
    /// Loaded in one query so the scan can reject unchanged files without
    /// opening them -- the single thing that keeps a rescan fast.
    pub fn fingerprints(&self) -> Result<std::collections::HashMap<String, (i64, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.dir || '/' || f.name, f.size, f.mtime_ns
             FROM file f JOIN stack s ON s.id = f.stack_id",
        )?;
        let mut map = std::collections::HashMap::new();
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        for row in rows {
            let (p, s, m) = row?;
            map.insert(p, (s, m));
        }
        Ok(map)
    }

    pub fn stack_id(&self, dir: &str, base: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM stack WHERE dir = ?1 AND base = ?2",
                rusqlite::params![dir, base],
                |r| r.get(0),
            )
            .optional()?)
    }
}

/// Bring an existing catalog up to date. The catalog is rebuildable, so a
/// migration that cannot be applied is not a disaster -- but silently
/// refusing to open would strand tags that only exist in sidecars until the
/// user works out they need a rescan, so migrate in place where possible.
fn migrate(conn: &Connection, from: i64, path: &Path) -> Result<()> {
    if from == 0 || from == SCHEMA_VERSION {
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        return Ok(());
    }
    if from > SCHEMA_VERSION {
        anyhow::bail!(
            "catalog {} was written by a newer build (schema {from}, this build expects \
             {SCHEMA_VERSION}). Delete it to rebuild, or use the newer binary.",
            path.display()
        );
    }

    // v1 -> v2: exposure compensation. Backfilled by `photo scan --full`;
    // until then the column is simply NULL and the UI shows nothing.
    if from < 2 {
        let has_column: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('file') WHERE name = 'exposure_bias'")?
            .exists([])?;
        if !has_column {
            conn.execute("ALTER TABLE file ADD COLUMN exposure_bias REAL", [])?;
        }
    }

    // v2 -> v3: RawTherapee profiles are no longer a concept. Existing
    // catalogs carry rows for them, marked absent by the first scan after the
    // change. They are harmless -- nothing counts an absent row -- but they
    // describe something the model no longer has, and `doctor` would later
    // have to explain them.
    if from < 3 {
        conn.execute("DELETE FROM file WHERE role = 'profile'", [])?;
        conn.execute("DELETE FROM orphan WHERE kind = 'profile'", [])?;
    }

    // v3 -> v4: events gain a unique day key. The table was never populated,
    // so there is nothing to preserve -- but a UNIQUE column cannot be added
    // by ALTER, so it is rebuilt.
    if from < 4 {
        conn.execute("DROP TABLE IF EXISTS event", [])?;
        conn.execute_batch(
            "CREATE TABLE event (
                 id         INTEGER PRIMARY KEY,
                 name       TEXT,
                 start_time INTEGER NOT NULL UNIQUE,
                 end_time   INTEGER,
                 auto       INTEGER NOT NULL DEFAULT 1
             );",
        )?;
        conn.execute("UPDATE stack SET event_id = NULL", [])?;
    }

    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_applies_and_is_idempotent() {
        let cat = Catalog::open_memory().unwrap();
        cat.conn.execute_batch(SCHEMA).unwrap(); // re-applying must not fail
        assert_eq!(cat.stack_count().unwrap(), 0);
    }

    #[test]
    fn deleting_a_stack_takes_its_files_and_tags_with_it() {
        let cat = Catalog::open_memory().unwrap();
        cat.conn
            .execute("INSERT INTO stack (dir, base) VALUES ('/d', 'b')", [])
            .unwrap();
        let sid = cat.conn.last_insert_rowid();
        cat.conn
            .execute(
                "INSERT INTO file (stack_id, name, role) VALUES (?1, 'b.NEF', 'raw')",
                [sid],
            )
            .unwrap();
        cat.conn
            .execute("INSERT INTO tag (name) VALUES ('beach')", [])
            .unwrap();
        let tid = cat.conn.last_insert_rowid();
        cat.conn
            .execute(
                "INSERT INTO stack_tag (stack_id, tag_id) VALUES (?1, ?2)",
                [sid, tid],
            )
            .unwrap();

        cat.conn
            .execute("DELETE FROM stack WHERE id = ?1", [sid])
            .unwrap();

        assert_eq!(cat.file_count().unwrap(), 0);
        let links: i64 = cat
            .conn
            .query_row("SELECT COUNT(*) FROM stack_tag", [], |r| r.get(0))
            .unwrap();
        assert_eq!(links, 0);
        // The tag itself survives; it may be used elsewhere.
        let tags: i64 = cat
            .conn
            .query_row("SELECT COUNT(*) FROM tag", [], |r| r.get(0))
            .unwrap();
        assert_eq!(tags, 1);
    }

    #[test]
    fn tag_names_are_case_insensitive_for_uniqueness() {
        let cat = Catalog::open_memory().unwrap();
        cat.conn
            .execute("INSERT INTO tag (name) VALUES ('Beach')", [])
            .unwrap();
        let dup = cat
            .conn
            .execute("INSERT INTO tag (name) VALUES ('beach')", []);
        assert!(dup.is_err(), "tag uniqueness must ignore case");
    }
}
