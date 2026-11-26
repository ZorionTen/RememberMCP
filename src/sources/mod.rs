//! Source adapters: normalised records + the `SourceAdapter` trait.
//!
//! Adapters are read-only views over OpenCode's data. The sync layer merges
//! their listings in priority order (V1 → V2 → JSON tree) and deduplicates by
//! session id, so SQLite always wins over the legacy JSON tree.

pub mod json_tree;
pub mod sqlite_v1;
pub mod sqlite_v2;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use thiserror::Error;

pub use json_tree::JsonTreeAdapter;
pub use sqlite_v1::SqliteV1Adapter;
pub use sqlite_v2::SqliteV2Adapter;

#[derive(Debug, Error)]
pub enum SourceError {
    #[error("source not found: {0}")]
    NotFound(PathBuf),
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Shared skip counter for unparseable/drifted rows.
#[derive(Debug, Default)]
pub struct SkipCounter(AtomicU64);

impl SkipCounter {
    pub fn bump(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "user" => Some(Role::User),
            "assistant" => Some(Role::Assistant),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SessionMeta {
    pub id: String,
    pub project_dir: String,
    pub title: String,
    pub source: &'static str,
    pub time_created: i64,
    pub time_updated: i64,
    pub archived: bool,
    pub message_count: i64,
}

#[derive(Debug, Clone)]
pub enum PartRecord {
    Text(String),
    Tool {
        name: String,
        input: String,
        output: Option<String>,
        /// Byte length of the raw output (for the >20KB size note).
        output_len: usize,
    },
    Reasoning(String),
    File {
        filename: String,
        mime: String,
    },
    /// Known-but-excluded kinds (patch, snapshot, step-*, compaction, …).
    Other(String),
}

#[derive(Debug, Clone)]
pub struct MessageRecord {
    pub id: String,
    pub role: Role,
    pub time_created: i64,
    pub parts: Vec<PartRecord>,
}

#[derive(Debug, Clone)]
pub struct SessionRecord {
    pub meta: SessionMeta,
    pub messages: Vec<MessageRecord>,
}

/// A read-only source of OpenCode conversation data.
pub trait SourceAdapter: Send + Sync {
    fn name(&self) -> &'static str;
    /// Cheap listing used by sync diffing.
    fn list_sessions(&self) -> Result<Vec<SessionMeta>, SourceError>;
    /// Full load of one session, messages ordered by time.
    fn load_session(&self, id: &str) -> Result<Option<SessionRecord>, SourceError>;
    /// Rows skipped due to unparseable/drifted data.
    fn skipped(&self) -> u64;
}

/// Build adapters in priority order. The JSON tree adapter is given the
/// session ids already claimed by SQLite so it never duplicates them.
pub fn build_adapters(
    opencode_db: &std::path::Path,
    storage_dir: &std::path::Path,
) -> Vec<Arc<dyn SourceAdapter>> {
    let mut adapters: Vec<Arc<dyn SourceAdapter>> = Vec::new();
    let mut claimed: HashSet<String> = HashSet::new();

    if opencode_db.exists() {
        let v1 = SqliteV1Adapter::new(opencode_db.to_path_buf());
        let v2 = SqliteV2Adapter::new(opencode_db.to_path_buf());
        for m in v1.list_sessions().unwrap_or_default() {
            claimed.insert(m.id);
        }
        for m in v2.list_sessions().unwrap_or_default() {
            claimed.insert(m.id);
        }
        adapters.push(Arc::new(v1));
        adapters.push(Arc::new(v2));
    }

    if storage_dir.exists() {
        adapters.push(Arc::new(JsonTreeAdapter::new(
            storage_dir.to_path_buf(),
            claimed,
        )));
    }

    adapters
}
