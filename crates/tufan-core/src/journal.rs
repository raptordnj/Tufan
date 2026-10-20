//! Durable, versioned job journal backed by SQLite.
//!
//! Item updates are batched (256 rows or 250 ms) for small-file throughput.
//! Updates that gate a destructive action (source deletion during a move)
//! use `put_durable`, which commits immediately with `synchronous=FULL`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::{Error, Result};
use crate::policy::{ConflictPolicy, IntegrityMode, JobKind, JobSpec, LinkPolicy, Options};

const SCHEMA_VERSION: i64 = 1;
const BATCH_ROWS: usize = 256;
const BATCH_AGE: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemState {
    /// Temp file partially written; `offset`/`prefix_hash` describe a checkpoint.
    Partial,
    /// Destination published; for moves the source still exists.
    Committed,
    Done,
    Skipped,
    Failed,
}

impl ItemState {
    fn as_i64(self) -> i64 {
        match self {
            ItemState::Partial => 0,
            ItemState::Committed => 1,
            ItemState::Done => 2,
            ItemState::Skipped => 3,
            ItemState::Failed => 4,
        }
    }
    fn from_i64(v: i64) -> Option<Self> {
        Some(match v {
            0 => ItemState::Partial,
            1 => ItemState::Committed,
            2 => ItemState::Done,
            3 => ItemState::Skipped,
            4 => ItemState::Failed,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Queued,
    Running,
    Paused,
    Completed,
    PartiallyCompleted,
    Cancelled,
    Failed,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            JobState::Queued => "queued",
            JobState::Running => "running",
            JobState::Paused => "paused",
            JobState::Completed => "completed",
            JobState::PartiallyCompleted => "partially completed",
            JobState::Cancelled => "cancelled",
            JobState::Failed => "failed",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "queued" => JobState::Queued,
            "running" => JobState::Running,
            "paused" => JobState::Paused,
            "completed" => JobState::Completed,
            "partially completed" => JobState::PartiallyCompleted,
            "cancelled" => JobState::Cancelled,
            "failed" => JobState::Failed,
            _ => return None,
        })
    }
    pub fn is_finished(self) -> bool {
        matches!(self, JobState::Completed | JobState::Cancelled)
    }
}

