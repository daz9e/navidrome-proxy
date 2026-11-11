//! Durable proxy state in SQLite.
//!
//! Every mutation is a small transaction in a WAL-mode database. Navidrome stays
//! the owner of the library and of likes/playlists; the proxy only stores what
//! Navidrome cannot know: external metadata, the mapping from an external track to
//! its file / Navidrome song id, the download queue and user intents that wait for
//! a track to become local.
//!
//! The schema and the JSON shapes in its columns are shared with the Python
//! generation of the proxy: an existing `proxy.sqlite3` opens unchanged.

use crate::util::{Json, now};
use parking_lot::Mutex;
use rusqlite::{
    Connection, OptionalExtension, Row, TransactionBehavior, params, params_from_iter,
    types::Value as Sql,
};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: i64 = 1;

/// Schema changes after version 1, keyed by the version they produce. `SCHEMA`
/// always describes the latest layout (new databases are created from it and then
/// stamped with SCHEMA_VERSION); an existing database runs every step above its
/// stored version, each in its own transaction.
const MIGRATIONS: &[(i64, &[&str])] = &[];

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("{0}")]
    SchemaTooNew(String),
    #[error("{0}")]
    Busy(String),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, DbError>;

/// One process at a time per state database, across containers too.
///
/// Twice a database got corrupted while the server and a maintenance container
/// wrote it at the same time, so the server and every command take this `flock`
/// first; the second one is refused instead of sharing the file.
pub struct OwnerLock {
    path: PathBuf,
    file: Option<std::fs::File>,
}

impl OwnerLock {
    pub fn new(db_path: &Path) -> Self {
        Self {
            path: PathBuf::from(format!("{}.lock", db_path.display())),
            file: None,
        }
    }

    pub fn acquire(&mut self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.path)?;
        // SAFETY: flock on a valid, owned descriptor.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(DbError::Busy(format!(
                "{} is held by another navidrome-proxy process; stop it first",
                self.path.display()
            )));
        }
        self.file = Some(file);
        Ok(())
    }

    pub fn release(&mut self) {
        // Closing the descriptor releases the lock.
        self.file = None;
    }
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);

CREATE TABLE IF NOT EXISTS tracks (
    id TEXT PRIMARY KEY,
    album_id TEXT,
    meta TEXT NOT NULL DEFAULT '{}',
    status TEXT NOT NULL DEFAULT 'new',
    file TEXT,
    local_id TEXT,
    source TEXT,
    source_ref TEXT,
    audio TEXT,
    error TEXT,
    attempts INTEGER NOT NULL DEFAULT 0,
    created_at REAL NOT NULL,
    updated_at REAL NOT NULL
);
CREATE INDEX IF NOT EXISTS tracks_album ON tracks(album_id);
CREATE INDEX IF NOT EXISTS tracks_local ON tracks(local_id);
CREATE INDEX IF NOT EXISTS tracks_status ON tracks(status);

-- Other identities of the same recording: "deezer:1", "yt:abc", "sc:2", "isrc:USUM1".
CREATE TABLE IF NOT EXISTS aliases (
    alias TEXT PRIMARY KEY,
    track_id TEXT NOT NULL,
    how TEXT NOT NULL,
    created_at REAL NOT NULL
);
CREATE INDEX IF NOT EXISTS aliases_track ON aliases(track_id);

-- "primary artist|base title" keys, for spotting one song under different ids.
CREATE TABLE IF NOT EXISTS recordings (
    key TEXT NOT NULL,
    track_id TEXT NOT NULL,
    duration REAL NOT NULL,
    PRIMARY KEY (key, track_id)
);

CREATE TABLE IF NOT EXISTS jobs (
    track_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    priority INTEGER NOT NULL,
    state TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    not_before REAL NOT NULL DEFAULT 0,
    enqueued_at REAL NOT NULL,
    reason TEXT,
    error TEXT,
    PRIMARY KEY (track_id, kind)
);
CREATE INDEX IF NOT EXISTS jobs_state ON jobs(state, priority, enqueued_at);

CREATE TABLE IF NOT EXISTS pending_stars (
    username TEXT NOT NULL,
    target TEXT NOT NULL,
    created_at REAL NOT NULL,
    PRIMARY KEY (username, target)
);

-- Likes the proxy set by mistake (a wrong import match), to take back as that user.
CREATE TABLE IF NOT EXISTS pending_unstars (
    username TEXT NOT NULL,
    local_id TEXT NOT NULL,
    created_at REAL NOT NULL,
    PRIMARY KEY (username, local_id)
);

CREATE TABLE IF NOT EXISTS pending_scrobbles (
    username TEXT NOT NULL,
    track_id TEXT NOT NULL,
    time INTEGER NOT NULL,
    submission INTEGER NOT NULL,
    PRIMARY KEY (username, track_id, time, submission)
);

CREATE TABLE IF NOT EXISTS pending_playlist_adds (
    username TEXT NOT NULL,
    playlist_id TEXT NOT NULL,
    track_id TEXT NOT NULL,
    created_at REAL NOT NULL,
    PRIMARY KEY (username, playlist_id, track_id)
);

CREATE TABLE IF NOT EXISTS cache (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    expires_at REAL NOT NULL
);

