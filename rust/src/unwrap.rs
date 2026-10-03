//! What a recorded line really ran. Agents (and people) wrap the command that matters:
//! `cd web && npx jest --silent 2>&1 | grep -E "Tests:"`, or
//! `Set-Location C:\x\rust; . .\msvc-env.ps1; cargo build --release 2>&1 | Select-String error`.
//! `commands` takes such a line apart into the commands it ran, each with the `cd`s before it and
//! the environment it ran in (`. .\msvc-env.ps1`, a venv's activate), without what only shaped
//! the output (`2>&1`, `> $null`, `| grep ...`, `|| echo CLEAN`). Nothing is invented: every
//! piece is text from the line, in its order.

pub struct Cmd {
    /// the folder changes before it, in order, as typed (`api`, `C:\x\rust`, `/c/x`)
    pub cds: Vec<String>,
    /// what it ran, after the environment it was run in: `. .\msvc-env.ps1; cargo build --release`
    pub text: String,
    /// just the command: `cargo build --release`
    pub main: String,
    /// what the line's exit status says about it
    pub outcome: Outcome,
    /// the output filters it went through (`tail`, `wc`, `select-string`), lowercased
    pub filters: Vec<String>,
    /// one of them decided the exit status (anything but a PowerShell cmdlet)
    pub piped: bool,
    /// `&&` follows it: when it fails, the rest of the chain doesn't run
    pub chained: bool,
}

/// What a line's exit status says about one command in it. A pipeline exits with its last
/// program's status (`npx tsc | wc -l` "works" when tsc fails), and so does `a; b` or
/// `a || echo x`. PowerShell keeps a native command's exit code through its own cmdlets
/// (`cargo build 2>&1 | Select-String error`), so those still count.
#[derive(Clone, Copy, PartialEq, PartialOrd, Debug)]
pub enum Outcome {
    /// nothing: something else decided the exit status
    Hidden,
    /// its success: `a && b` exiting 0 means `a` worked (a failure may be `b`'s)
    OnSuccess,
    /// the line's exit status is its own
    Own,
}

/// The commands a line ran, in order, each once. None when the line can't be read this way: a
/// `cd` with stray words is two lines pasted into one (`cd .\web\npm run dev`).
pub fn commands(line: &str) -> Option<Vec<Cmd>> {
    let line = line.trim();
    if line.contains('\n') {
        // a heredoc, a pasted block: kept whole
        return Some(vec![Cmd::whole(Vec::new(), line)]);
    }
    let mut steps = Vec::new();
    flatten(line, "", &mut steps);
    // the environment so far: each step, and the variable it sets when it only matters to a
    // command that uses it (`$py = "..."`, not `$env:CI = 1`)
    let (mut cds, mut env, mut out): (Vec<String>, Vec<(String, Option<String>)>, Vec<Cmd>) = (Vec::new(), Vec::new(), Vec::new());
    for (step, then) in steps {
        let (s, filters) = without_filters(step);
        let piped = filters.iter().any(|f| !CMDLETS.contains(&f.as_str()));
        let s = without_redirects(&s);
        if s.is_empty() {
            continue;
        }
        let outcome = match then {
            _ if piped => Outcome::Hidden,
            "" => Outcome::Own,
            "&&" => Outcome::OnSuccess,
            _ => Outcome::Hidden,
        };
        let w = words(&s);
        let first = w[0].to_lowercase();
        let first = first.strip_suffix(".exe").unwrap_or(&first);
        match first {
            "cd" | "chdir" | "set-location" | "sl" | "pushd" | "push-location" => {
                let args: Vec<&str> = w[1..].iter().copied().filter(|a| !is_cd_flag(a)).collect();
                match args[..] {
                    [dir] => cds.push(unquote(dir).to_string()),
                    _ => return None,
                }
            }
            "popd" | "pop-location" => {
                cds.pop();
            }
            // session plumbing and talking, not work
            "set-executionpolicy" | "echo" | "write-host" | "write-output" | "printf" | "sleep" | "start-sleep" | "cls" | "clear" => {}
            _ if is_drive_hop(first) || s.starts_with(['"', '\'']) => {}
            _ => match env_step(&s, &w) {
                Some(var) => env.push((s, var)),
                None => {
                    let needed = env_for(&env, &s);
                    let text = if needed.is_empty() { s.clone() } else { format!("{}; {s}", needed.join("; ")) };
                    let cmd = Cmd { cds: cds.clone(), text, main: s, outcome, filters, piped, chained: then == "&&" };
                    match out.iter_mut().find(|c| c.text == cmd.text) {
                        // the same command twice: the one whose exit status says more
                        Some(c) if cmd.outcome > c.outcome => *c = cmd,
                        Some(_) => {}
                        None => out.push(cmd),
                    }
                }
            },
        }
    }
    // only an environment set up (the venv VS Code activates in every new terminal): that is
    // the command, when it's an activate
    if out.is_empty() {
        if let Some((a, _)) = env.iter().rev().find(|e| is_activate(&e.0)) {
            out.push(Cmd::whole(cds, a));
        }
    }
    Some(out)
}

