//! V1 SQLite adapter: `session` / `message` / `part` tables, read-only.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

use super::{
    MessageRecord, PartRecord, Role, SessionMeta, SessionRecord, SkipCounter, SourceAdapter,
    SourceError,
};
use crate::util::{get_str, parse_json_lenient};

pub struct SqliteV1Adapter {
    path: PathBuf,
    skips: SkipCounter,
}

impl SqliteV1Adapter {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            skips: SkipCounter::default(),
        }
    }

    /// Read-only connection: URI `mode=ro` equivalent + `query_only` belt and
    /// suspenders + busy_timeout so WAL readers never fail against a live writer.
    fn open(&self) -> Result<Connection, SourceError> {
        if !self.path.exists() {
            return Err(SourceError::NotFound(self.path.clone()));
        }
        let conn = Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.busy_timeout(Duration::from_millis(5_000))?;
        conn.execute_batch("PRAGMA query_only = ON")?;
        Ok(conn)
    }

    fn message_count_map(&self, conn: &Connection) -> Result<HashMap<String, i64>, SourceError> {
        let mut stmt =
            conn.prepare("SELECT session_id, COUNT(*) FROM message GROUP BY session_id")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

impl SourceAdapter for SqliteV1Adapter {
    fn name(&self) -> &'static str {
        "sqlite_v1"
    }

    fn list_sessions(&self) -> Result<Vec<SessionMeta>, SourceError> {
        let mut conn = self.open()?;
        let tx = conn.transaction()?;
        let counts = self.message_count_map(&tx)?;
        let mut out: Vec<SessionMeta> = {
            let mut stmt = tx.prepare(
                "SELECT id, directory, title, time_created, time_updated, time_archived
                 FROM session ORDER BY time_updated DESC",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(SessionMeta {
                    id: r.get(0)?,
                    project_dir: r.get::<_, String>(1).unwrap_or_default(),
                    title: r.get::<_, String>(2).unwrap_or_default(),
                    source: "sqlite_v1",
                    time_created: r.get(3)?,
                    time_updated: r.get(4)?,
                    archived: r.get::<_, Option<i64>>(5)?.map(|v| v != 0).unwrap_or(false),
                    message_count: 0,
                })
            })?;
            rows.collect::<Result<_, _>>()?
        };
        for m in &mut out {
            m.message_count = counts.get(&m.id).copied().unwrap_or(0);
        }
        tx.commit()?;
        Ok(out)
    }

    fn load_session(&self, id: &str) -> Result<Option<SessionRecord>, SourceError> {
        let mut conn = self.open()?;
        let tx = conn.transaction()?;

        let meta = {
            let mut stmt = tx.prepare(
                "SELECT id, directory, title, time_created, time_updated, time_archived
                 FROM session WHERE id = ?1",
            )?;
            let mut rows = stmt.query_map([id], |r| {
                Ok(SessionMeta {
                    id: r.get(0)?,
                    project_dir: r.get::<_, String>(1).unwrap_or_default(),
                    title: r.get::<_, String>(2).unwrap_or_default(),
                    source: "sqlite_v1",
                    time_created: r.get(3)?,
                    time_updated: r.get(4)?,
                    archived: r.get::<_, Option<i64>>(5)?.map(|v| v != 0).unwrap_or(false),
                    message_count: 0,
                })
            })?;
            match rows.next() {
                Some(m) => m?,
                None => {
                    drop(rows);
                    drop(stmt);
                    tx.commit()?;
                    return Ok(None);
                }
            }
        };

        // Messages in chronological order.
        let mut messages = Vec::new();
        {
            let mut stmt = tx.prepare(
                "SELECT id, time_created, data FROM message
                 WHERE session_id = ?1 ORDER BY time_created, id",
            )?;
            let rows = stmt.query_map([id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?;
            for row in rows {
                let (mid, ts, raw) = row?;
                let data = match parse_json_lenient(&raw) {
                    Ok(d) => d,
                    Err(_) => {
                        self.skips.bump();
                        continue;
                    }
                };
                let role = match get_str(&data, &["role"]).and_then(Role::parse) {
                    Some(r) => r,
                    None => {
                        self.skips.bump();
                        continue;
                    }
                };
                messages.push(MessageRecord {
                    id: mid,
                    role,
                    time_created: ts,
                    parts: Vec::new(),
                });
            }
        }

        // Parts grouped per message (query order preserved within a message).
        let mut parts_by_msg: HashMap<String, Vec<PartRecord>> = HashMap::new();
        {
            let mut stmt = tx.prepare(
                "SELECT message_id, data FROM part
                 WHERE session_id = ?1 ORDER BY time_created, id",
            )?;
            let rows = stmt.query_map([id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (msg_id, raw) = row?;
                let data = match parse_json_lenient(&raw) {
                    Ok(d) => d,
                    Err(_) => {
                        self.skips.bump();
                        continue;
                    }
                };
                match parse_part(&data) {
                    ParseOutcome::Part(p) => {
                        parts_by_msg.entry(msg_id).or_default().push(p);
                    }
                    ParseOutcome::Excluded(kind) => {
                        parts_by_msg
                            .entry(msg_id)
                            .or_default()
                            .push(PartRecord::Other(kind));
                    }
                    ParseOutcome::Skip => self.skips.bump(),
                }
            }
        }

        for m in &mut messages {
            if let Some(parts) = parts_by_msg.remove(&m.id) {
                m.parts = parts;
            }
        }
        tx.commit()?;

        Ok(Some(SessionRecord { meta, messages }))
    }

    fn skipped(&self) -> u64 {
        self.skips.get()
    }
}

enum ParseOutcome {
    Part(PartRecord),
    /// Known type that chunking excludes by default.
    Excluded(String),
    /// Unparseable/drifted row.
    Skip,
}

fn parse_part(data: &serde_json::Value) -> ParseOutcome {
    let kind = match data.get("type").and_then(|t| t.as_str()) {
        Some(t) => t,
        None => return ParseOutcome::Skip,
    };
    match kind {
        "text" => match data.get("text").and_then(|t| t.as_str()) {
            Some(t) => ParseOutcome::Part(PartRecord::Text(t.to_string())),
            None => ParseOutcome::Skip,
        },
        "reasoning" => match data.get("text").and_then(|t| t.as_str()) {
            Some(t) => ParseOutcome::Part(PartRecord::Reasoning(t.to_string())),
            None => ParseOutcome::Skip,
        },
        "tool" => {
            let name = match data.get("tool").and_then(|t| t.as_str()) {
                Some(n) => n.to_string(),
                None => return ParseOutcome::Skip,
            };
            let state = data
                .get("state")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let input = match state.get("input") {
                Some(serde_json::Value::String(s)) => s.clone(),
                Some(v) if !v.is_null() => v.to_string(),
                _ => String::new(),
            };
            let (output, output_len) = match state.get("output") {
                Some(serde_json::Value::String(s)) => (Some(s.clone()), s.len()),
                Some(v) if !v.is_null() => {
                    let s = v.to_string();
                    (Some(s.clone()), s.len())
                }
                // V2-style inline content array inside state.
                _ => match state.get("content").and_then(|c| c.as_array()) {
                    Some(items) => {
                        let text = items
                            .iter()
                            .filter_map(|i| i.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>()
                            .join("\n");
                        if text.is_empty() {
                            (None, 0)
                        } else {
                            (Some(text.clone()), text.len())
                        }
                    }
                    None => (None, 0),
                },
            };
            ParseOutcome::Part(PartRecord::Tool {
                name,
                input,
                output,
                output_len,
            })
        }
        "file" => {
            let filename = get_str(data, &["filename"]).unwrap_or("").to_string();
            let mime = get_str(data, &["mime"]).unwrap_or("").to_string();
            if filename.is_empty() {
                ParseOutcome::Skip
            } else {
                ParseOutcome::Part(PartRecord::File { filename, mime })
            }
        }
        "patch" | "snapshot" | "step-start" | "step-finish" | "compaction" => {
            ParseOutcome::Excluded(kind.to_string())
        }
        _ => ParseOutcome::Skip,
    }
}