CREATE TABLE IF NOT EXISTS imports (
    id TEXT PRIMARY KEY,
    username TEXT NOT NULL,
    status TEXT NOT NULL,
    options TEXT NOT NULL DEFAULT '{}',
    rows TEXT NOT NULL DEFAULT '[]',
    created_at REAL NOT NULL,
    updated_at REAL NOT NULL
);
CREATE INDEX IF NOT EXISTS imports_user ON imports(username, created_at);
"#;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Track {
    pub id: String,
    pub meta: Json,
    pub status: String,
    pub file: Option<String>,
    pub local_id: Option<String>,
    pub source: Option<String>,
    pub source_ref: Option<String>,
    pub audio: Json,
    pub error: Option<String>,
    pub attempts: i64,
    pub album_id: Option<String>,
    pub created_at: f64,
    pub updated_at: f64,
}

impl Track {
    pub fn title(&self) -> String {
        crate::util::as_str(self.meta.get("title"))
    }

    pub fn artist(&self) -> String {
        crate::util::as_str(self.meta.get("artist"))
    }

    pub fn artists(&self) -> Vec<String> {
        if let Some(Value::Array(list)) = self.meta.get("artists")
            && !list.is_empty()
        {
            return list.iter().map(|v| crate::util::as_str(Some(v))).collect();
        }
        let artist = self.artist();
        if artist.is_empty() {
            vec![]
        } else {
            vec![artist]
        }
    }

    pub fn duration(&self) -> f64 {
        crate::util::as_f64(self.meta.get("duration")).unwrap_or(0.0)
    }

    pub fn is_ready(&self) -> bool {
        self.status == "ready"
            && self
                .file
                .as_deref()
                .is_some_and(|f| !f.is_empty() && Path::new(f).is_file())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    pub track_id: String,
    pub kind: String,
    pub priority: i64,
    pub state: String,
    pub attempts: i64,
    pub not_before: f64,
    pub enqueued_at: f64,
    pub reason: Option<String>,
    pub error: Option<String>,
}

/// A column of `tracks` that `update_track` may change.
#[derive(Debug, Clone)]
pub enum Field {
    Status(String),
    File(Option<String>),
    LocalId(Option<String>),
    Source(Option<String>),
    SourceRef(Option<String>),
    Audio(Option<Json>),
    Error(Option<String>),
    Attempts(i64),
}

impl Field {
    pub fn status(s: &str) -> Self {
        Field::Status(s.to_string())
    }

    fn column(&self) -> (&'static str, Sql) {
        let text = |v: &Option<String>| v.clone().map(Sql::Text).unwrap_or(Sql::Null);
        match self {
            Field::Status(s) => ("status", Sql::Text(s.clone())),
            Field::File(v) => ("file", text(v)),
            Field::LocalId(v) => ("local_id", text(v)),
            Field::Source(v) => ("source", text(v)),
            Field::SourceRef(v) => ("source_ref", text(v)),
            Field::Audio(v) => (
                "audio",
                v.as_ref()
                    .map(|a| Sql::Text(Value::Object(a.clone()).to_string()))
                    .unwrap_or(Sql::Null),
            ),
            Field::Error(v) => ("error", text(v)),
            Field::Attempts(n) => ("attempts", Sql::Integer(*n)),
        }
    }
}

fn json_map(text: Option<String>) -> Json {
    text.and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| match v {
            Value::Object(m) => Some(m),
            _ => None,
        })
        .unwrap_or_default()
}

fn track_row(r: &Row) -> rusqlite::Result<Track> {
    Ok(Track {
        id: r.get("id")?,
        meta: json_map(r.get("meta")?),
        status: r.get("status")?,
        file: r.get("file")?,
        local_id: r.get("local_id")?,
        source: r.get("source")?,
        source_ref: r.get("source_ref")?,
        audio: json_map(r.get("audio")?),
        error: r.get("error")?,
        attempts: r.get("attempts")?,
        album_id: r.get("album_id")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
    })
}

fn job_row(r: &Row) -> rusqlite::Result<Job> {
    Ok(Job {
        track_id: r.get("track_id")?,
        kind: r.get("kind")?,
        priority: r.get("priority")?,
        state: r.get("state")?,
        attempts: r.get("attempts")?,
        not_before: r.get("not_before")?,
        enqueued_at: r.get("enqueued_at")?,
        reason: r.get("reason")?,
        error: r.get("error")?,
    })
}

/// `album_id` is stored as text even when Deezer gave a number.
fn sql_text(v: Option<&Value>) -> Sql {
    match v {
        None | Some(Value::Null) => Sql::Null,
        Some(Value::String(s)) => Sql::Text(s.clone()),
        Some(other) => Sql::Text(other.to_string()),
    }
}

fn dedupe<'a>(items: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut seen = HashSet::new();
    items
        .into_iter()
        .filter(|i| seen.insert(*i))
        .map(String::from)
        .collect()
}

