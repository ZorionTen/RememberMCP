#![allow(dead_code)]
//! Synthetic OpenCode data builder for integration tests.
//!
//! Creates a fixture directory containing:
//! - `opencode.db` with the real V1 schema (`project`/`session`/`message`/
//!   `part`) and the V2 family (`session_v2`/`session_message`/`session_part`),
//!   covering: missing JSON fields, huge tool outputs, a code-fenced message,
//!   a tool-only session, an archived session, an unparseable row, a V2-only
//!   session, and a session with secrets that must be redacted.
//! - the legacy JSON tree under `storage/` with a JSON-only session and a
//!   session duplicated in SQLite (SQLite must win).

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};

pub const BASE_TS: i64 = 1_778_956_991_614; // 2026-05-16/17

pub struct Fixture {
    pub dir: PathBuf,
}

impl Fixture {
    pub fn db(&self) -> PathBuf {
        self.dir.join("opencode.db")
    }
    pub fn storage(&self) -> PathBuf {
        self.dir.join("storage")
    }
    pub fn index(&self) -> PathBuf {
        self.dir.join("index.db")
    }
}

/// Build the whole fixture under a fresh temp dir.
pub fn build() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir").keep();
    let db_path = dir.join("opencode.db");
    let conn = Connection::open(&db_path).expect("create db");
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    seed(&conn);
    drop(conn);
    write_json_tree(&dir.join("storage"));
    Fixture { dir }
}

fn open_schema(conn: &Connection) {
    conn.execute_batch(
        r#"
CREATE TABLE project (
  id TEXT PRIMARY KEY, worktree TEXT NOT NULL, vcs TEXT, name TEXT,
  icon_url TEXT, icon_color TEXT, time_created INTEGER NOT NULL,
  time_updated INTEGER NOT NULL, time_initialized INTEGER, sandboxes TEXT NOT NULL,
  commands TEXT, icon_url_override TEXT, time_active INTEGER NOT NULL DEFAULT 0,
  time_archived INTEGER
);
CREATE TABLE session (
  id TEXT PRIMARY KEY, project_id TEXT NOT NULL, parent_id TEXT, slug TEXT NOT NULL,
  directory TEXT NOT NULL, title TEXT NOT NULL, version TEXT NOT NULL, share_url TEXT,
  summary_additions INTEGER, summary_deletions INTEGER, summary_files INTEGER,
  summary_diffs TEXT, revert TEXT, permission TEXT,
  time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
  time_compacting INTEGER, time_archived INTEGER, workspace_id TEXT, path TEXT,
  agent TEXT, model TEXT, cost REAL NOT NULL DEFAULT 0,
  tokens_input INTEGER NOT NULL DEFAULT 0, tokens_output INTEGER NOT NULL DEFAULT 0,
  tokens_reasoning INTEGER NOT NULL DEFAULT 0, tokens_cache_read INTEGER NOT NULL DEFAULT 0,
  tokens_cache_write INTEGER NOT NULL DEFAULT 0, metadata TEXT
);
CREATE TABLE message (
  id TEXT PRIMARY KEY, session_id TEXT NOT NULL, time_created INTEGER NOT NULL,
  time_updated INTEGER NOT NULL, data TEXT NOT NULL
);
CREATE TABLE part (
  id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL,
  time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL
);
CREATE TABLE session_v2 (
  id TEXT PRIMARY KEY, project_id TEXT NOT NULL, workspace_id TEXT, parent_id TEXT,
  fork_session_id TEXT, fork_boundary TEXT, slug TEXT NOT NULL, directory TEXT NOT NULL,
  path TEXT, title TEXT, version TEXT NOT NULL, share_url TEXT,
  summary_additions INTEGER, summary_deletions INTEGER, summary_files INTEGER,
  summary_diffs TEXT, metadata TEXT, cost REAL NOT NULL DEFAULT 0,
  tokens_input INTEGER NOT NULL DEFAULT 0, tokens_output INTEGER NOT NULL DEFAULT 0,
  tokens_reasoning INTEGER NOT NULL DEFAULT 0, tokens_cache_read INTEGER NOT NULL DEFAULT 0,
  tokens_cache_write INTEGER NOT NULL DEFAULT 0, revert TEXT, permission TEXT,
  agent TEXT, model TEXT, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
  time_compacting INTEGER, time_archived INTEGER, time_suspended INTEGER,
  resume_attempts INTEGER NOT NULL DEFAULT 0, time_idle INTEGER, time_viewed INTEGER,
  idle_outcome TEXT, time_busy INTEGER
);
CREATE TABLE session_message (
  id TEXT PRIMARY KEY, session_id TEXT NOT NULL, type TEXT NOT NULL, seq INTEGER NOT NULL,
  time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL
);
CREATE TABLE session_part (
  message_id TEXT NOT NULL, session_id TEXT NOT NULL, ordinal INTEGER NOT NULL DEFAULT 0,
  type TEXT NOT NULL, call_id TEXT, time_created INTEGER NOT NULL,
  time_updated INTEGER NOT NULL, data TEXT NOT NULL, PRIMARY KEY (message_id, ordinal)
);
"#,
    )
    .expect("schema");
}

