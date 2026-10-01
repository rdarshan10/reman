//! What a command's output says about how it went, for when its exit status doesn't: an agent
//! piped it (`npx jest 2>&1 | tail -5` exits with tail's 0), chained it (`a && b` failing may be
//! either's), or its tool gives no exit code at all (Codex).
//!
//! `read` runs in the agent hook, on the output the agent was handed: it keeps only one verdict
//! per command, so the output itself never leaves the hook and is never stored. `adjust` runs
//! in the daemon: it turns those verdicts into corrections to what the exit status recorded.
//! A verdict needs a tool's own words (`Tests: 2 failed`, `test result: ok`, `error TS2345`):
//! no words, no verdict. Failure words win over success words.
use crate::unwrap::{self, Outcome};
use regex::Regex;
use std::sync::OnceLock;

/// Per command of `line` (as `unwrap::commands` lists them): Some(true) its output says it
/// worked, Some(false) failed, None it says nothing clear.
pub fn read(line: &str, output: &str) -> Vec<Option<bool>> {
    let Some(cmds) = unwrap::commands(line) else { return Vec::new() };
    // one command: everything printed is its own, whatever runs inside it (`npm test` -> jest)
    let alone = cmds.len() == 1;
    cmds.iter().map(|c| read_one(&c.main, &c.filters, output, alone)).collect()
}

/// A correction for one command of a recorded line: runs, successes and failures to add (or,
/// negative, to take back) to what the line's exit status recorded.
#[derive(Debug, PartialEq)]
pub struct Adjust {
    pub cmd: String,
    pub runs: i32,
    pub ok: i32,
    pub fail: i32,
}

