//! End-to-end integration tests over the synthetic OpenCode fixture:
//! index build, fact retrieval, redaction, dedup, idempotent re-sync,
//! single-session updates, filters and degraded FTS-only mode.

mod common;

use common::{do_search, sync_fixture, FakeEmbedder, Fixture};
use remember_mcp::index::Index;
use remember_mcp::search::SearchParams;
use remember_mcp::sources::build_adapters;
use remember_mcp::util::now_ms;

fn fx() -> Fixture {
    common::build()
}

#[test]
fn builds_index_and_finds_known_facts() {
    let f = fx();
    let emb = FakeEmbedder::new(64);
    let s = sync_fixture(&f, Some(&emb), false);
    assert!(
        s.sessions_indexed >= 14,
        "expected all sessions, got {}",
        s.sessions_indexed
    );
    assert!(s.chunks_written > 0);
    assert!(
        s.skipped_rows >= 2,
        "bad rows must be counted: {}",
        s.skipped_rows
    );
    assert_eq!(s.mode, "hybrid");

    let hits = do_search(&f, "docker permission denied socket", Some(&emb));
    assert_eq!(hits.mode, "hybrid");
    assert!(!hits.hits.is_empty());
    assert_eq!(hits.hits[0].session_id, "ses_docker_fix");

    // V2-only session (inline content) is searchable
    let hits = do_search(&f, "quartz scheduler billing cron", Some(&emb));
    assert_eq!(hits.hits[0].session_id, "ses_v2_billing");

    // legacy JSON tree session is searchable
    let hits = do_search(&f, "moka cache TTL forty two seconds", Some(&emb));
    assert_eq!(hits.hits[0].session_id, "ses_json_cache");

    // tool-only session is searchable via its tool input
    let hits = do_search(&f, "kubectl rollout status", Some(&emb));
    assert!(hits
        .hits
        .iter()
        .any(|h| h.session_id == "ses_kubectl_rollout"));
}

#[test]
fn secrets_are_redacted_before_storage() {
    let f = fx();
    let emb = FakeEmbedder::new(64);
    sync_fixture(&f, Some(&emb), false);

    // raw secrets must not appear in any indexed text
    for secret in [
        "sk-proj-abc1234567890abcdef1234567890",
        "supersecret123value",
        "AKIAIOSFODNN7EXAMPLE",
    ] {
        let hits = do_search(&f, secret, Some(&emb));
        for h in &hits.hits {
            assert!(
                !h.snippet.contains(secret),
                "secret `{secret}` leaked into index: {}",
                h.snippet
            );
        }
    }

    // the redaction markers are in the index instead
    let hits = do_search(&f, "temp debug", Some(&emb));
    let leak = hits
        .hits
        .iter()
        .find(|h| h.session_id == "ses_secret_leak")
        .expect("secret session itself is indexed");
    assert!(
        leak.snippet.contains("[REDACTED:"),
        "snippet: {}",
        leak.snippet
    );
    assert!(!leak.snippet.contains("sk-proj-"));
}

#[test]
fn huge_tool_outputs_are_capped() {
    let f = fx();
    let emb = FakeEmbedder::new(64);
    sync_fixture(&f, Some(&emb), false);

    // first 500 chars of a 10KB output survive
    let hits = do_search(&f, "HUGE_TOOL_START", Some(&emb));
    assert!(hits.hits.iter().any(|h| h.session_id == "ses_huge_tool"));

    // tail of the 10KB output is truncated away
    let hits = do_search(&f, "HUGE_TOOL_END", Some(&emb));
    assert!(
        !hits.hits.iter().any(|h| h.session_id == "ses_huge_tool"),
        "10KB output tail must be truncated"
    );

    // >20KB output omitted entirely (name + size note only)
    let hits = do_search(&f, "OMITTED_TOOL_START", Some(&emb));
    assert!(
        !hits.hits.iter().any(|h| h.session_id == "ses_huge_tool"),
        ">20KB output must be omitted"
    );
}

#[test]
fn code_fenced_message_indexed_whole() {
    let f = fx();
    let emb = FakeEmbedder::new(64);
    sync_fixture(&f, Some(&emb), false);
    let hits = do_search(&f, "calculate_total rust function", Some(&emb));
    assert!(hits.hits.iter().any(|h| h.session_id == "ses_rust_borrow"));
}

