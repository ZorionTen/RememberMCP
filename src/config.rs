//! Environment configuration with spec defaults.

use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Config {
    /// OpenCode SQLite database (read-only source).
    pub opencode_db: PathBuf,
    /// Legacy JSON storage directory (`<data-dir>/storage`).
    pub storage_dir: PathBuf,
    /// Our own index database (disposable, rebuildable).
    pub index_path: PathBuf,
    /// Embedding model name.
    pub embed_model: String,
    /// Lazy re-sync TTL before a search.
    pub sync_ttl: Duration,
    /// Include reasoning parts in chunks.
    pub include_reasoning: bool,
    /// Token window used for embeddings (cold-build speed vs. tail recall).
    pub embed_max_len: usize,
    /// Extra redaction regexes (user-supplied).
    pub extra_redact: Vec<String>,
}

impl Config {
    pub fn from_env() -> Self {
        let data_dir = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
        let opencode_db = std::env::var_os("OPENCODE_DB_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| data_dir.join("opencode").join("opencode.db"));
        let storage_dir = opencode_db
            .parent()
            .map(|p| p.join("storage"))
            .unwrap_or_else(|| data_dir.join("opencode").join("storage"));
        let index_path = std::env::var_os("OPENCODE_MEMORY_INDEX")
            .map(PathBuf::from)
            .unwrap_or_else(|| data_dir.join("opencode-memory-mcp").join("index.db"));
        let embed_model =
            std::env::var("EMBED_MODEL").unwrap_or_else(|_| "BAAI/bge-small-en-v1.5".to_string());
        let sync_ttl_secs: u64 = std::env::var("SYNC_TTL_SECONDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30);
        let embed_max_len: usize = std::env::var("EMBED_MAX_LEN")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|v| (64..=512).contains(v))
            .unwrap_or(256);
        let include_reasoning = matches!(
            std::env::var("INCLUDE_REASONING").as_deref(),
            Ok("1") | Ok("true") | Ok("yes")
        );
        let extra_redact = std::env::var("EXTRA_REDACT_REGEX")
            .map(|s| {
                s.split('\n')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();

        Self {
            opencode_db,
            storage_dir,
            index_path,
            embed_model,
            sync_ttl: Duration::from_secs(sync_ttl_secs),
            include_reasoning,
            embed_max_len,
            extra_redact,
        }
    }
}
