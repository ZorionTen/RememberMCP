//! Protocol test (spec §14): spawn the server binary over stdio and call
//! every tool plus the resource through a minimal MCP client.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use common::Fixture;
use serde_json::{json, Value};

struct Client {
    child: Child,
    stdin: std::process::ChildStdin,
    rx: Receiver<Value>,
    next_id: i64,
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start(fx: &Fixture) -> Client {
    let mut child = Command::new(env!("CARGO_BIN_EXE_remember-mcp"))
        .env("OPENCODE_DB_PATH", fx.db())
        .env("OPENCODE_MEMORY_INDEX", fx.index())
        .env("EMBED_MODEL", "none") // offline: fts-only mode
        .env("LOG_LEVEL", "warn")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn server");

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let stdin = child.stdin.take().unwrap();

    let (tx, rx): (Sender<Value>, Receiver<Value>) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(trimmed) {
                Ok(v) => {
                    // only responses (have an id); ignore notifications
                    if v.get("id").is_some() {
                        let _ = tx.send(v);
                    }
                }
                Err(_) => eprintln!("non-json stdout line: {trimmed}"),
            }
        }
    });
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap_or(0) > 0 {
            eprintln!("[server] {}", line.trim_end());
            line.clear();
        }
    });

    Client {
        child,
        stdin,
        rx,
        next_id: 1,
    }
}

impl Client {
    fn send(&mut self, method: &str, params: Value) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{}", msg).expect("write stdin");
        self.stdin.flush().ok();
        id
    }

    fn notify(&mut self, method: &str, params: Value) {
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
        writeln!(self.stdin, "{}", msg).expect("write stdin");
        self.stdin.flush().ok();
    }

    fn recv(&self, id: i64) -> Value {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let remaining = deadline
                .checked_duration_since(std::time::Instant::now())
                .unwrap_or_else(|| panic!("timeout waiting for response id={id}"));
            match self.rx.recv_timeout(remaining) {
                Ok(v) if v["id"].as_i64() == Some(id) => return v,
                Ok(_) => continue, // unrelated response, keep waiting
                Err(_) => panic!("timeout waiting for response id={id}"),
            }
        }
    }

    fn call(&mut self, tool: &str, args: Value) -> Value {
        let id = self.send("tools/call", json!({"name": tool, "arguments": args}));
        let resp = self.recv(id);
        assert!(
            resp.get("error").is_none(),
            "tool {tool} returned error: {resp}"
        );
        resp["result"].clone()
    }
}

