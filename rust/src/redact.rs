//! Secret scrubbing for anything handed to an agent (port of reman_mcp._redact / _residual_secret).
//! Keeps the command's SHAPE, hides the value. Rust `regex` has no lookahead, so the high-entropy
//! residual check is a small scanner instead of the Python lookahead regex.
use regex::Regex;
use std::sync::OnceLock;

fn patterns() -> &'static [(Regex, &'static str)] {
    static P: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    P.get_or_init(|| {
        [
            (
                r"(?i)\b(password|passwd|pwd|secret|token|api[_-]?key|apikey|access[_-]?key|auth[_-]?token|client[_-]?secret)(\s*[=:]\s*)(\S+)",
                "${1}${2}***",
            ),
            (r"(?i)(--password[=\s]+|--token[=\s]+)(\S+)", "${1}***"),
            (r"(?i)(authorization:\s*(?:bearer|basic)\s+)(\S+)", "${1}***"),
            (r"(://[^:@/\s]+:)([^@/\s]+)(@)", "${1}***${3}"),
            (r"\bAKIA[0-9A-Z]{16}\b", "***"),
            (r"(?i)\b(?:ghp|gho|ghs|ghu|github_pat)_[A-Za-z0-9_]{20,}\b", "***"),
            (r"\bxox[baprs]-[A-Za-z0-9-]{10,}\b", "***"),
            (r"\bsk-[A-Za-z0-9]{20,}\b", "***"),
        ]
        .into_iter()
        .map(|(p, r)| (Regex::new(p).expect("redact pattern"), r))
        .collect()
    })
}

pub fn redact(text: &str) -> String {
    let mut out = text.to_string();
    for (rx, repl) in patterns() {
        out = rx.replace_all(&out, *repl).into_owned();
    }
    out
}

fn residual_regexes() -> &'static [Regex] {
    static R: OnceLock<Vec<Regex>> = OnceLock::new();
    R.get_or_init(|| {
        vec![
            Regex::new(r"eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{4,}").unwrap(), // JWT
            Regex::new(r"(?i)-----BEGIN[ A-Z]*PRIVATE KEY").unwrap(),                          // PEM
        ]
    })
}

/// Opaque high-entropy token: a run of 40+ token chars mixing lower + upper + digit. Pure-hex SHAs
/// and digests have no uppercase, UUIDs are shorter - both are spared.
fn high_entropy(text: &str) -> bool {
    let is_tok = |c: char| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '_' | '-');
    text.split(|c: char| !is_tok(c)).any(|run| {
        run.len() >= 40
            && run.chars().any(|c| c.is_ascii_lowercase())
            && run.chars().any(|c| c.is_ascii_uppercase())
            && run.chars().any(|c| c.is_ascii_digit())
    })
}

pub fn residual_secret(text: &str) -> bool {
    residual_regexes().iter().any(|r| r.is_match(text)) || high_entropy(text)
}

/// Redact; in strict mode withhold entirely if something still looks secret.
pub fn safe_command(cmd: &str, strict: bool) -> Option<String> {
    let red = redact(cmd);
    if strict && residual_secret(&red) { None } else { Some(red) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrubs_known_shapes() {
        assert_eq!(redact("mysql --password=hunter2 db"), "mysql --password=*** db");
        assert_eq!(redact("export API_KEY=abc123"), "export API_KEY=***");
        assert_eq!(redact("git clone https://bob:s3cret@github.com/x"), "git clone https://bob:***@github.com/x");
        assert_eq!(redact("curl -H 'Authorization: Bearer abc.def'"), "curl -H 'Authorization: Bearer ***");
        assert_eq!(redact("aws AKIAABCDEFGHIJKLMNOP"), "aws ***");
        assert_eq!(redact("gh ghp_aaaaaaaaaaaaaaaaaaaaaaaaaaaa"), "gh ***");
        assert_eq!(redact("git status"), "git status");
    }

    #[test]
    fn residual() {
        assert!(residual_secret("curl -d eyJhbGciOiJIUzI1.eyJzdWIiOiIxMjM0.SflKxwRJSMeKKF2QT4"));
        assert!(residual_secret("-----BEGIN RSA PRIVATE KEY"));
        assert!(residual_secret("x Ab3kL9mQ2pR7sT1vW5yZ8aC4dF6gH0jK2lN4pQ7rS9tU"));
        assert!(!residual_secret("git checkout 3f786850e387550fdab836ed7e6dc881de23001b"));
        assert!(!residual_secret("docker rm 123e4567-e89b-12d3-a456-426614174000"));
        assert_eq!(safe_command("x Ab3kL9mQ2pR7sT1vW5yZ8aC4dF6gH0jK2lN4pQ7rS9tU", true), None);
    }
}
