//! Hybrid retrieval (spec §9): FTS5 BM25 top-50 + vector KNN top-50,
//! merged with Reciprocal Rank Fusion, adjacent/overlapping chunks from the
//! same session collapsed, snippets located around query terms.
//!
//! Degrades to FTS-only when the embedder is unavailable; the mode is always
//! reported in the outcome.

use std::collections::HashMap;
use std::path::Path;

use rusqlite::{params, params_from_iter};

use crate::embed::Embedder;
use crate::index::{Index, IndexError, SyncProgress};
use crate::sources::Role;
use crate::util::escape_like;

/// Candidates pulled from each retrieval leg (spec §9.2).
const LEG_LIMIT: usize = 50;
/// RRF constant (spec §9.3).
const RRF_K: f64 = 60.0;
/// Snippet size in chars (spec §9.5).
const SNIPPET_CHARS: usize = 600;

#[derive(Debug, Clone, Default)]
pub struct SearchParams {
    pub query: String,
    pub project: Option<String>,
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub role: Option<Role>,
    pub session_id: Option<String>,
    pub limit: usize,
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub chunk_id: i64,
    pub session_id: String,
    pub session_title: String,
    pub project: String,
    pub timestamp: i64,
    pub score: f64,
    pub snippet: String,
}

#[derive(Debug)]
pub struct SearchOutcome {
    pub hits: Vec<SearchHit>,
    /// `hybrid` or `fts-only` (degraded).
    pub mode: &'static str,
    /// Present while the index is empty and the first build is running.
    pub indexing: Option<(usize, usize)>,
    /// True when results were cut to fit the response cap.
    pub truncated: bool,
}

pub struct SearchContext<'a> {
    pub index_path: &'a Path,
    pub embedder: Option<&'a dyn Embedder>,
    pub progress: &'a SyncProgress,
}