impl Cmd {
    /// A command that is all of what ran, with nothing around it.
    fn whole(cds: Vec<String>, text: &str) -> Cmd {
        Cmd { cds, text: text.to_string(), main: text.to_string(), outcome: Outcome::Own, filters: Vec::new(), piped: false, chained: false }
    }
}

/// Do two lines run the same commands? An agent's terminal tool may rewrite a line before
/// typing it (VS Code drops a `cd <here> &&` prefix, and turns `&&` into `;` for PowerShell), so
/// the shell's record and the agent's differ in text, not in what ran.
pub fn same_commands(a: &str, b: &str) -> bool {
    let key = |l: &str| commands(l).map(|cs| cs.into_iter().map(|c| c.text).collect::<Vec<_>>()).filter(|k| !k.is_empty());
    a.trim() == b.trim() || key(a).is_some_and(|k| key(b) == Some(k))
}

/// The folder a command ran in: `base` (where the line was typed) after its `cd`s. None when it
/// can't be known from the line (`cd ~`, `cd $env:TEMP`, `cd -`), or it names another machine's
/// folder (`/opt/app` typed on Windows: a line meant for a server, pasted here).
pub fn resolve_dir(base: &str, cds: &[String]) -> Option<String> {
    let win = is_win_path(base);
    let mut parts = split_path(&base.replace('\\', "/"))?;
    for t in cds {
        let t = t.replace('\\', "/");
        if t.is_empty() || t == "-" || t.starts_with('~') || t.contains(['$', '%']) {
            return None;
        }
        let t = if win { msys_to_drive(&t).unwrap_or(t) } else { t };
        if is_abs(&t) {
            if win && t.starts_with('/') {
                return None;
            }
            parts = split_path(&t)?;
            continue;
        }
        for c in t.split('/') {
            match c {
                "" | "." => {}
                ".." => {
                    if parts.len() > 1 {
                        parts.pop();
                    }
                }
                c => parts.push(c.to_string()),
            }
        }
    }
    let sep = if win { "\\" } else { "/" };
    Some(if parts.len() == 1 { format!("{}{sep}", parts[0]) } else { parts.join(sep) })
}

// ---------------------------------------------------------------------------------------------

/// The steps of a line: split on `&&`, `||` and `;`, and into `( ... )` groups; each with the
/// operator after it (`""` at the end). The last step of a group is followed by what follows the
/// group (`then`).
fn flatten<'a>(s: &'a str, then: &'a str, out: &mut Vec<(&'a str, &'a str)>) {
    for (step, op) in split_top(s, false) {
        let op = if op.is_empty() { then } else { op };
        let t = step.trim();
        match t.strip_prefix('(').and_then(|r| r.strip_suffix(')')) {
            Some(inner) if closes_at_end(t) => flatten(inner, op, out),
            _ if !t.is_empty() => out.push((t, op)),
            _ => {}
        }
    }
}

