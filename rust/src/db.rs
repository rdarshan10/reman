//! SQLite schema, migrations and the single write path (`record_run`).
//! Schema stays compatible with the Python tools so both can run side by side during cutover.
use crate::config::DIM;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::path::Path;

pub const SCHEMA_VERSION: i64 = 2;

pub fn open(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let db = Connection::open(path)?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    db.pragma_update(None, "journal_mode", "WAL")?;
    db.pragma_update(None, "synchronous", "NORMAL")?;
    // the Python tools never enforced FKs; existing databases may hold rows in any order
    db.pragma_update(None, "foreign_keys", "OFF")?;
    Ok(db)
}

fn columns(db: &Connection, table: &str) -> Result<Vec<String>> {
    let mut st = db.prepare(&format!("PRAGMA table_info({table})"))?;
    let cols = st.query_map([], |r| r.get::<_, String>(1))?.collect::<Result<Vec<_>, _>>()?;
    Ok(cols)
}

fn add_column(db: &Connection, table: &str, col: &str, ddl: &str) -> Result<()> {
    if !columns(db, table)?.iter().any(|c| c == col) {
        db.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {col} {ddl}"))?;
    }
    Ok(())
}

/// Idempotent: creates everything the Python phases created (reman.connect, enrich.migrate,
/// actor.migrate_actor, daemon indexes) plus the Rust additions (duration, pinned, fix_pairs).
pub fn migrate(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS commands (
            id INTEGER PRIMARY KEY, cmd_text TEXT, cmd_hash TEXT UNIQUE, cwd TEXT,
            first_seen INTEGER, last_used INTEGER, run_count INTEGER DEFAULT 1,
            success_count INTEGER DEFAULT 0, fail_count INTEGER DEFAULT 0, last_exit INTEGER);
         CREATE TABLE IF NOT EXISTS command_vec (command_id INTEGER PRIMARY KEY, vec BLOB);
         CREATE TABLE IF NOT EXISTS command_desc_vec
            (command_id INTEGER, kind TEXT, vec BLOB, PRIMARY KEY(command_id, kind));
         CREATE TABLE IF NOT EXISTS executions (
            id INTEGER PRIMARY KEY, command_id INTEGER, actor TEXT,
            exit INTEGER, cwd TEXT, session TEXT, ts INTEGER);
         CREATE TABLE IF NOT EXISTS reman_meta(key TEXT PRIMARY KEY, value TEXT);
         CREATE TABLE IF NOT EXISTS fix_pairs (
            id INTEGER PRIMARY KEY, failed_text TEXT NOT NULL, fixed_cmd TEXT NOT NULL,
            confidence TEXT NOT NULL, cwd TEXT, count INTEGER DEFAULT 1,
            first_seen INTEGER, last_seen INTEGER, UNIQUE(failed_text, fixed_cmd));",
    )?;
    add_column(db, "commands", "description", "TEXT")?;
    add_column(db, "commands", "desc_source", "TEXT DEFAULT 'none'")?;
    add_column(db, "commands", "human_runs", "INTEGER DEFAULT 0")?;
    add_column(db, "commands", "agent_runs", "INTEGER DEFAULT 0")?;
    add_column(db, "commands", "last_actor", "TEXT")?;
    add_column(db, "commands", "pinned", "INTEGER DEFAULT 0")?;
    add_column(db, "executions", "duration_ms", "INTEGER")?;
    add_column(db, "executions", "err", "TEXT")?;
    add_column(db, "executions", "seen", "TEXT")?;
    // a run two records reported (the shell an agent typed into, and that agent's hook): merged
    add_column(db, "executions", "paired", "INTEGER")?;
    // the git branch and commit checked out in its folder when it ran (git.rs)
    add_column(db, "executions", "branch", "TEXT")?;
    add_column(db, "executions", "head", "TEXT")?;
    // what it printed (the end of it, redacted), and `reman shell`'s id for it (output_op)
    add_column(db, "executions", "output", "TEXT")?;
    add_column(db, "executions", "capture", "TEXT")?;
    db.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_cmd_text ON commands(cmd_text);
         CREATE INDEX IF NOT EXISTS idx_last_used ON commands(last_used);
         CREATE INDEX IF NOT EXISTS idx_cwd ON commands(cwd);
         CREATE INDEX IF NOT EXISTS idx_exec_cmd ON executions(command_id);
         CREATE INDEX IF NOT EXISTS idx_exec_ts ON executions(ts);
         CREATE INDEX IF NOT EXISTS idx_exec_capture ON executions(capture) WHERE capture IS NOT NULL;",
    )?;
    db.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