enum Part<'a> {
    Text(&'a str),
    Tool {
        name: &'a str,
        input: &'a str,
        output: &'a str,
    },
}

struct Msg<'a> {
    id: &'a str,
    user: bool,
    parts: Vec<Part<'a>>,
}

fn insert_session(
    conn: &Connection,
    id: &str,
    title: &str,
    dir: &str,
    created: i64,
    updated: i64,
    archived: Option<i64>,
) {
    conn.execute(
        "INSERT INTO session(id, project_id, slug, directory, title, version,
                             time_created, time_updated, time_archived)
         VALUES(?1, 'global', ?2, ?3, ?4, '1', ?5, ?6, ?7)",
        params![
            id,
            id.replace('_', "-"),
            dir,
            title,
            created,
            updated,
            archived
        ],
    )
    .unwrap();
}

fn insert_msg(conn: &Connection, session: &str, id: &str, ts: i64, data: &str) {
    conn.execute(
        "INSERT INTO message(id, session_id, time_created, time_updated, data)
         VALUES(?1, ?2, ?3, ?3, ?4)",
        params![id, session, ts, data],
    )
    .unwrap();
}

fn insert_part(conn: &Connection, session: &str, msg: &str, id: &str, ts: i64, data: &str) {
    conn.execute(
        "INSERT INTO part(id, message_id, session_id, time_created, time_updated, data)
         VALUES(?1, ?2, ?3, ?4, ?4, ?5)",
        params![id, msg, session, ts, data],
    )
    .unwrap();
}

