//! What a program tldr has no page for is, from what this machine's own files say about it, and
//! never by running it: no `--help`, since running an unknown program to learn what it does is
//! how you set off what it does. In order:
//!   * a Python console script (`venv\Scripts\lightrag-server.exe`): its package's Summary in
//!     `*.dist-info/METADATA`, found by the entry point that names it;
//!   * an npm command (`eas`: a shim in npm's folder or in `node_modules/.bin`): the package's
//!     `description` in package.json;
//!   * a Windows program: the FileDescription (or Comments) of its version information;
//!   * a script (`.ps1`, `.py`, `.sh`, `.bat`, `.cmd`, or a file starting `#!`): its `.SYNOPSIS`,
//!     docstring or opening comment.
//! The daemon keeps the answers in program_help and re-describes the commands they improve
//! (Daemon::learn_programs).
use regex::Regex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Endings a program file has, longest first so `.ps1` is not read as `.p...`
const RUNNABLE: &[&str] = &[".exe", ".cmd", ".bat", ".ps1", ".com", ".py", ".sh"];
/// Larger files are not read for their version information
const MAX_EXE: u64 = 128 << 20;

/// A name worth looking for: a program's, not a fragment of shell syntax.
pub fn name_like(prog: &str) -> bool {
    prog.len() >= 2 && prog.starts_with(|c: char| c.is_ascii_alphanumeric()) && prog.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+'))
}

/// The folders of the virtual environments a command activates (`& d:\api\.venv\Scripts\Activate.ps1`,
/// `venv/Scripts/activate` run in a known folder): where its console scripts are.
pub fn activated(cmd: &str, cwds: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for t in cmd.split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | ';' | '&' | '|')) {
        let t = t.trim_matches(|c| c == '"' || c == '\'').replace('\\', "/");
        let low = t.to_lowercase();
        let Some(dir) = ["/activate", "/activate.ps1", "/activate.bat"].iter().find_map(|x| low.strip_suffix(x).map(|_| &t[..t.len() - x.len()])) else { continue };
        let low_dir = dir.to_lowercase();
        if !(low_dir.ends_with("scripts") || low_dir.ends_with("bin")) {
            continue;
        }
        let p = Path::new(dir);
        if p.is_absolute() {
            out.push(p.to_path_buf());
        } else {
            out.extend(cwds.iter().map(|c| Path::new(c).join(dir)));
        }
    }
    out
}

fn conda_envs() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else { return vec![] };
    let list = std::fs::read_to_string(home.join(".conda").join("environments.txt")).unwrap_or_default();
    let mut out: Vec<PathBuf> = Vec::new();
    for l in list.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if !out.iter().any(|o| o.to_string_lossy().eq_ignore_ascii_case(l)) {
            out.push(PathBuf::from(l));
        }
    }
    out
}

fn stem_of(name: &str) -> &str {
    RUNNABLE.iter().find_map(|x| name.strip_suffix(x)).unwrap_or(name)
}

pub struct Finder {
    /// program name (lowercase, no ending) -> its file, from the folders below, first found wins
    files: HashMap<String, PathBuf>,
    /// an environment's folder -> its console scripts' package summaries
    envs: HashMap<PathBuf, HashMap<String, String>>,
}

impl Finder {
    /// Each folder a program may be in, listed once: PATH's; the folders the history ran in, their
    /// virtual environments and node_modules/.bin; the venvs it activated; conda's environments.
    pub fn new(cwds: &[String], activated: &[PathBuf]) -> Finder {
        // (folder, may a file without an ending be a program there)
        // Windows runs no file without an ending (npm writes a .cmd beside its sh shim)
        let bare = !cfg!(windows);
        let mut dirs: Vec<(PathBuf, bool)> = crate::onpath::path_folders().into_iter().map(|d| (d, bare)).collect();
        dirs.extend(activated.iter().map(|d| (d.clone(), bare)));
        for c in cwds.iter().map(Path::new).filter(|c| c.is_absolute()) {
            for v in [".venv", "venv", "env"] {
                for b in ["Scripts", "bin"] {
                    dirs.push((c.join(v).join(b), bare));
                }
            }
            dirs.push((c.join("node_modules").join(".bin"), bare));
            // not the project's own folder: its `server.py` is no other project's `server`
            // (Finder::find looks there only for the commands run in it)
        }
        for env in conda_envs() {
            dirs.push((env.join("Scripts"), bare));
            dirs.push((env.join("bin"), bare));
        }
        let mut files = HashMap::new();
        for (d, bare) in dirs {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.flatten().take(5000) {
                let low = e.file_name().to_string_lossy().to_lowercase();
                let stem = stem_of(&low);
                if stem.is_empty() || (stem == low && (!bare || low.contains('.'))) {
                    continue;
                }
                files.entry(stem.to_string()).or_insert_with(|| e.path());
            }
        }
        Finder { files, envs: HashMap::new() }
    }

