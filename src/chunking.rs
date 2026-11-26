//! Exchange-based chunking (spec §6).
//!
//! Unit = one user message plus the assistant messages that follow it until
//! the next user message. Target 300–600 tokens (chars/4); longer exchanges
//! split on paragraph boundaries with ~80 tokens of overlap, never inside a
//! fenced code block unless the block alone exceeds the limit.

use crate::redact::Redactor;
use crate::sources::{MessageRecord, PartRecord, Role, SessionRecord};
use crate::util::{date_str, sha256_hex, take_chars};

/// Hard ceiling: 600 tokens ≈ 2400 chars (target range starts at ~1200).
const MAX_CHARS: usize = 2400;
/// Overlap between split pieces: ~80 tokens ≈ 320 chars.
const OVERLAP_CHARS: usize = 320;
/// Tool input render cap (chars).
const TOOL_INPUT_CHARS: usize = 300;
/// Tool output render cap (chars).
const TOOL_OUTPUT_CHARS: usize = 500;
/// Tool outputs above this keep only name + size note (bytes).
const TOOL_OUTPUT_MAX_BYTES: usize = 20 * 1024;

pub const CHUNKER_VERSION: &str = "1";

#[derive(Debug, Clone)]
pub struct Chunk {
    pub session_id: String,
    pub first_message_id: String,
    pub last_message_id: String,
    pub ts: i64,
    /// Distinct roles contributing, e.g. `user,assistant`.
    pub roles: String,
    /// Prefix + body (redaction already applied). This exact string is stored,
    /// embedded and hashed.
    pub text: String,
    pub token_est: usize,
    pub content_hash: String,
}

pub struct ChunkOptions<'a> {
    pub include_reasoning: bool,
    pub redactor: &'a Redactor,
}

/// Chunk one session into indexable pieces.
pub fn chunk_session(rec: &SessionRecord, opts: &ChunkOptions<'_>) -> Vec<Chunk> {
    let prefix = format!(
        "Project: {} | Session: {} | {}",
        basename(&rec.meta.project_dir),
        if rec.meta.title.trim().is_empty() {
            "untitled"
        } else {
            rec.meta.title.trim()
        },
        date_str(rec.meta.time_created)
    );

    let exchanges = group_exchanges(&rec.messages);
    let mut out = Vec::new();

    for ex in &exchanges {
        let body = render_exchange(ex, opts.include_reasoning);
        if body.trim().is_empty() {
            continue;
        }
        let ts = ex.first().map(|m| m.time_created).unwrap_or(0);
        let first_id = ex.first().map(|m| m.id.clone()).unwrap_or_default();
        let last_id = ex.last().map(|m| m.id.clone()).unwrap_or_default();
        let mut roles: Vec<&str> = Vec::new();
        for m in ex {
            let s = m.role.as_str();
            if !roles.contains(&s) {
                roles.push(s);
            }
        }
        if roles.contains(&"user") && roles.contains(&"assistant") {
            roles = vec!["user", "assistant"];
        }
        let roles = roles.join(",");

        for piece in split_exchange(&body) {
            let raw_text = format!("{prefix}\n\n{piece}");
            let text = opts.redactor.redact(&raw_text);
            let token_est = text.chars().count() / 4;
            let content_hash = sha256_hex(&text);
            out.push(Chunk {
                session_id: rec.meta.id.clone(),
                first_message_id: first_id.clone(),
                last_message_id: last_id.clone(),
                ts,
                roles: roles.clone(),
                text,
                token_est,
                content_hash,
            });
        }
    }
    out
}

fn basename(p: &str) -> String {
    p.rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or(p)
        .to_string()
}

/// Group messages into exchanges: a user message starts a new exchange;
/// assistant messages join the current one. Leading assistant messages (no
/// user yet) form their own exchange.
fn group_exchanges(messages: &[MessageRecord]) -> Vec<Vec<&MessageRecord>> {
    let mut exchanges: Vec<Vec<&MessageRecord>> = Vec::new();
    for m in messages {
        let start_new = m.role == Role::User;
        if start_new || exchanges.is_empty() {
            exchanges.push(Vec::new());
        }
        if let Some(last) = exchanges.last_mut() {
            last.push(m);
        }
    }
    exchanges
}