fn json_str(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

fn filler(len: usize, start: &str, end: &str) -> String {
    let mut s = String::from(start);
    while s.len() < len {
        s.push_str("filler line for the large tool output test. ");
    }
    s.push_str(end);
    s
}

fn seed(conn: &Connection) {
    open_schema(conn);
    conn.execute(
        "INSERT INTO project(id, worktree, time_created, time_updated, sandboxes)
         VALUES('global', '/home/u', ?1, ?1, '')",
        [BASE_TS],
    )
    .unwrap();

    let mut ts = BASE_TS;
    let mut next_ts = || {
        ts += 1_000;
        ts
    };

    // Large tool outputs: 10KB (rendered, first 500 chars only) and 30KB
    // (omitted entirely with a size note).
    let out_10k = filler(10_000, "HUGE_TOOL_START ", "HUGE_TOOL_END");
    let out_30k = filler(30_000, "OMITTED_TOOL_START ", "OMITTED_TOOL_END");

    let v1: Vec<(&str, &str, &str, Vec<Msg>)> = vec![
        (
            "ses_docker_fix",
            "Docker permission denied fix",
            "/home/u/webapp",
            vec![
                Msg { id: "msg_d1", user: true, parts: vec![Part::Text("docker compose up fails with permission denied on /var/run/docker.sock")] },
                Msg { id: "msg_d2", user: false, parts: vec![Part::Text("Add your user to the docker group: sudo usermod -aG docker $USER, then log out and back in. Membership grants root-equivalent access, so only give it to trusted users.")] },
            ],
        ),
        (
            "ses_pg_pool",
            "Postgres pool exhaustion",
            "/home/u/webapp",
            vec![
                Msg { id: "msg_p1", user: true, parts: vec![Part::Text("postgres connection pool exhausted: FATAL: too many clients already")] },
                Msg { id: "msg_p2", user: false, parts: vec![Part::Text("Put pgbouncer in transaction pooling mode in front of Postgres and raise max_connections; the app was leaking prepared statements per connection.")] },
            ],
        ),
        (
            "ses_rust_borrow",
            "Rust borrow checker E0502",
            "/home/u/cli",
            vec![
                Msg { id: "msg_r1", user: true, parts: vec![Part::Text("rust borrow checker error E0502 cannot borrow as mutable because also borrowed as immutable")] },
                Msg { id: "msg_r2", user: false, parts: vec![Part::Text("Clone the value before mutating, or restructure:\n\n```rust\nfn calculate_total(items: &[u32]) -> u64 {\n    items.iter().map(|x| *x as u64).sum()\n}\n```\n\nKeep the slice borrow scoped to the read.")] },
            ],
        ),
        (
            "ses_kubectl_rollout",
            "kubectl rollout tool-only",
            "/home/u/cluster",
            vec![Msg {
                id: "msg_k1",
                user: false,
                parts: vec![Part::Tool {
                    name: "shell",
                    input: "kubectl rollout status deployment/api --timeout=300s",
                    output: "deployment \"api\" successfully rolled out",
                }],
            }],
        ),
        (
            "ses_tf_state",
            "Terraform state lock",
            "/home/u/infra",
            vec![
                Msg { id: "msg_t1", user: true, parts: vec![Part::Text("terraform apply fails: error acquiring the state lock")] },
                Msg { id: "msg_t2", user: false, parts: vec![Part::Text("Run terraform force-unlock -lock-id=<id>. The S3 backend uses a DynamoDB lock table; check nobody else is applying.")] },
            ],
        ),
        (
            "ses_nginx_timeout",
            "Nginx 504 gateway timeout",
            "/home/u/infra",
            vec![
                Msg { id: "msg_n1", user: true, parts: vec![Part::Text("nginx returns 504 gateway time-out on the api location")] },
                Msg { id: "msg_n2", user: false, parts: vec![Part::Text("Set proxy_read_timeout 300s on the location and enable upstream keepalive so nginx reuses backend connections.")] },
            ],
        ),
        (
            "ses_redis_eviction",
            "Redis OOM evictions",
            "/home/u/queue",
            vec![
                Msg { id: "msg_e1", user: true, parts: vec![Part::Text("redis running out of memory and evicting keys randomly")] },
                Msg { id: "msg_e2", user: false, parts: vec![Part::Text("Set maxmemory-policy allkeys-lru and raise maxmemory; random eviction was dropping cache keys the API needed.")] },
            ],
        ),
        (
            "ses_git_rebase",
            "Git rebase conflict",
            "/home/u/webapp",
            vec![
                Msg { id: "msg_g1", user: true, parts: vec![Part::Text("git rebase stopped with conflict markers in index.ts")] },
                Msg { id: "msg_g2", user: false, parts: vec![Part::Text("Resolve the conflict markers, git add the file, then git rebase --continue. To replay onto the new base: git rebase --onto origin/main <old-base>.")] },
            ],
        ),
        (
            "ses_secret_leak",
            "Debug session with secrets",
            "/home/u/webapp",
            vec![Msg {
                id: "msg_s1",
                user: true,
                parts: vec![Part::Text(
                    "temp debug: api key is sk-proj-abc1234567890abcdef1234567890, aws AKIAIOSFODNN7EXAMPLE, TOKEN=supersecret123value, bearer abcdef1234567890",
                )],
            }],
        ),
        (
            "ses_huge_tool",
            "Huge tool outputs",
            "/home/u/webapp",
            vec![Msg {
                id: "msg_h1",
                user: false,
                parts: vec![
                    Part::Tool {
                        name: "bash",
                        input: "cat very_large.log",
                        output: &out_10k,
                    },
                    Part::Tool {
                        name: "shell",
                        input: "cat enormous.log",
                        output: &out_30k,
                    },
                ],
            }],
        ),
        (
            "ses_archived",
            "Old archived refactor",
            "/home/u/legacy",
            vec![
                Msg { id: "msg_a1", user: true, parts: vec![Part::Text("split the monolith into modules refactoring plan")] },
                Msg { id: "msg_a2", user: false, parts: vec![Part::Text("Move each controller into src/modules/<name> and keep shared kernels in src/core.")] },
            ],
        ),
        (
            "ses_missing_fields",
            "Drifted message fields",
            "/home/u/api",
            vec![
                Msg { id: "msg_m1", user: true, parts: vec![Part::Text("what is our api rate limit?")] },
                Msg { id: "msg_m2", user: false, parts: vec![Part::Text("The api rate limit is 500 requests per minute per key.")] },
            ],
        ),
    ];

    let mut part_seq = 0usize;
    for (sid, title, dir, msgs) in &v1 {
        let created = next_ts();
        let updated = next_ts();
        let archived = (*sid == "ses_archived").then(&mut next_ts);
        insert_session(conn, sid, title, dir, created, updated, archived);
        for m in msgs {
            let ts = next_ts();
            // Missing agent/model/parentID on purpose (older-row drift).
            let data = if m.user {
                format!(r#"{{"role":"user","time":{{"created":{ts}}}}}"#)
            } else {
                format!(
                    r#"{{"role":"assistant","time":{{"created":{ts}}},"agent":"build","model":{{"providerID":"openai","modelID":"gpt-x"}},"parentID":"msg_parent"}}"#
                )
            };
            insert_msg(conn, sid, m.id, ts, &data);
            for p in &m.parts {
                part_seq += 1;
                let pid = format!("prt_{part_seq:04}");
                let pdata = match p {
                    Part::Text(t) => format!(r#"{{"type":"text","text":{}}}"#, json_str(t)),
                    Part::Tool {
                        name,
                        input,
                        output,
                    } => format!(
                        r#"{{"type":"tool","tool":{},"callID":"call_{part_seq}","state":{{"status":"completed","input":{{"command":{}}},"output":{}}}}}"#,
                        json_str(name),
                        json_str(input),
                        json_str(output),
                    ),
                };
                insert_part(conn, sid, m.id, &pid, ts, &pdata);
            }
        }
    }

    // Unparseable message row (counted as skipped).
    insert_msg(
        conn,
        "ses_missing_fields",
        "msg_bad",
        next_ts(),
        r#"{"role":"user","time":{"created":"#,
    );
    // Unparseable part row (counted as skipped).
    insert_part(
        conn,
        "ses_missing_fields",
        "msg_m2",
        "prt_bad",
        next_ts(),
        "not-json-at-all",
    );

    // ---- V2-only session --------------------------------------------------
    conn.execute(
        "INSERT INTO session_v2(id, project_id, slug, directory, title, version,
                                time_created, time_updated)
         VALUES('ses_v2_billing', 'global', 'ses-v2-billing', '/home/u/billing',
                'Billing cron to Quartz', '2', ?1, ?2)",
        params![next_ts(), next_ts()],
    )
    .unwrap();
    let v2_user = r#"{"text":"billing cron moved to quartz scheduler, when does it fire?","time":{"created":1778957000000}}"#;
    let v2_asst = r#"{"time":{"created":1778957001000},"agent":"build","content":[
        {"type":"reasoning","text":"User wants the cron schedule."},
        {"type":"text","text":"The quartz cron expression 0 0 2 * * ? triggers the billing run at 02:00 daily."},
        {"type":"tool","name":"shell","state":{"status":"completed","input":{"command":"grep -r quartz billing/"},"content":[{"type":"text","text":"billing/scheduler.xml: cron 0 0 2 * * ?"}]}}
    ]}"#;
    conn.execute(
        "INSERT INTO session_message(id, session_id, type, seq, time_created, time_updated, data)
         VALUES('msg_v2_1','ses_v2_billing','user',4,?1,?1,?2)",
        params![next_ts(), v2_user],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session_message(id, session_id, type, seq, time_created, time_updated, data)
         VALUES('msg_v2_2','ses_v2_billing','assistant',5,?1,?1,?2)",
        params![next_ts(), v2_asst],
    )
    .unwrap();
    // Noise rows that must NOT be indexed as conversation content.
    conn.execute(
        "INSERT INTO session_message(id, session_id, type, seq, time_created, time_updated, data)
         VALUES('msg_v2_sys','ses_v2_billing','system',1,?1,?1,'{\"text\":\"tools renamed\"}')",
        [next_ts()],
    )
    .unwrap();

    // ---- dup session: exists in V1 (SQLite wins) and in the JSON tree -----
    insert_session(
        conn,
        "ses_dup",
        "SQLite source of truth",
        "/home/u/dup",
        next_ts(),
        next_ts(),
        None,
    );
    insert_msg(
        conn,
        "ses_dup",
        "msg_dup1",
        next_ts(),
        r#"{"role":"user","time":{"created":1778957100000}}"#,
    );
    insert_part(
        conn,
        "ses_dup",
        "msg_dup1",
        "prt_dup1",
        next_ts(),
        r#"{"type":"text","text":"what is the answer? the answer is forty two from sqlite"}"#,
    );
}

