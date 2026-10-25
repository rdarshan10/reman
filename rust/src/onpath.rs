//! Is a command's program on this machine at all? `reman_check` tells an agent when it isn't
//! (`gh: command not found`), before the agent builds a polling loop on it. Only bare names are
//! looked up (`gh`, `cargo`), never a path (`./build.sh`, `C:\x\tool.exe`), and never what a
//! shell runs itself (`cd`, `Get-ChildItem`, PowerShell's `ls`): those are not files on PATH.
use std::path::PathBuf;

/// The programs a line runs, as typed, that can be looked up on PATH: the first word of each
/// command in it, after `cd`s, the environment it ran in, and `NAME=value` assignments.
pub fn programs(line: &str) -> Vec<String> {
    let parts: Vec<String> = match crate::unwrap::commands(line) {
        Some(c) => c.into_iter().map(|c| c.main).collect(),
        None => vec![line.to_string()],
    };
    let mut out: Vec<String> = Vec::new();
    for p in parts {
        let Some(name) = p.split_whitespace().find(|t| !is_assignment(t)).map(|t| t.trim_matches(|c| c == '"' || c == '\'')) else { continue };
        if lookup_able(name) && !out.iter().any(|o| o.eq_ignore_ascii_case(name)) {
            out.push(name.to_string());
        }
    }
    out
}

/// Is `name` a file in one of PATH's folders (with one of PATHEXT's endings on Windows)? `path`
/// is the agent's PATH when its bridge sent one, else this process's.
pub fn on_path(name: &str, path: Option<&str>) -> bool {
    let own = std::env::var("PATH").unwrap_or_default();
    let endings: Vec<String> = if cfg!(windows) {
        let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        // the name as typed (`python3.exe`), then with each ending; PowerShell also runs a .ps1
        std::iter::once(String::new()).chain(pathext.split(';').filter(|x| !x.is_empty()).map(str::to_string)).chain([".PS1".to_string()]).collect()
    } else {
        vec![String::new()]
    };
    // symlink_metadata, not exists(): Windows' app execution aliases (WindowsApps\winget.exe)
    // are reparse points that exists() can't follow
    folders(path.unwrap_or(&own)).iter().any(|d| endings.iter().any(|x| d.join(format!("{name}{x}")).symlink_metadata().is_ok()))
}

/// This process's PATH, as folders.
pub fn path_folders() -> Vec<PathBuf> {
    folders(&std::env::var("PATH").unwrap_or_default())
}

/// PATH's folders. On Windows a PATH from Git Bash or MSYS (`/c/Users/me/bin:/usr/bin`) is
/// read as such, its drive folders turned back into `C:/Users/me/bin`.
fn folders(path: &str) -> Vec<PathBuf> {
    if cfg!(windows) && path.starts_with('/') {
        return path
            .split(':')
            .filter_map(|p| {
                let b = p.as_bytes();
                (b.len() >= 2 && b[0] == b'/' && b[1].is_ascii_alphabetic() && (b.len() == 2 || b[2] == b'/'))
                    .then(|| PathBuf::from(format!("{}:/{}", (b[1] as char).to_ascii_uppercase(), p.get(3..).unwrap_or(""))))
            })
            .collect();
    }
    let sep = if cfg!(windows) { ';' } else { ':' };
    path.split(sep).map(|p| p.trim().trim_matches('"')).filter(|p| !p.is_empty()).map(PathBuf::from).collect()
}

