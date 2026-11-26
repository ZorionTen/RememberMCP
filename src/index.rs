//! The index store: `index.db` schema, sync, and maintenance.
//!
//! One SQLite file holding `sessions_idx`, `chunks`, FTS5 external-content
//! `chunks_fts`, sqlite-vec `chunks_vec`, and `sync_state`. Disposable and
//! fully rebuildable — never touches OpenCode's database.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Instant;

use rusqlite::{params, Connection, OptionalExtension, Transaction};
use thiserror::Error;

use crate::chunking::{chunk_session, ChunkOptions, CHUNKER_VERSION};
use crate::embed::{f32s_to_bytes, Embedder};
use crate::redact::Redactor;
use crate::sources::{SessionMeta, SourceAdapter};
use crate::util::now_ms;

pub const SCHEMA_VERSION: &str = "1";

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS sessions_idx(
  session_id TEXT PRIMARY KEY,
  project_dir TEXT NOT NULL DEFAULT '',
  title TEXT NOT NULL DEFAULT '',
  source TEXT NOT NULL DEFAULT '',
  time_created INTEGER NOT NULL DEFAULT 0,
  time_updated INTEGER NOT NULL DEFAULT 0,
  archived INTEGER NOT NULL DEFAULT 0,
  message_count INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS chunks(
  id INTEGER PRIMARY KEY,
  session_id TEXT NOT NULL,
  first_message_id TEXT NOT NULL DEFAULT '',
  last_message_id TEXT NOT NULL DEFAULT '',
  ts INTEGER NOT NULL DEFAULT 0,
  roles TEXT NOT NULL DEFAULT '',
  text TEXT NOT NULL,
  token_est INTEGER NOT NULL DEFAULT 0,
  content_hash TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_chunks_session ON chunks(session_id);
CREATE INDEX IF NOT EXISTS idx_chunks_ts ON chunks(ts);
CREATE INDEX IF NOT EXISTS idx_chunks_hash ON chunks(content_hash);
CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
  text, content='chunks', content_rowid='id', tokenize='porter unicode61'
);
CREATE TABLE IF NOT EXISTS sync_state(key TEXT PRIMARY KEY, value TEXT);
CREATE TRIGGER IF NOT EXISTS chunks_ai AFTER INSERT ON chunks BEGIN
  INSERT INTO chunks_fts(rowid, text) VALUES (new.id, new.text);
END;
CREATE TRIGGER IF NOT EXISTS chunks_ad AFTER DELETE ON chunks BEGIN
  INSERT INTO chunks_fts(chunks_fts, rowid, text) VALUES ('delete', old.id, old.text);
END;
"#;

fn vec_ddl(dims: usize) -> String {
    format!("CREATE VIRTUAL TABLE IF NOT EXISTS chunks_vec USING vec0(embedding float[{dims}])")
}

#[derive(Debug, Error)]
pub enum IndexError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("embed: {0}")]
    Embed(String),
}

/// Shared, lock-free progress visible to `index_status` while building.
#[derive(Debug, Default)]
pub struct SyncProgress {
    pub building: std::sync::atomic::AtomicBool,
    pub done: std::sync::atomic::AtomicUsize,
    pub total: std::sync::atomic::AtomicUsize,
    pub last_sync_ms: std::sync::atomic::AtomicI64,
}

impl SyncProgress {
    pub fn snapshot(&self) -> (bool, usize, usize, i64) {
        (
            self.building.load(Ordering::Relaxed),
            self.done.load(Ordering::Relaxed),
            self.total.load(Ordering::Relaxed),
            self.last_sync_ms.load(Ordering::Relaxed),
        )
    }
}

#[derive(Debug, Default, Clone)]
pub struct SyncSummary {
    pub sessions_indexed: usize,
    pub sessions_removed: usize,
    pub chunks_written: usize,
    pub embeds_computed: usize,
    pub embeds_reused: usize,
    pub skipped_rows: u64,
    pub duration_ms: u128,
    pub mode: String,
}

pub enum SyncOutcome {
    Done(SyncSummary),
    /// Another sync holds the lock (same or another process).
    Busy,
}

pub struct SyncOptions<'a> {
    pub adapters: &'a [std::sync::Arc<dyn SourceAdapter>],
    pub embedder: Option<&'a dyn Embedder>,
    pub full: bool,
    pub include_reasoning: bool,
    pub redactor: &'a Redactor,
    pub progress: &'a SyncProgress,
}

