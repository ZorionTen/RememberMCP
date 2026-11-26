//! OpenCode 2 (beta) adapter: `session_v2` / `session_message` tables.
//!
//! Only serves sessions that do **not** exist in the V1 `session` table —
//! for dual-written sessions the V1 adapter wins because it has the full
//! `part` rows with complete tool I/O. Verified against the real database:
//! all user/assistant messages of V1 sessions live in V1 tables.

use std::path::PathBuf;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

use super::{
    MessageRecord, PartRecord, Role, SessionMeta, SessionRecord, SkipCounter, SourceAdapter,
    SourceError,
};
use crate::util::parse_json_lenient;

pub struct SqliteV2Adapter {
    path: PathBuf,
    skips: SkipCounter,
}

impl SqliteV2Adapter {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            skips: SkipCounter::default(),
        }
    }

    fn open(&self) -> Result<Connection, SourceError> {
        if !self.path.exists() {
            return Err(SourceError::NotFound(self.path.clone()));
        }
        let conn = Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.busy_timeout(Duration::from_millis(5_000))?;
        conn.execute_batch("PRAGMA query_only = ON")?;
        Ok(conn)
    }
}

impl SourceAdapter for SqliteV2Adapter {
    fn name(&self) -> &'static str {
        "sqlite_v2"
    }

    fn list_sessions(&self) -> Result<Vec<SessionMeta>, SourceError> {
        let mut conn = self.open()?;
        let tx = conn.transaction()?;
        let mut out: Vec<SessionMeta> = {
            let mut stmt = tx.prepare(
                "SELECT id, directory, title, time_created, time_updated, time_archived
                 FROM session_v2
                 WHERE id NOT IN (SELECT id FROM session)
                 ORDER BY time_updated DESC",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(SessionMeta {
                    id: r.get(0)?,
                    project_dir: r.get::<_, String>(1).unwrap_or_default(),
                    title: r.get::<_, String>(2).unwrap_or_default(),
                    source: "sqlite_v2",
                    time_created: r.get(3)?,
                    time_updated: r.get(4)?,
                    archived: r.get::<_, Option<i64>>(5)?.map(|v| v != 0).unwrap_or(false),
                    message_count: 0,
                })
            })?;
            rows.collect::<Result<_, _>>()?
        };
        for m in &mut out {
            let mut stmt = tx.prepare(
                "SELECT COUNT(*) FROM session_message
                 WHERE session_id = ?1 AND type IN ('user','assistant')",
            )?;
            m.message_count = stmt.query_row([&m.id], |r| r.get(0))?;
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
                 FROM session_v2 WHERE id = ?1",
            )?;
            let mut rows = stmt.query_map([id], |r| {
                Ok(SessionMeta {
                    id: r.get(0)?,
                    project_dir: r.get::<_, String>(1).unwrap_or_default(),
                    title: r.get::<_, String>(2).unwrap_or_default(),
                    source: "sqlite_v2",
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

        let mut messages = Vec::new();
        {
            let mut stmt = tx.prepare(
                "SELECT id, type, time_created, data FROM session_message
                 WHERE session_id = ?1 ORDER BY seq, time_created, id",
            )?;
            let rows = stmt.query_map([id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?;
            for row in rows {
                let (mid, mtype, ts, raw) = row?;
                // Only conversation roles are indexed; system/synthetic/idle/
                // model-switched/compaction rows are intentional non-content.
                let role = match mtype.as_str() {
                    "user" => Role::User,
                    "assistant" => Role::Assistant,
                    _ => continue,
                };
                let data = match parse_json_lenient(&raw) {
                    Ok(d) => d,
                    Err(_) => {
                        self.skips.bump();
                        continue;
                    }
                };
                let parts = match role {
                    Role::User => match data.get("text").and_then(|t| t.as_str()) {
                        Some(t) => vec![PartRecord::Text(t.to_string())],
                        None => {
                            self.skips.bump();
                            continue;
                        }
                    },
                    Role::Assistant => parse_assistant_content(&data, &self.skips),
                };
                messages.push(MessageRecord {
                    id: mid,
                    role,
                    time_created: ts,
                    parts,
                });
            }
        }
        tx.commit()?;
        Ok(Some(SessionRecord { meta, messages }))
    }

    fn skipped(&self) -> u64 {
        self.skips.get()
    }
}

fn parse_assistant_content(data: &serde_json::Value, skips: &SkipCounter) -> Vec<PartRecord> {
    // Inline content array (OpenCode 2 style).
    if let Some(items) = data.get("content").and_then(|c| c.as_array()) {
        let mut out = Vec::new();
        for item in items {
            let kind = match item.get("type").and_then(|t| t.as_str()) {
                Some(k) => k,
                None => {
                    skips.bump();
                    continue;
                }
            };
            match kind {
                "text" => match item.get("text").and_then(|t| t.as_str()) {
                    Some(t) => out.push(PartRecord::Text(t.to_string())),
                    None => skips.bump(),
                },
                "reasoning" => match item.get("text").and_then(|t| t.as_str()) {
                    Some(t) => out.push(PartRecord::Reasoning(t.to_string())),
                    None => skips.bump(),
                },
                "tool" => {
                    let name = match item.get("name").and_then(|n| n.as_str()) {
                        Some(n) => n.to_string(),
                        None => {
                            skips.bump();
                            continue;
                        }
                    };
                    let state = item
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
                        _ => match state.get("content").and_then(|c| c.as_array()) {
                            Some(arr) => {
                                let text = arr
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
                    out.push(PartRecord::Tool {
                        name,
                        input,
                        output,
                        output_len,
                    });
                }
                // step-start/step-finish etc. inside content: known, excluded.
                "step-start" | "step-finish" => {}
                _ => skips.bump(),
            }
        }
        out
    } else if let Some(t) = data.get("text").and_then(|t| t.as_str()) {
        vec![PartRecord::Text(t.to_string())]
    } else {
        // Assistant message with empty/absent content: not an error, just empty.
        Vec::new()
    }
}