    /// The file a program word names: a path (absolute, or under a folder it ran in), a file in a
    /// folder it ran in, else a name in one of the folders listed.
    pub fn find(&self, word: &str, cwds: &[String]) -> Option<PathBuf> {
        let w = word.replace('\\', "/");
        let path = w.contains('/');
        let p = Path::new(&w);
        let bases: Vec<PathBuf> = if p.is_absolute() { vec![p.to_path_buf()] } else { cwds.iter().map(|c| Path::new(c).join(&w)).collect() };
        // a bare word is a file of its folder only with a program's ending (`build` is no `build/`)
        let endings: &[&str] = if path || stem_of(&w.to_lowercase()).len() < w.len() { &["", ".exe", ".cmd", ".bat", ".ps1"] } else { RUNNABLE };
        let here = bases.iter().flat_map(|b| endings.iter().map(move |x| PathBuf::from(format!("{}{x}", b.display())))).find(|f| f.is_file());
        if here.is_some() || path {
            return here;
        }
        self.files.get(stem_of(&w.to_lowercase())).cloned()
    }

    /// What `file` says it is, in one sentence; None when it says nothing useful.
    pub fn describe(&mut self, file: &Path) -> Option<String> {
        let name = stem_of(&file.file_name()?.to_string_lossy().to_lowercase()).to_string();
        let ext = file.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        let said = self
            .console_script(file, &name)
            .or_else(|| npm_package(file))
            .or_else(|| matches!(ext.as_str(), "exe" | "com").then(|| version_info(file)).flatten())
            .or_else(|| script_header(file, &ext));
        said.and_then(|s| tidy(&s, &name))
    }

    /// A console script of a Python environment (`<env>/Scripts/x.exe`, `<env>/bin/x`): the
    /// Summary of the package whose entry points name it.
    fn console_script(&mut self, file: &Path, name: &str) -> Option<String> {
        let dir = file.parent()?;
        let d = dir.file_name()?.to_string_lossy().to_lowercase();
        if d != "scripts" && d != "bin" {
            return None;
        }
        let env = dir.parent()?.to_path_buf();
        let scripts = self.envs.entry(env.clone()).or_insert_with(|| entry_points(&env));
        scripts.get(name).cloned()
    }
}

/// An environment's console scripts: script name -> its package's Summary.
fn entry_points(env: &Path) -> HashMap<String, String> {
    // a venv's or conda's `Lib\site-packages`; a user install's `Python38\site-packages` beside
    // its Scripts; Linux and macOS's `lib/python3.x/site-packages`
    let mut sites = vec![env.join("Lib").join("site-packages"), env.join("site-packages")];
    if let Ok(rd) = std::fs::read_dir(env.join("lib")) {
        sites.extend(rd.flatten().map(|e| e.path().join("site-packages")));
    }
    let mut out = HashMap::new();
    for site in sites {
        let Ok(rd) = std::fs::read_dir(&site) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if !p.extension().is_some_and(|x| x == "dist-info") {
                continue;
            }
            let Ok(eps) = std::fs::read_to_string(p.join("entry_points.txt")) else { continue };
            let mut scripts = Vec::new();
            let mut in_scripts = false;
            for l in eps.lines().map(str::trim) {
                if l.starts_with('[') {
                    in_scripts = l == "[console_scripts]" || l == "[gui_scripts]";
                } else if let (true, Some((k, _))) = (in_scripts, l.split_once('=')) {
                    scripts.push(k.trim().to_lowercase());
                }
            }
            if scripts.is_empty() {
                continue;
            }
            let meta = std::fs::read_to_string(p.join("METADATA")).unwrap_or_default();
            // the headers end at the first blank line; the long description follows
            let summary = meta.lines().take_while(|l| !l.trim().is_empty()).find_map(|l| l.strip_prefix("Summary:")).map(str::trim);
            if let Some(s) = summary.filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("unknown")) {
                for k in scripts {
                    out.entry(k).or_insert_with(|| s.to_string());
                }
            }
        }
    }
    out
}