#[test]
fn every_tool_and_resource_over_stdio() {
    let fx = common::build();
    let mut c = start(&fx);

    // ---- initialize -------------------------------------------------------
    let id = c.send(
        "initialize",
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "protocol-test", "version": "0.0.0"}
        }),
    );
    let init = c.recv(id);
    assert_eq!(init["result"]["serverInfo"]["name"], "remember-mcp");
    assert!(init["result"]["capabilities"]["tools"].is_object());
    assert!(init["result"]["capabilities"]["resources"].is_object());
    c.notify("notifications/initialized", json!({}));

    // ---- tools/list -------------------------------------------------------
    let id = c.send("tools/list", json!({}));
    let tools = c.recv(id)["result"]["tools"].clone();
    let names: Vec<&str> = tools
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for expected in [
        "search_conversations",
        "list_sessions",
        "get_session",
        "get_chunk_context",
        "index_status",
        "reindex",
    ] {
        assert!(
            names.contains(&expected),
            "missing tool {expected}: {names:?}"
        );
    }
    for t in tools.as_array().unwrap() {
        assert_eq!(
            t["annotations"]["readOnlyHint"],
            json!(true),
            "tool {} must be read-only",
            t["name"]
        );
        let desc = t["description"].as_str().unwrap_or("");
        assert!(
            desc.contains("past OpenCode conversations")
                || t["name"] == "reindex"
                || t["name"] == "index_status",
            "tool {} should describe its data source: {desc}",
            t["name"]
        );
    }

    // ---- validation errors are protocol errors ----------------------------
    let id = c.send(
        "tools/call",
        json!({"name": "search_conversations", "arguments": {"query": "   "}}),
    );
    let resp = c.recv(id);
    assert_eq!(
        resp["error"]["code"],
        json!(-32602),
        "empty query → invalid_params: {resp}"
    );

    // ---- index_status -----------------------------------------------------
    let st = c.call("index_status", json!({}));
    let sc = &st["structuredContent"];
    assert!(sc["sessions"].as_i64().unwrap() >= 14, "sessions: {sc}");
    assert!(sc["chunks"].as_i64().unwrap() > 0, "chunks: {sc}");
    assert_eq!(sc["mode"], json!("fts-only"));
    assert!(st["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("sessions"));

    // ---- search (retry until startup sync finishes) -----------------------
    let mut results = Value::Null;
    for _ in 0..40 {
        let r = c.call(
            "search_conversations",
            json!({"query": "docker permission denied", "limit": 5}),
        );
        if r["structuredContent"]["results"]
            .as_array()
            .map(|a| !a.is_empty())
            == Some(true)
        {
            results = r["structuredContent"]["results"].clone();
            assert_eq!(r["structuredContent"]["mode"], json!("fts-only"));
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    assert!(
        results.is_array(),
        "search never returned results: {results}"
    );
    assert_eq!(results[0]["session_id"], json!("ses_docker_fix"));
    // text block present + quoted-history delimiter
    let r = c.call(
        "search_conversations",
        json!({"query": "docker permission denied", "limit": 5}),
    );
    let text = r["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("QUOTED HISTORY"));

    // ---- list_sessions ----------------------------------------------------
    let r = c.call("list_sessions", json!({"limit": 50}));
    let sessions = r["structuredContent"]["sessions"].as_array().unwrap();
    assert!(sessions.len() >= 14, "listed {}", sessions.len());
    assert!(sessions.iter().any(|s| s["session_id"] == "ses_json_cache"));

    // ---- get_session ------------------------------------------------------
    let r = c.call(
        "get_session",
        json!({"session_id": "ses_docker_fix", "include_tools": true}),
    );
    let text = r["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("docker group"), "transcript: {text}");
    assert!(text.contains("<<<HISTORY"));
    assert!(text.contains("<<<END HISTORY>>>"));

    // missing session → tool-level error, not a crash
    let r = c.call("get_session", json!({"session_id": "ses_nope"}));
    assert_eq!(r["isError"], json!(true));

    // ---- get_chunk_context ------------------------------------------------
    let chunk_id = results[0]["chunk_id"].as_i64().unwrap();
    let r = c.call(
        "get_chunk_context",
        json!({"chunk_id": chunk_id, "before": 1, "after": 1}),
    );
    let ctx = r["structuredContent"]["context"].as_array().unwrap();
    assert!(!ctx.is_empty());
    assert!(ctx.iter().any(|c| c["chunk_id"] == json!(chunk_id)));

    // ---- reindex ----------------------------------------------------------
    let r = c.call("reindex", json!({"full": false}));
    let sc = &r["structuredContent"];
    assert!(sc.get("duration_ms").is_some(), "summary: {sc}");
    assert_eq!(sc["mode"], json!("fts-only"));

    // ---- resources --------------------------------------------------------
    let id = c.send("resources/templates/list", json!({}));
    let templates = c.recv(id)["result"]["resourceTemplates"].clone();
    assert_eq!(
        templates[0]["uriTemplate"],
        json!("opencode://session/{session_id}")
    );

    let id = c.send(
        "resources/read",
        json!({"uri": "opencode://session/ses_docker_fix"}),
    );
    let resp = c.recv(id);
    let contents = &resp["result"]["contents"];
    let text = contents[0]["text"].as_str().unwrap();
    assert!(text.contains("docker group"), "resource: {text}");

    // unknown resource → resource not found
    let id = c.send(
        "resources/read",
        json!({"uri": "opencode://session/ses_missing"}),
    );
    let resp = c.recv(id);
    assert!(resp.get("error").is_some(), "expected error: {resp}");
}