pub fn meta_get(db: &Connection, key: &str) -> Result<Option<String>> {
    Ok(db.query_row("SELECT value FROM reman_meta WHERE key=?", [key], |r| r.get(0)).optional()?)
}

pub fn meta_set(db: &Connection, key: &str, value: &str) -> Result<()> {
    db.execute("INSERT OR REPLACE INTO reman_meta(key,value) VALUES(?,?)", params![key, value])?;
    Ok(())
}

pub fn pack(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

pub fn unpack(b: &[u8]) -> Option<Vec<f32>> {
    if b.len() != DIM * 4 {
        return None;
    }
    Some(b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect())
}

/// Same identity the Python code used: sha256(cmd \0 cwd) -> one `commands` row per (command, folder).
pub fn cmd_hash(cmd: &str, cwd: Option<&str>) -> String {
    let d = Sha256::digest(format!("{cmd}\0{}", cwd.unwrap_or("")).as_bytes());
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// One run to record. exit: Some(0)=ok, Some(>0)=fail, None or negative = unknown (never a failure).
#[derive(Debug, Clone, Default)]
pub struct Run {
    pub cmd: String,
    pub exit: Option<i64>,
    pub cwd: Option<String>,
    pub session: String,
    pub actor: String,
    pub ts: i64,
    pub duration_ms: Option<i64>,
    /// what it printed when it failed (redacted, trimmed): agents' output, PowerShell's error
    pub err: Option<String>,
    /// per command of the line, what its output said (verdict::read): from an agent's hook
    pub reads: Option<Vec<Option<bool>>>,
    /// what it printed, to read those verdicts from; never stored
    pub output: Option<String>,
    /// the corrections the verdicts make (verdict::adjust), as stored: [[command, runs, ok, fail], ...]
    pub seen: Option<String>,
    /// the git branch and commit (12 characters) its folder had checked out when it ran
    pub branch: Option<String>,
    pub head: Option<String>,
    /// what it printed, as kept (the end of it, redacted): an agent's, or one `reman shell` saw
    pub printed: Option<String>,
    /// `reman shell`'s id for the run, which its output arrives under (daemon: output_op)
    pub capture: Option<String>,
}

impl Run {
    pub fn is_agent(&self) -> bool {
        self.actor.starts_with("agent:")
    }
    pub fn ok(&self) -> bool {
        self.exit == Some(0)
    }
    pub fn failed(&self) -> bool {
        matches!(self.exit, Some(e) if e > 0)
    }
}

pub struct Recorded {
    pub command_id: i64,
    pub new_row: bool,
}

/// Upsert the (command, cwd) row and append an execution. Port of reman_actor.record_run.
/// Vectors for brand-new rows are written by the caller (it owns the embedder).
pub fn record_run(db: &Connection, r: &Run) -> Result<Recorded> {
    let h = cmd_hash(&r.cmd, r.cwd.as_deref());
    let (ok, bad) = (r.ok() as i64, r.failed() as i64);
    let (hinc, ainc) = if r.is_agent() { (0, 1) } else { (1, 0) };
    let existing: Option<i64> =
        db.query_row("SELECT id FROM commands WHERE cmd_hash=?", [&h], |row| row.get(0)).optional()?;
    let (cid, new_row) = match existing {
        Some(cid) => {
            db.execute(
                "UPDATE commands SET run_count=COALESCE(run_count,0)+1, success_count=COALESCE(success_count,0)+?,
                   fail_count=COALESCE(fail_count,0)+?, last_exit=?, last_used=MAX(COALESCE(last_used,0),?),
                   human_runs=COALESCE(human_runs,0)+?, agent_runs=COALESCE(agent_runs,0)+?, last_actor=?
                 WHERE id=?",
                params![ok, bad, r.exit, r.ts, hinc, ainc, r.actor, cid],
            )?;
            (cid, false)
        }
        None => {
            db.execute(
                "INSERT INTO commands (cmd_text, cmd_hash, cwd, first_seen, last_used, run_count, success_count,
                   fail_count, last_exit, human_runs, agent_runs, last_actor, desc_source)
                 VALUES (?,?,?,?,?,1,?,?,?,?,?,?,'none')",
                params![r.cmd, h, r.cwd, r.ts, r.ts, ok, bad, r.exit, hinc, ainc, r.actor],
            )?;
            (db.last_insert_rowid(), true)
        }
    };
    db.execute(
        "INSERT INTO executions (command_id, actor, exit, cwd, session, ts, duration_ms, err, seen, branch, head, output, capture) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)",
        params![cid, r.actor, r.exit, r.cwd, r.session, r.ts, r.duration_ms, r.err, r.seen, r.branch, r.head, r.printed, r.capture],
    )?;
    Ok(Recorded { command_id: cid, new_row })
}

/// Which runs a retry streak is counted over: one agent session, or any agent's runs in a folder
/// since a time (when the session isn't known).
pub enum StreakOf<'a> {
    Session(&'a str),
    AgentsIn { cwd: &'a str, since: i64 },
}

/// The failures that end with the latest run of `cmd`, each failing the same way as the latest
/// (the same error signature, else the same exit code): how many, and what the latest printed.
/// 0 when the latest run didn't fail.
pub fn same_failures(db: &Connection, cmd: &str, of: StreakOf) -> Result<(u32, Option<String>)> {
    let sql = "SELECT e.exit, e.err, e.cwd FROM executions e JOIN commands c ON c.id = e.command_id WHERE c.cmd_text = ?1 AND ";
    let map = |r: &rusqlite::Row| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?));
    let rows: Vec<(Option<i64>, Option<String>, Option<String>)> = match of {
        StreakOf::Session(s) => {
            let mut st = db.prepare(&format!("{sql} e.session = ?2 ORDER BY e.ts DESC, e.id DESC LIMIT 20"))?;
            st.query_map(params![cmd, s], map)?.collect::<rusqlite::Result<_>>()?
        }
        StreakOf::AgentsIn { cwd, since } => {
            let want = crate::config::norm_path(cwd);
            let mut st = db.prepare(&format!("{sql} e.actor LIKE 'agent:%' AND e.ts >= ?2 ORDER BY e.ts DESC, e.id DESC LIMIT 40"))?;
            let all: Vec<_> = st.query_map(params![cmd, since], map)?.collect::<rusqlite::Result<_>>()?;
            all.into_iter().filter(|r| r.2.as_deref().is_some_and(|c| crate::config::norm_path(c) == want)).take(20).collect()
        }
    };
    let way = |exit: Option<i64>, err: &Option<String>| err.as_deref().and_then(crate::errors::signature).unwrap_or_else(|| format!("exit {}", exit.unwrap_or(0)));
    let Some((exit, err, _)) = rows.first().filter(|r| r.0.is_some_and(|e| e > 0)) else { return Ok((0, None)) };
    let first = way(*exit, err);
    let n = rows.iter().take_while(|r| r.0.is_some_and(|e| e > 0) && way(r.0, &r.1) == first).count() as u32;
    Ok((n, err.clone()))
}