pub fn search(params: &SearchParams, ctx: &SearchContext<'_>) -> Result<SearchOutcome, IndexError> {
    let index = Index::open(ctx.index_path)?;
    let conn = index.conn()?;
    let mode = if ctx.embedder.is_some() {
        "hybrid"
    } else {
        "fts-only"
    };

    let chunk_total = index.chunk_count()?;
    if chunk_total == 0 {
        let (building, done, total, _) = ctx.progress.snapshot();
        let indexing = if building || total > 0 {
            Some((done, total.max(1)))
        } else {
            Some((0, 0))
        };
        return Ok(SearchOutcome {
            hits: vec![],
            mode,
            indexing,
            truncated: false,
        });
    }

    let (filter_sql, filter_args) = build_filters(params);
    let terms = query_terms(&params.query);

    // ---- leg 1: FTS5 BM25 -------------------------------------------------
    let mut fts_rank: Vec<i64> = Vec::new();
    if !terms.is_empty() {
        let fts_query = terms
            .iter()
            .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" OR ");
        let sql = format!(
            "SELECT c.id, bm25(chunks_fts) AS rank
             FROM chunks_fts
             JOIN chunks c ON c.id = chunks_fts.rowid
             JOIN sessions_idx s ON s.session_id = c.session_id
             WHERE chunks_fts MATCH ?1{filter_sql}
             ORDER BY rank
             LIMIT {LEG_LIMIT}"
        );
        let mut all_args = vec![rusqlite::types::Value::Text(fts_query)];
        all_args.extend(filter_args.iter().cloned());
        let rows = conn.prepare(&sql).and_then(|mut stmt| {
            stmt.query_map(params_from_iter(all_args.iter()), |r| r.get::<_, i64>(0))
                .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
        });
        match rows {
            Ok(ids) => fts_rank = ids,
            Err(e) => log::warn!("fts query failed: {e}"),
        }
    }

    // ---- leg 2: vector KNN ------------------------------------------------
    let mut vec_rank: Vec<i64> = Vec::new();
    let mut degraded = false;
    if let Some(embedder) = ctx.embedder {
        match embedder.embed(std::slice::from_ref(&params.query)) {
            Ok(vectors) if !vectors.is_empty() => {
                let blob = crate::embed::f32s_to_bytes(&vectors[0]);
                let sql = if filter_sql.is_empty() {
                    format!(
                        "SELECT rowid FROM chunks_vec
                         WHERE embedding MATCH ?1 AND k = {LEG_LIMIT}"
                    )
                } else {
                    format!(
                        "SELECT rowid FROM chunks_vec
                         WHERE embedding MATCH ?1 AND k = {LEG_LIMIT}
                           AND rowid IN (
                             SELECT c.id FROM chunks c
                             JOIN sessions_idx s ON s.session_id = c.session_id
                             WHERE 1=1{filter_sql})"
                    )
                };
                let mut all_args = vec![rusqlite::types::Value::Blob(blob)];
                all_args.extend(filter_args.iter().cloned());
                match conn.prepare(&sql).and_then(|mut stmt| {
                    stmt.query_map(params_from_iter(all_args.iter()), |r| r.get::<_, i64>(0))
                        .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
                }) {
                    Ok(ids) => vec_rank = ids,
                    Err(e) => {
                        log::warn!("vector query failed, degrading to fts-only: {e}");
                        degraded = true;
                    }
                }
            }
            Ok(_) => {}
            Err(e) => {
                log::warn!("query embedding failed, degrading to fts-only: {e}");
                degraded = true;
            }
        }
    }

    // ---- RRF merge --------------------------------------------------------
    let mut scores: HashMap<i64, f64> = HashMap::new();
    for (rank, id) in fts_rank.iter().enumerate() {
        *scores.entry(*id).or_default() += 1.0 / (RRF_K + (rank + 1) as f64);
    }
    for (rank, id) in vec_rank.iter().enumerate() {
        *scores.entry(*id).or_default() += 1.0 / (RRF_K + (rank + 1) as f64);
    }
    if scores.is_empty() {
        return Ok(SearchOutcome {
            hits: vec![],
            mode: if degraded { "fts-only" } else { mode },
            indexing: None,
            truncated: false,
        });
    }

    // ---- metadata for candidates -----------------------------------------
    let ids: Vec<i64> = scores.keys().copied().collect();
    let placeholders = (1..=ids.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT c.id, c.session_id, c.first_message_id, c.last_message_id,
                c.ts, c.text, s.title, s.project_dir
         FROM chunks c JOIN sessions_idx s ON s.session_id = c.session_id
         WHERE c.id IN ({placeholders})"
    );
    struct Row {
        id: i64,
        session_id: String,
        first_msg: String,
        last_msg: String,
        ts: i64,
        text: String,
        title: String,
        project: String,
    }
    let rows: Vec<Row> = conn
        .prepare(&sql)?
        .query_map(params_from_iter(ids.iter()), |r| {
            Ok(Row {
                id: r.get(0)?,
                session_id: r.get(1)?,
                first_msg: r.get(2)?,
                last_msg: r.get(3)?,
                ts: r.get(4)?,
                text: r.get(5)?,
                title: r.get(6)?,
                project: r.get(7)?,
            })
        })?
        .collect::<Result<_, _>>()?;

    // rank by score desc (ties: lower id first)
    let mut ranked: Vec<(Row, f64)> = rows
        .into_iter()
        .filter_map(|r| scores.get(&r.id).map(|s| (r, *s)))
        .collect();
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.id.cmp(&b.0.id))
    });

    // ---- collapse adjacent/overlapping chunks of one session (spec §9.4) --
    struct Kept {
        row: Row,
        score: f64,
    }
    let mut kept: Vec<Kept> = Vec::new();
    for (row, score) in ranked {
        if let Some(last) = kept.last_mut() {
            if last.row.session_id == row.session_id {
                let overlap =
                    row.first_msg <= last.row.last_msg && last.row.first_msg <= row.last_msg;
                let adjacent = if overlap || row.id == last.row.id {
                    false
                } else {
                    let (lo, hi) = if last.row.id < row.id {
                        (last.row.id, row.id)
                    } else {
                        (row.id, last.row.id)
                    };
                    let n: i64 = conn.query_row(
                        "SELECT COUNT(*) FROM chunks
                         WHERE session_id = ?1 AND id > ?2 AND id < ?3",
                        params![row.session_id, lo, hi],
                        |r| r.get(0),
                    )?;
                    n == 0
                };
                if overlap || adjacent {
                    last.score += score;
                    continue;
                }
            }
        }
        kept.push(Kept { row, score });
    }
    kept.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.row.id.cmp(&b.row.id))
    });

    let limit = params.limit.max(1);
    let truncated = kept.len() > limit;
    let hits: Vec<SearchHit> = kept
        .into_iter()
        .take(limit)
        .map(|k| SearchHit {
            chunk_id: k.row.id,
            session_id: k.row.session_id,
            session_title: k.row.title,
            project: k.row.project,
            timestamp: k.row.ts,
            score: k.score,
            snippet: snippet(&k.row.text, &terms),
        })
        .collect();

    Ok(SearchOutcome {
        hits,
        mode: if degraded { "fts-only" } else { mode },
        indexing: None,
        truncated,
    })
}

