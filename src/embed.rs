//! Embedding behind a small `Embedder` protocol.
//!
//! Production uses fastembed's ONNX Runtime backend with
//! `BAAI/bge-small-en-v1.5` (384 dims, CPU) — the spec stack. The model is
//! downloaded once from Hugging Face on first run; after that everything is
//! local. Tests use deterministic offline embedders. If the model cannot be
//! initialised (e.g. offline first run), the server degrades to FTS-only
//! search.

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

pub trait Embedder: Send + Sync {
    /// Embed texts, one vector per input, same order.
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError>;
    fn model(&self) -> &str;
    fn dims(&self) -> usize;
}

#[derive(Debug)]
pub enum EmbedError {
    Init(String),
    Embed(String),
}

impl std::fmt::Display for EmbedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EmbedError::Init(e) => write!(f, "embedder init failed: {e}"),
            EmbedError::Embed(e) => write!(f, "embedding failed: {e}"),
        }
    }
}
impl std::error::Error for EmbedError {}

/// Map config model names (spec default `BAAI/bge-small-en-v1.5` and common
/// aliases) to a fastembed model. `None` = embeddings disabled or unknown.
pub fn map_model(name: &str) -> Option<EmbeddingModel> {
    match name.trim().to_ascii_lowercase().as_str() {
        "none" | "off" | "false" | "fts-only" => None,
        "baai/bge-small-en-v1.5"
        | "bge-small-en-v1.5"
        | "bgesmallenv15"
        | "xenova/bge-small-en-v1.5" => Some(EmbeddingModel::BGESmallENV15),
        "bge-small-en-v1.5-q" | "bgesmallenv15q" => Some(EmbeddingModel::BGESmallENV15Q),
        _ => None,
    }
}

/// Create the production embedder. `Ok(None)` means embeddings disabled or
/// unavailable (FTS-only degraded mode).
pub fn create_embedder(model_cfg: &str, max_len: usize) -> Option<Box<dyn Embedder>> {
    let lower = model_cfg.trim().to_ascii_lowercase();
    if matches!(lower.as_str(), "none" | "off" | "false" | "fts-only") {
        return None; // explicitly disabled, no warning
    }
    let model = match map_model(model_cfg) {
        Some(m) => m,
        None => {
            log::warn!("unknown EMBED_MODEL `{model_cfg}`, using FTS-only mode");
            return None;
        }
    };
    // fastembed defaults to a CWD-relative cache; pin it to a stable location
    // so the model is downloaded once regardless of the client's working dir.
    if std::env::var_os("FASTEMBED_CACHE_DIR").is_none() {
        if let Some(base) = dirs::cache_dir() {
            let dir = base.join("remember-mcp").join("fastembed");
            let _ = std::fs::create_dir_all(&dir);
            std::env::set_var("FASTEMBED_CACHE_DIR", &dir);
        }
    }
    match OnnxEmbedder::new(model, max_len) {
        Ok(e) => Some(Box::new(e)),
        Err(e) => {
            log::warn!("{e}; falling back to FTS-only mode");
            None
        }
    }
}

pub struct OnnxEmbedder {
    inner: TextEmbedding,
    model: String,
    dims: usize,
}

impl OnnxEmbedder {
    pub fn new(model: EmbeddingModel, max_len: usize) -> Result<Self, EmbedError> {
        let inner = TextEmbedding::try_new(
            InitOptions::new(model.clone())
                .with_max_length(max_len.clamp(64, 512))
                .with_show_download_progress(true),
        )
        .map_err(|e| EmbedError::Init(e.to_string()))?;
        // determine dims with a single probe embedding
        let dims = inner
            .embed(vec!["probe".to_string()], Some(1))
            .map_err(|e| EmbedError::Init(e.to_string()))?
            .first()
            .map(|v| v.len())
            .unwrap_or(384);
        Ok(Self {
            inner,
            // model identity includes the token window: changing EMBED_MAX_LEN
            // rebuilds vectors (sync_state mismatch → full re-embed)
            model: format!("{model:?}@{max_len}"),
            dims,
        })
    }
}

impl Embedder for OnnxEmbedder {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.inner
            .embed(texts.to_vec(), Some(64))
            .map_err(|e| EmbedError::Embed(e.to_string()))
    }
    fn model(&self) -> &str {
        &self.model
    }
    fn dims(&self) -> usize {
        self.dims
    }
}

/// f32 vector → little-endian bytes (sqlite-vec blob format).
pub fn f32s_to_bytes(v: &[f32]) -> Vec<u8> {
    let mut b = Vec::with_capacity(v.len() * 4);
    for f in v {
        b.extend_from_slice(&f.to_le_bytes());
    }
    b
}

/// Little-endian blob → f32 vector.
pub fn bytes_to_f32s(b: &[u8]) -> Vec<f32> {
    b.as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect()
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    /// Deterministic offline embedder: bag-of-words hashed into `dims` buckets.
    /// Overlapping vocabularies produce closer vectors — good enough to test
    /// the vector leg + RRF without any network.
    pub struct FakeEmbedder {
        dims: usize,
        model: String,
    }

    impl FakeEmbedder {
        pub fn new(dims: usize) -> Self {
            Self {
                dims,
                model: "fake-bow".into(),
            }
        }
    }

    impl Embedder for FakeEmbedder {
        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
            Ok(texts
                .iter()
                .map(|t| {
                    let mut v = vec![0f32; self.dims];
                    for word in t.split_whitespace() {
                        let mut h = DefaultHasher::new();
                        word.to_ascii_lowercase().hash(&mut h);
                        let idx = (h.finish() as usize) % self.dims;
                        v[idx] += 1.0;
                    }
                    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                    if norm > 0.0 {
                        for x in &mut v {
                            *x /= norm;
                        }
                    }
                    v
                })
                .collect())
        }
        fn model(&self) -> &str {
            &self.model
        }
        fn dims(&self) -> usize {
            self.dims
        }
    }

    #[test]
    fn model_mapping() {
        assert!(map_model("BAAI/bge-small-en-v1.5").is_some());
        assert!(map_model("bge-small-en-v1.5").is_some());
        assert!(map_model("BGESmallENV15").is_some());
        assert_eq!(map_model("none"), None);
        assert_eq!(map_model("off"), None);
        // unknown models degrade to FTS-only (create_embedder warns)
        assert_eq!(map_model("totally-unknown-model"), None);
    }

    #[test]
    fn bytes_roundtrip() {
        let v = vec![1.5f32, -2.25, 0.0, 3.125];
        assert_eq!(bytes_to_f32s(&f32s_to_bytes(&v)), v);
    }

    #[test]
    fn fake_embedder_is_deterministic_and_similar() {
        let e = FakeEmbedder::new(64);
        let a = e.embed(&["docker permission denied".into()]).unwrap();
        let b = e.embed(&["docker permission denied".into()]).unwrap();
        let c = e.embed(&["postgres connection pool".into()]).unwrap();
        assert_eq!(a[0], b[0]);
        let dot = |x: &[f32], y: &[f32]| x.iter().zip(y).map(|(p, q)| p * q).sum::<f32>();
        assert!(dot(&a[0], &b[0]) > 0.99);
        assert!(dot(&a[0], &c[0]) < dot(&a[0], &b[0]));
    }
}