/// The corrections `reads` (from `read`) make to a line that exited with `exit`:
///  - a command whose result a pipe hid, or whose tool gave no exit code, gets its verdict;
///  - in `a && b` that failed, the failure goes to the step that printed it...
///  - ...and a step after it never ran: it isn't counted as a run, or as a failure.
pub fn adjust(line: &str, exit: Option<i64>, reads: &[Option<bool>]) -> Vec<Adjust> {
    let Some(cmds) = unwrap::commands(line) else { return Vec::new() };
    // the line changed on the way (a secret masked into it): the verdicts belong to another
    if reads.len() != cmds.len() {
        return Vec::new();
    }
    let failed = exit.is_some_and(|e| e > 0);
    let unknown = exit.is_none_or(|e| e < 0);
    let mut out = Vec::new();
    let mut skipping = false;
    for (c, r) in cmds.iter().zip(reads) {
        let mut a = Adjust { cmd: c.text.clone(), runs: 0, ok: 0, fail: 0 };
        if skipping {
            // the recorded exit status counted a failure for it only when it was its own
            (a.runs, a.fail) = (-1, if c.outcome == Outcome::Own && failed { -1 } else { 0 });
            skipping = c.chained;
            out.push(a);
            continue;
        }
        let blind = c.outcome == Outcome::Hidden || unknown;
        match r {
            Some(true) if blind => a.ok = 1,
            Some(false) if blind || (c.outcome == Outcome::OnSuccess && failed) => a.fail = 1,
            _ => {}
        }
        // it failed and decided the chain: what `&&` joins after it never ran
        if *r == Some(false) && c.chained && !c.piped && (failed || unknown) {
            skipping = true;
        }
        if (a.runs, a.ok, a.fail) != (0, 0, 0) {
            out.push(a);
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Tool {
    Jest,
    Pytest,
    CargoTest,
    Cargo,
    GoTest,
    Tsc,
    Eslint,
    Ruff,
    Mypy,
    NpmInstall,
    /// anything else: only a lone command's output is read, with every tool's words
    Other,
}

/// Which tool a command runs, through the usual wrappers: `npx jest`, `python -m pytest`,
/// `.venv/Scripts/python.exe -m pytest`, `node_modules/.bin/tsc`, `uv run ruff`.
fn tool(main: &str) -> Tool {
    let w: Vec<String> = main
        .split_whitespace()
        .map(|t| {
            let t = t.trim_matches(['"', '\'']).replace('\\', "/").to_lowercase();
            let base = t.rsplit('/').next().unwrap_or("").to_string();
            base.trim_end_matches(".exe").trim_end_matches(".cmd").to_string()
        })
        .collect();
    let mut i = 0;
    while i < w.len() {
        match w[i].as_str() {
            "&" | "npx" | "pnpx" | "bunx" | "sudo" => i += 1,
            "uv" | "poetry" | "pipenv" | "pdm" if w.get(i + 1).map(String::as_str) == Some("run") => i += 2,
            "python" | "python3" | "py" if w.get(i + 1).map(String::as_str) == Some("-m") => i += 2,
            t if t.contains('=') => i += 1, // `CI=1 npm test`
            _ => break,
        }
    }
    let (p, s) = (w.get(i).map(String::as_str).unwrap_or(""), w.get(i + 1).map(String::as_str).unwrap_or(""));
    match (p, s) {
        ("jest" | "vitest", _) => Tool::Jest,
        ("pytest" | "py.test", _) => Tool::Pytest,
        ("cargo", "test" | "nextest") => Tool::CargoTest,
        ("cargo", "build" | "check" | "clippy" | "run" | "doc" | "b" | "c" | "r") => Tool::Cargo,
        ("go", "test") => Tool::GoTest,
        ("tsc" | "vue-tsc", _) => Tool::Tsc,
        ("eslint", _) => Tool::Eslint,
        ("ruff", _) => Tool::Ruff,
        ("mypy", _) => Tool::Mypy,
        ("npm" | "pnpm" | "yarn" | "bun", "install" | "i" | "ci" | "add") => Tool::NpmInstall,
        _ => Tool::Other,
    }
}

/// (failure words, success words) of a tool.
fn words(t: Tool) -> (&'static [&'static str], &'static [&'static str]) {
    match t {
        // `Tests:  2 failed, 30 passed` (jest), `Tests  30 passed (30)` (vitest), `FAIL src/a.test.ts`
        Tool::Jest => (&[r"(?m)^\s*Tests?:?\s+(?:\d+ \w+, )*\d+ failed", r"(?m)^\s*FAIL\s+\S"], &[r"(?m)^\s*Tests?:?\s+(?:\d+ (?:passed|skipped|todo), )*\d+ passed"]),
        // `2 failed, 3 passed in 0.41s`, `1 error in 0.1s`, `=== 3 passed in 0.12s ===`
        Tool::Pytest => (&[r"(?m)\b\d+ (?:failed|errors?)\b.* in [\d.]+s"], &[r"(?m)\b\d+ passed\b.* in [\d.]+s"]),
        Tool::CargoTest => (&[r"test result: FAILED", r"(?m)^error(?:\[E\d+\])?:", r"could not compile"], &[r"test result: ok\."]),
        Tool::Cargo => (&[r"(?m)^error(?:\[E\d+\])?:", r"could not compile"], &[r"(?m)^\s*Finished\b"]),
        Tool::GoTest => (&[r"(?m)^(?:FAIL|--- FAIL)"], &[r"(?m)^ok\s", r"(?m)^PASS$"]),
        Tool::Tsc => (&[r"error TS\d+"], &[r"Found 0 errors"]),
        Tool::Eslint => (&[r"\(([1-9]\d*) errors?"], &[]),
        Tool::Ruff => (&[r"Found \d+ errors?"], &[r"All checks passed!"]),
        Tool::Mypy => (&[r"Found \d+ errors? in \d+ files?"], &[r"Success: no issues found"]),
        Tool::NpmInstall => (&[r"npm ERR!", r"(?m)^npm error"], &[r"(?m)^(?:added|removed|changed) \d+ packages?", r"(?m)^up to date\b"]),
        Tool::Other => (&[], &[]),
    }
}

/// Words that mean a failure whatever the tool, for a lone command.
const ANY_FAIL: &[&'static str] = &[
    r"npm ERR!",
    r"(?m)^npm error",
    r"Traceback \(most recent call last\)",
    r"(?i)command not found",
    r"is not recognized as (?:an internal or external command|the name of a cmdlet)",
    r"(?m)^fatal: ",
];

/// Does any of the patterns match? Each is compiled once.
fn found(patterns: &[&'static str], text: &str) -> bool {
    use std::collections::HashMap;
    use std::sync::Mutex;
    static COMPILED: OnceLock<Mutex<HashMap<&'static str, Regex>>> = OnceLock::new();
    let mut m = COMPILED.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    patterns.iter().any(|p| m.entry(p).or_insert_with(|| Regex::new(p).expect("a valid pattern")).is_match(text))
}

/// Tools that print nothing when all is well: an empty output (or `| wc -l` saying 0) is a pass.
fn silent(t: Tool) -> bool {
    matches!(t, Tool::Tsc | Tool::Eslint)
}

fn read_one(main: &str, filters: &[String], output: &str, alone: bool) -> Option<bool> {
    let t = tool(main);
    let tools: Vec<Tool> = match t {
        Tool::Other if alone => vec![Tool::Jest, Tool::Pytest, Tool::CargoTest, Tool::Cargo, Tool::GoTest, Tool::Tsc, Tool::Ruff, Tool::Mypy, Tool::NpmInstall],
        Tool::Other => return None,
        t => vec![t],
    };
    let fail = tools.iter().any(|&t| found(words(t).0, output)) || (alone && found(ANY_FAIL, output));
    if fail {
        return Some(false);
    }
    if tools.iter().any(|&t| found(words(t).1, output)) {
        return Some(true);
    }
    // a tool silent on success, alone in the line: its (filtered) output is all there is
    if alone && silent(t) {
        let out = output.trim();
        // only filters that pass everything through, or count it: a `grep` may have hidden errors
        let passes = filters.iter().all(|f| matches!(f.as_str(), "tail" | "head" | "sort" | "uniq" | "cat" | "select-object" | "select" | "out-string" | "out-host" | "out-default" | "tee" | "tee-object" | "wc"));
        if passes {
            if filters.iter().any(|f| f == "wc") {
                return out.parse::<u64>().ok().map(|n| n == 0);
            }
            return out.is_empty().then_some(true);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_reads_what_the_agent_saw() {
        // piped: the exit status was tail's
        assert_eq!(read("cd web && npx jest 2>&1 | tail -5", "Tests:       2 failed, 30 passed, 32 total\nTime: 4s"), [Some(false)]);
        assert_eq!(read("cd web && npx jest 2>&1 | tail -5", "Tests:       32 passed, 32 total"), [Some(true)]);
        assert_eq!(read("npx vitest run | tail -3", " Test Files  4 passed (4)\n      Tests  30 passed (30)"), [Some(true)]);
        assert_eq!(read(".venv/Scripts/python.exe -m pytest -q 2>&1 | tail -3", "..F\n1 failed, 2 passed in 0.41s"), [Some(false)]);
        assert_eq!(read("pytest -q | tail -1", "3 passed in 0.12s"), [Some(true)]);
        assert_eq!(read(r#". .\env.ps1; cargo test 2>&1 | Select-String "test result""#, "test result: ok. 70 passed; 0 failed"), [Some(true)]);
        assert_eq!(read("cargo build --release 2>&1 | tail -3", "error[E0425]: cannot find value `x`\nerror: could not compile `reman`"), [Some(false)]);
        // a lone `npm test` prints jest's words
        assert_eq!(read("npm test 2>&1 | tail -4", "Tests:  1 failed, 9 passed"), [Some(false)]);
        // nothing clear, no verdict
        assert_eq!(read("npm run build | tail -2", "done"), [None]);
        assert_eq!(read("npx jest | grep Tests", ""), [None]);
    }

    #[test]
    fn a_tool_silent_on_success() {
        assert_eq!(read("npx tsc --noEmit 2>&1 | wc -l", "0\n"), [Some(true)]);
        assert_eq!(read("npx tsc --noEmit 2>&1 | wc -l", "12"), [Some(false)]);
        assert_eq!(read("npx tsc --noEmit 2>&1 | head -20", ""), [Some(true)]);
        assert_eq!(read("npx tsc --noEmit 2>&1 | head -20", "src/a.ts(3,1): error TS2304: Cannot find name 'x'."), [Some(false)]);
        // a grep may have hidden every error: nothing printed proves nothing
        assert_eq!(read(r#"npx tsc --noEmit 2>&1 | grep -v node_modules"#, ""), [None]);
    }

    #[test]
    fn several_commands_each_by_its_own_words() {
        let line = r#"npx tsc --noEmit 2>&1 | head -5 && npx jest 2>&1 | grep -E "^Tests:""#;
        assert_eq!(read(line, "Tests:  3 failed, 1 passed"), [None, Some(false)]);
        assert_eq!(read("npm ci && npm run build | tail -1", "added 312 packages in 9s\nbuilt"), [Some(true), None]);
    }

    #[test]
    fn corrections() {
        let a = |line: &str, exit: Option<i64>, reads: &[Option<bool>]| {
            adjust(line, exit, reads).into_iter().map(|a| (a.cmd, a.runs, a.ok, a.fail)).collect::<Vec<_>>()
        };
        // a hidden result gets its verdict
        assert_eq!(a("npx jest | tail -5", Some(0), &[Some(false)]), [("npx jest".into(), 0, 0, 1)]);
        assert_eq!(a("npx jest | tail -5", Some(0), &[Some(true)]), [("npx jest".into(), 0, 1, 0)]);
        // a result the exit status already told: nothing to correct
        assert!(a("npx jest", Some(1), &[Some(false)]).is_empty());
        // `a && b` failed in a: a's failure, and b never ran (its failure taken back)
        assert_eq!(a("cargo build && cargo test", Some(101), &[Some(false), None]), [("cargo build".into(), 0, 0, 1), ("cargo test".into(), -1, 0, -1)]);
        // ...failed in b: nothing to move
        assert!(a("cargo build && cargo test", Some(101), &[Some(true), Some(false)]).is_empty());
        // no exit code at all (Codex): the verdicts are all there is
        assert_eq!(a("pytest -q", None, &[Some(true)]), [("pytest -q".into(), 0, 1, 0)]);
        // the line isn't the one read (a secret masked into it on the way)
        assert!(a("npm test", Some(0), &[Some(true), None]).is_empty());
        // a piped step decides nothing for the chain: `a | tail && b` runs b either way
        assert_eq!(a("npx tsc | tail -3 && npx jest", Some(0), &[Some(false), None]), [("npx tsc".into(), 0, 0, 1)]);
    }
}