/// Agents that run commands by typing them into a real terminal, whose shell (with reman's
/// prompt hook) records them too: VS Code's Copilot, Cursor, Windsurf.
pub const TERMINAL_AGENTS: [&str; 3] = ["agent:copilot", "agent:cursor", "agent:windsurf"];

/// The other record of one run: the shell's, when an agent that types into that shell reports
/// it; or that agent's, when the shell reports second.
pub struct Twin {
    pub exec_id: i64,
    pub command_id: i64,
    pub exit: Option<i64>,
    pub seen: Option<String>,
    pub ts: i64,
}

/// A record of the run `cmd` in `cwd` by one of `actors`, within `window` seconds of `ts`, not
/// yet paired. The same run, not the same text: the agent's terminal tool may have rewritten the
/// line it typed (unwrap::same_commands).
pub fn find_twin(db: &Connection, cmd: &str, cwd: Option<&str>, ts: i64, window: i64, actors: &[&str]) -> Result<Option<Twin>> {
    let want = cwd.map(crate::config::norm_path);
    let mut st = db.prepare(
        "SELECT e.id, e.command_id, e.exit, e.seen, e.cwd, e.actor, e.ts, c.cmd_text FROM executions e JOIN commands c ON c.id = e.command_id
         WHERE e.ts BETWEEN ?1 AND ?2 AND e.paired IS NULL ORDER BY ABS(e.ts - ?3) LIMIT 64",
    )?;
    let mut rows = st.query(params![ts - window, ts + window, ts])?;
    while let Some(r) = rows.next()? {
        let (c, actor, text): (Option<String>, Option<String>, String) = (r.get(4)?, r.get(5)?, r.get(7)?);
        if actors.contains(&actor.as_deref().unwrap_or("")) && c.as_deref().map(crate::config::norm_path) == want && crate::unwrap::same_commands(&text, cmd) {
            return Ok(Some(Twin { exec_id: r.get(0)?, command_id: r.get(1)?, exit: r.get(2)?, seen: r.get(3)?, ts: r.get(6)? }));
        }
    }
    Ok(None)
}

