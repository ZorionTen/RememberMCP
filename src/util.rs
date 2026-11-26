//! Small shared helpers: lenient JSON, hashing, char truncation, LIKE escaping, time.
use chrono::TimeZone;

use sha2::{Digest, Sha256};

/// Parse JSON that may contain raw control characters inside strings
/// (observed in real OpenCode rows). Falls back to sanitising control
/// characters to spaces, which is valid in every JSON position.
pub fn parse_json_lenient(raw: &str) -> Result<serde_json::Value, serde_json::Error> {
    match serde_json::from_str(raw) {
        Ok(v) => Ok(v),
        Err(_) => {
            let cleaned: String = raw
                .chars()
                .map(|c| if (c as u32) < 0x20 { ' ' } else { c })
                .collect();
            serde_json::from_str(&cleaned)
        }
    }
}

/// First present string among `keys`.
pub fn get_str<'a>(v: &'a serde_json::Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| v.get(*k).and_then(|x| x.as_str()))
}

/// First present integer among `keys`, also probing `v.time.created` style nests.
pub fn get_ts(v: &serde_json::Value, keys: &[&str]) -> Option<i64> {
    for k in keys {
        match v.get(*k) {
            Some(serde_json::Value::Number(n)) => return n.as_i64(),
            Some(serde_json::Value::Object(_)) => {
                if let Some(n) = v
                    .get(*k)
                    .and_then(|o| o.get("created"))
                    .and_then(|n| n.as_i64())
                {
                    return Some(n);
                }
            }
            _ => {}
        }
    }
    v.get("time")
        .and_then(|t| t.get("created"))
        .and_then(|n| n.as_i64())
}

/// SHA-256 hex of a string (chunk content hash).
pub fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    let out = h.finalize();
    out.iter().map(|b| format!("{b:02x}")).collect()
}

/// Truncate to at most `n` chars.
pub fn take_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Escape `%`, `_` and `\` for use after `ESCAPE '\'` in LIKE.
pub fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Current wall-clock time in epoch ms.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Epoch ms → `YYYY-MM-DD` in local time (what the user experienced).
pub fn date_str(ms: i64) -> String {
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

/// Epoch ms → `YYYY-MM-DD HH:MM` in local time.
pub fn datetime_str(ms: i64) -> String {
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| ms.to_string())
}

/// Parse an ISO date (`YYYY-MM-DD`) or RFC3339 timestamp to epoch ms.
/// Date-only `since` → start of day; date-only `until` → end of day (inclusive).
pub fn parse_iso(s: &str) -> Option<(i64, bool)> {
    let s = s.trim();
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some((dt.timestamp_millis(), false));
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let start = d
            .and_hms_opt(0, 0, 0)
            .map(|dt| dt.and_utc().timestamp() * 1000)
            .unwrap_or(0);
        // date-only: treat `until` as inclusive end of day
        return Some((start, true));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lenient_json_handles_raw_control_chars() {
        let raw = "{\"a\":\"line1\nline2\"}"; // raw newline inside string
        assert!(serde_json::from_str::<serde_json::Value>(raw).is_err());
        assert!(parse_json_lenient(raw).is_ok());
    }

    #[test]
    fn ts_reads_nested_time_created() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{"time":{"created":1778956991630}}"#).unwrap();
        assert_eq!(get_ts(&v, &["time_created"]), Some(1778956991630));
    }

    #[test]
    fn like_escaping() {
        assert_eq!(escape_like("a%b_c\\d"), "a\\%b\\_c\\\\d");
    }

    #[test]
    fn iso_parsing() {
        let (ms, date_only) = parse_iso("2026-05-17").unwrap();
        assert!(date_only);
        assert!(ms > 0);
        assert!(parse_iso("2026-05-17T12:00:00Z").is_some());
        assert!(parse_iso("not-a-date").is_none());
    }
}
