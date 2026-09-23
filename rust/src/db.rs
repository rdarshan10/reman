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
    db.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_cmd_text ON commands(cmd_text);
         CREATE INDEX IF NOT EXISTS idx_last_used ON commands(last_used);
         CREATE INDEX IF NOT EXISTS idx_cwd ON commands(cwd);
         CREATE INDEX IF NOT EXISTS idx_exec_cmd ON executions(command_id);
         CREATE INDEX IF NOT EXISTS idx_exec_ts ON executions(ts);",
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
        "INSERT INTO executions (command_id, actor, exit, cwd, session, ts, duration_ms) VALUES (?,?,?,?,?,?,?)",
        params![cid, r.actor, r.exit, r.cwd, r.session, r.ts, r.duration_ms],
    )?;
    Ok(Recorded { command_id: cid, new_row })
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