#[derive(Debug, Clone)]
pub struct ItemRecord {
    pub key: Vec<u8>,
    pub state: ItemState,
    pub dest: Option<PathBuf>,
    pub size: u64,
    pub src_id: Option<String>,
    pub offset: u64,
    pub prefix_hash: Option<Vec<u8>>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct JobRow {
    pub id: i64,
    pub spec: JobSpec,
    pub state: JobState,
}

struct Inner {
    conn: Connection,
    pending: Vec<(i64, ItemRecord)>,
    oldest: Option<Instant>,
}

pub struct Journal {
    inner: Mutex<Inner>,
}

pub fn path_key(p: &Path) -> Vec<u8> {
    p.as_os_str().as_encoded_bytes().to_vec()
}

/// Inverse of [`path_key`]; `None` for keys that are not a valid path here.
pub fn key_path(key: &[u8]) -> Option<PathBuf> {
    path_from_bytes(key)
}

fn path_from_bytes(b: &[u8]) -> Option<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(b)))
    }
    #[cfg(not(unix))]
    {
        // Journal contents are untrusted: only accept valid UTF-8 here.
        String::from_utf8(b.to_vec()).ok().map(PathBuf::from)
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Journal {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
        }
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        match version {
            0 => {
                conn.execute_batch(
                    "BEGIN;
                     CREATE TABLE jobs(
                        id INTEGER PRIMARY KEY,
                        kind TEXT NOT NULL,
                        sources BLOB NOT NULL,
                        dest BLOB NOT NULL,
                        options TEXT NOT NULL,
                        state TEXT NOT NULL,
                        created INTEGER NOT NULL,
                        updated INTEGER NOT NULL);
                     CREATE TABLE items(
                        job_id INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
                        key BLOB NOT NULL,
                        state INTEGER NOT NULL,
                        dest BLOB,
                        size INTEGER NOT NULL,
                        src_id TEXT,
                        offset INTEGER NOT NULL DEFAULT 0,
                        prefix_hash BLOB,
                        error TEXT,
                        PRIMARY KEY(job_id, key)) WITHOUT ROWID;
                     PRAGMA user_version = 1;
                     COMMIT;",
                )?;
            }
            SCHEMA_VERSION => {}
            v => return Err(Error::Corrupt(format!("unsupported schema version {v}"))),
        }
        Ok(Journal {
            inner: Mutex::new(Inner {
                conn,
                pending: Vec::new(),
                oldest: None,
            }),
        })
    }

    pub fn create_job(&self, spec: &JobSpec) -> Result<i64> {
        let g = self.inner.lock().unwrap();
        let sources: Vec<u8> = spec
            .sources
            .iter()
            .map(|p| path_key(p))
            .collect::<Vec<_>>()
            .join(&0u8);
        let t = now();
        g.conn.execute(
            "INSERT INTO jobs(kind, sources, dest, options, state, created, updated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![
                spec.kind.as_str(),
                sources,
                path_key(&spec.dest_dir),
                encode_options(&spec.options),
                JobState::Queued.as_str(),
                t
            ],
        )?;
        Ok(g.conn.last_insert_rowid())
    }

    pub fn set_job_state(&self, id: i64, state: JobState) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        flush_locked(&mut g)?;
        g.conn.execute(
            "UPDATE jobs SET state = ?1, updated = ?2 WHERE id = ?3",
            params![state.as_str(), now(), id],
        )?;
        Ok(())
    }

    pub fn load_job(&self, id: i64) -> Result<Option<JobRow>> {
        let g = self.inner.lock().unwrap();
        g.conn
            .query_row(
                "SELECT id, kind, sources, dest, options, state FROM jobs WHERE id = ?1",
                params![id],
                row_to_job,
            )
            .optional()?
            .transpose()
    }

    pub fn list_jobs(&self) -> Result<Vec<JobRow>> {
        let g = self.inner.lock().unwrap();
        let mut st = g
            .conn
            .prepare("SELECT id, kind, sources, dest, options, state FROM jobs ORDER BY id")?;
        let rows = st.query_map([], row_to_job)?;
        let mut out = Vec::new();
        for r in rows {
            // Skip invalid rows instead of failing the whole listing.
            if let Ok(j) = r? {
                out.push(j);
            }
        }
        Ok(out)
    }

    pub fn delete_job(&self, id: i64) -> Result<()> {
        let g = self.inner.lock().unwrap();
        g.conn
            .execute("DELETE FROM jobs WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn item(&self, job: i64, key: &[u8]) -> Result<Option<ItemRecord>> {
        let g = self.inner.lock().unwrap();
        if let Some((_, r)) = g
            .pending
            .iter()
            .rev()
            .find(|(j, r)| *j == job && r.key == key)
        {
            return Ok(Some(r.clone()));
        }
        let r = g
            .conn
            .query_row(
                "SELECT key, state, dest, size, src_id, offset, prefix_hash, error
                 FROM items WHERE job_id = ?1 AND key = ?2",
                params![job, key],
                |r| {
                    Ok((
                        r.get::<_, Vec<u8>>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, Option<Vec<u8>>>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, i64>(5)?,
                        r.get::<_, Option<Vec<u8>>>(6)?,
                        r.get::<_, Option<String>>(7)?,
                    ))
                },
            )
            .optional()?;
        let Some((key, state, dest, size, src_id, offset, prefix_hash, error)) = r else {
            return Ok(None);
        };
        let state = ItemState::from_i64(state)
            .ok_or_else(|| Error::Corrupt(format!("invalid item state {state}")))?;
        if size < 0 || offset < 0 || offset > size {
            return Err(Error::Corrupt("invalid item size/offset".into()));
        }
        Ok(Some(ItemRecord {
            key,
            state,
            dest: dest.as_deref().and_then(path_from_bytes),
            size: size as u64,
            src_id,
            offset: offset as u64,
            prefix_hash,
            error,
        }))
    }

    pub fn failed_items(&self, job: i64) -> Result<u64> {
        let g = self.inner.lock().unwrap();
        let n: i64 = g.conn.query_row(
            "SELECT COUNT(*) FROM items WHERE job_id = ?1 AND state = ?2",
            params![job, ItemState::Failed.as_i64()],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    /// Every item recorded for a job, in source-path order, for the job log.
    pub fn items(&self, job: i64) -> Result<Vec<ItemRecord>> {
        let mut g = self.inner.lock().unwrap();
        flush_locked(&mut g)?;
        let mut st = g.conn.prepare(
            "SELECT key, state, dest, size, src_id, offset, prefix_hash, error
             FROM items WHERE job_id = ?1 ORDER BY key",
        )?;
        let rows = st.query_map(params![job], |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<Vec<u8>>>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, Option<Vec<u8>>>(6)?,
                r.get::<_, Option<String>>(7)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (key, state, dest, size, src_id, offset, prefix_hash, error) = r?;
            // Skip invalid rows instead of failing the whole listing.
            let Some(state) = ItemState::from_i64(state) else {
                continue;
            };
            out.push(ItemRecord {
                key,
                state,
                dest: dest.as_deref().and_then(path_from_bytes),
                size: size.max(0) as u64,
                src_id,
                offset: offset.max(0) as u64,
                prefix_hash,
                error,
            });
        }
        Ok(out)
    }

    /// Batched update. A failed batch write is kept and retried by the
    /// next flush rather than failing the item that happened to trigger it;
    /// `flush` at the end of a job reports a journal that stays unwritable.
    pub fn put(&self, job: i64, rec: ItemRecord) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        g.pending.push((job, rec));
        g.oldest.get_or_insert_with(Instant::now);
        let due =
            g.pending.len() >= BATCH_ROWS || g.oldest.is_some_and(|t| t.elapsed() >= BATCH_AGE);
        if due {
            let _ = flush_locked(&mut g);
        }
        Ok(())
    }

    /// Update committed synchronously before returning.
    pub fn put_durable(&self, job: i64, rec: ItemRecord) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        g.pending.push((job, rec));
        flush_locked(&mut g)
    }

    pub fn flush(&self) -> Result<()> {
        flush_locked(&mut self.inner.lock().unwrap())
    }
}

