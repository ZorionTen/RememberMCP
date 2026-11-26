//! Golden queries (spec §14): 15 queries with expected session ids,
//! recall@5 tracked. Runs offline against the fixture in CI.

mod common;

use common::{do_search, sync_fixture, FakeEmbedder, Fixture};

/// (query, expected session id)
const GOLDEN: [(&str, &str); 15] = [
    ("docker permission denied socket", "ses_docker_fix"),
    ("sudo usermod docker group", "ses_docker_fix"),
    (
        "postgres connection pool exhausted pgbouncer",
        "ses_pg_pool",
    ),
    (
        "rust borrow checker E0502 mutable immutable",
        "ses_rust_borrow",
    ),
    ("calculate_total rust sum function", "ses_rust_borrow"),
    ("kubectl rollout status deployment", "ses_kubectl_rollout"),
    ("terraform state lock dynamodb force-unlock", "ses_tf_state"),
    (
        "nginx 504 proxy_read_timeout keepalive",
        "ses_nginx_timeout",
    ),
    ("redis eviction allkeys-lru maxmemory", "ses_redis_eviction"),
    ("git rebase conflict markers continue", "ses_git_rebase"),
    (
        "api rate limit 500 requests per minute",
        "ses_missing_fields",
    ),
    ("quartz cron expression billing scheduler", "ses_v2_billing"),
    ("moka cache TTL forty two seconds", "ses_json_cache"),
    ("monolith split into modules refactoring", "ses_archived"),
    ("answer forty two from sqlite", "ses_dup"),
];

#[test]
fn golden_queries_recall_at_5() {
    let f: Fixture = common::build();
    let emb = FakeEmbedder::new(64);
    sync_fixture(&f, Some(&emb), false);

    let mut hits = 0usize;
    let mut misses = Vec::new();
    for (query, expected) in GOLDEN {
        let outcome = do_search(&f, query, Some(&emb));
        let found = outcome
            .hits
            .iter()
            .take(5)
            .any(|h| h.session_id == expected);
        if found {
            hits += 1;
        } else {
            let got: Vec<&str> = outcome
                .hits
                .iter()
                .take(5)
                .map(|h| h.session_id.as_str())
                .collect();
            misses.push(format!("`{query}` → expected {expected}, got {got:?}"));
        }
    }

    let recall = hits as f64 / GOLDEN.len() as f64;
    eprintln!("recall@5 = {recall} ({hits}/{})", GOLDEN.len());
    assert!(
        recall >= 0.8,
        "recall@5 = {recall} below 0.8:\n{}",
        misses.join("\n")
    );
}