fn write_json_tree(storage: &Path) {
    let sess_dir = storage.join("session").join("proj_json");
    std::fs::create_dir_all(&sess_dir).unwrap();
    std::fs::create_dir_all(storage.join("message").join("ses_json_cache")).unwrap();
    std::fs::create_dir_all(storage.join("part").join("msg_json1")).unwrap();
    std::fs::create_dir_all(storage.join("part").join("msg_json2")).unwrap();
    std::fs::create_dir_all(storage.join("message").join("ses_dup")).unwrap();
    std::fs::create_dir_all(storage.join("part").join("msg_dup_json")).unwrap();

    // JSON-only session (never made it into SQLite)
    std::fs::write(
        sess_dir.join("ses_json_cache.json"),
        r#"{"id":"ses_json_cache","title":"Cache TTL tuning","directory":"/home/u/cache","time_created":1778957200000,"time_updated":1778957210000}"#,
    )
    .unwrap();
    std::fs::write(
        storage.join("message/ses_json_cache/msg_json1.json"),
        r#"{"id":"msg_json1","role":"user","time_created":1778957201000}"#,
    )
    .unwrap();
    std::fs::write(
        storage.join("part/msg_json1/prt_json1.json"),
        r#"{"id":"prt_json1","message_id":"msg_json1","type":"text","text":"we raised the cache TTL to 42 seconds using moka"}"#,
    )
    .unwrap();
    std::fs::write(
        storage.join("message/ses_json_cache/msg_json2.json"),
        r#"{"id":"msg_json2","role":"assistant","time_created":1778957202000}"#,
    )
    .unwrap();
    std::fs::write(
        storage.join("part/msg_json2/prt_json2.json"),
        r#"{"id":"prt_json2","message_id":"msg_json2","type":"text","text":"moka cache with 42 second TTL after the change"}"#,
    )
    .unwrap();

    // Duplicate of a SQLite session — the adapter must skip it (SQLite wins).
    std::fs::write(
        sess_dir.join("ses_dup.json"),
        r#"{"id":"ses_dup","title":"JSON impostor","directory":"/home/u/dup","time_created":1778957300000,"time_updated":1778957310000}"#,
    )
    .unwrap();
    std::fs::write(
        storage.join("message/ses_dup/msg_dup_json.json"),
        r#"{"id":"msg_dup_json","role":"user","time_created":1778957301000}"#,
    )
    .unwrap();
    std::fs::write(
        storage.join("part/msg_dup_json/prt_dup_json.json"),
        r#"{"id":"prt_dup_json","message_id":"msg_dup_json","type":"text","text":"the answer is forty three from json"}"#,
    )
    .unwrap();
}

