//! Secret redaction — runs before any text is stored in the index or embedded,
//! and again on transcripts returned to MCP clients.
//!
//! Every match is replaced with `[REDACTED:<kind>]`. Patterns avoid regex
//! lookarounds (unsupported by the `regex` crate).

use std::sync::LazyLock;

use regex::Regex;

struct Pattern {
    kind: &'static str,
    re: Regex,
    /// Custom replacement (env-style `KEY=value` keeps the key name).
    env_style: bool,
}

static BUILTIN: LazyLock<Vec<Pattern>> = LazyLock::new(|| {
    let p = |kind: &'static str, pat: &str, env_style: bool| Pattern {
        kind,
        re: Regex::new(pat).expect("builtin redaction pattern"),
        env_style,
    };
    vec![
        // Private key blocks first (multi-line).
        p(
            "private_key",
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
            false,
        ),
        // Cloud/API keys.
        p(
            "aws_key",
            r"\b(?:AKIA|ASIA|AIDA|AROA|AIPA|ANPA|ANVA|A3T)[A-Z0-9]{16}\b",
            false,
        ),
        p("api_key", r"\bsk-[A-Za-z0-9_\-]{16,}\b", false),
        p(
            "github_token",
            r"\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{36,44}\b",
            false,
        ),
        p("github_token", r"\bgithub_pat_[A-Za-z0-9_]{22,}\b", false),
        p("slack_token", r"\bxox[baprs]-[A-Za-z0-9\-]{10,}\b", false),
        p("stripe_key", r"\b[sr]k_live_[A-Za-z0-9]{16,}\b", false),
        p("google_key", r"\bAIza[0-9A-Za-z_\-]{35}\b", false),
        // Auth headers + JWTs.
        p("auth", r"(?i)\bbearer\s+[A-Za-z0-9_\-.~+/=]{8,}", false),
        p(
            "jwt",
            r"\beyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{4,}\b",
            false,
        ),
        // KEY=value where the key name contains SECRET/TOKEN/PASSWORD/KEY.
        p(
            "env",
            r#"(?i)\b([A-Za-z0-9_\-]*(?:SECRET|TOKEN|PASSWORD|KEY)[A-Za-z0-9_\-]*)\s*=\s*["']?[^\s"',;]{4,}"#,
            true,
        ),
    ]
});

pub struct Redactor {
    extra: Vec<Pattern>,
}

impl Redactor {
    pub fn new(extra_patterns: &[String]) -> Self {
        let mut extra = Vec::new();
        for pat in extra_patterns {
            match Regex::new(pat) {
                Ok(re) => extra.push(Pattern {
                    kind: "custom",
                    re,
                    env_style: false,
                }),
                Err(e) => log::warn!("ignoring invalid EXTRA_REDACT_REGEX `{pat}`: {e}"),
            }
        }
        Self { extra }
    }

    pub fn redact(&self, input: &str) -> String {
        let mut out = input.to_string();
        for pat in BUILTIN.iter().chain(self.extra.iter()) {
            if pat.env_style {
                out = pat
                    .re
                    .replace_all(&out, |caps: &regex::Captures| {
                        format!("{}=[REDACTED:env]", &caps[1])
                    })
                    .into_owned();
            } else {
                out = pat
                    .re
                    .replace_all(&out, format!("[REDACTED:{}]", pat.kind))
                    .into_owned();
            }
        }
        out
    }
}

impl Default for Redactor {
    fn default() -> Self {
        Self::new(&[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(s: &str) -> String {
        Redactor::default().redact(s)
    }

    #[test]
    fn aws_key() {
        assert_eq!(r("id = AKIAIOSFODNN7EXAMPLE"), "id = [REDACTED:aws_key]");
        // negative: same shape but wrong prefix
        let neg = "AKIAIOSFODNN7EXAMPL";
        assert_eq!(r(neg), neg);
    }

    #[test]
    fn sk_prefixed_keys() {
        assert_eq!(
            r("use sk-proj-abc1234567890abcdef1234567890abcdef"),
            "use [REDACTED:api_key]"
        );
        let neg = "task-abc1234567890abcdef";
        assert_eq!(r(neg), neg);
    }

    #[test]
    fn github_tokens() {
        assert_eq!(
            r("token ghp_1234567890123456789012345678901234567890"),
            "token [REDACTED:github_token]"
        );
        let neg = "ghp_12345"; // too short
        assert_eq!(r(neg), neg);
    }

    #[test]
    fn slack_and_stripe() {
        assert_eq!(
            r("hook xoxb-1234567890-abcdef123456"),
            "hook [REDACTED:slack_token]"
        );
        assert_eq!(
            r("key sk_live_1234567890abcdef"),
            "key [REDACTED:stripe_key]"
        );
        let neg = "sk_test_1234567890abcdef"; // test keys not matched
        assert_eq!(r(neg), neg);
    }

    #[test]
    fn google_key() {
        let k = "AIzaSyA1234567890abcdefghijklmnopqrstuv";
        assert_eq!(r(&format!("k={k}")), "k=[REDACTED:google_key]");
        let neg = "AIza_short";
        assert_eq!(r(neg), neg);
    }

    #[test]
    fn bearer_header() {
        assert_eq!(
            r("Authorization: Bearer abcdef1234567890"),
            "Authorization: [REDACTED:auth]"
        );
        let neg = "Bearer short"; // < 8 chars of token
        assert_eq!(r(neg), neg);
    }

    #[test]
    fn jwt() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
        assert_eq!(r(jwt), "[REDACTED:jwt]");
        let neg = "eyJhbGciOiJIUzI1NiJ9.not.a.jwt-format-here";
        assert_eq!(r(neg), neg);
    }

    #[test]
    fn private_key_block() {
        let block = "-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBg\n-----END PRIVATE KEY-----";
        assert_eq!(r(block), "[REDACTED:private_key]");
        let neg = "-----BEGIN PUBLIC KEY-----\nabc\n-----END PUBLIC KEY-----";
        assert_eq!(r(neg), neg);
    }

    #[test]
    fn env_assignments_keep_key_name() {
        assert_eq!(
            r("DATABASE_PASSWORD=hunter2secret"),
            "DATABASE_PASSWORD=[REDACTED:env]"
        );
        assert_eq!(r("api_key = abcd1234"), "api_key=[REDACTED:env]");
        // negative: key name without the trigger words
        assert_eq!(r("PORT=8080"), "PORT=8080");
        // negative: value too short to be a secret
        assert_eq!(r("TOKEN=ab"), "TOKEN=ab");
    }

    #[test]
    fn extra_patterns() {
        let red = Redactor::new(&[r"(?i)internal-\d{4}".to_string()]);
        assert_eq!(
            red.redact("case internal-9971 opened"),
            "case [REDACTED:custom] opened"
        );
        // invalid patterns are ignored, not fatal
        let red2 = Redactor::new(&["(".to_string()]);
        assert_eq!(red2.redact("nothing"), "nothing");
    }

    #[test]
    fn clean_text_untouched() {
        let s = "the docker permission denied fix was usermod -aG docker";
        assert_eq!(r(s), s);
    }
}
