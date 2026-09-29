//! One-time (or catch-up) history imports. After cutover reman captures everything itself;
//! these exist to bring the past along: Atuin (full provenance: cwd, exit, duration, session),
//! and plain shell history files (PSReadLine, bash, zsh, fish - text + maybe timestamps).
use crate::daemon::{Daemon, is_junk};
use crate::db::{self, Run};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};

pub struct Imported {
    pub ingested: usize,
    pub new: usize,
    pub remaining: usize,
}

pub fn run(d: &Daemon, source: &str, path: Option<&str>, limit: i64) -> Result<Imported> {
    match source {
        "atuin" => atuin(d, path.map(PathBuf::from), limit),
        "psreadline" | "bash" | "zsh" | "fish" => {
            let p = path.map(PathBuf::from).or_else(|| default_path(source)).context("history file not found")?;
            let runs = parse_file(source, &p)?;
            file_runs(d, runs)
        }
        other => bail!("unknown import source {other:?} (atuin|psreadline|bash|zsh|fish)"),
    }
}

pub fn default_path(source: &str) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let p = match source {
        "psreadline" => {
            let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| home.join(".local/share"));
            if cfg!(windows) {
                base.join(r"Microsoft\Windows\PowerShell\PSReadLine\ConsoleHost_history.txt")
            } else {
                home.join(".local/share/powershell/PSReadLine/ConsoleHost_history.txt")
            }
        }
        "bash" => home.join(".bash_history"),
        "zsh" => std::env::var_os("HISTFILE").map(PathBuf::from).unwrap_or_else(|| home.join(".zsh_history")),
        "fish" => home.join(".local/share/fish/fish_history"),
        _ => return None,
    };
    p.exists().then_some(p)
}

