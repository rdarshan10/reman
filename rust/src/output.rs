//! What a command printed, as reman keeps it: plain text (no colours or cursor moves), its end
//! (where errors and results are), redacted unless the user keeps secrets as typed.

/// The most kept of one run's output: its last lines up to this many bytes.
pub const MAX_BYTES: usize = 8 * 1024;

/// Text without terminal escapes: colours, cursor moves, titles, marks. A carriage return that
/// isn't part of a line end rewrites the line from its start (progress bars keep their last state).
pub fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut line = String::new();
    let mut cs = text.chars().peekable();
    while let Some(c) = cs.next() {
        match c {
            '\x1b' => match cs.next() {
                // CSI: parameters, then one final byte
                Some('[') => {
                    while let Some(&n) = cs.peek() {
                        cs.next();
                        if ('@'..='~').contains(&n) {
                            break;
                        }
                    }
                }
                // OSC, DCS, APC, PM, SOS: up to BEL or ESC \
                Some(']' | 'P' | '_' | '^' | 'X') => {
                    while let Some(n) = cs.next() {
                        if n == '\x07' || (n == '\x1b' && cs.peek() == Some(&'\\')) {
                            if n == '\x1b' {
                                cs.next();
                            }
                            break;
                        }
                    }
                }
                // a charset choice takes one more character
                Some('(' | ')' | '*' | '+') => {
                    cs.next();
                }
                _ => {}
            },
            '\r' if cs.peek() == Some(&'\n') => {}
            '\r' => line.clear(),
            '\n' => {
                out.push_str(line.trim_end());
                out.push('\n');
                line.clear();
            }
            '\x08' => {
                line.pop();
            }
            '\t' => line.push_str("    "),
            c if c.is_control() => {}
            c => line.push(c),
        }
    }
    out.push_str(line.trim_end());
    out
}

/// What to keep of a run's output: plain, without blank lines at either end or runs of them in
/// between, its end only when long (`… 120 lines before` first), redacted when `redact`. None
/// for nothing printed.
pub fn kept(text: &str, redact: bool) -> Option<String> {
    let text = plain(text);
    let mut lines: Vec<&str> = Vec::new();
    for l in text.lines() {
        // one blank line at most between two others
        if l.is_empty() && lines.last().is_none_or(|p| p.is_empty()) {
            continue;
        }
        lines.push(l);
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        return None;
    }
    let mut size = 0;
    let mut from = lines.len();
    while from > 0 && size + lines[from - 1].len() + 1 <= MAX_BYTES {
        from -= 1;
        size += lines[from].len() + 1;
    }
    let mut out = String::new();
    if from > 0 && from < lines.len() {
        out.push_str(&format!("… {from} line{} before\n", if from == 1 { "" } else { "s" }));
    }
    if from == lines.len() {
        // one line longer than the whole budget: its end
        let l = lines[from - 1];
        let cut = l.char_indices().map(|(i, _)| i).find(|i| l.len() - i <= MAX_BYTES).unwrap_or(0);
        out.push_str(&l[cut..]);
    } else {
        out.push_str(&lines[from..].join("\n"));
    }
    Some(if redact { crate::redact::redact(&out) } else { out })
}

/// Output captured from the terminal starts with the command's own line when the shell echoed it
/// after the start mark (the prompt and what was typed, or a terminal's redraw of its end):
/// dropped.
pub fn without_echo(text: &str, cmd: &str) -> String {
    let first = cmd.lines().next().unwrap_or("").trim();
    // blank lines first are no line at all
    let mut text = text;
    while let Some((head, rest)) = text.split_once('\n').filter(|(h, _)| h.trim().is_empty()) {
        let _ = head;
        text = rest;
    }
    let (raw, rest) = text.split_once('\n').unwrap_or((text, ""));
    let head = raw.trim();
    // the whole line (prompt and command); or a redraw of the command's end, which stands where
    // the command did, indented (output starts at the line's start: `echo hi` prints `hi` there)
    let redraw = raw.starts_with(' ') && head.chars().count() >= 4 && first.ends_with(head);
    let echo = !first.is_empty() && !head.is_empty() && (head.ends_with(first) || redraw);
    if echo { rest.to_string() } else { text.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text() {
        assert_eq!(plain("\x1b[31merror\x1b[0m: no\r\n"), "error: no\n");
        assert_eq!(plain("\x1b]0;title\x07ok\x1b]133;D;0\x1b\\"), "ok");
        assert_eq!(plain("10%\r50%\r100%\ndone"), "100%\ndone");
        assert_eq!(plain("ab\x08c\x1b[2K\x1b[1Gx"), "acx");
    }

    #[test]
    fn keeps_the_end() {
        let long: String = (0..3000).map(|i| format!("line {i}\n")).collect();
        let k = kept(&long, false).unwrap();
        assert!(k.len() <= MAX_BYTES + 40, "{}", k.len());
        assert!(k.starts_with("… ") && k.ends_with("line 2999"), "{}", &k[..40]);
        assert_eq!(kept("\n\n  \n", false), None);
        assert_eq!(kept("a\n\n\n\nb\n\n", false).unwrap(), "a\n\nb");
        assert_eq!(kept("export API_TOKEN=abc123", true).unwrap(), "export API_TOKEN=***");
        let one = "x".repeat(MAX_BYTES * 2);
        assert_eq!(kept(&one, false).unwrap().len(), MAX_BYTES);
    }

    #[test]
    fn drops_the_echoed_line() {
        assert_eq!(without_echo("PS C:\\x> npm test\nok\n", "npm test"), "ok\n");
        assert_eq!(without_echo("ok\n", "npm test"), "ok\n");
        // a redraw of only its end, after blank rows
        assert_eq!(without_echo("\n\n       run test --watch\nPASS", "npm run test --watch"), "PASS");
        assert_eq!(without_echo("est\nPASS", "npm test"), "est\nPASS", "too short to tell");
        // what `echo` prints is the end of the command too, but at the line's start: output
        assert_eq!(without_echo("zz-x-0", "echo zz-x-0"), "zz-x-0");
        assert_eq!(without_echo("\nhello world\n", "echo hello world"), "hello world\n");
    }
}