/// `NAME=value` before a command (`RUST_LOG=debug cargo run`)
fn is_assignment(t: &str) -> bool {
    t.split_once('=').is_some_and(|(k, _)| !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
}

/// A bare program name, not something the shell runs itself.
fn lookup_able(name: &str) -> bool {
    let plain = !name.is_empty()
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+'));
    let l = name.to_ascii_lowercase();
    plain && !SHELL_OWN.contains(&l.as_str()) && !cmdlet(&l)
}

/// `Get-ChildItem`, `invoke-webrequest`: a PowerShell verb, a dash, a noun
fn cmdlet(l: &str) -> bool {
    l.split_once('-').is_some_and(|(verb, noun)| !noun.is_empty() && PS_VERBS.contains(&verb))
}

/// What shells run themselves, and PowerShell's and reman's own aliases and functions.
const SHELL_OWN: &[&str] = &[
    // POSIX shells, zsh, fish
    "alias", "bg", "bind", "break", "builtin", "case", "command", "continue", "declare", "dirs", "disown", "do", "done", "echo", "elif", "else",
    "esac", "eval", "exec", "exit", "export", "false", "fc", "fg", "fi", "for", "function", "hash", "help", "history", "if", "jobs", "kill", "let",
    "local", "popd", "printf", "pushd", "pwd", "read", "readonly", "rehash", "return", "set", "setopt", "shift", "shopt", "source", "test", "then",
    "time", "trap", "true", "type", "typeset", "ulimit", "umask", "unalias", "unset", "wait", "whence", "where", "which", "while", "cd", "abbr",
    "functions", "contains", "string", "math", "status", "end", "begin", "and", "or", "not",
    // Command Prompt
    "assoc", "call", "cls", "copy", "del", "dir", "endlocal", "erase", "ftype", "goto", "md", "mklink", "move", "path", "pause", "prompt", "rd",
    "ren", "rename", "rmdir", "setlocal", "start", "title", "ver", "vol", "color",
    // PowerShell aliases
    "cat", "chdir", "clear", "cp", "cpi", "diff", "foreach", "gc", "gci", "gcm", "gi", "gl", "gm", "gp", "gps", "group", "gsv", "gv", "h", "icm",
    "iex", "ii", "ipmo", "irm", "iwr", "ls", "man", "measure", "mi", "mkdir", "mount", "mv", "ni", "nv", "ogv", "ps", "r", "ri", "rm", "rni",
    "rp", "rv", "sal", "sc", "select", "sl", "sleep", "sls", "sort", "sp", "spps", "sv", "tee", "write", "fl", "ft", "fw", "gu", "oh",
    // reman's own shell functions
    "rr", "rcd",
];

const PS_VERBS: &[&str] = &[
    "add", "approve", "assert", "backup", "block", "build", "checkpoint", "clear", "close", "compare", "complete", "compress", "confirm", "connect",
    "convert", "convertfrom", "convertto", "copy", "debug", "deny", "deploy", "disable", "disconnect", "dismount", "edit", "enable", "enter",
    "exit", "expand", "export", "find", "format", "get", "grant", "group", "hide", "import", "initialize", "install", "invoke", "join", "limit",
    "lock", "measure", "merge", "mount", "move", "new", "open", "optimize", "out", "ping", "pop", "protect", "publish", "push", "read", "receive",
    "redo", "register", "remove", "rename", "repair", "request", "reset", "resize", "resolve", "restart", "restore", "resume", "revoke", "save",
    "search", "select", "send", "set", "show", "skip", "split", "start", "step", "stop", "submit", "suspend", "switch", "sync", "test", "trace",
    "unblock", "undo", "uninstall", "unlock", "unprotect", "unpublish", "unregister", "update", "use", "wait", "watch", "where", "write",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn programs_are_the_bare_names_a_line_runs() {
        assert_eq!(programs("gh pr checks 12"), vec!["gh"]);
        assert_eq!(programs("cd web && RUST_LOG=debug cargo run --release 2>&1 | tail -5"), vec!["cargo"]);
        assert_eq!(programs(r". .\msvc-env.ps1; cargo build --release"), vec!["cargo"]);
        assert_eq!(programs("git add -A && git commit -m x && git push"), vec!["git"]);
        // a path, a cmdlet, what the shell runs itself: nothing to look up
        assert!(programs("./build.sh").is_empty());
        assert!(programs(r"C:\tools\x.exe --help").is_empty());
        assert!(programs("Get-ChildItem -Recurse").is_empty());
        assert!(programs("echo hi").is_empty());
        assert!(programs("ls -la").is_empty());
    }

    #[test]
    fn a_program_is_found_on_the_path_it_is_given() {
        let dir = std::env::temp_dir().join(format!("reman-onpath-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = if cfg!(windows) { "mytool.exe" } else { "mytool" };
        std::fs::write(dir.join(file), b"").unwrap();
        let path = dir.to_string_lossy().into_owned();
        assert!(on_path("mytool", Some(&path)));
        assert!(!on_path("not-a-tool-anywhere", Some(&path)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(windows)]
    #[test]
    fn a_git_bash_path_is_read_as_one() {
        assert_eq!(folders("/c/Users/me/bin:/usr/bin"), vec![PathBuf::from("C:/Users/me/bin")]);
        assert_eq!(folders(r"C:\a;C:\b"), vec![PathBuf::from(r"C:\a"), PathBuf::from(r"C:\b")]);
    }
}