/// Thread-safe facade over one SQLite connection.
///
/// All statements are short; they are executed synchronously from async code
/// (WAL + `synchronous=NORMAL` keeps them in the sub-millisecond range),
/// serialized by one lock.
pub struct Store {
    pub path: PathBuf,
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let memory = path.as_os_str() == ":memory:";
        if !memory && let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = if memory {
            Connection::open_in_memory()?
        } else {
            Connection::open(path)?
        };
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA)?;
        let store = Self {
            path: path.to_path_buf(),
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn memory() -> Self {
        Self::open(Path::new(":memory:")).expect("in-memory database")
    }

    fn migrate(&self) -> Result<()> {
        self.tx(|c| {
            c.execute(
                "INSERT OR IGNORE INTO meta(key, value) VALUES ('schema_version', ?)",
                [SCHEMA_VERSION.to_string()],
            )
            .map(|_| ())
        })?;
        let current: i64 = self
            .get_meta("schema_version")
            .and_then(|v| v.parse().ok())
            .unwrap_or(SCHEMA_VERSION);
        if current > SCHEMA_VERSION {
            return Err(DbError::SchemaTooNew(format!(
                "{} has schema version {current}, this navidrome-proxy knows {SCHEMA_VERSION}; \
                 upgrade the proxy or restore a backup",
                self.path.display()
            )));
        }
        for (version, statements) in MIGRATIONS.iter().filter(|(v, _)| *v > current) {
            self.tx(|c| {
                for statement in *statements {
                    c.execute_batch(statement)?;
                }
                c.execute(
                    "UPDATE meta SET value = ? WHERE key = 'schema_version'",
                    [version.to_string()],
                )?;
                Ok(())
            })?;
        }
        Ok(())
    }

    /// Problems reported by SQLite (empty when the database is healthy).
    pub fn integrity(&self, quick: bool) -> Vec<String> {
        let pragma = if quick {
            "quick_check"
        } else {
            "integrity_check"
        };
        let conn = self.conn.lock();
        let result = conn
            .prepare(&format!("PRAGMA {pragma}"))
            .and_then(|mut st| {
                st.query_map([], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()
            });
        match result {
            Ok(problems) if problems == ["ok"] => vec![],
            Ok(problems) => problems,
            Err(e) => vec![e.to_string()],
        }
    }

    /// Consistent online copy into `directory` (verified), keeping the newest `keep`.
    ///
    /// Nothing is written when the live database already fails its check, so the
    /// copies taken before a corruption survive it.
    pub fn backup(&self, directory: &Path, keep: usize) -> Result<Option<PathBuf>> {
        if !self.integrity(true).is_empty() {
            return Ok(None);
        }
        std::fs::create_dir_all(directory)?;
        let stamp = local_stamp();
        let dest = directory.join(format!("proxy-{stamp}.sqlite3"));
        let tmp = dest.with_extension("tmp");
        let _ = std::fs::remove_file(&tmp);
        self.conn.lock().backup(rusqlite::MAIN_DB, &tmp, None)?;
        let ok = Connection::open(&tmp)
            .and_then(|c| c.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0)))
            .is_ok_and(|v| v == "ok");
        if !ok {
            let _ = std::fs::remove_file(&tmp);
            return Ok(None);
        }
        std::fs::rename(&tmp, &dest)?;
        let mut old: Vec<PathBuf> = std::fs::read_dir(directory)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("proxy-") && n.ends_with(".sqlite3"))
            })
            .collect();
        old.sort();
        let excess = old.len().saturating_sub(keep);
        for path in &old[..excess] {
            let _ = std::fs::remove_file(path);
        }
        Ok(Some(dest))
    }

    /// Run `f` inside `BEGIN IMMEDIATE ... COMMIT`.
    pub fn tx<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> rusqlite::Result<T> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    fn query<T>(
        &self,
        sql: &str,
        args: impl rusqlite::Params,
        map: impl FnMut(&Row) -> rusqlite::Result<T>,
    ) -> Vec<T> {
        let conn = self.conn.lock();
        let result = conn
            .prepare_cached(sql)
            .and_then(|mut st| st.query_map(args, map)?.collect());
        result.unwrap_or_else(|e| {
            tracing::error!("query failed: {e}: {sql}");
            Vec::new()
        })
    }

    fn write(&self, sql: &str, args: impl rusqlite::Params) -> usize {
        self.tx(|c| c.execute(sql, args)).unwrap_or_else(|e| {
            tracing::error!("write failed: {e}: {sql}");
            0
        })
    }

    /// `SELECT ... WHERE col IN (...)` in chunks of 500 parameters.
    fn query_in<T>(
        &self,
        sql_prefix: &str,
        keys: &[String],
        mut map: impl FnMut(&Row) -> rusqlite::Result<T>,
    ) -> Vec<T> {
        let mut out = Vec::new();
        for chunk in keys.chunks(500) {
            let marks = vec!["?"; chunk.len()].join(",");
            out.extend(self.query(
                &format!("{sql_prefix} ({marks})"),
                params_from_iter(chunk),
                &mut map,
            ));
        }
        out
    }

    // ------------------------------------------------------------------ meta
    pub fn get_meta(&self, key: &str) -> Option<String> {
        self.query("SELECT value FROM meta WHERE key = ?", [key], |r| r.get(0))
            .into_iter()
            .next()
    }

    pub fn set_meta(&self, key: &str, value: &str) {
        self.write(
            "INSERT INTO meta(key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [key, value],
        );
    }

    // ---------------------------------------------------------------- tracks
    pub fn get_track(&self, track_id: &str) -> Option<Track> {
        self.query("SELECT * FROM tracks WHERE id = ?", [track_id], track_row)
            .into_iter()
            .next()
    }

    pub fn get_tracks<'a>(&self, ids: impl IntoIterator<Item = &'a str>) -> HashMap<String, Track> {
        self.query_in("SELECT * FROM tracks WHERE id IN", &dedupe(ids), track_row)
            .into_iter()
            .map(|t| (t.id.clone(), t))
            .collect()
    }

    fn remember_in(c: &Connection, track_id: &str, meta: &Json, now: f64) -> rusqlite::Result<()> {
        let clean: Json = meta
            .iter()
            .filter(|(_, v)| !v.is_null())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let album = sql_text(clean.get("album_id"));
        let existing: Option<Option<String>> = c
            .query_row("SELECT meta FROM tracks WHERE id = ?", [track_id], |r| {
                r.get(0)
            })
            .optional()?;
        match existing {
            None => {
                c.execute(
                    "INSERT INTO tracks(id, album_id, meta, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
                    params![track_id, album, Value::Object(clean).to_string(), now, now],
                )?;
            }
            Some(old) => {
                let mut merged = json_map(old);
                merged.extend(clean);
                c.execute(
                    "UPDATE tracks SET meta = ?, album_id = COALESCE(?, album_id), updated_at = ? WHERE id = ?",
                    params![Value::Object(merged).to_string(), album, now, track_id],
                )?;
            }
        }
        Ok(())
    }

    /// Insert or refresh the external metadata of a track; never touches its state.
    pub fn remember_track(&self, track_id: &str, meta: &Json) -> Track {
        if let Err(e) = self.tx(|c| Self::remember_in(c, track_id, meta, now())) {
            tracing::error!("remember_track {track_id}: {e}");
        }
        self.get_track(track_id).unwrap_or_else(|| Track {
            id: track_id.into(),
            meta: meta.clone(),
            status: "new".into(),
            ..Default::default()
        })
    }

    pub fn remember_tracks(&self, items: &[(String, Json)]) {
        let t = now();
        if let Err(e) = self.tx(|c| {
            items
                .iter()
                .try_for_each(|(id, meta)| Self::remember_in(c, id, meta, t))
        }) {
            tracing::error!("remember_tracks: {e}");
        }
    }

    pub fn update_track(&self, track_id: &str, fields: &[Field]) {
        if fields.is_empty() {
            return;
        }
        let (names, mut values): (Vec<_>, Vec<_>) = fields.iter().map(Field::column).unzip();
        let assignments: Vec<String> = names.iter().map(|n| format!("{n} = ?")).collect();
        values.push(Sql::Real(now()));
        values.push(Sql::Text(track_id.into()));
        self.write(
            &format!(
                "UPDATE tracks SET {}, updated_at = ? WHERE id = ?",
                assignments.join(", ")
            ),
            params_from_iter(values),
        );
    }

    pub fn track_by_local_id(&self, local_id: &str) -> Option<Track> {
        self.query(
            "SELECT * FROM tracks WHERE local_id = ? LIMIT 1",
            [local_id],
            track_row,
        )
        .into_iter()
        .next()
    }

    pub fn tracks_by_local_ids<'a>(
        &self,
        ids: impl IntoIterator<Item = &'a str>,
    ) -> HashMap<String, Track> {
        let keys = dedupe(ids.into_iter().filter(|i| !i.is_empty()));
        self.query_in("SELECT * FROM tracks WHERE local_id IN", &keys, track_row)
            .into_iter()
            .filter_map(|t| Some((t.local_id.clone()?, t)))
            .collect()
    }

    pub fn tracks_in_album(&self, album_id: &str) -> Vec<Track> {
        self.query(
            "SELECT * FROM tracks WHERE album_id = ?",
            [album_id],
            track_row,
        )
    }

    pub fn ready_track_by_artist_id(&self, artist_id: &str) -> Option<Track> {
        // Deezer ids may be stored as JSON numbers or strings.
        self.query(
            "SELECT * FROM tracks WHERE status = 'ready' AND CAST(json_extract(meta, '$.artist_id') AS TEXT) = ? LIMIT 1",
            [artist_id],
            track_row,
        )
        .into_iter()
        .next()
    }

    pub fn tracks_with_status(&self, statuses: &[&str]) -> Vec<Track> {
        let keys: Vec<String> = statuses.iter().map(|s| s.to_string()).collect();
        self.query_in("SELECT * FROM tracks WHERE status IN", &keys, track_row)
    }

    pub fn ready_without_local_id(&self, limit: i64) -> Vec<Track> {
        self.query(
            "SELECT * FROM tracks WHERE status = 'ready' AND local_id IS NULL ORDER BY updated_at LIMIT ?",
            [limit],
            track_row,
        )
    }

    pub fn status_counts(&self) -> HashMap<String, i64> {
        self.query(
            "SELECT status, COUNT(*) FROM tracks GROUP BY status",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .into_iter()
        .collect()
    }

    /// After a restart nothing is downloading; such tracks wait for their requeued job.
    pub fn reset_downloading_tracks(&self) -> usize {
        self.write(
            "UPDATE tracks SET status = 'queued' WHERE status = 'downloading'",
            [],
        )
    }

    // --------------------------------------------------------------- aliases
    /// Point `alias` at `track_id`; an existing alias is never re-pointed.
    pub fn add_alias(&self, alias: &str, track_id: &str, how: &str) -> bool {
        if alias == track_id {
            return false;
        }
        self.write(
            "INSERT OR IGNORE INTO aliases VALUES (?, ?, ?, ?)",
            params![alias, track_id, how, now()],
        ) > 0
    }

    pub fn resolve_alias(&self, alias: &str) -> Option<String> {
        self.query(
            "SELECT track_id FROM aliases WHERE alias = ?",
            [alias],
            |r| r.get(0),
        )
        .into_iter()
        .next()
    }

    pub fn aliases_of(&self, track_id: &str) -> Vec<(String, String)> {
        self.query(
            "SELECT alias, how FROM aliases WHERE track_id = ? ORDER BY created_at",
            [track_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
    }

    pub fn resolve_aliases<'a>(
        &self,
        aliases: impl IntoIterator<Item = &'a str>,
    ) -> HashMap<String, String> {
        self.query_in(
            "SELECT alias, track_id FROM aliases WHERE alias IN",
            &dedupe(aliases),
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .into_iter()
        .collect()
    }

    pub fn add_recording(&self, key: &str, track_id: &str, duration: f64) {
        self.write(
            "INSERT OR REPLACE INTO recordings VALUES (?, ?, ?)",
            params![key, track_id, duration],
        );
    }

    /// Forget every recording key (they are rebuilt with `Identity::register`).
    pub fn clear_recordings(&self) {
        self.write("DELETE FROM recordings", []);
    }

    pub fn recordings(&self, key: &str) -> Vec<(String, f64)> {
        self.query(
            "SELECT track_id, duration FROM recordings WHERE key = ?",
            [key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
    }

    // ------------------------------------------------------------------ jobs
    /// Queue a job or raise the priority of an existing one.
    ///
    /// Returns true when the job is (re)queued and a worker should look at it.
    /// A running job is left alone; a failed or finished one is revived.
    pub fn enqueue(
        &self,
        track_id: &str,
        kind: &str,
        priority: i64,
        reason: Option<&str>,
        reset_attempts: bool,
    ) -> bool {
        let t = now();
        self.tx(|c| {
            let row = c
                .query_row("SELECT * FROM jobs WHERE track_id = ? AND kind = ?", [track_id, kind], job_row)
                .optional()?;
            let Some(row) = row else {
                c.execute(
                    "INSERT INTO jobs(track_id, kind, priority, state, enqueued_at, reason) VALUES (?, ?, ?, 'queued', ?, ?)",
                    params![track_id, kind, priority, t, reason],
                )?;
                return Ok(true);
            };
            match row.state.as_str() {
                "running" => {
                    if priority < row.priority {
                        c.execute(
                            "UPDATE jobs SET priority = ? WHERE track_id = ? AND kind = ?",
                            params![priority, track_id, kind],
                        )?;
                    }
                    Ok(false)
                }
                "queued" => {
                    let changed = priority < row.priority || reset_attempts;
                    if changed {
                        c.execute(
                            "UPDATE jobs SET priority = MIN(priority, ?), \
                             not_before = CASE WHEN ? THEN 0 ELSE not_before END, \
                             attempts = CASE WHEN ? THEN 0 ELSE attempts END \
                             WHERE track_id = ? AND kind = ?",
                            params![priority, reset_attempts, reset_attempts, track_id, kind],
                        )?;
                    }
                    Ok(changed)
                }
                _ => {
                    c.execute(
                        "UPDATE jobs SET state = 'queued', priority = ?, not_before = 0, enqueued_at = ?, \
                         reason = ?, error = NULL, attempts = CASE WHEN ? THEN 0 ELSE attempts END \
                         WHERE track_id = ? AND kind = ?",
                        params![priority, t, reason.or(row.reason.as_deref()), reset_attempts, track_id, kind],
                    )?;
                    Ok(true)
                }
            }
        })
        .unwrap_or_else(|e| {
            tracing::error!("enqueue {track_id}: {e}");
            false
        })
    }

    pub fn get_job(&self, track_id: &str, kind: &str) -> Option<Job> {
        self.query(
            "SELECT * FROM jobs WHERE track_id = ? AND kind = ?",
            [track_id, kind],
            job_row,
        )
        .into_iter()
        .next()
    }

    pub fn jobs_in_state(&self, state: &str) -> Vec<Job> {
        self.query(
            "SELECT * FROM jobs WHERE state = ? ORDER BY priority, enqueued_at",
            [state],
            job_row,
        )
    }

    /// Atomically move the best eligible queued job to `running`.
    pub fn claim_next_job(
        &self,
        max_priority: Option<i64>,
        min_priority: Option<i64>,
        at: Option<f64>,
    ) -> Option<Job> {
        let at = at.unwrap_or_else(now);
        let sql = "SELECT * FROM jobs WHERE state = 'queued' AND not_before <= ?1 \
                   AND priority <= ?2 AND priority >= ?3 ORDER BY priority, enqueued_at LIMIT 1";
        self.tx(|c| {
            let row = c
                .query_row(sql, params![at, max_priority.unwrap_or(i64::MAX), min_priority.unwrap_or(i64::MIN)], job_row)
                .optional()?;
            let Some(mut job) = row else { return Ok(None) };
            c.execute(
                "UPDATE jobs SET state = 'running', attempts = attempts + 1 WHERE track_id = ? AND kind = ?",
                [&job.track_id, &job.kind],
            )?;
            job.state = "running".into();
            job.attempts += 1;
            Ok(Some(job))
        })
        .unwrap_or_else(|e| {
            tracing::error!("claim_next_job: {e}");
            None
        })
    }

    pub fn finish_job(
        &self,
        track_id: &str,
        kind: &str,
        state: &str,
        error: Option<&str>,
        retry_at: Option<f64>,
    ) {
        match retry_at {
            Some(at) => self.write(
                "UPDATE jobs SET state = 'queued', not_before = ?, error = ? WHERE track_id = ? AND kind = ?",
                params![at, error, track_id, kind],
            ),
            None => self.write(
                "UPDATE jobs SET state = ?, error = ? WHERE track_id = ? AND kind = ?",
                params![state, error, track_id, kind],
            ),
        };
    }

    /// After a restart nothing is running; put interrupted jobs back in line.
    pub fn requeue_running_jobs(&self) -> usize {
        self.write(
            "UPDATE jobs SET state = 'queued', not_before = 0 WHERE state = 'running'",
            [],
        )
    }

    /// The earliest *future* retry. Jobs already due are not counted: a lane that
    /// cannot claim them (they belong to the other lane) must not busy-poll.
    pub fn next_retry_time(&self) -> Option<f64> {
        self.query(
            "SELECT MIN(not_before) FROM jobs WHERE state = 'queued' AND not_before > ?",
            [now()],
            |r| r.get::<_, Option<f64>>(0),
        )
        .into_iter()
        .next()
        .flatten()
    }

    pub fn job_counts(&self) -> HashMap<String, i64> {
        self.query(
            "SELECT kind || ':' || state, COUNT(*) FROM jobs GROUP BY kind, state",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .into_iter()
        .collect()
    }

    // --------------------------------------------------------- user intents
    pub fn add_pending_star(&self, username: &str, target: &str) {
        self.write(
            "INSERT OR IGNORE INTO pending_stars VALUES (?, ?, ?)",
            params![username, target, now()],
        );
    }

    pub fn remove_pending_star(&self, username: &str, target: &str) -> bool {
        self.write(
            "DELETE FROM pending_stars WHERE username = ? AND target = ?",
            [username, target],
        ) > 0
    }

    pub fn pending_stars(&self, username: Option<&str>) -> Vec<(String, String, f64)> {
        let map = |r: &Row| Ok((r.get(0)?, r.get(1)?, r.get(2)?));
        match username {
            None => self.query("SELECT username, target, created_at FROM pending_stars ORDER BY created_at", [], map),
            Some(u) => self.query(
                "SELECT username, target, created_at FROM pending_stars WHERE username = ? ORDER BY created_at",
                [u],
                map,
            ),
        }
    }

    pub fn add_pending_scrobble(
        &self,
        username: &str,
        track_id: &str,
        when: i64,
        submission: bool,
    ) {
        self.write(
            "INSERT OR IGNORE INTO pending_scrobbles VALUES (?, ?, ?, ?)",
            params![username, track_id, when, submission as i64],
        );
    }

    pub fn pending_scrobbles(&self, username: &str) -> Vec<(String, i64, bool)> {
        self.query(
            "SELECT track_id, time, submission FROM pending_scrobbles WHERE username = ? ORDER BY time",
            [username],
            |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? != 0)),
        )
    }

    pub fn remove_pending_scrobble(
        &self,
        username: &str,
        track_id: &str,
        when: i64,
        submission: bool,
    ) {
        self.write(
            "DELETE FROM pending_scrobbles WHERE username = ? AND track_id = ? AND time = ? AND submission = ?",
            params![username, track_id, when, submission as i64],
        );
    }

    pub fn add_pending_playlist_add(&self, username: &str, playlist_id: &str, track_id: &str) {
        self.write(
            "INSERT OR IGNORE INTO pending_playlist_adds VALUES (?, ?, ?, ?)",
            params![username, playlist_id, track_id, now()],
        );
    }

    pub fn pending_playlist_adds(&self, username: &str) -> Vec<(String, String)> {
        // rowid breaks created_at ties so one request's songs keep their order.
        self.query(
            "SELECT playlist_id, track_id FROM pending_playlist_adds WHERE username = ? ORDER BY created_at, rowid",
            [username],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
    }

    pub fn remove_pending_playlist_add(&self, username: &str, playlist_id: &str, track_id: &str) {
        self.write(
            "DELETE FROM pending_playlist_adds WHERE username = ? AND playlist_id = ? AND track_id = ?",
            [username, playlist_id, track_id],
        );
    }

    pub fn add_pending_unstar(&self, username: &str, local_id: &str) {
        self.write(
            "INSERT OR IGNORE INTO pending_unstars VALUES (?, ?, ?)",
            params![username, local_id, now()],
        );
    }

    pub fn pending_unstars(&self, username: &str) -> Vec<String> {
        self.query(
            "SELECT local_id FROM pending_unstars WHERE username = ? ORDER BY created_at",
            [username],
            |r| r.get(0),
        )
    }

    pub fn remove_pending_unstars(&self, username: &str, local_ids: &[String]) {
        let result = self.tx(|c| {
            for id in local_ids {
                c.execute(
                    "DELETE FROM pending_unstars WHERE username = ? AND local_id = ?",
                    [username, id],
                )?;
            }
            Ok(())
        });
        if let Err(e) = result {
            tracing::error!("remove_pending_unstars: {e}");
        }
    }

    pub fn users_with_pending_work(&self) -> HashSet<String> {
        self.query(
            "SELECT username FROM pending_stars UNION SELECT username FROM pending_scrobbles \
             UNION SELECT username FROM pending_playlist_adds UNION SELECT username FROM pending_unstars",
            [],
            |r| r.get(0),
        )
        .into_iter()
        .collect()
    }

    // ----------------------------------------------------------------- cache
    pub fn cache_get(&self, key: &str) -> Option<Value> {
        let (value, expires): (String, f64) = self
            .query(
                "SELECT value, expires_at FROM cache WHERE key = ?",
                [key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .into_iter()
            .next()?;
        if expires < now() {
            return None;
        }
        serde_json::from_str(&value).ok()
    }

    pub fn cache_set(&self, key: &str, value: &Value, ttl: f64) {
        self.write(
            "INSERT INTO cache(key, value, expires_at) VALUES (?, ?, ?) ON CONFLICT(key) DO UPDATE SET \
             value = excluded.value, expires_at = excluded.expires_at",
            params![key, value.to_string(), now() + ttl],
        );
    }

    pub fn cache_purge_expired(&self) -> usize {
        self.write("DELETE FROM cache WHERE expires_at < ?", [now()])
    }

    // --------------------------------------------------------------- imports
    pub fn save_import(
        &self,
        import_id: &str,
        username: &str,
        status: &str,
        options: &Value,
        rows: &Value,
    ) {
        let t = now();
        self.write(
            "INSERT INTO imports(id, username, status, options, rows, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT(id) DO UPDATE SET status = excluded.status, \
             options = excluded.options, rows = excluded.rows, updated_at = excluded.updated_at",
            params![
                import_id,
                username,
                status,
                options.to_string(),
                rows.to_string(),
                t,
                t
            ],
        );
    }

    pub fn get_import(&self, import_id: &str) -> Option<Json> {
        self.query("SELECT * FROM imports WHERE id = ?", [import_id], |r| {
            let parse = |s: String| serde_json::from_str::<Value>(&s).unwrap_or(Value::Null);
            let mut m = Json::new();
            m.insert("id".into(), Value::String(r.get("id")?));
            m.insert("username".into(), Value::String(r.get("username")?));
            m.insert("status".into(), Value::String(r.get("status")?));
            m.insert("options".into(), parse(r.get("options")?));
            m.insert("rows".into(), parse(r.get("rows")?));
            m.insert(
                "created_at".into(),
                serde_json::json!(r.get::<_, f64>("created_at")?),
            );
            m.insert(
                "updated_at".into(),
                serde_json::json!(r.get::<_, f64>("updated_at")?),
            );
            Ok(m)
        })
        .into_iter()
        .next()
    }

    pub fn list_imports(&self, username: &str, limit: i64) -> Vec<Json> {
        let ids: Vec<String> = self.query(
            "SELECT id FROM imports WHERE username = ? ORDER BY created_at DESC LIMIT ?",
            params![username, limit],
            |r| r.get(0),
        );
        ids.iter().filter_map(|id| self.get_import(id)).collect()
    }

    pub fn imports_with_playlist(&self) -> Vec<String> {
        self.query(
            "SELECT id FROM imports WHERE json_extract(options, '$.playlist') IS NOT NULL",
            [],
            |r| r.get(0),
        )
    }

    pub fn unfinished_imports(&self) -> Vec<Json> {
        let ids: Vec<String> = self.query(
            "SELECT id FROM imports WHERE status IN ('queued', 'processing') ORDER BY created_at",
            [],
            |r| r.get(0),
        );
        ids.iter().filter_map(|id| self.get_import(id)).collect()
    }
}

fn local_stamp() -> String {
    let dt = time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        dt.year(),
        dt.month() as u8,
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn m(v: Value) -> Json {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn remember_merges_meta_and_keeps_state() {
        let s = Store::memory();
        s.remember_track(
            "deezer:1",
            &m(json!({"title": "A", "album_id": 5, "x": null})),
        );
        s.update_track(
            "deezer:1",
            &[Field::status("ready"), Field::File(Some("/f".into()))],
        );
        let t = s.remember_track("deezer:1", &m(json!({"artist": "B"})));
        assert_eq!(
            t.meta,
            m(json!({"title": "A", "album_id": 5, "artist": "B"}))
        );
        assert_eq!(t.album_id.as_deref(), Some("5"));
        assert_eq!(t.status, "ready");
        assert_eq!(t.file.as_deref(), Some("/f"));
        assert!(!t.is_ready(), "the file does not exist");
    }

    #[test]
    fn update_track_audio_is_json() {
        let s = Store::memory();
        s.remember_track("deezer:1", &Json::new());
        s.update_track(
            "deezer:1",
            &[
                Field::Audio(Some(m(json!({"codec": "aac"})))),
                Field::LocalId(Some("L".into())),
            ],
        );
        let t = s.get_track("deezer:1").unwrap();
        assert_eq!(t.audio["codec"], "aac");
        assert_eq!(s.track_by_local_id("L").unwrap().id, "deezer:1");
        assert_eq!(s.tracks_by_local_ids(["L", "", "L"]).len(), 1);
    }

    #[test]
    fn job_lifecycle() {
        let s = Store::memory();
        assert!(s.enqueue("t", "download", 2, Some("album"), false));
        assert!(
            !s.enqueue("t", "download", 2, None, false),
            "same priority is a no-op"
        );
        assert!(
            s.enqueue("t", "download", 0, Some("play"), false),
            "raising priority wakes"
        );
        let job = s.claim_next_job(Some(1), None, None).unwrap();
        assert_eq!(
            (job.priority, job.attempts, job.state.as_str()),
            (0, 1, "running")
        );
        assert!(s.claim_next_job(None, None, None).is_none());
        assert!(
            !s.enqueue("t", "download", 0, None, false),
            "running job left alone"
        );
        s.finish_job("t", "download", "queued", Some("boom"), Some(now() + 60.0));
        assert!(
            s.claim_next_job(None, None, None).is_none(),
            "retry not due"
        );
        assert!(s.next_retry_time().unwrap() > now());
        s.finish_job("t", "download", "failed", Some("x"), None);
        assert!(
            s.enqueue("t", "download", 1, None, true),
            "failed jobs are revived"
        );
        let j = s.get_job("t", "download").unwrap();
        assert_eq!(
            (j.state.as_str(), j.attempts, j.reason.as_deref()),
            ("queued", 0, Some("album"))
        );
        assert_eq!(s.job_counts()["download:queued"], 1);
    }

    #[test]
    fn lanes_filter_by_priority_and_order_by_age() {
        let s = Store::memory();
        s.enqueue("bg", "download", 2, None, false);
        s.enqueue("p1", "download", 1, None, false);
        s.enqueue("p0", "download", 0, None, false);
        assert_eq!(
            s.claim_next_job(None, Some(2), None).unwrap().track_id,
            "bg"
        );
        assert_eq!(
            s.claim_next_job(Some(1), None, None).unwrap().track_id,
            "p0"
        );
        assert_eq!(
            s.claim_next_job(Some(1), None, None).unwrap().track_id,
            "p1"
        );
        assert_eq!(s.requeue_running_jobs(), 3);
    }

    #[test]
    fn aliases_are_never_repointed() {
        let s = Store::memory();
        assert!(s.add_alias("yt:x", "deezer:1", "source"));
        assert!(!s.add_alias("yt:x", "deezer:2", "source"));
        assert!(!s.add_alias("deezer:1", "deezer:1", "self"));
        assert_eq!(s.resolve_alias("yt:x").as_deref(), Some("deezer:1"));
        assert_eq!(s.resolve_aliases(["yt:x", "nope"]).len(), 1);
        assert_eq!(
            s.aliases_of("deezer:1"),
            vec![("yt:x".to_string(), "source".to_string())]
        );
    }

    #[test]
    fn intents_and_cache() {
        let s = Store::memory();
        s.add_pending_star("u", "deezer:1");
        s.add_pending_star("u", "deezer:1");
        assert_eq!(s.pending_stars(Some("u")).len(), 1);
        s.add_pending_playlist_add("u", "P", "b");
        s.add_pending_playlist_add("u", "P", "a");
        assert_eq!(
            s.pending_playlist_adds("u"),
            vec![("P".into(), "b".into()), ("P".into(), "a".into())]
        );
        s.add_pending_scrobble("v", "deezer:1", 5, true);
        s.add_pending_unstar("w", "L1");
        assert_eq!(
            s.users_with_pending_work(),
            HashSet::from(["u".into(), "v".into(), "w".into()])
        );
        assert!(s.remove_pending_star("u", "deezer:1"));
        s.remove_pending_scrobble("v", "deezer:1", 5, true);
        s.remove_pending_unstars("w", &["L1".into()]);
        s.remove_pending_playlist_add("u", "P", "a");
        s.remove_pending_playlist_add("u", "P", "b");
        assert!(s.users_with_pending_work().is_empty());

        s.cache_set("k", &json!({"a": "ё"}), 60.0);
        assert_eq!(s.cache_get("k").unwrap()["a"], "ё");
        s.cache_set("old", &json!(1), -1.0);
        assert!(s.cache_get("old").is_none());
        assert_eq!(s.cache_purge_expired(), 1);
    }

    #[test]
    fn imports_round_trip() {
        let s = Store::memory();
        s.save_import(
            "i1",
            "u",
            "queued",
            &json!({"playlist": "P"}),
            &json!([{"input": "a"}]),
        );
        s.save_import("i2", "u", "done", &json!({}), &json!([]));
        assert_eq!(s.get_import("i1").unwrap()["rows"][0]["input"], "a");
        assert_eq!(s.list_imports("u", 20).len(), 2);
        assert_eq!(s.imports_with_playlist(), vec!["i1".to_string()]);
        assert_eq!(s.unfinished_imports().len(), 1);
    }

    #[test]
    fn schema_too_new_is_refused_and_lock_is_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.sqlite3");
        Store::open(&path).unwrap().set_meta("schema_version", "99");
        assert!(matches!(Store::open(&path), Err(DbError::SchemaTooNew(_))));

        let mut a = OwnerLock::new(&path);
        let mut b = OwnerLock::new(&path);
        a.acquire().unwrap();
        assert!(matches!(b.acquire(), Err(DbError::Busy(_))));
        a.release();
        b.acquire().unwrap();
    }

    #[test]
    fn backups_are_verified_and_rotated() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("p.sqlite3")).unwrap();
        s.remember_track("deezer:1", &Json::new());
        let backups = dir.path().join("backups");
        std::fs::create_dir_all(&backups).unwrap();
        for i in 0..3 {
            std::fs::write(
                backups.join(format!("proxy-2000010{i}-000000.sqlite3")),
                b"x",
            )
            .unwrap();
        }
        let dest = s.backup(&backups, 2).unwrap().unwrap();
        let copy = Store::open(&dest).unwrap();
        assert!(copy.get_track("deezer:1").is_some());
        let n = std::fs::read_dir(&backups)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".sqlite3")
            })
            .count();
        assert_eq!(n, 2);
        assert!(s.integrity(false).is_empty());
    }

    #[test]
    fn artist_lookup_matches_numeric_ids() {
        let s = Store::memory();
        s.remember_track("deezer:1", &m(json!({"artist_id": 27})));
        s.update_track("deezer:1", &[Field::status("ready")]);
        assert!(s.ready_track_by_artist_id("27").is_some());
    }
}