/// SQL filter conditions with placeholders numbered from `?2` (`?1` is the
/// MATCH/vector anchor in both leg queries).
fn build_filters(p: &SearchParams) -> (String, Vec<rusqlite::types::Value>) {
    let mut conds: Vec<String> = Vec::new();
    let mut args: Vec<rusqlite::types::Value> = Vec::new();
    let mut n = 2usize; // ?1 is the leg anchor

    if let Some(sid) = &p.session_id {
        conds.push(format!("c.session_id = ?{n}"));
        n += 1;
        args.push(sid.clone().into());
    }
    if let Some(ts) = p.since {
        conds.push(format!("c.ts >= ?{n}"));
        n += 1;
        args.push(ts.into());
    }
    if let Some(ts) = p.until {
        conds.push(format!("c.ts <= ?{n}"));
        n += 1;
        args.push(ts.into());
    }
    if let Some(role) = p.role {
        conds.push(format!("c.roles LIKE ?{n}"));
        n += 1;
        args.push(format!("%{}%", role.as_str()).into());
    }
    if let Some(proj) = &p.project {
        conds.push(format!(
            "(s.project_dir = ?{n} OR s.project_dir LIKE '%/' || ?{} ESCAPE '\\')",
            n + 1
        ));
        args.push(proj.clone().into());
        args.push(escape_like(proj).into());
    }

    if conds.is_empty() {
        (String::new(), args)
    } else {
        (format!(" AND {}", conds.join(" AND ")), args)
    }
}

/// Split the query into safe FTS terms (alphanumeric + underscore).
fn query_terms(q: &str) -> Vec<String> {
    q.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect()
}

/// ~600 char snippet with the earliest query-term occurrence located.
fn snippet(text: &str, terms: &[String]) -> String {
    if text.chars().count() <= SNIPPET_CHARS {
        return text.to_string();
    }
    // earliest case-insensitive occurrence of any term (len ≥ 2)
    let mut best: Option<usize> = None;
    for t in terms {
        if t.chars().count() < 2 {
            continue;
        }
        if let Some(p) = find_ci(text, t) {
            best = Some(best.map_or(p, |b: usize| b.min(p)));
        }
    }
    let start_byte = match best {
        Some(p) => p.saturating_sub(120),
        None => 0,
    };
    // snap to char boundary
    let mut start = 0usize;
    for (i, _) in text.char_indices() {
        if i >= start_byte {
            start = i;
            break;
        }
        start = i;
    }
    let tail = &text[start..];
    let cut: String = tail.chars().take(SNIPPET_CHARS).collect();
    let prefix = if start > 0 { "…" } else { "" };
    let suffix = if tail.chars().count() > SNIPPET_CHARS {
        "…"
    } else {
        ""
    };
    format!("{prefix}{cut}{suffix}")
}

/// Byte position of the first case-insensitive occurrence of `needle`.
fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    let n = needle.to_lowercase();
    if n.len() > hay.len() {
        return None;
    }
    for (i, _) in hay.char_indices() {
        if hay.len() - i < n.len() {
            break;
        }
        if hay[i..].to_lowercase().starts_with(&n) {
            return Some(i);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terms_split_and_escape() {
        assert_eq!(
            query_terms("docker permission-denied!"),
            vec!["docker", "permission", "denied"]
        );
        assert!(query_terms("   ").is_empty());
        // FTS metacharacters are stripped by splitting, quotes doubled
        assert_eq!(
            query_terms(r#"foo"bar NEAR(2)"#),
            vec!["foo", "bar", "NEAR", "2"]
        );
    }

    #[test]
    fn snippet_locates_terms() {
        let text = format!(
            "{} docker permission denied {}",
            "pad ".repeat(300),
            "tail ".repeat(100)
        );
        let s = snippet(&text, &["docker".to_string()]);
        assert!(s.contains("docker"));
        assert!(s.chars().count() <= SNIPPET_CHARS + 2);
        assert!(s.starts_with('…'));
    }

    #[test]
    fn snippet_short_text_returned_whole() {
        assert_eq!(snippet("short text", &[]), "short text");
    }

    #[test]
    fn find_ci_works() {
        assert_eq!(find_ci("Hello World", "world"), Some(6));
        assert_eq!(find_ci("Hello", "xyz"), None);
    }

    #[test]
    fn filters_number_placeholders_from_two() {
        let p = SearchParams {
            query: "q".into(),
            since: Some(100),
            role: Some(Role::User),
            ..Default::default()
        };
        let (sql, args) = build_filters(&p);
        assert!(sql.contains("?2"));
        assert!(sql.contains("?3"));
        assert_eq!(args.len(), 2);
        assert!(!sql.contains("project"));

        let p2 = SearchParams {
            project: Some("/home/u/proj".into()),
            ..Default::default()
        };
        let (sql2, args2) = build_filters(&p2);
        assert!(sql2.contains("?2"));
        assert!(sql2.contains("?3"));
        assert_eq!(args2.len(), 2);
        assert!(sql2.contains("ESCAPE"));
    }
}
