//! Legacy JSON tree adapter: `<data-dir>/storage/{session,message,part}`.
//!
//! Some users have sessions here that never made it into SQLite. Sessions
//! whose ids already exist in SQLite are skipped (SQLite wins). Field names
//! are read defensively (snake_case and camelCase) because old exports drift.

use std::collections::HashSet;
use std::path::PathBuf;

use super::{
    MessageRecord, PartRecord, Role, SessionMeta, SessionRecord, SkipCounter, SourceAdapter,
    SourceError,
};
use crate::util::{get_str, get_ts, parse_json_lenient};

pub struct JsonTreeAdapter {
    storage_dir: PathBuf,
    /// Session ids already claimed by the SQLite adapters.
    claimed: HashSet<String>,
    skips: SkipCounter,
}

impl JsonTreeAdapter {
    pub fn new(storage_dir: PathBuf, claimed: HashSet<String>) -> Self {
        Self {
            storage_dir,
            claimed,
            skips: SkipCounter::default(),
        }
    }

    fn session_dir(&self) -> PathBuf {
        self.storage_dir.join("session")
    }

    /// Locate a session file by id (scan project subdirectories).
    fn find_session_file(&self, id: &str) -> Option<PathBuf> {
        let dir = self.session_dir();
        let projects = std::fs::read_dir(&dir).ok()?;
        for proj in projects.flatten() {
            let p = proj.path().join(format!("{id}.json"));
            if p.is_file() {
                return Some(p);
            }
        }
        None
    }

    fn meta_from_file(&self, path: &std::path::Path, id_hint: &str) -> Option<SessionMeta> {
        let raw = std::fs::read_to_string(path).ok()?;
        let v = parse_json_lenient(&raw).ok()?;
        let id = get_str(&v, &["id"]).unwrap_or(id_hint).to_string();
        let project_dir = get_str(&v, &["directory", "projectID", "project_dir", "cwd"])
            .unwrap_or("")
            .to_string();
        let title = get_str(&v, &["title", "name"]).unwrap_or("").to_string();
        let time_created = get_ts(&v, &["time_created", "timeCreated"]).unwrap_or(0);
        let time_updated = get_ts(&v, &["time_updated", "timeUpdated"]).unwrap_or(time_created);
        let archived = v
            .get("time_archived")
            .or_else(|| v.get("timeArchived"))
            .and_then(|a| a.as_i64())
            .map(|a| a != 0)
            .unwrap_or(false)
            || v.get("archived").and_then(|a| a.as_bool()).unwrap_or(false);
        Some(SessionMeta {
            id,
            project_dir,
            title,
            source: "json_tree",
            time_created,
            time_updated,
            archived,
            message_count: 0,
        })
    }

    fn message_files(&self, session_id: &str) -> Vec<PathBuf> {
        let dir = self.storage_dir.join("message").join(session_id);
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
                    .collect()
            })
            .unwrap_or_default();
        files.sort();
        files
    }

    fn part_files(&self, message_id: &str) -> Vec<PathBuf> {
        let dir = self.storage_dir.join("part").join(message_id);
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
                    .collect()
            })
            .unwrap_or_default();
        files.sort();
        files
    }
}

impl SourceAdapter for JsonTreeAdapter {
    fn name(&self) -> &'static str {
        "json_tree"
    }

    fn list_sessions(&self) -> Result<Vec<SessionMeta>, SourceError> {
        let mut out = Vec::new();
        let dir = self.session_dir();
        let projects = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => return Ok(out),
        };
        for proj in projects.flatten() {
            if !proj.path().is_dir() {
                continue;
            }
            let files = match std::fs::read_dir(proj.path()) {
                Ok(rd) => rd,
                Err(_) => continue,
            };
            for f in files.flatten() {
                let p = f.path();
                if p.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                if self.claimed.contains(stem) {
                    continue;
                }
                if let Some(mut meta) = self.meta_from_file(&p, stem) {
                    meta.message_count = self.message_files(&meta.id).len() as i64;
                    out.push(meta);
                } else {
                    self.skips.bump();
                }
            }
        }
        out.sort_by_key(|m| std::cmp::Reverse(m.time_updated));
        Ok(out)
    }

    fn load_session(&self, id: &str) -> Result<Option<SessionRecord>, SourceError> {
        if self.claimed.contains(id) {
            return Ok(None);
        }
        let path = match self.find_session_file(id) {
            Some(p) => p,
            None => return Ok(None),
        };
        let meta = match self.meta_from_file(&path, id) {
            Some(m) => m,
            None => {
                self.skips.bump();
                return Ok(None);
            }
        };

        let mut messages = Vec::new();
        for mf in self.message_files(id) {
            let raw = match std::fs::read_to_string(&mf) {
                Ok(r) => r,
                Err(_) => {
                    self.skips.bump();
                    continue;
                }
            };
            let v = match parse_json_lenient(&raw) {
                Ok(v) => v,
                Err(_) => {
                    self.skips.bump();
                    continue;
                }
            };
            let stem = mf.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            let mid = get_str(&v, &["id"]).unwrap_or(stem).to_string();
            let role = match get_str(&v, &["role"]).and_then(Role::parse) {
                Some(r) => r,
                None => {
                    self.skips.bump();
                    continue;
                }
            };
            let ts = get_ts(&v, &["time_created", "timeCreated"]).unwrap_or(0);

            let mut parts = Vec::new();
            for pf in self.part_files(&mid) {
                let praw = match std::fs::read_to_string(&pf) {
                    Ok(r) => r,
                    Err(_) => {
                        self.skips.bump();
                        continue;
                    }
                };
                let pv = match parse_json_lenient(&praw) {
                    Ok(v) => v,
                    Err(_) => {
                        self.skips.bump();
                        continue;
                    }
                };
                match parse_json_part(&pv) {
                    Some(p) => parts.push(p),
                    None => self.skips.bump(),
                }
            }

            messages.push(MessageRecord {
                id: mid,
                role,
                time_created: ts,
                parts,
            });
        }
        messages.sort_by_key(|a| a.time_created);

        Ok(Some(SessionRecord { meta, messages }))
    }

    fn skipped(&self) -> u64 {
        self.skips.get()
    }
}

fn parse_json_part(v: &serde_json::Value) -> Option<PartRecord> {
    let kind = v.get("type").and_then(|t| t.as_str())?;
    match kind {
        "text" => Some(PartRecord::Text(v.get("text")?.as_str()?.to_string())),
        "reasoning" => Some(PartRecord::Reasoning(v.get("text")?.as_str()?.to_string())),
        "tool" => {
            let name = v.get("tool").and_then(|t| t.as_str())?.to_string();
            let state = v.get("state")?;
            let input = match state.get("input") {
                Some(serde_json::Value::String(s)) => s.clone(),
                Some(x) if !x.is_null() => x.to_string(),
                _ => String::new(),
            };
            let (output, output_len) = match state.get("output") {
                Some(serde_json::Value::String(s)) => (Some(s.clone()), s.len()),
                Some(x) if !x.is_null() => {
                    let s = x.to_string();
                    (Some(s.clone()), s.len())
                }
                _ => (None, 0),
            };
            Some(PartRecord::Tool {
                name,
                input,
                output,
                output_len,
            })
        }
        "file" => Some(PartRecord::File {
            filename: v
                .get("filename")
                .and_then(|f| f.as_str())
                .unwrap_or("")
                .to_string(),
            mime: v
                .get("mime")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string(),
        }),
        "patch" | "snapshot" | "step-start" | "step-finish" | "compaction" => {
            Some(PartRecord::Other(kind.to_string()))
        }
        _ => None,
    }
}
