//! What a failed command printed, kept small and safe, and its SIGNATURE: the error with the
//! particulars (paths, names, numbers, quoted values) taken out, so two different commands that
//! fail the same way share one ("npm ERR! ERESOLVE unable to resolve dependency tree").
//! A proven fix for one of them is then worth offering for the other.

/// Lines that say what went wrong, most telling first.
const ERROR_WORDS: &[&str] = &[
    "error", "err!", "fatal", "failed", "failure", "not found", "no such", "denied", "refused", "cannot", "can't", "could not",
    "couldn't", "unable", "invalid", "unknown", "missing", "exception", "traceback", "not recognized", "timed out", "conflict",
];

fn error_line(line: &str) -> bool {
    let l = line.to_lowercase();
    ERROR_WORDS.iter().any(|w| l.contains(w))
}

/// Keep what a failure printed: redacted, the error lines first (else its last lines), at most
/// 300 characters. None for nothing useful.
pub fn clean(text: &str) -> Option<String> {
    let text = crate::redact::redact(text);
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if lines.is_empty() {
        return None;
    }
    let picked: Vec<&str> = {
        let errs: Vec<&str> = lines.iter().copied().filter(|l| error_line(l)).take(3).collect();
        if errs.is_empty() { lines.iter().rev().take(3).rev().copied().collect() } else { errs }
    };
    let mut out = picked.join("\n");
    if out.chars().count() > 300 {
        out = format!("{}…", out.chars().take(299).collect::<String>());
    }
    Some(out)
}

/// The error with its particulars taken out, or None when there's no error line. Two failures
/// with the same signature failed the same way.
pub fn signature(err: &str) -> Option<String> {
    let line = err.lines().find(|l| error_line(l))?;
    let mut out: Vec<String> = Vec::new();
    for w in line.split_whitespace() {
        let t = w.trim_matches(|c: char| matches!(c, ',' | ';' | '(' | ')' | '[' | ']' | '.'));
        let particular = t.contains(['/', '\\', '@', '='])
            || t.starts_with(['\'', '"', '`'])
            || t.chars().any(|c| c.is_ascii_digit())
            || (t.len() > 24);
        let w = if particular { "_".to_string() } else { t.to_lowercase() };
        if !(w == "_" && out.last().is_some_and(|l| l == "_")) {
            out.push(w);
        }
    }
    let s = out.join(" ");
    // too little left to mean anything ("error: _")
    (s.split_whitespace().filter(|w| *w != "_").count() >= 3).then(|| s.chars().take(120).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_error_lines() {
        let out = "added 3 packages\nnpm ERR! code ERESOLVE\nnpm ERR! ERESOLVE unable to resolve dependency tree\nnpm ERR! Found: react@18.2.0";
        assert_eq!(clean(out).unwrap().lines().next().unwrap(), "npm ERR! code ERESOLVE");
        assert_eq!(clean("\n\n"), None);
        assert!(clean("export API_TOKEN=abc123 failed").unwrap().contains("API_TOKEN=***"));
    }

    #[test]
    fn same_error_same_signature() {
        let a = signature("fatal: The current branch feat/search has no upstream branch.").unwrap();
        let b = signature("fatal: The current branch fix-login-7 has no upstream branch.").unwrap();
        assert_eq!(a, b);
        assert_eq!(a, "fatal: the current branch _ has no upstream branch");
        let c = signature("npm ERR! ERESOLVE unable to resolve dependency tree").unwrap();
        assert_ne!(a, c);
        assert_eq!(signature("all good"), None);
        assert_eq!(signature("error: 42"), None);
    }
}