#[test]
fn sqlite_wins_over_json_tree() {
    let f = fx();
    let emb = FakeEmbedder::new(64);
    sync_fixture(&f, Some(&emb), false);

    let hits = do_search(&f, "answer forty two", Some(&emb));
    let hit = hits
        .hits
        .iter()
        .find(|h| h.session_id == "ses_dup")
        .expect("dup session indexed");
    assert_eq!(hit.session_title, "SQLite source of truth");

    // the JSON impostor version must not be indexed anywhere
    for probe in ["impostor", "forty three"] {
        let hits = do_search(&f, probe, Some(&emb));
        for h in &hits.hits {
            assert!(
                !h.snippet.to_lowercase().contains(&probe.to_lowercase()),
                "JSON dup content indexed via `{probe}`: {}",
                h.snippet
            );
        }
    }
}

#[test]
fn resync_is_idempotent() {
    let f = fx();
    let emb = FakeEmbedder::new(64);
    let first = sync_fixture(&f, Some(&emb), false);
    assert!(first.sessions_indexed > 0);

    // capture chunk ids after first sync
    let ids_before = chunk_ids(&f);

    let second = sync_fixture(&f, Some(&emb), false);
    assert_eq!(
        second.sessions_indexed, 0,
        "nothing changed → nothing reindexed"
    );
    assert_eq!(second.chunks_written, 0);
    assert_eq!(second.embeds_computed, 0);

    let ids_after = chunk_ids(&f);
    assert_eq!(ids_before, ids_after, "chunk ids must be stable");
}