/// An npm command: the package a shim runs (`"%dp0%\node_modules\eas-cli\bin\run"`,
/// `"$basedir/../eas-cli/bin/run"`), or the package a linked file is in. Its description.
fn npm_package(file: &Path) -> Option<String> {
    static PKG: OnceLock<Regex> = OnceLock::new();
    let rx = PKG.get_or_init(|| Regex::new(r#"(node_modules|\.\.)[\\/]+((?:@[\w.-]+[\\/]+)?[\w.-]+)[\\/]"#).unwrap());
    let dir = file.parent()?;
    let mut dirs: Vec<PathBuf> = Vec::new();
    if std::fs::metadata(file).ok()?.len() < 64 << 10 {
        if let Ok(text) = std::fs::read_to_string(file) {
            for c in rx.captures_iter(&text) {
                let base = if &c[1] == ".." { dir.parent().map(Path::to_path_buf) } else { Some(dir.join("node_modules")) };
                dirs.extend(base.map(|b| b.join(c[2].replace('\\', "/"))));
            }
        }
    }
    // a link into the package (`node_modules/.bin/x` on Linux and macOS)
    if let Ok(real) = std::fs::canonicalize(file) {
        dirs.extend(real.ancestors().skip(1).take(4).filter(|a| a.parent().is_some_and(|p| p.file_name().is_some_and(|n| n == "node_modules" || n.to_string_lossy().starts_with('@')))).map(Path::to_path_buf));
    }
    dirs.iter().find_map(|d| {
        let pkg: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(d.join("package.json")).ok()?).ok()?;
        pkg.get("description")?.as_str().map(str::to_string)
    })
}

/// A Windows program's own description, from its version information (a VS_VERSIONINFO
/// resource): the FileDescription string, or Comments when that only repeats the name.
fn version_info(file: &Path) -> Option<String> {
    if std::fs::metadata(file).ok()?.len() > MAX_EXE {
        return None;
    }
    let bytes = std::fs::read(file).ok()?;
    let name = stem_of(&file.file_name()?.to_string_lossy().to_lowercase()).to_string();
    ["FileDescription", "Comments"].iter().filter_map(|k| version_string(&bytes, k)).find(|v| tidy(v, &name).is_some())
}

/// The value of a String entry in a version resource: wLength, wValueLength, wType, the key
/// (UTF-16, null-ended), padding to 4 bytes, the value (UTF-16, null-ended).
fn version_string(bytes: &[u8], key: &str) -> Option<String> {
    let pat: Vec<u8> = key.encode_utf16().chain([0]).flat_map(|u| u.to_le_bytes()).collect();
    let mut from = 0;
    while let Some(i) = bytes[from..].windows(pat.len()).position(|w| w == pat.as_slice()).map(|i| i + from) {
        from = i + 2;
        let Some(start) = i.checked_sub(6) else { continue };
        let value_len = u16::from_le_bytes([bytes[start + 2], bytes[start + 3]]) as usize;
        let mut at = i + pat.len();
        at += (4 - (at - start) % 4) % 4;
        let units: Vec<u16> = bytes.get(at..at + value_len.min(256) * 2)?.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
        let s = String::from_utf16_lossy(&units);
        if value_len > 0 && !s.trim().is_empty() {
            return Some(s);
        }
    }
    None
}

/// A script's own words about itself: PowerShell's `.SYNOPSIS`, a Python docstring, or the
/// comment it opens with (after `#!`, encodings and `@echo off`).
fn script_header(file: &Path, ext: &str) -> Option<String> {
    let mut head = vec![0u8; 8192];
    let n = std::io::Read::read(&mut std::fs::File::open(file).ok()?, &mut head).ok()?;
    let text = String::from_utf8_lossy(&head[..n]).replace("\r\n", "\n");
    let text = text.trim_start_matches('\u{feff}');
    let shebang = text.starts_with("#!");
    if !matches!(ext, "ps1" | "py" | "sh" | "bash" | "bat" | "cmd") && !(ext.is_empty() && shebang) {
        return None;
    }
    if ext == "ps1" {
        static SYN: OnceLock<Regex> = OnceLock::new();
        let rx = SYN.get_or_init(|| Regex::new(r"(?is)\.SYNOPSIS[ \t]*\n(.+?)(?:\n[ \t]*\n|\n[ \t]*\.[A-Z]|#>)").unwrap());
        if let Some(c) = rx.captures(text) {
            return Some(c[1].lines().map(|l| l.trim().trim_start_matches('#').trim()).collect::<Vec<_>>().join(" "));
        }
    }
    let mut lines = text.lines().map(str::trim).skip_while(|l| l.is_empty() || l.starts_with("#!") || l.contains("coding:") || l.contains("coding=") || l.eq_ignore_ascii_case("@echo off"));
    let first = lines.clone().next()?;
    if ext == "py" {
        if let Some(q) = ["\"\"\"", "'''"].into_iter().find(|q| first.starts_with(q)) {
            let body = text[text.find(q)? + 3..].split(q).next()?;
            return body.split("\n\n").map(str::trim).find(|p| !p.is_empty()).map(str::to_string);
        }
    }
    let comment = |l: &str| -> Option<String> {
        let low = l.to_lowercase();
        let rest = if l.starts_with('#') {
            l.trim_start_matches('#')
        } else if l.starts_with("::") {
            &l[2..]
        } else if low.starts_with("rem ") || low == "rem" {
            &l[3..]
        } else if l.starts_with("<#") {
            &l[2..]
        } else {
            return None;
        };
        Some(rest.trim().to_string())
    };
    let mut said: Vec<String> = Vec::new();
    for l in lines.by_ref() {
        let Some(c) = comment(l) else { break };
        if c.is_empty() {
            if said.is_empty() {
                continue;
            }
            break;
        }
        if BOILERPLATE.iter().any(|b| c.to_lowercase().starts_with(b)) || c.chars().all(|ch| !ch.is_alphanumeric()) {
            if said.is_empty() {
                continue;
            }
            break;
        }
        said.push(c);
    }
    (!said.is_empty()).then(|| said.join(" "))
}

/// Opening comments that say nothing about what a script does
const BOILERPLATE: &[&str] = &[
    "copyright", "(c)", "©", "license", "licensed", "spdx", "author", "auto-generated", "autogenerated", "generated by", "this file was generated",
    "do not edit", "-*-", "vim:", "pylint:", "noqa", "type:", "shellcheck", "requires -", "#requires", "set -", "!/", "todo", "fixme",
    "usage:", "this file is utf-8",
    // what the program that starts a Python console script says of itself (pip's, distlib's)
    "simple launcher", "console launcher",
];

/// One clean sentence, or None when it says nothing beyond the program's name. Redacted: a
/// header is the author's text, and an agent may read it.
fn tidy(text: &str, name: &str) -> Option<String> {
    let t = text.replace(['®', '™'], "").split_whitespace().collect::<Vec<_>>().join(" ");
    let t = t.split(". ").next().unwrap_or(&t).trim().trim_end_matches('.').trim();
    let t: String = if t.chars().count() > 160 {
        let cut: String = t.chars().take(160).collect();
        cut.rsplit_once(' ').map_or(cut.clone(), |(a, _)| a.to_string())
    } else {
        t.to_string()
    };
    let squash = |s: &str| s.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect::<String>();
    let words = t.split_whitespace().count();
    let low = t.to_lowercase();
    if words < 2 || squash(&t) == squash(name) || BOILERPLATE.iter().any(|b| low.starts_with(b)) || crate::redact::residual_secret(&t) {
        return None;
    }
    Some(crate::redact::redact(&t))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("reman-localhelp-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn scripts_say_what_they_do() {
        let d = tmp("scripts");
        std::fs::write(d.join("deploy.ps1"), "<#\n.SYNOPSIS\n  Deploys the API to the staging cluster.\n.DESCRIPTION\n  Long text\n#>\nparam()").unwrap();
        std::fs::write(d.join("train.py"), "#!/usr/bin/env python\n# -*- coding: utf-8 -*-\n\"\"\"Train the ranking model on last week's clicks.\n\nMore.\"\"\"\nimport os").unwrap();
        std::fs::write(d.join("backup.sh"), "#!/bin/bash\n# Copyright 2024 Someone\n#\n# Back up the database to S3\n# every night\nset -e\n").unwrap();
        std::fs::write(d.join("build.bat"), "@echo off\nREM Build the installer for Windows\ncall x").unwrap();
        std::fs::write(d.join("bare.sh"), "#!/bin/sh\nexec node x.js\n").unwrap();
        let mut f = Finder { files: HashMap::new(), envs: HashMap::new() };
        assert_eq!(f.describe(&d.join("deploy.ps1")).as_deref(), Some("Deploys the API to the staging cluster"));
        assert_eq!(f.describe(&d.join("train.py")).as_deref(), Some("Train the ranking model on last week's clicks"));
        assert_eq!(f.describe(&d.join("backup.sh")).as_deref(), Some("Back up the database to S3 every night"));
        assert_eq!(f.describe(&d.join("build.bat")).as_deref(), Some("Build the installer for Windows"));
        assert_eq!(f.describe(&d.join("bare.sh")), None, "no comment, nothing made up");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn packages_say_what_their_commands_are() {
        let d = tmp("pkgs");
        // npm's shim and the package it runs
        std::fs::create_dir_all(d.join("node_modules").join("eas-cli")).unwrap();
        std::fs::write(d.join("node_modules").join("eas-cli").join("package.json"), r#"{"name":"eas-cli","description":"EAS command line tool"}"#).unwrap();
        std::fs::write(d.join("eas.cmd"), "@ECHO off\r\n\"%_prog%\"  \"%dp0%\\node_modules\\eas-cli\\bin\\run\" %*\r\n").unwrap();
        // a venv's console script and its package
        let env = d.join("venv");
        std::fs::create_dir_all(env.join("Scripts")).unwrap();
        let di = env.join("Lib").join("site-packages").join("lightrag_hku-1.4.9.dist-info");
        std::fs::create_dir_all(&di).unwrap();
        std::fs::write(di.join("entry_points.txt"), "[console_scripts]\nlightrag-server = lightrag.api.lightrag_server:main\n").unwrap();
        std::fs::write(di.join("METADATA"), "Metadata-Version: 2.1\nName: lightrag-hku\nSummary: LightRAG: Simple and Fast Retrieval-Augmented Generation\n\nLong description. Summary: not this\n").unwrap();
        std::fs::write(env.join("Scripts").join("lightrag-server.exe"), b"MZ\0\0").unwrap();
        let mut f = Finder::new(&[d.display().to_string()], &[]);
        let eas = f.find("eas", &[]).expect("found in the folder's listing");
        assert_eq!(f.describe(&eas).as_deref(), Some("EAS command line tool"));
        let lr = f.find("lightrag-server", &[]).expect("found in the folder's venv");
        assert_eq!(f.describe(&lr).as_deref(), Some("LightRAG: Simple and Fast Retrieval-Augmented Generation"));
        assert!(f.find("readme", &[]).is_none());
        // a project's own file, only for what ran in that project
        std::fs::write(d.join("server.py"), "\"\"\"Serve the dashboard on port 8000.\"\"\"\n").unwrap();
        let here = [d.display().to_string()];
        assert_eq!(f.find("server", &here), Some(d.join("server.py")));
        assert_eq!(f.find("server", &[]), None, "another folder's server.py is not this command's");
        assert_eq!(f.find("node_modules", &here), None, "a folder is no program");
        assert_eq!(f.find(r".\server.py", &here), Some(d.join("./server.py")));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn version_information() {
        // a String entry as a version resource lays it out, inside other bytes
        let mut b = vec![0xAAu8; 37];
        let start = b.len();
        let key: Vec<u8> = "FileDescription".encode_utf16().chain([0]).flat_map(|u| u.to_le_bytes()).collect();
        let val = "Transparent desktop clock";
        b.extend([0, 0, (val.len() + 1) as u8, 0, 1, 0]);
        b.extend(&key);
        while (b.len() - start) % 4 != 0 {
            b.push(0);
        }
        b.extend(val.encode_utf16().chain([0]).flat_map(|u| u.to_le_bytes()));
        b.extend([0xBB; 9]);
        assert_eq!(version_string(&b, "FileDescription").as_deref(), Some(val));
        assert_eq!(version_string(&b, "Comments"), None);
    }

    #[test]
    fn only_what_says_something() {
        assert_eq!(tidy("TransparentClock", "transparentclock"), None);
        assert_eq!(tidy("Python", "python"), None);
        assert_eq!(tidy("Copyright (c) 2020 Acme", "x"), None);
        assert_eq!(tidy("  Run the   nightly sync. Then more.", "x").as_deref(), Some("Run the nightly sync"));
    }

    #[test]
    fn activated_environments() {
        let here = vec![r"D:\code\api".to_string()];
        if cfg!(windows) {
            assert_eq!(activated(r"(Set-ExecutionPolicy -Scope Process) ; (& d:\code\api\.venv\Scripts\Activate.ps1)", &[]), vec![PathBuf::from("d:/code/api/.venv/Scripts")]);
        }
        assert_eq!(activated(r"venv\Scripts\activate", &here), vec![Path::new(r"D:\code\api").join("venv/Scripts")]);
        assert!(activated("conda activate llms", &here).is_empty());
    }
}
