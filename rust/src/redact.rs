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
                // the secret word anywhere in a variable name: API_TOKEN, GITHUB_TOKEN, DB_PASSWORD,
                // AWS_SECRET_ACCESS_KEY (a bare \b before it missed every one after an underscore);
                // and any name ending in _KEY: AZURE_STORAGE_ACCOUNT_KEY, GOOGLE_SERVICE_ACCOUNT_KEY
                // (but not SSH_KEY_FILE, whose value is a path)
                r"(?i)\b([a-z0-9_]*?(?:password|passwd|pwd|secret|token|api[_-]?key|apikey|access[_-]?key|private[_-]?key|credentials?|_key\b)[a-z0-9_]*)(\s*[=:]\s*)(\S+)",
                "${1}${2}***",
            ),
            (r"(?i)(--password[=\s]+|--token[=\s]+)(\S+)", "${1}***"),
            (r"(?i)(authorization:\s*(?:bearer|basic)\s+)(\S+)", "${1}***"),
            (r"(://[^:@/\s]+:)([^@/\s]+)(@)", "${1}***${3}"),
            (r"(?i)\b(?:ghp|gho|ghs|ghu|github_pat)_[A-Za-z0-9_]{20,}\b", "***"),
            (r"\bxox[baprs]-[A-Za-z0-9-]{10,}\b", "***"),
            (r"\bsk-[A-Za-z0-9]{20,}\b", "***"),
            // Provider token shapes adapted from Atuin's secrets.rs (MIT License, Copyright (c) 2021
            // Ellie Huxtable; see THIRD_PARTY_NOTICES.md): each masked in place, the command kept.
            (r"\bA[KS]IA[0-9A-Z]{16}\b", "***"),                              // AWS access key id (ASIA: temporary)
            (r"\bghr_[A-Za-z0-9]{76}\b", "***"),                              // GitHub refresh token
            (r"\bgh1_[A-Za-z0-9]{21}_[A-Za-z0-9]{59}\b", "***"),              // GitHub PAT (new)
            (r"\bv1\.[0-9A-Fa-f]{40}\b", "***"),                              // GitHub App installation token v1
            (r"\bglpat-[A-Za-z0-9_-]{20,}\b", "***"),                         // GitLab PAT
            (r"\bT[A-Za-z0-9_]{8}/B[A-Za-z0-9_]{8}/[A-Za-z0-9_]{24}\b", "***"), // Slack webhook path
            (r"\b[rs]k_(?:test|live)_[0-9A-Za-z]{24,}\b", "***"),             // Stripe secret / restricted key
            (r"\bnf[pcoub]_[0-9A-Za-z]{36}\b", "***"),                        // Netlify
            (r"\bnpm_[A-Za-z0-9]{36}\b", "***"),                              // npm
            (r"\bpul-[0-9a-f]{40}\b", "***"),                                 // Pulumi
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
    // a bare token (no `NAME=` in front) and a JWT: masked in place, the command kept
    out = residual_regexes()[0].replace_all(&out, "***").into_owned();
    mask_high_entropy(&out)
}

fn is_tok(c: char) -> bool {
    // not `/` or `\`: a path is not a token, however long and mixed its parts
    c.is_ascii_alphanumeric() || matches!(c, '+' | '=' | '_' | '-')
}

fn high_entropy_run(run: &str) -> bool {
    run.len() >= 40 && run.chars().any(|c| c.is_ascii_lowercase()) && run.chars().any(|c| c.is_ascii_uppercase()) && run.chars().any(|c| c.is_ascii_digit())
}