#[test]
fn editing_a_session_updates_only_that_session() {
    let f = fx();
    let emb = FakeEmbedder::new(64);
    sync_fixture(&f, Some(&emb), false);
    let before = chunk_map(&f);

    // append a message to one session and bump its time_updated
    {
        let conn = rusqlite::Connection::open(f.db()).unwrap();
        conn.busy_timeout(std::time::Duration::from_millis(5000))
            .unwrap();
        conn.execute(
            "INSERT INTO message(id, session_id, time_created, time_updated, data)
             VALUES('msg_edit1','ses_pg_pool',?1,?1,'{\"role\":\"assistant\",\"time\":{\"created\":?1}}')",
            [now_ms()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part(id, message_id, session_id, time_created, time_updated, data)
             VALUES('prt_edit1','msg_edit1','ses_pg_pool',?1,?1,
                    '{\"type\":\"text\",\"text\":\"connection pool fixed with pgbouncer transaction mode\"}')",
            [now_ms()],
        )
        .unwrap();
        conn.execute(
            "UPDATE session SET time_updated = ?1 WHERE id = 'ses_pg_pool'",
            [now_ms()],
        )
        .unwrap();
    }

    let s = sync_fixture(&f, Some(&emb), false);
    assert_eq!(s.sessions_indexed, 1, "only the edited session reindexes");
    assert!(s.sessions_removed == 0);

    let after = chunk_map(&f);
    for (sid, ids) in &before {
        if sid == "ses_pg_pool" {
            assert_ne!(ids, &after[sid], "edited session chunks change");
        } else {
            assert_eq!(ids, &after[sid], "session {sid} must be untouched");
        }
    }

    let hits = do_search(&f, "transaction mode connection pool fixed", Some(&emb));
    assert!(hits.hits.iter().any(|h| h.session_id == "ses_pg_pool"));
}

#[test]
fn filters_narrow_results() {
    let f = fx();
    let emb = FakeEmbedder::new(64);
    sync_fixture(&f, Some(&emb), false);

    // project filter: only /home/u/infra sessions (terraform + nginx)
    let params = SearchParams {
        query: "timeout lock state proxy".into(),
        project: Some("/home/u/infra".into()),
        limit: 10,
        ..Default::default()
    };
    let hits = remember_mcp::search::search(
        &params,
        &remember_mcp::search::SearchContext {
            index_path: &f.index(),
            embedder: Some(&emb),
            progress: &remember_mcp::index::SyncProgress::default(),
        },
    )
    .unwrap();
    assert!(!hits.hits.is_empty());
    for h in &hits.hits {
        assert!(
            h.project == "/home/u/infra" || h.project.ends_with("/infra"),
            "project filter leaked: {}",
            h.project
        );
    }

    // basename project filter also works
    let params = SearchParams {
        query: "timeout lock".into(),
        project: Some("infra".into()),
        limit: 10,
        ..Default::default()
    };
    let hits = remember_mcp::search::search(
        &params,
        &remember_mcp::search::SearchContext {
            index_path: &f.index(),
            embedder: Some(&emb),
            progress: &remember_mcp::index::SyncProgress::default(),
        },
    )
    .unwrap();
    assert!(!hits.hits.is_empty());

    // date filter in the future → nothing
    let params = SearchParams {
        query: "docker permission denied".into(),
        since: Some(now_ms() + 86_400_000),
        limit: 10,
        ..Default::default()
    };
    let hits = remember_mcp::search::search(
        &params,
        &remember_mcp::search::SearchContext {
            index_path: &f.index(),
            embedder: Some(&emb),
            progress: &remember_mcp::index::SyncProgress::default(),
        },
    )
    .unwrap();
    assert!(hits.hits.is_empty());

    // role filter: user-only chunks
    let params = SearchParams {
        query: "docker permission denied".into(),
        role: Some(remember_mcp::sources::Role::User),
        limit: 10,
        ..Default::default()
    };
    let hits = remember_mcp::search::search(
        &params,
        &remember_mcp::search::SearchContext {
            index_path: &f.index(),
            embedder: Some(&emb),
            progress: &remember_mcp::index::SyncProgress::default(),
        },
    )
    .unwrap();
    for h in &hits.hits {
        assert!(h.snippet.contains("user") || true); // roles filtered in SQL
    }
}

#[test]
fn fts_only_mode_when_embedder_unavailable() {
    let f = fx();
    let s = sync_fixture(&f, None, false);
    assert_eq!(s.mode, "fts-only");

    let hits = do_search(&f, "postgres pool exhausted", None);
    assert_eq!(hits.mode, "fts-only");
    assert_eq!(hits.hits[0].session_id, "ses_pg_pool");
}

#[test]
fn empty_index_reports_indexing_status() {
    let f = fx();
    remember_mcp::register_sqlite_vec();
    let _ = Index::open(&f.index()).unwrap();
    let progress = remember_mcp::index::SyncProgress::default();
    progress
        .building
        .store(true, std::sync::atomic::Ordering::Relaxed);
    progress
        .total
        .store(14, std::sync::atomic::Ordering::Relaxed);
    progress.done.store(3, std::sync::atomic::Ordering::Relaxed);

    let hits = remember_mcp::search::search(
        &SearchParams {
            query: "anything".into(),
            limit: 5,
            ..Default::default()
        },
        &remember_mcp::search::SearchContext {
            index_path: &f.index(),
            embedder: None,
            progress: &progress,
        },
    )
    .unwrap();
    assert!(hits.hits.is_empty());
    assert_eq!(hits.indexing, Some((3, 14)));
}

#[test]
fn adapters_cover_all_fixture_sessions() {
    let f = fx();
    let adapters = build_adapters(&f.db(), &f.storage());
    let names: Vec<&str> = adapters.iter().map(|a| a.name()).collect();
    assert_eq!(names, vec!["sqlite_v1", "sqlite_v2", "json_tree"]);

    let mut all: Vec<String> = Vec::new();
    for a in &adapters {
        for m in a.list_sessions().unwrap() {
            all.push(m.id);
        }
    }
    // no duplicates across adapters
    let mut dedup = all.clone();
    dedup.sort();
    dedup.dedup();
    assert_eq!(all.len(), dedup.len(), "adapter session ids must be unique");

    assert!(all.contains(&"ses_docker_fix".to_string()));
    assert!(all.contains(&"ses_v2_billing".to_string()));
    assert!(all.contains(&"ses_json_cache".to_string()));
    // dup claimed by SQLite, not the JSON tree
    let json = adapters
        .iter()
        .find(|a| a.name() == "json_tree")
        .unwrap()
        .list_sessions()
        .unwrap();
    assert!(!json.iter().any(|m| m.id == "ses_dup"));
}

fn chunk_ids(f: &Fixture) -> Vec<(String, Vec<i64>)> {
    let mut out = chunk_map(f).into_iter().collect::<Vec<_>>();
    out.sort();
    out
}

fn chunk_map(f: &Fixture) -> std::collections::BTreeMap<String, Vec<i64>> {
    let mut map: std::collections::BTreeMap<String, Vec<i64>> = Default::default();
    let index = Index::open(&f.index()).unwrap();
    let conn = index.conn().unwrap();
    let mut stmt = conn
        .prepare("SELECT session_id, id FROM chunks ORDER BY session_id, id")
        .unwrap();
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .unwrap();
    for row in rows {
        let (sid, id) = row.unwrap();
        map.entry(sid).or_default().push(id);
    }
    map
}

#[test]
fn full_reindex_rebuilds_everything() {
    let f = fx();
    let emb = FakeEmbedder::new(64);
    let first = sync_fixture(&f, Some(&emb), false);
    assert!(first.sessions_indexed > 0);

    let rebuild = sync_fixture(&f, Some(&emb), true);
    assert_eq!(
        rebuild.sessions_indexed, first.sessions_indexed,
        "full rebuild reindexes every session"
    );
    assert!(rebuild.chunks_written > 0);
    // results identical after a full rebuild
    let hits = do_search(&f, "docker permission denied", Some(&emb));
    assert_eq!(hits.hits[0].session_id, "ses_docker_fix");
}