/// Split outside quotes, parentheses and braces: on `&&` / `||` / `;`, or (`pipes`) on `|`.
/// Each piece comes with the operator after it (`""` for the last).
fn split_top(s: &str, pipes: bool) -> Vec<(&str, &str)> {
    let b = s.as_bytes();
    let (mut out, mut start, mut depth, mut quote, mut i) = (Vec::new(), 0, 0i32, 0u8, 0);
    while i < b.len() {
        let c = b[i];
        if quote != 0 {
            if c == quote {
                quote = 0;
            }
        } else {
            match c {
                b'"' | b'\'' => quote = c,
                b'(' | b'{' => depth += 1,
                b')' | b'}' => depth -= 1,
                _ if depth == 0 => {
                    let pair = b.get(i + 1) == Some(&c);
                    let cut = if pipes { c == b'|' && !pair && (i == 0 || b[i - 1] != b'|') } else { c == b';' || (pair && (c == b'&' || c == b'|')) };
                    if cut {
                        let w = if !pipes && c != b';' { 2 } else { 1 };
                        out.push((&s[start..i], &s[i..i + w]));
                        start = i + w;
                        i += w;
                        continue;
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    out.push((&s[start..], ""));
    out
}

/// Does the `(` opening `t` close at its very end (`(a) ; (b)` does not)?
fn closes_at_end(t: &str) -> bool {
    let (mut depth, mut quote) = (0i32, 0u8);
    for (i, &c) in t.as_bytes().iter().enumerate() {
        if quote != 0 {
            if c == quote {
                quote = 0;
            }
            continue;
        }
        match c {
            b'"' | b'\'' => quote = c,
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return i == t.len() - 1;
                }
            }
            _ => {}
        }
    }
    false
}

/// Programs that only shape what a command printed.
const FILTERS: &[&str] = &[
    "grep", "egrep", "fgrep", "rg", "findstr", "select-string", "sls", "select-object", "select", "head", "tail", "wc", "sort",
    "sort-object", "uniq", "less", "more", "out-string", "out-null", "out-host", "out-default", "format-table", "ft", "format-list",
    "fl", "format-wide", "tee", "tee-object", "measure-object", "measure", "where-object", "where", "?", "jq", "cut", "awk", "sed",
    "tr", "column", "cat", "nl", "convertto-json", "convertfrom-json", "foreach-object", "%",
];

/// PowerShell's own filters: they leave a native command's exit code as it was.
const CMDLETS: &[&str] = &[
    "select-string", "sls", "select-object", "select", "out-string", "out-null", "out-host", "out-default", "format-table", "ft",
    "format-list", "fl", "format-wide", "tee-object", "measure-object", "measure", "where-object", "where", "?", "convertto-json",
    "convertfrom-json", "foreach-object", "%", "sort-object",
];

/// The step without the output filters at its end: `npx jest 2>&1 | grep -E "Tests:"` -> `npx jest 2>&1`;
/// and those filters, first to last.
fn without_filters(step: &str) -> (String, Vec<String>) {
    let mut segs = split_top(step, true);
    let mut filters = Vec::new();
    while segs.len() > 1 {
        let last = segs[segs.len() - 1].0.split_whitespace().next().unwrap_or("").to_lowercase();
        if !FILTERS.contains(&last.as_str()) {
            break;
        }
        filters.insert(0, last);
        segs.pop();
    }
    // a pipe that stays (`git ls-files | xargs wc -l`) exits with its last program: itself
    (segs.iter().map(|s| s.0.trim()).collect::<Vec<_>>().join(" | "), filters)
}

/// The step without redirections that only merge or discard output: `2>&1`, `*> $null`,
/// `>/dev/null`. A redirect into a file stays: it's part of what the command does.
fn without_redirects(step: &str) -> String {
    let w = words(step);
    let mut kept: Vec<&str> = Vec::with_capacity(w.len());
    let mut i = 0;
    while i < w.len() {
        let (op, target) = split_redirect(w[i]);
        match op {
            Some(_) if target.is_empty() && w.get(i + 1).is_some_and(|t| is_sink(t)) => i += 2,
            Some(_) if !target.is_empty() && (is_sink(target) || target.starts_with('&')) => i += 1,
            _ => {
                kept.push(w[i]);
                i += 1;
            }
        }
    }
    kept.join(" ")
}

/// `2>&1` -> (Some("2>"), "&1"); `>` -> (Some(">"), ""); anything else -> (None, word).
fn split_redirect(w: &str) -> (Option<&str>, &str) {
    let b = w.as_bytes();
    let mut i = usize::from(matches!(b.first(), Some(b'0'..=b'9' | b'*' | b'&')));
    if b.get(i) != Some(&b'>') {
        return (None, w);
    }
    i += 1;
    if b.get(i) == Some(&b'>') {
        i += 1;
    }
    (Some(&w[..i]), &w[i..])
}

fn is_sink(t: &str) -> bool {
    matches!(t.to_lowercase().as_str(), "$null" | "/dev/null" | "nul")
}

/// Words, with quoted strings kept whole (quotes included).
fn words(s: &str) -> Vec<&str> {
    let b = s.as_bytes();
    let (mut out, mut start, mut quote) = (Vec::new(), None::<usize>, 0u8);
    for (i, &c) in b.iter().enumerate() {
        if quote != 0 {
            if c == quote {
                quote = 0;
            }
            continue;
        }
        if c.is_ascii_whitespace() {
            if let Some(st) = start.take() {
                out.push(&s[st..i]);
            }
            continue;
        }
        if start.is_none() {
            start = Some(i);
        }
        if c == b'"' || c == b'\'' {
            quote = c;
        }
    }
    if let Some(st) = start {
        out.push(&s[st..]);
    }
    out
}

fn unquote(s: &str) -> &str {
    let t = s.trim();
    for q in ['"', '\''] {
        if let Some(r) = t.strip_prefix(q).and_then(|r| r.strip_suffix(q)) {
            return r;
        }
    }
    t
}

/// `cd /d D:\x` (cmd), `Set-Location -Path C:\x` (PowerShell)
fn is_cd_flag(a: &str) -> bool {
    matches!(a.to_lowercase().as_str(), "/d" | "-path" | "-literalpath" | "--")
}

fn is_drive_hop(t: &str) -> bool {
    let b = t.as_bytes();
    b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

fn is_activate(s: &str) -> bool {
    let l = s.to_lowercase();
    l.contains("activate") && !l.contains("deactivate")
}

/// A step that sets up the environment the next ones run in: dot-sourcing a script, a venv's
/// activate, `nvm use`, environment variables (`export X=1`, `$env:X = 1`): Some(None). A plain
/// variable (`S=/tmp/x`, `$py = "..."`) matters only to a command that uses it: Some(its name).
/// `X=1 npm test` is a command with a variable, not a step of its own: None.
fn env_step(s: &str, w: &[&str]) -> Option<Option<String>> {
    let first = w[0].to_lowercase();
    let name = |n: &str| n.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if matches!(first.as_str(), "." | "source" | "export") || is_activate(s) || (first == "nvm" && w.get(1).is_some_and(|x| x.eq_ignore_ascii_case("use"))) {
        return Some(None);
    }
    if let Some(v) = first.strip_prefix('$') {
        let (n, rest) = v.split_once('=').unwrap_or((v, ""));
        let assigns = w.get(1) == Some(&"=") || !rest.is_empty() || v.ends_with('=');
        if assigns && n.starts_with("env:") {
            return Some(None);
        }
        return (assigns && name(n)).then(|| Some(n.to_string()));
    }
    match first.split_once('=') {
        Some((n, _)) if w.len() == 1 && name(n) => Some(Some(n.to_string())),
        _ => None,
    }
}

/// The environment `main` ran in: every step that sets one up, and the variables it (or a step
/// it keeps) uses.
fn env_for(env: &[(String, Option<String>)], main: &str) -> Vec<String> {
    let mut uses = main.to_lowercase();
    let mut kept: Vec<String> = Vec::new();
    for (step, var) in env.iter().rev() {
        if var.as_ref().is_none_or(|v| uses.contains(&format!("${v}")) || uses.contains(&format!("${{{v}}}"))) {
            uses.push(' ');
            uses.push_str(&step.to_lowercase());
            kept.push(step.clone());
        }
    }
    kept.reverse();
    kept
}

fn is_win_path(p: &str) -> bool {
    let b = p.as_bytes();
    p.contains('\\') || (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
}

fn is_abs(p: &str) -> bool {
    let b = p.as_bytes();
    p.starts_with('/') || (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
}

/// Git Bash's `/c/Users/x` (or `/cygdrive/c/...`) is `C:/Users/x`.
fn msys_to_drive(p: &str) -> Option<String> {
    let rest = p.strip_prefix("/cygdrive").unwrap_or(p);
    let b = rest.as_bytes();
    (b.len() >= 2 && b[0] == b'/' && b[1].is_ascii_alphabetic() && (b.len() == 2 || b[2] == b'/'))
        .then(|| format!("{}:{}", (b[1] as char).to_ascii_uppercase(), if b.len() == 2 { "/" } else { &rest[2..] }))
}

/// `C:/a/b` -> ["C:", "a", "b"]; `/a/b` -> ["", "a", "b"]
fn split_path(p: &str) -> Option<Vec<String>> {
    if !is_abs(p) {
        return None;
    }
    let mut it = p.split('/');
    let mut parts = vec![it.next()?.to_string()];
    for c in it {
        match c {
            "" | "." => {}
            ".." => {
                if parts.len() > 1 {
                    parts.pop();
                }
            }
            c => parts.push(c.to_string()),
        }
    }
    Some(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(line: &str) -> Vec<String> {
        commands(line).unwrap().into_iter().map(|c| c.text).collect()
    }

    #[test]
    fn plain_commands_stay_as_they_are() {
        assert_eq!(texts("npm run dev"), ["npm run dev"]);
        assert_eq!(texts("git commit -m \"a; b && c | d\""), ["git commit -m \"a; b && c | d\""]);
        assert_eq!(texts("docker compose exec web alembic upgrade head"), ["docker compose exec web alembic upgrade head"]);
    }

    #[test]
    fn what_an_agent_wrapped() {
        // reman's own build, as Claude Code ran it
        let c = commands(r#"Set-Location C:\Users\dev\reman\rust; . .\msvc-env.ps1; cargo build --release 2>&1 | Select-String -Pattern "^(warning|error)|-->|Finished" | Select-Object -First 20"#).unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].text, r". .\msvc-env.ps1; cargo build --release");
        assert_eq!(c[0].main, "cargo build --release");
        assert_eq!(c[0].cds, [r"C:\Users\dev\reman\rust"]);
        // two commands, each in the environment set up before them
        assert_eq!(
            texts(r#". .\msvc-env.ps1; cargo build --release 2>&1 | Select-Object -Last 5; cargo test --release 2>&1 | Select-String -Pattern "test result""#),
            [r". .\msvc-env.ps1; cargo build --release", r". .\msvc-env.ps1; cargo test --release"]
        );
        let c = commands(r#"cd C:/code/shop/frontend && npx jest --silent 2>&1 | grep -E "^(PASS|FAIL)|Tests:" | sort"#).unwrap();
        assert_eq!((c[0].text.as_str(), c[0].cds.as_slice()), ("npx jest --silent", &["C:/code/shop/frontend".to_string()][..]));
        // a group, a fallback that only talks, the same command twice
        assert_eq!(
            texts(r#"npx tsc --noEmit 2>&1 | wc -l && (npx tsc --noEmit 2>&1 | grep -vE "node_modules|x" || echo "CLEAN") && npx jest 2>&1 | grep -E "^Tests:""#),
            ["npx tsc --noEmit", "npx jest"]
        );
        assert_eq!(texts(r#"& .\target\release\reman.exe setup --no-connect *> $null; "installed: exit $LASTEXITCODE""#), [r"& .\target\release\reman.exe setup --no-connect"]);
        assert_eq!(texts("cargo test 2> /dev/null"), ["cargo test"]);
    }

    #[test]
    fn whose_exit_status_it_is() {
        let o = |line: &str| commands(line).unwrap().into_iter().map(|c| (c.main, c.outcome)).collect::<Vec<_>>();
        use Outcome::*;
        assert_eq!(o("npm test"), [("npm test".into(), Own)]);
        assert_eq!(o("cd web && npm test"), [("npm test".into(), Own)]);
        // a pipeline exits with its last program's status
        assert_eq!(o("npx tsc --noEmit 2>&1 | wc -l"), [("npx tsc --noEmit".into(), Hidden)]);
        assert_eq!(o("cd web && npx jest 2>&1 | tail -5"), [("npx jest".into(), Hidden)]);
        // ...but PowerShell keeps a native command's exit code through its cmdlets
        assert_eq!(o(r#". .\env.ps1; cargo build --release 2>&1 | Select-String "error""#), [("cargo build --release".into(), Own)]);
        // `a && b` exiting 0 says a worked; `a; b` and `a || b` say nothing about a
        assert_eq!(o("npm ci && npm test"), [("npm ci".into(), OnSuccess), ("npm test".into(), Own)]);
        assert_eq!(o("cargo build; echo done"), [("cargo build".into(), Hidden)]);
        assert_eq!(o(r#"(npx tsc --noEmit || echo "CLEAN") && npx jest"#), [("npx tsc --noEmit".into(), Hidden), ("npx jest".into(), Own)]);
        assert_eq!(o("(npm ci) && npm test")[0], ("npm ci".into(), OnSuccess));
        // the same command twice: what the clearer of the two says
        assert_eq!(o("npx tsc | wc -l; npx tsc"), [("npx tsc".into(), Own)]);
        assert_eq!(o("git ls-files | xargs wc -l"), [("git ls-files | xargs wc -l".into(), Own)]);
    }

    #[test]
    fn the_same_run_rewritten_by_an_agents_terminal_tool() {
        assert!(same_commands(r"cd D:\app && npm test", "npm test"));
        assert!(same_commands("npm ci && npm test", "npm ci; npm test"));
        assert!(same_commands(" npm test", "npm test"));
        assert!(!same_commands("npm test", "npm run test"));
        assert!(!same_commands("npm ci && npm test", "npm test"));
        assert!(!same_commands("cd web", "cd api"), "nothing ran: not the same run");
    }

    #[test]
    fn a_pipe_that_does_work_stays() {
        assert_eq!(texts("git ls-files | xargs wc -l"), ["git ls-files | xargs wc -l"]);
        assert_eq!(texts("cat schema.sql | psql app"), ["cat schema.sql | psql app"]);
        assert_eq!(texts("pytest > out.txt"), ["pytest > out.txt"]);
    }

    #[test]
    fn the_venv_vs_code_activates() {
        let c = commands(r"(Set-ExecutionPolicy -Scope Process -ExecutionPolicy RemoteSigned) ; (& d:\app\api\.venv\Scripts\Activate.ps1)").unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].text, r"& d:\app\api\.venv\Scripts\Activate.ps1");
        assert_eq!(texts(r".\venv\Scripts\activate; pytest -q"), [r".\venv\Scripts\activate; pytest -q"]);
        assert!(texts("export FOO=1").is_empty());
        // variables are the environment of what follows
        assert_eq!(texts("S=/tmp/x; python $S/a.py"), ["S=/tmp/x; python $S/a.py"]);
        assert_eq!(texts(r#"$py = "C:\x\python.exe"; & $py t.py"#), [r#"$py = "C:\x\python.exe"; & $py t.py"#]);
        assert_eq!(texts("$env:CI=1; npm test"), ["$env:CI=1; npm test"]);
        assert_eq!(texts("FOO=1 npm test"), ["FOO=1 npm test"]);
        // ...only when the command uses them
        assert_eq!(texts(r#"$box = Join-Path $env:TEMP "x"; . .\env.ps1; cargo build --release"#), [r". .\env.ps1; cargo build --release"]);
        assert_eq!(texts("D=/tmp/a; S=$D/b; python $S/c.py"), ["D=/tmp/a; S=$D/b; python $S/c.py"]);
    }

    #[test]
    fn two_lines_pasted_into_one_are_left_out() {
        assert!(commands(r"cd .\frontend\(Set-ExecutionPolicy -Scope Process -ExecutionPolicy RemoteSigned) ; (& d:\x\Activate.ps1)").is_none());
        assert!(commands(r"cd c:\code\shop\frontend npx eas build --profile production").is_none());
    }

    #[test]
    fn looking_around_runs_nothing() {
        assert!(texts("cd api").is_empty());
        assert!(texts(r#"echo "done""#).is_empty());
        assert_eq!(texts("cd /d D:\\x && npm test"), ["npm test"]);
    }

    #[test]
    fn folders() {
        let r = |base: &str, cds: &[&str]| resolve_dir(base, &cds.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(r(r"D:\app", &[]).as_deref(), Some(r"D:\app"));
        assert_eq!(r(r"D:\app", &["frontend"]).as_deref(), Some(r"D:\app\frontend"));
        assert_eq!(r(r"D:\app\frontend", &[r"..\api"]).as_deref(), Some(r"D:\app\api"));
        assert_eq!(r(r"D:\app", &["D:/other/x"]).as_deref(), Some(r"D:\other\x"));
        assert_eq!(r(r"C:\Users\me", &["/c/Users/me/reman"]).as_deref(), Some(r"C:\Users\me\reman"));
        assert_eq!(r(r"D:\app", &["/opt/app-staging"]), None, "a server's folder, typed on Windows");
        assert_eq!(r(r"D:\app", &["~/x"]), None);
        assert_eq!(r(r"D:\app", &["$env:TEMP"]), None);
        assert_eq!(r("/home/me/app", &["web"]).as_deref(), Some("/home/me/app/web"));
        assert_eq!(r("/home/me/app", &["/srv/x", ".."]).as_deref(), Some("/srv"));
        assert_eq!(r(r"D:\", &[]).as_deref(), Some(r"D:\"));
    }
}