/// The shell's record of a run becomes the agent's (it keeps the shell's exact exit code).
pub fn claim_for_agent(db: &Connection, t: &Twin, actor: &str, seen: Option<&str>) -> Result<()> {
    db.execute("UPDATE executions SET actor = ?1, paired = 1, seen = ?2 WHERE id = ?3", params![actor, seen, t.exec_id])?;
    db.execute(
        "UPDATE commands SET human_runs = MAX(COALESCE(human_runs, 0) - 1, 0), agent_runs = COALESCE(agent_runs, 0) + 1, last_actor = ?1 WHERE id = ?2",
        params![actor, t.command_id],
    )?;
    Ok(())
}

/// A run's record, which had no exit code, learns one (from its twin); None: nothing to learn.
pub fn learn_exit(db: &Connection, t: &Twin, exit: Option<i64>) -> Result<()> {
    if let (None, Some(x)) = (t.exit, exit.filter(|x| *x >= 0)) {
        db.execute("UPDATE executions SET exit = ?1 WHERE id = ?2", params![x, t.exec_id])?;
        db.execute(
            "UPDATE commands SET success_count = COALESCE(success_count, 0) + ?1, fail_count = COALESCE(fail_count, 0) + ?2, last_exit = ?3 WHERE id = ?4",
            params![(x == 0) as i64, (x > 0) as i64, x, t.command_id],
        )?;
    }
    Ok(())
}

/// The agent's record of a run, reported first, learns the shell's exit code (when it had none);
/// what its output said is then superseded.
pub fn settle_twin(db: &Connection, t: &Twin, exit: Option<i64>) -> Result<()> {
    match (t.exit, exit) {
        (None, Some(x)) if x >= 0 => {
            db.execute("UPDATE executions SET exit = ?1, paired = 1, seen = NULL WHERE id = ?2", params![x, t.exec_id])?;
            db.execute(
                "UPDATE commands SET success_count = COALESCE(success_count, 0) + ?1, fail_count = COALESCE(fail_count, 0) + ?2, last_exit = ?3 WHERE id = ?4",
                params![(x == 0) as i64, (x > 0) as i64, x, t.command_id],
            )?;
        }
        _ => {
            db.execute("UPDATE executions SET paired = 1 WHERE id = ?1", params![t.exec_id])?;
        }
    }
    Ok(())
}