fn atuin(d: &Daemon, path: Option<PathBuf>, limit: i64) -> Result<Imported> {
    let apath = path.or_else(crate::config::atuin_db_path).context("atuin history.db not found")?;
    let adb = Connection::open_with_flags(&apath, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    let cursor: i64 = {
        let conn = d.db.lock();
        db::meta_get(&conn, "atuin_cursor")?.and_then(|v| v.parse().ok()).unwrap_or(0)
    };
    let lim = if limit <= 0 { i64::MAX } else { limit };
    let mut st = adb.prepare(
        "SELECT timestamp, exit, cwd, command, session, duration FROM history
         WHERE timestamp > ? AND deleted_at IS NULL AND command NOT LIKE '#%'
         ORDER BY timestamp ASC LIMIT ?",
    )?;
    let rows: Vec<(i64, Option<i64>, Option<String>, Option<String>, Option<String>, Option<i64>)> =
        st.query_map([cursor, lim], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?.collect::<Result<_, _>>()?;
    let mut maxts = cursor;
    let mut runs = Vec::new();
    for (ts, exit, cwd, cmd, sess, dur) in &rows {
        maxts = maxts.max(*ts);
        let cmd = cmd.as_deref().unwrap_or("").trim();
        if is_junk(cmd) {
            continue;
        }
        runs.push(Run {
            cmd: cmd.to_string(),
            exit: exit.filter(|e| *e >= 0),
            cwd: cwd.clone().filter(|c| c != "unknown" && !c.is_empty()),
            session: sess.clone().unwrap_or_default(),
            actor: "human".into(),
            ts: ts / 1_000_000_000,
            duration_ms: dur.filter(|d| *d > 0).map(|d| d / 1_000_000),
            err: None,
        });
    }
    let mut new = 0;
    for chunk in runs.chunks(500) {
        new += d.ingest_runs(chunk)?.0;
    }
    if !rows.is_empty() {
        db::meta_set(&d.db.lock(), "atuin_cursor", &maxts.to_string())?;
    }
    let remaining: i64 = adb.query_row(
        "SELECT COUNT(*) FROM history WHERE timestamp > ? AND deleted_at IS NULL AND command NOT LIKE '#%'",
        [maxts],
        |r| r.get(0),
    )?;
    Ok(Imported { ingested: runs.len(), new, remaining: remaining as usize })
}

/// History files carry no exit/cwd. Only texts reman has never seen are added (so re-imports
/// don't inflate run counts), with timestamps spread before the file's mtime to keep order.
fn file_runs(d: &Daemon, parsed: Vec<(Option<i64>, String)>) -> Result<Imported> {
    let fresh: Vec<(Option<i64>, String)> = {
        let st = d.store.read();
        let mut seen = std::collections::HashSet::new();
        parsed.into_iter().filter(|(_, c)| !is_junk(c) && !st.knows(c) && seen.insert(c.clone())).collect()
    };
    let now = crate::config::now();
    let n = fresh.len() as i64;
    let runs: Vec<Run> = fresh
        .into_iter()
        .enumerate()
        .map(|(i, (ts, cmd))| Run {
            cmd,
            exit: None,
            cwd: None,
            session: "import".into(),
            actor: "human".into(),
            ts: ts.unwrap_or(now - 86400 - (n - i as i64)),
            duration_ms: None,
            err: None,
        })
        .collect();
    let mut new = 0;
    for chunk in runs.chunks(500) {
        new += d.ingest_runs(chunk)?.0;
    }
    Ok(Imported { ingested: runs.len(), new, remaining: 0 })
}

/// (timestamp?, command) in file order.
pub fn parse_file(kind: &str, p: &Path) -> Result<Vec<(Option<i64>, String)>> {
    let bytes = std::fs::read(p)?;
    let text = String::from_utf8_lossy(&bytes);
    Ok(parse_text(kind, &text))
}

pub fn parse_text(kind: &str, text: &str) -> Vec<(Option<i64>, String)> {
    let mut out: Vec<(Option<i64>, String)> = Vec::new();
    match kind {
        "psreadline" => {
            // a trailing backtick continues the command on the next line
            let mut cur = String::new();
            for ln in text.lines() {
                if let Some(stripped) = ln.strip_suffix('`') {
                    cur.push_str(stripped);
                    cur.push('\n');
                } else {
                    cur.push_str(ln);
                    out.push((None, std::mem::take(&mut cur)));
                }
            }
        }
        "bash" => {
            let mut ts = None;
            for ln in text.lines() {
                if let Some(t) = ln.strip_prefix('#').and_then(|t| t.trim().parse::<i64>().ok()) {
                    ts = Some(t);
                    continue;
                }
                out.push((ts.take(), ln.to_string()));
            }
        }
        "zsh" => {
            // extended format `: 1700000000:0;cmd`, multi-line commands end lines with `\`
            let mut cur: Option<(Option<i64>, String)> = None;
            for ln in text.lines() {
                if let Some((ts, rest)) = ln.strip_prefix(": ").and_then(|r| r.split_once(';')).filter(|_| cur.is_none()) {
                    let t = ts.split(':').next().and_then(|t| t.trim().parse().ok());
                    cur = Some((t, rest.to_string()));
                } else if cur.is_none() {
                    cur = Some((None, ln.to_string()));
                } else if let Some(c) = cur.as_mut() {
                    c.1.push('\n');
                    c.1.push_str(ln);
                }
                if let Some(c) = cur.as_mut() {
                    if c.1.ends_with('\\') {
                        c.1.pop();
                        continue;
                    }
                }
                if let Some(c) = cur.take() {
                    out.push(c);
                }
            }
        }
        "fish" => {
            // - cmd: git status\n  when: 1700000000
            for ln in text.lines() {
                if let Some(c) = ln.strip_prefix("- cmd: ") {
                    out.push((None, c.replace("\\n", "\n").replace("\\\\", "\\")));
                } else if let Some(w) = ln.trim().strip_prefix("when: ") {
                    if let Some(last) = out.last_mut() {
                        last.0 = w.trim().parse().ok();
                    }
                }
            }
        }
        _ => {}
    }
    out.retain(|(_, c)| !c.trim().is_empty());
    for x in out.iter_mut() {
        x.1 = x.1.trim().to_string();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_formats() {
        assert_eq!(parse_text("zsh", ": 1700000000:0;git status\n: 1700000001:0;echo a\\\nb\n"), vec![
            (Some(1700000000), "git status".to_string()),
            (Some(1700000001), "echo a\nb".to_string())
        ]);
        assert_eq!(parse_text("bash", "#1700000000\nls -la\npwd\n"), vec![(Some(1700000000), "ls -la".into()), (None, "pwd".into())]);
        assert_eq!(parse_text("psreadline", "Get-Foo `\n  -Bar\ngit status\n"), vec![(None, "Get-Foo \n  -Bar".into()), (None, "git status".into())]);
        assert_eq!(parse_text("fish", "- cmd: git push\n  when: 1700000005\n"), vec![(Some(1700000005), "git push".into())]);
    }
}