pub struct Index {
    path: PathBuf,
    /// Cross-process single-writer lock (held only during sync).
    lock: Option<std::fs::File>,
}

impl Index {
    pub fn open(path: &Path) -> Result<Self, IndexError> {
        crate::register_sqlite_vec(); // before any connection to this file
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Self::raw_conn(path)?;
        conn.execute_batch(SCHEMA_SQL)?;
        // default vec table (dims corrected later by ensure_compatible)
        conn.execute(&vec_ddl(384), [])?;
        if conn
            .query_row(
                "SELECT value FROM sync_state WHERE key='schema_version'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .is_none()
        {
            conn.execute(
                "INSERT INTO sync_state(key, value) VALUES('schema_version', ?1)",
                [SCHEMA_VERSION],
            )?;
        }
        drop(conn);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(Self {
            path: path.to_path_buf(),
            lock: None,
        })
    }

    fn raw_conn(path: &Path) -> Result<Connection, IndexError> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_millis(5_000))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Ok(conn)
    }

    pub fn conn(&self) -> Result<Connection, IndexError> {
        Self::raw_conn(&self.path)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    // ---- state ------------------------------------------------------------

    pub fn get_state(conn: &Connection, key: &str) -> Result<Option<String>, IndexError> {
        Ok(conn
            .query_row("SELECT value FROM sync_state WHERE key = ?1", [key], |r| {
                r.get::<_, String>(0)
            })
            .optional()?)
    }

    fn set_state(conn: &Connection, key: &str, value: &str) -> Result<(), IndexError> {
        conn.execute(
            "INSERT INTO sync_state(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn chunk_count(&self) -> Result<i64, IndexError> {
        let conn = self.conn()?;
        Ok(conn.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))?)
    }

    pub fn session_count(&self) -> Result<i64, IndexError> {
        let conn = self.conn()?;
        Ok(conn.query_row("SELECT COUNT(*) FROM sessions_idx", [], |r| r.get(0))?)
    }

    pub fn sessions_indexed(&self) -> Result<HashMap<String, i64>, IndexError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare("SELECT session_id, time_updated FROM sessions_idx")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // ---- schema maintenance ----------------------------------------------

    /// Ensure schema/chunker/embed settings match; drop and rebuild chunks
    /// when they differ (spec §5).
    fn ensure_compatible(
        conn: &Connection,
        embedder: Option<&dyn Embedder>,
    ) -> Result<(), IndexError> {
        let schema = Self::get_state(conn, "schema_version")?;
        if schema.as_deref() != Some(SCHEMA_VERSION) {
            log::info!("schema version changed, rebuilding index");
            Self::drop_everything(conn)?;
            Self::set_state(conn, "schema_version", SCHEMA_VERSION)?;
        }

        let chunker = Self::get_state(conn, "chunker_version")?;
        if chunker.as_deref() != Some(CHUNKER_VERSION) {
            log::info!("chunker version changed, rebuilding chunks");
            Self::drop_chunks(conn)?;
            Self::set_state(conn, "chunker_version", CHUNKER_VERSION)?;
        }

        if let Some(e) = embedder {
            let stored = Self::get_state(conn, "embed_model")?;
            let current = e.model();
            if let Some(s) = stored {
                if s != current {
                    log::info!("embed model changed ({s} -> {current}), rebuilding chunks");
                    Self::drop_chunks(conn)?;
                }
            }
            let declared = Self::vec_dims(conn)?;
            if declared != Some(e.dims()) {
                log::info!(
                    "vec dims changed ({} -> {}), rebuilding chunks",
                    declared.unwrap_or(0),
                    e.dims()
                );
                Self::drop_chunks(conn)?;
                conn.execute("DROP TABLE IF EXISTS chunks_vec", [])?;
                conn.execute(&vec_ddl(e.dims()), [])?;
            }
            Self::set_state(conn, "embed_model", current)?;
            Self::set_state(conn, "embed_dim", &e.dims().to_string())?;
        }
        Ok(())
    }

    fn vec_dims(conn: &Connection) -> Result<Option<usize>, IndexError> {
        let sql: Option<String> = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'chunks_vec'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        Ok(sql.and_then(|s| {
            let start = s.find("float[")? + 6;
            let end = s[start..].find(']')? + start;
            s[start..end].parse().ok()
        }))
    }

    fn drop_chunks(conn: &Connection) -> Result<(), IndexError> {
        conn.execute("DELETE FROM chunks", [])?; // fts maintained by trigger
        conn.execute("DELETE FROM chunks_vec", [])?;
        conn.execute("DELETE FROM sessions_idx", [])?;
        Ok(())
    }

    fn drop_everything(conn: &Connection) -> Result<(), IndexError> {
        conn.execute_batch(
            "DROP TRIGGER IF EXISTS chunks_ai;
             DROP TRIGGER IF EXISTS chunks_ad;
             DROP TABLE IF EXISTS chunks;
             DROP TABLE IF EXISTS chunks_fts;
             DROP TABLE IF EXISTS chunks_vec;
             DROP TABLE IF EXISTS sessions_idx;
             DROP TABLE IF EXISTS sync_state;",
        )?;
        conn.execute_batch(SCHEMA_SQL)?;
        conn.execute(&vec_ddl(384), [])?;
        Ok(())
    }

    // ---- sync -------------------------------------------------------------

    /// Run one sync pass (spec §8). Single-writer: holds a lock file next to
    /// `index.db`; returns `Busy` if another instance is syncing.
    pub fn sync(&mut self, opts: &SyncOptions<'_>) -> Result<SyncOutcome, IndexError> {
        let lock_path = self.path.with_extension("db.lock");
        let lock = std::fs::File::create(&lock_path)?;
        if let Err(e) = lock.try_lock() {
            log::debug!("sync lock busy: {e}");
            return Ok(SyncOutcome::Busy);
        }
        self.lock = Some(lock);

        let started = Instant::now();
        opts.progress.building.store(true, Ordering::Relaxed);
        let result = self.sync_inner(opts, started);

        if let Some(l) = self.lock.take() {
            let _ = l.unlock();
        }
        opts.progress.building.store(false, Ordering::Relaxed);
        result
    }

    fn sync_inner(
        &mut self,
        opts: &SyncOptions<'_>,
        started: Instant,
    ) -> Result<SyncOutcome, IndexError> {
        let conn = self.conn()?;
        Self::ensure_compatible(&conn, opts.embedder)?;
        if opts.full {
            log::info!("full rebuild requested, dropping all chunks");
            Self::drop_chunks(&conn)?;
        }

        // 1. Cheap listing from every adapter, deduplicated by priority
        //    (V1 → V2 → JSON tree; SQLite wins).
        let mut source: HashMap<String, (SessionMeta, usize)> = HashMap::new();
        for (idx, adapter) in opts.adapters.iter().enumerate() {
            match adapter.list_sessions() {
                Ok(list) => {
                    for m in list {
                        source.entry(m.id.clone()).or_insert((m, idx));
                    }
                }
                Err(e) => log::warn!("adapter {} listing failed: {e}", adapter.name()),
            }
        }

        // 2. Diff against the index.
        let indexed = self.sessions_indexed()?;
        let removed: Vec<String> = indexed
            .keys()
            .filter(|id| !source.contains_key(*id))
            .cloned()
            .collect();
        let mut changed: Vec<(SessionMeta, usize)> = source
            .values()
            .filter(|(m, _)| indexed.get(&m.id) != Some(&m.time_updated))
            .map(|(m, idx)| (m.clone(), *idx))
            .collect();
        // newest first so searches get fresh data early
        changed.sort_by_key(|(m, _)| std::cmp::Reverse(m.time_updated));

        opts.progress.total.store(changed.len(), Ordering::Relaxed);
        opts.progress.done.store(0, Ordering::Relaxed);

        let mut summary = SyncSummary {
            mode: if opts.embedder.is_some() {
                "hybrid".into()
            } else {
                "fts-only".into()
            },
            ..Default::default()
        };

        // 3. Sessions that vanished from the source are removed.
        {
            let tx = conn.unchecked_transaction()?;
            for id in &removed {
                delete_session_chunks(&tx, id)?;
                tx.execute("DELETE FROM sessions_idx WHERE session_id = ?1", [id])?;
                summary.sessions_removed += 1;
            }
            tx.commit()?;
        }

        // 4. Re-chunk changed sessions; embed in batches of 64; commit per
        //    session so a crash loses at most one session.
        log::debug!(
            "sync diff: {} removed, {} changed of {} source sessions",
            removed.len(),
            changed.len(),
            source.len()
        );
        for (meta, adapter_idx) in &changed {
            let t_sess = Instant::now();
            let adapter = &opts.adapters[*adapter_idx];
            let rec = match adapter.load_session(&meta.id) {
                Ok(Some(r)) => r,
                Ok(None) => continue,
                Err(e) => {
                    log::warn!("load {} failed: {e}", meta.id);
                    continue;
                }
            };
            let t_load = t_sess.elapsed();
            let t_chunk = Instant::now();
            let chunks = chunk_session(
                &rec,
                &ChunkOptions {
                    include_reasoning: opts.include_reasoning,
                    redactor: opts.redactor,
                },
            );
            let n_chunks = chunks.len();
            let t_chunk = t_chunk.elapsed();

            // Reuse embeddings: session-local content hashes first, then
            // a global hash → embedding lookup (spec §6: unchanged hashes are
            // never re-embedded).
            let mut reuse: HashMap<String, Vec<u8>> = HashMap::new();
            let t_reuse = Instant::now();
            {
                let mut stmt = conn.prepare(
                    "SELECT c.content_hash, v.embedding FROM chunks c
                     JOIN chunks_vec v ON v.rowid = c.id
                     WHERE c.session_id = ?1",
                )?;
                let rows = stmt.query_map([&meta.id], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
                })?;
                for row in rows {
                    let (h, e) = row?;
                    reuse.insert(h, e);
                }
            }

            let t_reuse = t_reuse.elapsed();
            let t_lookup = Instant::now();
            let mut to_insert: Vec<(crate::chunking::Chunk, Option<Vec<u8>>)> = Vec::new();
            for chunk in chunks {
                let embedded = reuse.get(&chunk.content_hash).cloned().or_else(|| {
                    if opts.embedder.is_none() {
                        None
                    } else {
                        let existing: Option<Vec<u8>> = conn
                            .query_row(
                                "SELECT v.embedding FROM chunks c
                                 JOIN chunks_vec v ON v.rowid = c.id
                                 WHERE c.content_hash = ?1 LIMIT 1",
                                [&chunk.content_hash],
                                |r| r.get::<_, Vec<u8>>(0),
                            )
                            .optional()
                            .ok()
                            .flatten();
                        existing
                    }
                });
                if embedded.is_some() {
                    summary.embeds_reused += 1;
                }
                to_insert.push((chunk, embedded));
            }
            let t_lookup = t_lookup.elapsed();

            // Compute missing embeddings in batches of 64.
            if let Some(embedder) = opts.embedder {
                let missing: Vec<usize> = to_insert
                    .iter()
                    .enumerate()
                    .filter(|(_, (_, e))| e.is_none())
                    .map(|(i, _)| i)
                    .collect();
                let t_embed = Instant::now();
                for batch in missing.chunks(64) {
                    let texts: Vec<String> =
                        batch.iter().map(|&i| to_insert[i].0.text.clone()).collect();
                    let vectors = embedder
                        .embed(&texts)
                        .map_err(|e| IndexError::Embed(e.to_string()))?;
                    let _ = &vectors;
                    for (j, &i) in batch.iter().enumerate() {
                        if let Some(v) = vectors.get(j) {
                            to_insert[i].1 = Some(f32s_to_bytes(v));
                            summary.embeds_computed += 1;
                        }
                    }
                }
                log::debug!("  embed phase: {:?}", t_embed.elapsed());
            }

            // One transaction per session: delete old, insert new.
            let t_write = Instant::now();
            let tx = conn.unchecked_transaction()?;
            delete_session_chunks(&tx, &meta.id)?;
            {
                let mut ins = tx.prepare(
                    "INSERT INTO chunks(session_id, first_message_id, last_message_id,
                                        ts, roles, text, token_est, content_hash)
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                )?;
                for (chunk, embedding) in &to_insert {
                    ins.execute(params![
                        chunk.session_id,
                        chunk.first_message_id,
                        chunk.last_message_id,
                        chunk.ts,
                        chunk.roles,
                        chunk.text,
                        chunk.token_est as i64,
                        chunk.content_hash,
                    ])?;
                    let rowid = tx.last_insert_rowid();
                    if let Some(e) = embedding {
                        tx.execute(
                            "INSERT INTO chunks_vec(rowid, embedding) VALUES(?1, ?2)",
                            params![rowid, e],
                        )?;
                    }
                }
            }
            tx.execute(
                "INSERT INTO sessions_idx(session_id, project_dir, title, source,
                                          time_created, time_updated, archived, message_count)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
                 ON CONFLICT(session_id) DO UPDATE SET
                   project_dir=excluded.project_dir, title=excluded.title,
                   source=excluded.source, time_created=excluded.time_created,
                   time_updated=excluded.time_updated, archived=excluded.archived,
                   message_count=excluded.message_count",
                params![
                    meta.id,
                    meta.project_dir,
                    meta.title,
                    meta.source,
                    meta.time_created,
                    meta.time_updated,
                    meta.archived as i64,
                    meta.message_count,
                ],
            )?;
            tx.commit()?;
            let t_write = t_write.elapsed();

            summary.sessions_indexed += 1;
            summary.chunks_written += to_insert.len();
            opts.progress.done.fetch_add(1, Ordering::Relaxed);
            log::debug!(
                "indexed {} chunks={} load={:?} chunk={:?} reuse={:?} lookup={:?} write={:?} total={:?}",
                meta.id,
                n_chunks,
                t_load,
                t_chunk,
                t_reuse,
                t_lookup,
                t_write,
                t_sess.elapsed()
            );
        }

        // 5. Backfill: chunks that never got a vector (e.g. a previous sync
        //    ran fts-only) are embedded now.
        if let Some(embedder) = opts.embedder {
            let gaps: Vec<(i64, String)> = {
                let mut stmt = conn.prepare(
                    "SELECT c.id, c.text FROM chunks c
                     WHERE NOT EXISTS (SELECT 1 FROM chunks_vec v WHERE v.rowid = c.id)",
                )?;
                let rows =
                    stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
                rows.collect::<Result<_, _>>()?
            };
            for batch in gaps.chunks(64) {
                let texts: Vec<String> = batch.iter().map(|(_, t)| t.clone()).collect();
                let vectors = embedder
                    .embed(&texts)
                    .map_err(|e| IndexError::Embed(e.to_string()))?;
                let tx = conn.unchecked_transaction()?;
                for (j, (id, _)) in batch.iter().enumerate() {
                    if let Some(v) = vectors.get(j) {
                        tx.execute(
                            "INSERT INTO chunks_vec(rowid, embedding) VALUES(?1, ?2)",
                            params![id, f32s_to_bytes(v)],
                        )?;
                        summary.embeds_computed += 1;
                    }
                }
                tx.commit()?;
            }
        }

        // 6. Watermarks.
        let skipped: u64 = opts.adapters.iter().map(|a| a.skipped()).sum();
        summary.skipped_rows = skipped;
        summary.duration_ms = started.elapsed().as_millis();
        let now = now_ms();
        Self::set_state(&conn, "last_sync_ms", &now.to_string())?;
        Self::set_state(&conn, "skipped_rows", &skipped.to_string())?;
        Self::set_state(&conn, "chunker_version", CHUNKER_VERSION)?;
        opts.progress.last_sync_ms.store(now, Ordering::Relaxed);

        log::info!(
            "sync done: {} sessions, {} removed, {} chunks ({} embeds, {} reused), {} skipped, {}ms, {}",
            summary.sessions_indexed,
            summary.sessions_removed,
            summary.chunks_written,
            summary.embeds_computed,
            summary.embeds_reused,
            summary.skipped_rows,
            summary.duration_ms,
            summary.mode
        );
        Ok(SyncOutcome::Done(summary))
    }
}

fn delete_session_chunks(tx: &Transaction, session_id: &str) -> Result<(), IndexError> {
    let ids: Vec<i64> = {
        let mut stmt = tx.prepare("SELECT id FROM chunks WHERE session_id = ?1")?;
        let rows = stmt.query_map([session_id], |r| r.get(0))?;
        rows.collect::<Result<_, _>>()?
    };
    for id in ids {
        tx.execute("DELETE FROM chunks_vec WHERE rowid = ?1", [id])?;
    }
    tx.execute("DELETE FROM chunks WHERE session_id = ?1", [session_id])?;
    Ok(())
}