fn flush_locked(g: &mut Inner) -> Result<()> {
    if g.pending.is_empty() {
        return Ok(());
    }
    let pending = std::mem::take(&mut g.pending);
    g.oldest = None;
    if let Err(e) = write_items(&mut g.conn, &pending) {
        // Keep the rows: dropping them would lose finished work, and the
        // next flush retries.
        let newer = std::mem::replace(&mut g.pending, pending);
        g.pending.extend(newer);
        g.oldest = Some(Instant::now());
        return Err(e);
    }
    Ok(())
}

fn write_items(conn: &mut Connection, pending: &[(i64, ItemRecord)]) -> Result<()> {
    let tx = conn.transaction()?;
    {
        let mut st = tx.prepare_cached(
            "INSERT OR REPLACE INTO items(job_id, key, state, dest, size, src_id, offset, prefix_hash, error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?;
        for (job, r) in pending {
            st.execute(params![
                job,
                r.key,
                r.state.as_i64(),
                r.dest.as_deref().map(path_key),
                r.size as i64,
                r.src_id,
                r.offset as i64,
                r.prefix_hash,
                r.error
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

fn row_to_job(r: &rusqlite::Row<'_>) -> rusqlite::Result<Result<JobRow>> {
    let id: i64 = r.get(0)?;
    let kind: String = r.get(1)?;
    let sources: Vec<u8> = r.get(2)?;
    let dest: Vec<u8> = r.get(3)?;
    let options: String = r.get(4)?;
    let state: String = r.get(5)?;
    Ok((|| {
        let bad = |m: &str| Error::Corrupt(format!("job {id}: {m}"));
        let kind = JobKind::parse(&kind).ok_or_else(|| bad("kind"))?;
        let state = JobState::parse(&state).ok_or_else(|| bad("state"))?;
        let sources = sources
            .split(|b| *b == 0)
            .map(|s| path_from_bytes(s).ok_or_else(|| bad("source path")))
            .collect::<Result<Vec<_>>>()?;
        let dest_dir = path_from_bytes(&dest).ok_or_else(|| bad("dest path"))?;
        let options = decode_options(&options).ok_or_else(|| bad("options"))?;
        Ok(JobRow {
            id,
            spec: JobSpec {
                kind,
                sources,
                dest_dir,
                options,
            },
            state,
        })
    })())
}

fn encode_options(o: &Options) -> String {
    format!(
        "integrity={};conflict={};links={};workers={};queue={};buffer={};retries={}",
        o.integrity.as_str(),
        o.conflict.as_str(),
        o.links.as_str(),
        o.workers,
        o.queue_depth,
        o.buffer_size,
        o.max_retries
    )
}

fn decode_options(s: &str) -> Option<Options> {
    let mut o = Options::default();
    for kv in s.split(';') {
        let (k, v) = kv.split_once('=')?;
        match k {
            "integrity" => o.integrity = IntegrityMode::parse(v)?,
            "conflict" => o.conflict = ConflictPolicy::parse(v)?,
            "links" => o.links = LinkPolicy::parse(v)?,
            "workers" => o.workers = v.parse::<usize>().ok()?.clamp(1, 256),
            "queue" => o.queue_depth = v.parse::<usize>().ok()?.clamp(1, 1 << 20),
            "buffer" => o.buffer_size = v.parse::<usize>().ok()?.clamp(4096, 64 << 20),
            "retries" => o.max_retries = v.parse::<u32>().ok()?.min(50),
            _ => {}
        }
    }
    Some(o)
}

/// Default journal location (per-user data directory).
pub fn default_path() -> PathBuf {
    let base = std::env::var_os("TUFAN_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            #[cfg(windows)]
            let b = std::env::var_os("LOCALAPPDATA").map(|p| PathBuf::from(p).join("Tufan"));
            #[cfg(target_os = "macos")]
            let b = std::env::var_os("HOME")
                .map(|h| PathBuf::from(h).join("Library/Application Support/Tufan"));
            #[cfg(not(any(windows, target_os = "macos")))]
            let b = std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
                .map(|p| p.join("tufan"));
            b.unwrap_or_else(|| PathBuf::from(OsString::from(".tufan")))
        });
    base.join("journal.sqlite3")
}