pub fn write_vectors(db: &Connection, cid: i64, raw: &[f32], desc: Option<&str>, descs: &[(&str, Vec<f32>)]) -> Result<()> {
    db.execute("INSERT OR REPLACE INTO command_vec (command_id, vec) VALUES (?,?)", params![cid, pack(raw)])?;
    if let Some(d) = desc {
        db.execute("UPDATE commands SET description=?, desc_source='tldr' WHERE id=?", params![d, cid])?;
    }
    for (kind, v) in descs {
        db.execute(
            "INSERT OR REPLACE INTO command_desc_vec (command_id, kind, vec) VALUES (?,?,?)",
            params![cid, kind, pack(v)],
        )?;
    }
    Ok(())
}

/// Replace a command's description + description vectors, leaving its raw vector alone.
pub fn write_descriptions(db: &Connection, cid: i64, desc: Option<&str>, descs: &[(&str, Vec<f32>)]) -> Result<()> {
    db.execute("DELETE FROM command_desc_vec WHERE command_id=?", [cid])?;
    db.execute("UPDATE commands SET description=?, desc_source='tldr' WHERE id=?", params![desc, cid])?;
    for (kind, v) in descs {
        db.execute("INSERT OR REPLACE INTO command_desc_vec (command_id, kind, vec) VALUES (?,?,?)", params![cid, kind, pack(v)])?;
    }
    Ok(())
}

pub fn delete_command_rows(db: &Connection, ids: &[i64]) -> Result<()> {
    // children first, parent last (the schema declares command_vec -> commands)
    for id in ids {
        db.execute("DELETE FROM executions WHERE command_id=?", [id])?;
        db.execute("DELETE FROM command_desc_vec WHERE command_id=?", [id])?;
        db.execute("DELETE FROM command_vec WHERE command_id=?", [id])?;
        db.execute("DELETE FROM commands WHERE id=?", [id])?;
    }
    Ok(())
}

/// Backup once, before the first Rust migration touches a pre-existing Python db.
pub fn backup_if_legacy(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let db = Connection::open(path)?;
    let v: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
    drop(db);
    if v < SCHEMA_VERSION {
        let bak = path.with_extension(format!("db.pre-rust-v{v}.bak"));
        if !bak.exists() {
            std::fs::copy(path, &bak)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_rows_with_fk_on() {
        let db = Connection::open_in_memory().unwrap();
        db.pragma_update(None, "foreign_keys", "ON").unwrap();
        migrate(&db).unwrap();
        let r = Run { cmd: "ls -la".into(), exit: Some(0), actor: "human".into(), ts: 1, ..Default::default() };
        let rec = record_run(&db, &r).unwrap();
        write_vectors(&db, rec.command_id, &vec![0.1; DIM], None, &[]).unwrap();
        delete_command_rows(&db, &[rec.command_id]).unwrap();
        let n: i64 = db.query_row("SELECT COUNT(*) FROM commands", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn hash_matches_python() {
        // python: hashlib.sha256("git status\x00C:\\x".encode()).hexdigest()
        let h = cmd_hash("git status", Some("C:\\x"));
        assert_eq!(h.len(), 64);
        assert_eq!(cmd_hash("a", None), cmd_hash("a", Some("")));
    }

    #[test]
    fn migrate_and_record() {
        let db = Connection::open_in_memory().unwrap();
        migrate(&db).unwrap();
        migrate(&db).unwrap(); // idempotent
        let mut r = Run { cmd: "git status".into(), exit: Some(0), cwd: Some("C:/a".into()), actor: "human".into(), ts: 10, ..Default::default() };
        assert!(record_run(&db, &r).unwrap().new_row);
        r.exit = Some(1);
        r.actor = "agent:claude-code".into();
        r.ts = 20;
        assert!(!record_run(&db, &r).unwrap().new_row);
        let row: (i64, i64, i64, i64, i64, String) = db
            .query_row("SELECT run_count, success_count, fail_count, human_runs, agent_runs, last_actor FROM commands", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
            })
            .unwrap();
        assert_eq!(row, (2, 1, 1, 1, 1, "agent:claude-code".into()));
        r.exit = Some(-1); // unknown: neither ok nor fail
        record_run(&db, &r).unwrap();
        let (sc, fc): (i64, i64) = db.query_row("SELECT success_count, fail_count FROM commands", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!((sc, fc), (1, 1));
    }
}
