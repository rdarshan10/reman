//! What git says about a folder, read from the files under `.git` (never runs git): the branch
//! and commit checked out when a command ran, and which repo the folder belongs to (its origin,
//! the same for every clone and worktree).

use std::path::{Path, PathBuf};

/// Where a folder's checkout keeps its git files.
struct Checkout {
    /// HEAD lives here (a worktree has its own)
    gitdir: PathBuf,
    /// refs, packed-refs and config live here (the main repo's .git, for a worktree)
    common: PathBuf,
    root: PathBuf,
}

/// The checkout `dir` is in: the nearest `.git` folder, or a worktree's `.git` file.
fn checkout(dir: &str) -> Option<Checkout> {
    let dir = crate::config::norm_path(dir);
    for d in Path::new(&dir).ancestors() {
        let git = d.join(".git");
        if git.is_dir() {
            return Some(Checkout { gitdir: git.clone(), common: git, root: d.to_path_buf() });
        }
        if git.is_file() {
            // a worktree: `gitdir: <repo>/.git/worktrees/<name>`, whose `commondir` names the repo's
            let text = std::fs::read_to_string(&git).ok()?;
            let gd = text.trim().strip_prefix("gitdir:")?.trim();
            let gitdir = d.join(gd);
            let common = std::fs::read_to_string(gitdir.join("commondir")).ok().map(|c| gitdir.join(c.trim())).unwrap_or_else(|| gitdir.clone());
            return Some(Checkout { gitdir, common, root: d.to_path_buf() });
        }
    }
    None
}

/// The branch and commit checked out in a folder. A detached HEAD has a commit and no branch.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct State {
    pub branch: Option<String>,
    /// the commit, abbreviated to 12 characters
    pub head: Option<String>,
}

pub fn state(dir: &str) -> State {
    let Some(c) = checkout(dir) else { return State::default() };
    let Ok(head) = std::fs::read_to_string(c.gitdir.join("HEAD")) else { return State::default() };
    let head = head.trim();
    let short = |sha: &str| (sha.len() >= 12 && sha.bytes().all(|b| b.is_ascii_hexdigit())).then(|| sha[..12].to_string());
    match head.strip_prefix("ref:").map(str::trim) {
        Some(r) => State { branch: Some(r.strip_prefix("refs/heads/").unwrap_or(r).to_string()), head: resolve(&c, r).as_deref().and_then(short) },
        None => State { branch: None, head: short(head) },
    }
}

/// A ref's commit: its loose file, else its line in packed-refs. None on a branch with no commit.
fn resolve(c: &Checkout, r: &str) -> Option<String> {
    for base in [&c.gitdir, &c.common] {
        if let Ok(s) = std::fs::read_to_string(base.join(r)) {
            return Some(s.trim().to_string());
        }
    }
    let packed = std::fs::read_to_string(c.common.join("packed-refs")).ok()?;
    packed.lines().find_map(|l| l.strip_suffix(r).and_then(|x| x.strip_suffix(' ')).map(str::to_string))
}

/// Which repo a folder belongs to: its origin (`github.com/user/repo`, however it was cloned),
/// else the checkout's root, else the folder itself. Cached: called once per distinct folder.
pub fn repo_identity(path: &str) -> String {
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::sync::OnceLock;
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(v) = cache.lock().get(path) {
        return v.clone();
    }
    let ident = match checkout(path) {
        Some(c) => origin(&c.common.join("config")).map(|u| normalize_remote(&u)).unwrap_or_else(|| crate::config::norm_path(&c.root.to_string_lossy())),
        None => crate::config::norm_path(path),
    };
    cache.lock().insert(path.to_string(), ident.clone());
    ident
}

/// The url of the `origin` remote in a git config, else of the first remote.
fn origin(cfg: &Path) -> Option<String> {
    let text = std::fs::read_to_string(cfg).ok()?;
    let mut sect: Option<String> = None;
    let mut urls: Vec<(String, String)> = Vec::new();
    for ln in text.lines() {
        let s = ln.trim();
        if s.starts_with('[') && s.ends_with(']') {
            sect = Some(s[1..s.len() - 1].trim().to_string());
        } else if let Some(sec) = &sect {
            if sec.starts_with("remote ") && s.to_lowercase().starts_with("url") && s.contains('=') {
                let name = sec.split('"').nth(1).unwrap_or(sec).to_string();
                urls.push((name, s.split_once('=').unwrap().1.trim().to_string()));
            }
        }
    }
    urls.iter().find(|(n, _)| n == "origin").or(urls.first()).map(|(_, u)| u.clone())
}