fn render_exchange(msgs: &[&MessageRecord], include_reasoning: bool) -> String {
    let mut blocks = Vec::new();
    for m in msgs {
        let mut block = String::new();
        for part in &m.parts {
            match part {
                PartRecord::Text(t) => {
                    block.push_str(t);
                    block.push('\n');
                }
                PartRecord::Tool {
                    name,
                    input,
                    output,
                    output_len,
                } => {
                    if *output_len > TOOL_OUTPUT_MAX_BYTES {
                        block.push_str(&format!(
                            "tool: {name} [result omitted: {output_len} bytes]\n"
                        ));
                    } else {
                        block.push_str(&format!(
                            "tool: {} {}\n",
                            name,
                            take_chars(input, TOOL_INPUT_CHARS)
                        ));
                        if let Some(o) = output {
                            block.push_str(&format!(
                                "result: {}\n",
                                take_chars(o, TOOL_OUTPUT_CHARS)
                            ));
                        }
                    }
                }
                PartRecord::Reasoning(t) => {
                    if include_reasoning {
                        block.push_str("reasoning: ");
                        block.push_str(t);
                        block.push('\n');
                    }
                }
                PartRecord::File { filename, mime } => {
                    if mime.starts_with("text/") {
                        block.push_str(&format!("file: {filename}\n"));
                    }
                    // binary/data-URL attachments excluded entirely
                }
                PartRecord::Other(_) => {
                    // patches, snapshots, step-*, compaction: excluded
                }
            }
        }
        if !block.trim().is_empty() {
            blocks.push(block.trim_end().to_string());
        }
    }
    blocks.join("\n\n")
}

/// Split an exchange body into ≤MAX_CHARS pieces on paragraph boundaries,
/// keeping fenced code blocks atomic, with ~OVERLAP_CHARS of overlap.
fn split_exchange(body: &str) -> Vec<String> {
    if body.chars().count() <= MAX_CHARS {
        return vec![body.to_string()];
    }

    let units = atomic_units(body);

    let mut pieces: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;

    for unit in units {
        let unit_len = unit.chars().count();
        if current.is_empty() {
            current = unit;
            current_len = unit_len;
            continue;
        }
        if current_len + 2 + unit_len <= MAX_CHARS {
            current.push_str("\n\n");
            current.push_str(&unit);
            current_len += 2 + unit_len;
        } else {
            pieces.push(std::mem::take(&mut current));
            current_len = 0;
            // overlap: trailing paragraphs of the previous piece ≤ OVERLAP_CHARS
            let overlap = tail_overlap(pieces.last().map(String::as_str).unwrap_or(""), &unit);
            if !overlap.is_empty() {
                current = overlap;
                current_len = current.chars().count();
            }
            current.push_str("\n\n");
            current.push_str(&unit);
            current_len += 2 + unit_len;
        }
    }
    if !current.trim().is_empty() {
        pieces.push(current);
    }

    // A single unit larger than the limit (giant code block) is line-split.
    let mut final_pieces = Vec::new();
    for p in pieces {
        if p.chars().count() <= MAX_CHARS {
            final_pieces.push(p);
        } else {
            final_pieces.extend(hard_split(&p));
        }
    }
    final_pieces
}

/// Split paragraphs into atomic units; a fenced code block (``` … ```) is one
/// unit even when it contains blank lines.
fn atomic_units(body: &str) -> Vec<String> {
    let paragraphs: Vec<&str> = body.split("\n\n").collect();
    let mut units: Vec<String> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    let mut in_fence = false;

    for para in paragraphs {
        current.push(para);
        let fence_markers = para
            .lines()
            .filter(|l| l.trim_start().starts_with("```"))
            .count();
        if fence_markers % 2 == 1 {
            in_fence = !in_fence;
        }
        if !in_fence {
            units.push(current.join("\n\n"));
            current.clear();
        }
    }
    if !current.is_empty() {
        units.push(current.join("\n\n"));
    }
    units
}