// ---------------------------------------------------------------------------
// Shared sync/search helpers for integration tests
// ---------------------------------------------------------------------------

pub fn sync_fixture(
    fx: &Fixture,
    embed: Option<&dyn remember_mcp::embed::Embedder>,
    full: bool,
) -> remember_mcp::index::SyncSummary {
    use remember_mcp::index::{Index, SyncOptions, SyncOutcome, SyncProgress};
    use remember_mcp::redact::Redactor;
    use remember_mcp::sources::build_adapters;

    remember_mcp::register_sqlite_vec();
    let mut index = Index::open(&fx.index()).expect("open index");
    let adapters = build_adapters(&fx.db(), &fx.storage());
    let progress = SyncProgress::default();
    let redactor = Redactor::default();
    match index
        .sync(&SyncOptions {
            adapters: &adapters,
            embedder: embed,
            full,
            include_reasoning: false,
            redactor: &redactor,
            progress: &progress,
        })
        .expect("sync")
    {
        SyncOutcome::Done(s) => s,
        SyncOutcome::Busy => panic!("sync lock busy"),
    }
}

pub fn do_search(
    fx: &Fixture,
    query: &str,
    embed: Option<&dyn remember_mcp::embed::Embedder>,
) -> remember_mcp::search::SearchOutcome {
    use remember_mcp::index::{Index, SyncProgress};
    use remember_mcp::search::{search, SearchContext, SearchParams};

    remember_mcp::register_sqlite_vec();
    let _ = Index::open(&fx.index()).expect("open index");
    let progress = SyncProgress::default();
    let params = SearchParams {
        query: query.to_string(),
        limit: 5,
        ..Default::default()
    };
    let ctx = SearchContext {
        index_path: &fx.index(),
        embedder: embed,
        progress: &progress,
    };
    search(&params, &ctx).expect("search")
}

// ---------------------------------------------------------------------------
// Deterministic offline embedder (no network) — bag-of-words hashed vectors
// ---------------------------------------------------------------------------

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

impl remember_mcp::embed::Embedder for FakeEmbedder {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, remember_mcp::embed::EmbedError> {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
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