/// One spelling per repo: `git@github.com:me/app.git`, `https://github.com/me/app` and
/// `ssh://git@github.com/me/app.git` are all `github.com/me/app`. A remote that is a local path
/// stays that path.
fn normalize_remote(url: &str) -> String {
    let u = url.trim().trim_end_matches('/');
    let u = u.strip_suffix(".git").unwrap_or(u);
    let (host, path) = if let Some((_, rest)) = u.split_once("://") {
        match rest.split_once('/') {
            Some((h, p)) => (h, p),
            None => (rest, ""),
        }
    } else {
        // scp-like `user@host:path`; a Windows drive (`C:\x`) or a plain path is not one
        match u.split_once(':') {
            Some((h, p)) if h.len() > 1 && !h.contains('/') && !h.contains('\\') && !p.starts_with('\\') => (h, p),
            _ => return crate::config::norm_path(u),
        }
    };
    let host = host.rsplit('@').next().unwrap_or(host).to_lowercase();
    let host = host.split(':').next().unwrap_or(&host).to_string(); // a port is not part of the repo
    if host.is_empty() || host == "file" {
        return crate::config::norm_path(path);
    }
    format!("{host}/{}", path.trim_start_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remotes_spelled_any_way_are_one_repo() {
        for u in ["git@github.com:me/app.git", "https://github.com/me/app", "https://github.com/me/app.git/", "ssh://git@github.com/me/app.git", "https://user@GitHub.com/me/app", "ssh://git@github.com:22/me/app"] {
            assert_eq!(normalize_remote(u), "github.com/me/app", "{u}");
        }
        assert_eq!(normalize_remote("git@gitlab.example.com:group/sub/app.git"), "gitlab.example.com/group/sub/app");
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("reman-git-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn branch_and_commit_loose_packed_detached_worktree() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let repo = tmp("repo");
        let git = repo.join(".git");
        std::fs::create_dir_all(git.join("refs/heads/feat")).unwrap();
        std::fs::create_dir_all(repo.join("src/deep")).unwrap();
        std::fs::write(git.join("config"), "[core]\n\tbare = false\n[remote \"origin\"]\n\turl = git@github.com:me/app.git\n").unwrap();
        // loose ref, seen from a subfolder
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(git.join("refs/heads/main"), format!("{sha}\n")).unwrap();
        let sub = repo.join("src/deep").to_string_lossy().into_owned();
        assert_eq!(state(&sub), State { branch: Some("main".into()), head: Some(sha[..12].into()) });
        // packed ref, a branch name with a slash
        std::fs::write(git.join("HEAD"), "ref: refs/heads/feat/x\n").unwrap();
        std::fs::write(git.join("packed-refs"), format!("# pack-refs with: peeled\n{sha} refs/heads/feat/x\n")).unwrap();
        assert_eq!(state(&sub), State { branch: Some("feat/x".into()), head: Some(sha[..12].into()) });
        // a new branch with no commit yet
        std::fs::write(git.join("HEAD"), "ref: refs/heads/empty\n").unwrap();
        assert_eq!(state(&sub), State { branch: Some("empty".into()), head: None });
        // detached
        std::fs::write(git.join("HEAD"), format!("{sha}\n")).unwrap();
        assert_eq!(state(&sub), State { branch: None, head: Some(sha[..12].into()) });
        // a worktree elsewhere: its own HEAD, the repo's refs and origin
        let wt = tmp("wt");
        let wgd = git.join("worktrees/wt");
        std::fs::create_dir_all(&wgd).unwrap();
        std::fs::write(wgd.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(wgd.join("commondir"), "../..\n").unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", wgd.to_string_lossy())).unwrap();
        let w = wt.to_string_lossy().into_owned();
        assert_eq!(state(&w), State { branch: Some("main".into()), head: Some(sha[..12].into()) });
        assert_eq!(repo_identity(&w), "github.com/me/app");
        assert_eq!(repo_identity(&sub), "github.com/me/app");
        // no checkout at all
        let none = tmp("none");
        assert_eq!(state(&none.to_string_lossy()), State::default());
        for d in [repo, wt, none] {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}