/// Trailing paragraphs of `prev` totaling ≤ OVERLAP_CHARS (empty if the last
/// paragraph alone is too big).
fn tail_overlap(prev: &str, _next: &str) -> String {
    let paras: Vec<&str> = prev.split("\n\n").collect();
    let mut acc: Vec<&str> = Vec::new();
    let mut len = 0usize;
    for p in paras.iter().rev() {
        let l = p.chars().count();
        if len + l > OVERLAP_CHARS {
            break;
        }
        acc.push(p);
        len += l;
    }
    if acc.is_empty() {
        return String::new();
    }
    acc.reverse();
    acc.join("\n\n")
}

/// Last-resort line split for a unit that alone exceeds MAX_CHARS
/// (only happens when a single code block is huge — allowed by spec).
fn hard_split(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for line in s.lines() {
        if cur.chars().count() + line.chars().count() + 1 > MAX_CHARS && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        cur.push_str(line);
        cur.push('\n');
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::{MessageRecord, PartRecord, Role, SessionMeta, SessionRecord};

    fn meta(id: &str) -> SessionMeta {
        SessionMeta {
            id: id.to_string(),
            project_dir: "/home/u/proj".into(),
            title: "Test session".into(),
            source: "sqlite_v1",
            time_created: 1_778_956_991_614,
            time_updated: 1_778_957_898_685,
            archived: false,
            message_count: 0,
        }
    }

    fn msg(id: &str, role: Role, parts: Vec<PartRecord>) -> MessageRecord {
        MessageRecord {
            id: id.into(),
            role,
            time_created: 1_778_956_991_614,
            parts,
        }
    }

    fn opts(red: &Redactor) -> ChunkOptions<'_> {
        ChunkOptions {
            include_reasoning: false,
            redactor: red,
        }
    }

    #[test]
    fn exchange_grouping() {
        let msgs = vec![
            msg("m1", Role::User, vec![PartRecord::Text("q1".into())]),
            msg("m2", Role::Assistant, vec![PartRecord::Text("a1".into())]),
            msg("m3", Role::Assistant, vec![PartRecord::Text("a2".into())]),
            msg("m4", Role::User, vec![PartRecord::Text("q2".into())]),
            msg("m5", Role::Assistant, vec![PartRecord::Text("a3".into())]),
        ];
        let ex = group_exchanges(&msgs);
        assert_eq!(ex.len(), 2);
        assert_eq!(ex[0].len(), 3);
        assert_eq!(ex[1].len(), 2);
    }

    #[test]
    fn chunk_has_prefix_and_hash() {
        let rec = SessionRecord {
            meta: meta("ses_x"),
            messages: vec![
                msg(
                    "m1",
                    Role::User,
                    vec![PartRecord::Text(
                        "how do I fix docker permission denied?".into(),
                    )],
                ),
                msg(
                    "m2",
                    Role::Assistant,
                    vec![PartRecord::Text("usermod -aG docker $USER".into())],
                ),
            ],
        };
        let red = Redactor::default();
        let chunks = chunk_session(&rec, &opts(&red));
        assert_eq!(chunks.len(), 1);
        let c = &chunks[0];
        let expected_prefix = format!(
            "Project: proj | Session: Test session | {}",
            crate::util::date_str(1_778_956_991_614)
        );
        assert!(c.text.starts_with(&expected_prefix));
        assert!(c.text.contains("docker permission denied"));
        assert_eq!(c.roles, "user,assistant");
        assert_eq!(c.content_hash, sha256_hex(&c.text));
        assert!(c.token_est > 0);
        assert_eq!(c.first_message_id, "m1");
        assert_eq!(c.last_message_id, "m2");
    }

    #[test]
    fn tool_rendering_caps() {
        let rec = SessionRecord {
            meta: meta("ses_t"),
            messages: vec![msg(
                "m1",
                Role::Assistant,
                vec![
                    PartRecord::Tool {
                        name: "bash".into(),
                        input: "x".repeat(1000),
                        output: Some("y".repeat(1000)),
                        output_len: 1000,
                    },
                    PartRecord::Tool {
                        name: "shell".into(),
                        input: "ignored".into(),
                        output: Some("big".into()),
                        output_len: 50_000,
                    },
                ],
            )],
        };
        let red = Redactor::default();
        let chunks = chunk_session(&rec, &opts(&red));
        let t = &chunks[0].text;
        assert!(t.contains("tool: bash "));
        let input_part = t.split("tool: bash ").nth(1).unwrap();
        assert!(input_part.lines().next().unwrap().chars().count() <= 300 + 10);
        assert!(t.contains("[result omitted: 50000 bytes]"));
        assert!(!t.contains("ignored")); // oversized result drops the input too
        let result_line = t.lines().find(|l| l.starts_with("result: ")).unwrap();
        assert!(result_line.chars().count() <= 500 + 8);
    }

    #[test]
    fn reasoning_excluded_by_default() {
        let rec = SessionRecord {
            meta: meta("ses_r"),
            messages: vec![msg(
                "m1",
                Role::Assistant,
                vec![
                    PartRecord::Reasoning("secret internal thought".into()),
                    PartRecord::Text("visible answer".into()),
                ],
            )],
        };
        let red = Redactor::default();
        let c = chunk_session(&rec, &opts(&red));
        assert!(!c[0].text.contains("secret internal thought"));
        assert!(c[0].text.contains("visible answer"));

        let o = ChunkOptions {
            include_reasoning: true,
            redactor: &red,
        };
        let c2 = chunk_session(&rec, &o);
        assert!(c2[0].text.contains("secret internal thought"));
    }

    #[test]
    fn long_exchange_splits_with_overlap_and_keeps_fences_atomic() {
        let fence =
            "```rust\nfn calculate_total(items: &[u32]) -> u32 {\n    items.iter().sum()\n}\n```";
        let para =
            "The deployment pipeline uses blue-green strategy on AWS ECS with health checks.";
        let body = format!("{fence}\n\n{}", vec![para; 60].join("\n\n"));
        let rec = SessionRecord {
            meta: meta("ses_long"),
            messages: vec![msg("m1", Role::User, vec![PartRecord::Text(body.clone())])],
        };
        let red = Redactor::default();
        let chunks = chunk_session(&rec, &opts(&red));
        assert!(chunks.len() > 1, "long exchange must split");
        for c in &chunks {
            assert!(
                c.text.chars().count() <= MAX_CHARS + 200,
                "piece over limit"
            );
        }
        // the fence must never be torn: every piece containing the opening
        // fence also contains the closing fence
        for c in &chunks {
            let opens = c.text.matches("```rust").count();
            let closes = c.text.matches("}\n```").count() + c.text.matches("\n```").count();
            if opens > 0 {
                assert!(
                    c.text.contains("```rust") && c.text.matches("```").count() % 2 == 0,
                    "unbalanced fence in chunk"
                );
            }
            let _ = closes;
        }
        // overlap: some content appears in two pieces
        let texts: Vec<&str> = chunks.iter().map(|c| c.text.as_str()).collect();
        let mut found_dup = false;
        for i in 0..texts.len() - 1 {
            if texts[i].len() > 100 && texts[i + 1].contains(&texts[i][texts[i].len() - 60..]) {
                found_dup = true;
            }
        }
        assert!(found_dup, "expected overlap between adjacent pieces");
    }

    #[test]
    fn redaction_runs_before_store() {
        let rec = SessionRecord {
            meta: meta("ses_sec"),
            messages: vec![msg(
                "m1",
                Role::User,
                vec![PartRecord::Text(
                    "key is sk-proj-abc1234567890abcdef1234567890".into(),
                )],
            )],
        };
        let red = Redactor::default();
        let c = chunk_session(&rec, &opts(&red));
        assert!(!c[0].text.contains("sk-proj-abc1234567890abcdef"));
        assert!(c[0].text.contains("[REDACTED:api_key]"));
    }
}