/// Opaque tokens replaced by `***`, everything around them kept.
fn mask_high_entropy(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        out.push_str(if high_entropy_run(run) { "***" } else { run });
        run.clear();
    };
    for c in text.chars() {
        if is_tok(c) {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
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
/// and digests have no uppercase, UUIDs are shorter, paths are split at `/` and `\` - all spared.
fn high_entropy(text: &str) -> bool {
    text.split(|c: char| !is_tok(c)).any(high_entropy_run)
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
    fn secret_words_inside_variable_names() {
        assert_eq!(redact("export API_TOKEN=abc123supersecret"), "export API_TOKEN=***");
        assert_eq!(redact("export GITHUB_TOKEN=abc"), "export GITHUB_TOKEN=***");
        assert_eq!(redact("DB_PASSWORD=hunter2 npm start"), "DB_PASSWORD=*** npm start");
        assert_eq!(redact("export AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI"), "export AWS_SECRET_ACCESS_KEY=***");
        assert_eq!(redact("set STRIPE_SECRET_KEY=abc"), "set STRIPE_SECRET_KEY=***");
        assert_eq!(redact("$env:OPENAI_API_KEY=\"abc\""), "$env:OPENAI_API_KEY=***");
        assert_eq!(redact("docker login --password-stdin"), "docker login --password-stdin");
        assert_eq!(redact("cat token-file.txt"), "cat token-file.txt");
    }

    #[test]
    fn provider_tokens() {
        // shapes from Atuin's secret patterns
        assert_eq!(redact("aws ASIAIOSFODNN7EXAMPLE s3 ls"), "aws *** s3 ls");
        assert_eq!(redact("export AZURE_STORAGE_ACCOUNT_KEY=KEYDATA"), "export AZURE_STORAGE_ACCOUNT_KEY=***");
        assert_eq!(redact("export GOOGLE_SERVICE_ACCOUNT_KEY=KEYDATA"), "export GOOGLE_SERVICE_ACCOUNT_KEY=***");
        assert_eq!(redact("export SSH_KEY_FILE=~/.ssh/id_ed25519"), "export SSH_KEY_FILE=~/.ssh/id_ed25519", "a path, not a key");
        assert_eq!(redact(&format!("git clone https://oauth2:{}@gitlab.com/x", "glpat-abcdefghij0123456789")), "git clone https://oauth2:***@gitlab.com/x");
        assert_eq!(redact("glab auth login --token glpat-abcdefghij0123456789"), "glab auth login --token ***");
        // fake tokens, put together here so that no token-shaped text sits in the source (a code
        // host's secret scanning would take it for a real one)
        let fake = |parts: &[&str]| parts.concat();
        let slack = fake(&["https://hooks.slack.com/services/", "T12345678", "/", "B12345678", "/", "abcdefghijklmnopqrstuvwx"]);
        assert_eq!(redact(&format!("curl -X POST {slack} -d x")), "curl -X POST https://hooks.slack.com/services/*** -d x");
        assert_eq!(redact(&format!("stripe login --api-key {}", fake(&["sk_", "live_", "abcdefghijklmnopqrstuvwx1234"]))), "stripe login --api-key ***");
        assert_eq!(redact(&format!("curl -u {}: https://api.stripe.com", fake(&["rk_", "test_", "abcdefghijklmnopqrstuvwx1234"]))), "curl -u ***: https://api.stripe.com");
        assert_eq!(redact(&format!("npm config set //registry.npmjs.org/:_authToken {}", fake(&["npm_", "abcdefghijklmnopqrstuvwxyz0123456789"]))), "npm config set //registry.npmjs.org/:_authToken ***");
        assert_eq!(redact(&format!("netlify deploy --auth {}", fake(&["nfp_", "abcdefghijklmnopqrstuvwxyz0123456789"]))), "netlify deploy --auth ***");
        assert_eq!(redact(&format!("pulumi login {}", fake(&["pul-", "0123456789abcdef", "0123456789abcdef01234567"]))), "pulumi login ***");
        assert_eq!(redact("git checkout v1.2.3"), "git checkout v1.2.3", "a version, not a token");
    }

    #[test]
    fn residual() {
        assert!(residual_secret("curl -d eyJhbGciOiJIUzI1.eyJzdWIiOiIxMjM0.SflKxwRJSMeKKF2QT4"));
        assert!(residual_secret("-----BEGIN RSA PRIVATE KEY"));
        assert!(residual_secret("x Ab3kL9mQ2pR7sT1vW5yZ8aC4dF6gH0jK2lN4pQ7rS9tU"));
        assert!(!residual_secret("git checkout 3f786850e387550fdab836ed7e6dc881de23001b"));
        assert!(!residual_secret("docker rm 123e4567-e89b-12d3-a456-426614174000"));
        assert!(residual_secret("-----BEGIN RSA PRIVATE KEY"));
    }

    #[test]
    fn tokens_masked_in_place_paths_spared() {
        // a bare token and a JWT are masked, the command kept
        assert_eq!(redact("curl -H 'x-key: Ab3kL9mQ2pR7sT1vW5yZ8aC4dF6gH0jK2lN4pQ7rS9tU' https://x"), "curl -H 'x-key: ***' https://x");
        assert_eq!(redact("curl -d eyJhbGciOiJIUzI1.eyJzdWIiOiIxMjM0.SflKxwRJSMeKKF2QT4 x"), "curl -d *** x");
        assert!(safe_command("x Ab3kL9mQ2pR7sT1vW5yZ8aC4dF6gH0jK2lN4pQ7rS9tU", true).is_some());
        // a long path is not a token
        let p = "ls /Users/Dev1/AppData/Local/Temp/claude/c--Users-dev-reman/48361776-88a3-41ab-a8a9-31cdc8b91774/scratchpad";
        assert_eq!(redact(p), p);
        assert!(!residual_secret(p));
        assert_eq!(redact(r"cd C:\Users\Dev1\AppData\Local\Programs\SomethingLongNamed2024Edition\bin"), r"cd C:\Users\Dev1\AppData\Local\Programs\SomethingLongNamed2024Edition\bin");
    }
}
