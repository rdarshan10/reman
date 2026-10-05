//! reman's keys, as the user has them: a preset (standard, gentle, vim) with the user's changes on
//! top, for the shell (what opens the finder, inserts a fix, ...) and inside the finder. One map,
//! written into each shell's own key syntax by `reman init`, and read by the finder.
use anyhow::{Result, bail};
use std::collections::BTreeMap;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Layer {
    Shell,
    Finder,
}

pub struct Action {
    pub id: &'static str,
    /// a few words, for a row: `Finder, this folder`
    pub name: &'static str,
    pub layer: Layer,
    /// what it does, in a few words
    pub what: &'static str,
    pub standard: &'static [&'static str],
    pub gentle: &'static [&'static str],
    /// on or off only, always this key (Tab completes first; Enter holds a failing command)
    pub fixed: Option<&'static str>,
}

const fn shell(id: &'static str, name: &'static str, what: &'static str, standard: &'static [&'static str], gentle: &'static [&'static str]) -> Action {
    Action { id, name, layer: Layer::Shell, what, standard, gentle, fixed: None }
}
const fn finder(id: &'static str, name: &'static str, what: &'static str, keys: &'static [&'static str]) -> Action {
    Action { id, name, layer: Layer::Finder, what, standard: keys, gentle: keys, fixed: None }
}

pub const ACTIONS: &[Action] = &[
    shell("find_here", "Finder, this folder", "open the finder for this folder", &["Up"], &[]),
    shell("find_all", "Finder, every folder", "open the finder for every folder", &["Ctrl+R"], &["Ctrl+R"]),
    shell("fix", "Insert the fix", "insert the fix offered after a failure", &["Alt+F"], &["Alt+F"]),
    shell("next", "Next step", "put what you usually run next here on the prompt", &["Alt+N"], &["Alt+N"]),
    Action { id: "tab", name: "Tab opens the finder", layer: Layer::Shell, what: "Tab: complete, else the grey suggestion, else the finder", standard: &["Tab"], gentle: &[], fixed: Some("Tab") },
    Action { id: "hold", name: "Enter holds a failing one", layer: Layer::Shell, what: "Enter: hold a command that keeps failing here, once", standard: &["Enter"], gentle: &[], fixed: Some("Enter") },
    finder("runs", "Every run", "every run of the command, and what it printed", &["Ctrl+O"]),
    finder("pin", "Pin", "pin the command: pinned ones rank first", &["Ctrl+P"]),
    finder("forget", "Forget", "forget the command everywhere (pressed twice)", &["Delete"]),
    finder("fold", "Fold variants", "fold variants of a command, or show each", &["Ctrl+G"]),
    finder("who", "Who ran it", "who ran it: you, you and agents, agents", &["F3"]),
    finder("outcome", "Outcome", "outcome: any, worked, failed", &["F2"]),
    finder("next_tab", "Next tab", "next tab: Recall, Fixes, Flows", &["Tab", "Ctrl+T"]),
    finder("prev_tab", "Previous tab", "previous tab", &["Shift+Tab"]),
    finder("all_steps", "All steps", "a flow: insert every step as one line", &["Ctrl+A"]),
    finder("stop_flow", "Stop the flow", "stop the flow in progress", &["Ctrl+X"]),
    finder("clear", "Clear the query", "clear the query", &["Ctrl+U"]),
    finder("delete_word", "Delete a word", "delete a word of the query", &["Ctrl+W"]),
    finder("help", "Every key", "show every key", &["F1"]),
    finder("settings", "Settings", "open the settings", &["F10"]),
];

pub const PRESETS: &[(&str, &str)] = &[
    ("standard", "↑, Ctrl+R, Tab, Alt+F, Alt+N and Enter, as reman has always had them"),
    ("gentle", "reman leaves ↑, Tab and Enter as they were: Ctrl+R, Alt+F and Alt+N only"),
    ("vim", "the standard keys, and vim's normal mode in the finder"),
];

pub fn action(id: &str) -> Option<&'static Action> {
    ACTIONS.iter().find(|a| a.id == id)
}

// ---------------------------------------------------------------------------------------------
// one key
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Code {
    /// a letter (lowercase), a digit or a punctuation mark
    Char(char),
    F(u8),
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    Backspace,
    Tab,
    Enter,
    Space,
    Esc,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Key {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub code: Code,
}

impl Key {
    /// `Ctrl+R`, `alt+f`, `Up`, `Shift+Tab`, `F3`, `Ctrl+Alt+Space`.
    pub fn parse(s: &str) -> Result<Key> {
        let parts: Vec<&str> = s.split('+').map(str::trim).collect();
        // `Ctrl++` is Ctrl and the plus key
        let (mods, name) = match parts.as_slice() {
            [m @ .., "", ""] => (m, "+"),
            [m @ .., n] => (m, *n),
            [] => bail!("no key"),
        };
        let mut k = Key { ctrl: false, alt: false, shift: false, code: Code::Space };
        for m in mods {
            match m.to_lowercase().as_str() {
                "ctrl" | "control" | "c" => k.ctrl = true,
                "alt" | "meta" | "option" | "opt" | "m" => k.alt = true,
                "shift" | "s" => k.shift = true,
                other => bail!("`{other}` isn't a modifier (Ctrl, Alt, Shift)"),
            }
        }
        let low = name.to_lowercase();
        k.code = match low.as_str() {
            "up" | "uparrow" => Code::Up,
            "down" | "downarrow" => Code::Down,
            "left" | "leftarrow" => Code::Left,
            "right" | "rightarrow" => Code::Right,
            "home" => Code::Home,
            "end" => Code::End,
            "pageup" | "pgup" => Code::PageUp,
            "pagedown" | "pgdn" | "pgdown" => Code::PageDown,
            "insert" | "ins" => Code::Insert,
            "delete" | "del" => Code::Delete,
            "backspace" | "bs" => Code::Backspace,
            "tab" => Code::Tab,
            "enter" | "return" => Code::Enter,
            "space" | "spacebar" => Code::Space,
            "esc" | "escape" => Code::Esc,
            f if f.len() >= 2 && f.starts_with('f') && f[1..].parse::<u8>().is_ok_and(|n| (1..=12).contains(&n)) => Code::F(f[1..].parse().unwrap_or(1)),
            c if c.chars().count() == 1 => {
                // `Ctrl+R` and `ctrl+r` are one key: Shift counts when it's written out
                let ch = name.chars().next().unwrap_or(' ');
                if !(ch.is_ascii_alphanumeric() || ch.is_ascii_punctuation()) {
                    bail!("`{name}`: only letters, digits and punctuation on a key");
                }
                Code::Char(ch.to_ascii_lowercase())
            }
            _ => bail!("`{name}` isn't a key reman knows (letters, digits, F1 to F12, Up, Down, Left, Right, Home, End, PageUp, PageDown, Insert, Delete, Backspace, Tab, Enter, Space, Esc)"),
        };
        Ok(k)
    }

    /// As people write it: `Ctrl+R`, `Alt+F`, `Shift+Tab`, `↑`.
    pub fn label(&self) -> String {
        let mut s = String::new();
        if self.ctrl {
            s.push_str("Ctrl+");
        }
        if self.alt {
            s.push_str("Alt+");
        }
        if self.shift {
            s.push_str("Shift+");
        }
        s.push_str(&match self.code {
            Code::Char(c) => c.to_ascii_uppercase().to_string(),
            Code::F(n) => format!("F{n}"),
            Code::Up => "Up".into(),
            Code::Down => "Down".into(),
            Code::Left => "Left".into(),
            Code::Right => "Right".into(),
            Code::Home => "Home".into(),
            Code::End => "End".into(),
            Code::PageUp => "PageUp".into(),
            Code::PageDown => "PageDown".into(),
            Code::Insert => "Insert".into(),
            Code::Delete => "Delete".into(),
            Code::Backspace => "Backspace".into(),
            Code::Tab => "Tab".into(),
            Code::Enter => "Enter".into(),
            Code::Space => "Space".into(),
            Code::Esc => "Esc".into(),
        });
        s
    }

    /// Short, for the finder's hint line: `^O`, `F3`, `Del`, `Alt+F`.
    pub fn short(&self) -> String {
        match (self.ctrl, self.alt, self.shift, self.code) {
            (true, false, false, Code::Char(c)) => format!("^{}", c.to_ascii_uppercase()),
            (false, false, false, Code::Delete) => "Del".into(),
            (false, false, false, Code::Up) => "↑".into(),
            _ => self.label(),
        }
    }

    /// A plain key that types something (a letter, a digit, Space): never an action's key.
    pub fn types(&self) -> bool {
        !self.ctrl && !self.alt && matches!(self.code, Code::Char(_) | Code::Space)
    }

    /// The key as a crossterm event reports it (the finder's own keys).
    pub fn matches(&self, k: &ratatui::crossterm::event::KeyEvent) -> bool {
        use ratatui::crossterm::event::{KeyCode as C, KeyModifiers as M};
        let ctrl = k.modifiers.contains(M::CONTROL);
        let alt = k.modifiers.contains(M::ALT);
        let shift = k.modifiers.contains(M::SHIFT);
        let (code, shift) = match k.code {
            C::Char(c) if c == ' ' => (Code::Space, shift),
            // an uppercase letter is a shifted one
            C::Char(c) => (Code::Char(c.to_ascii_lowercase()), shift || c.is_ascii_uppercase()),
            C::F(n) => (Code::F(n), shift),
            C::Up => (Code::Up, shift),
            C::Down => (Code::Down, shift),
            C::Left => (Code::Left, shift),
            C::Right => (Code::Right, shift),
            C::Home => (Code::Home, shift),
            C::End => (Code::End, shift),
            C::PageUp => (Code::PageUp, shift),
            C::PageDown => (Code::PageDown, shift),
            C::Insert => (Code::Insert, shift),
            C::Delete => (Code::Delete, shift),
            C::Backspace => (Code::Backspace, shift),
            C::Tab => (Code::Tab, shift),
            C::BackTab => (Code::Tab, true),
            C::Enter => (Code::Enter, shift),
            C::Esc => (Code::Esc, shift),
            _ => return false,
        };
        // punctuation: the shift that made it is part of the character
        let shift_matters = !matches!(code, Code::Char(c) if !c.is_ascii_alphabetic());
        code == self.code && ctrl == self.ctrl && alt == self.alt && (!shift_matters || shift == self.shift)
    }

    /// From a key pressed in `reman settings` (None for a key that can't be one).
    pub fn from_event(k: &ratatui::crossterm::event::KeyEvent) -> Option<Key> {
        use ratatui::crossterm::event::{KeyCode as C, KeyModifiers as M};
        let mut key = Key { ctrl: k.modifiers.contains(M::CONTROL), alt: k.modifiers.contains(M::ALT), shift: k.modifiers.contains(M::SHIFT), code: Code::Space };
        key.code = match k.code {
            C::Char(' ') => Code::Space,
            C::Char(c) if c.is_ascii_uppercase() => {
                key.shift = true;
                Code::Char(c.to_ascii_lowercase())
            }
            C::Char(c) if c.is_ascii_alphanumeric() || c.is_ascii_punctuation() => Code::Char(c),
            // a terminal reports Ctrl+letter as the control character on some systems
            C::Char(c) if (c as u32) >= 1 && (c as u32) <= 26 => {
                key.ctrl = true;
                Code::Char((b'a' + c as u8 - 1) as char)
            }
            C::F(n) if (1..=12).contains(&n) => Code::F(n),
            C::Up => Code::Up,
            C::Down => Code::Down,
            C::Left => Code::Left,
            C::Right => Code::Right,
            C::Home => Code::Home,
            C::End => Code::End,
            C::PageUp => Code::PageUp,
            C::PageDown => Code::PageDown,
            C::Insert => Code::Insert,
            C::Delete => Code::Delete,
            C::Backspace => Code::Backspace,
            C::Tab => Code::Tab,
            C::BackTab => {
                key.shift = true;
                Code::Tab
            }
            C::Enter => Code::Enter,
            _ => return None,
        };
        Some(key)
    }
}

// ---------------------------------------------------------------------------------------------
// the map
// ---------------------------------------------------------------------------------------------

/// Every action's keys, as the user has them now (an action with none is off).
#[derive(Clone, Debug)]
pub struct Map {
    pub preset: String,
    keys: BTreeMap<&'static str, Vec<Key>>,
}

impl Map {
    /// The preset's keys with the user's changes (`keys` in config.json: action -> keys, [] = off).
    pub fn of(preset: Option<&str>, changes: &BTreeMap<String, Vec<String>>) -> Map {
        let preset = preset.filter(|p| PRESETS.iter().any(|x| x.0 == *p)).unwrap_or("standard").to_string();
        let mut keys = BTreeMap::new();
        for a in ACTIONS {
            let base = if preset == "gentle" { a.gentle } else { a.standard };
            let chosen: Vec<Key> = match changes.get(a.id) {
                Some(list) => list.iter().filter_map(|s| Key::parse(s).ok()).collect(),
                None => base.iter().filter_map(|s| Key::parse(s).ok()).collect(),
            };
            // a fixed action is that key or nothing
            let chosen = match a.fixed {
                Some(f) => {
                    let f = Key::parse(f).ok();
                    if chosen.is_empty() { vec![] } else { f.into_iter().collect() }
                }
                None => chosen,
            };
            keys.insert(a.id, chosen);
        }
        Map { preset, keys }
    }

    /// From the settings file.
    pub fn load() -> Map {
        let st = crate::settings::load();
        Map::of(st.key_preset.as_deref(), &st.keys)
    }

    pub fn get(&self, id: &str) -> &[Key] {
        self.keys.get(id).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn on(&self, id: &str) -> bool {
        !self.get(id).is_empty()
    }

    /// The finder action a pressed key is.
    pub fn finder_action(&self, k: &ratatui::crossterm::event::KeyEvent) -> Option<&'static str> {
        ACTIONS.iter().filter(|a| a.layer == Layer::Finder).find(|a| self.get(a.id).iter().any(|x| x.matches(k))).map(|a| a.id)
    }

    /// The first key of an action, short (`^O`), or None when it's off.
    pub fn short(&self, id: &str) -> Option<String> {
        self.get(id).first().map(Key::short)
    }

    pub fn label(&self, id: &str) -> Option<String> {
        self.get(id).first().map(Key::label)
    }
}

/// Why a key can't (or shouldn't) be an action's: None when it's fine; (refused, why).
pub fn check(map: &Map, id: &str, key: &Key) -> Option<(bool, String)> {
    let a = action(id)?;
    if let Some(f) = a.fixed {
        return (key.label() != f).then(|| (true, format!("this one is always {f}: Enter turns it on or off")));
    }
    if key.types() {
        return Some((true, format!("{} types a character: add Ctrl or Alt", key.label())));
    }
    let l = key.label();
    let core = |k: &str| l.eq_ignore_ascii_case(k);
    match a.layer {
        Layer::Shell => {
            if ["Ctrl+C", "Ctrl+D", "Enter", "Tab", "Esc", "Backspace", "Left", "Right", "Home", "End", "Ctrl+Z"].iter().any(|k| core(k)) {
                return Some((true, format!("{l} is one your shell needs for itself")));
            }
        }
        Layer::Finder => {
            if ["Ctrl+C", "Ctrl+D", "Enter", "Esc", "Backspace", "Up", "Down", "PageUp", "PageDown", "Left", "Right"].iter().any(|k| core(k)) {
                return Some((true, format!("{l} moves, picks or closes in the finder")));
            }
        }
    }
    // the same key for two actions of one layer
    if let Some(o) = ACTIONS.iter().find(|o| o.id != id && o.layer == a.layer && map.get(o.id).contains(key)) {
        return Some((true, format!("{l} is already the key for {}: change that one first", o.name)));
    }
    // taken before reman ever sees it, in some places
    let terminal: &[(&str, &str)] = &[
        ("Alt+Enter", "Windows Terminal: full screen"),
        ("F11", "Windows Terminal: full screen"),
        ("Ctrl+Tab", "Windows Terminal: next tab"),
        ("Ctrl+Shift+Tab", "Windows Terminal: previous tab"),
        ("Ctrl+Shift+T", "Windows Terminal: new tab"),
        ("Ctrl+Shift+W", "Windows Terminal: close the pane"),
        ("Ctrl+Shift+F", "Windows Terminal: find"),
        ("Ctrl+Shift+P", "Windows Terminal and VS Code: the command palette"),
        ("Ctrl+P", "VS Code's terminal may open Quick Open instead"),
        ("Ctrl+O", "VS Code's terminal may open a file instead"),
        ("Ctrl+G", "VS Code's terminal may go to a line instead"),
        ("Ctrl+B", "VS Code's terminal may toggle the sidebar instead"),
        ("Ctrl+J", "VS Code's terminal may toggle the panel instead"),
        ("Ctrl+K", "VS Code's terminal may wait for a second key instead"),
        ("F1", "VS Code's terminal may open its command palette instead"),
    ];
    if let Some((_, why)) = terminal.iter().find(|(k, _)| core(k)) {
        return Some((false, (*why).to_string()));
    }
    let default = a.standard.iter().any(|d| d.eq_ignore_ascii_case(&l));
    let history: &[(&str, &str)] = &[
        ("Up", "goes back through history in your shell"),
        ("Down", "goes forward through history in your shell"),
        ("PageUp", "scrolls or searches history in your shell"),
        ("PageDown", "scrolls or searches history in your shell"),
        ("Ctrl+P", "goes back through history in bash, zsh and fish"),
        ("Ctrl+N", "goes forward through history in bash, zsh and fish"),
        ("Ctrl+S", "searches history forward in bash and zsh"),
    ];
    if let Some((_, does)) = history.iter().find(|(k, _)| core(k)).filter(|_| a.layer == Layer::Shell && !default) {
        return Some((false, format!("{l} no longer {does}")));
    }
    if a.layer == Layer::Shell && ["Ctrl+A", "Ctrl+E", "Ctrl+K", "Ctrl+U", "Ctrl+W", "Ctrl+Y", "Ctrl+L", "Alt+B", "Alt+D"].iter().any(|k| core(k)) {
        return Some((false, format!("{l} edits the line in most shells; it won't any more")));
    }
    None
}

// ---------------------------------------------------------------------------------------------
// in each shell's words
// ---------------------------------------------------------------------------------------------

/// xterm's modifier number: 1 + shift + 2 alt + 4 ctrl.
fn xmod(k: &Key) -> u8 {
    1 + k.shift as u8 + 2 * k.alt as u8 + 4 * k.ctrl as u8
}

/// The bytes a terminal sends for the key, readline-escaped (`\C-r`, `\ef`, `\e[A`). None for one
/// a terminal has no plain way to send.
pub fn readline(k: &Key) -> Option<String> {
    let m = xmod(k);
    let csi = |end: &str| if m == 1 { format!("\\e[{end}") } else { format!("\\e[1;{m}{end}") };
    let tilde = |n: u8| if m == 1 { format!("\\e[{n}~") } else { format!("\\e[{n};{m}~") };
    Some(match k.code {
        Code::Char(c) => {
            if k.shift && c.is_ascii_alphabetic() && k.ctrl {
                return None; // Ctrl+Shift+letter: the same byte as Ctrl+letter
            }
            let c = if k.shift && c.is_ascii_alphabetic() { c.to_ascii_uppercase() } else { c };
            let base = if k.ctrl {
                if !c.is_ascii_alphabetic() {
                    return None;
                }
                format!("\\C-{c}")
            } else {
                match c {
                    '\\' => "\\\\".into(),
                    '"' => "\\\"".into(),
                    c => c.to_string(),
                }
            };
            if k.alt { format!("\\e{base}") } else { base }
        }
        Code::Space if k.ctrl => "\\C-@".into(),
        Code::Space if k.alt => "\\e ".into(),
        Code::Up => csi("A"),
        Code::Down => csi("B"),
        Code::Right => csi("C"),
        Code::Left => csi("D"),
        Code::Home => csi("H"),
        Code::End => csi("F"),
        Code::Insert => tilde(2),
        Code::Delete => tilde(3),
        Code::PageUp => tilde(5),
        Code::PageDown => tilde(6),
        Code::F(n @ 1..=4) => {
            let c = ["P", "Q", "R", "S"][n as usize - 1];
            if m == 1 { format!("\\eO{c}") } else { format!("\\e[1;{m}{c}") }
        }
        Code::F(n) => tilde([0, 0, 0, 0, 0, 15, 17, 18, 19, 20, 21, 23, 24][n as usize]),
        Code::Tab if k.shift => "\\e[Z".into(),
        Code::Tab => "\\t".into(),
        Code::Enter => "\\r".into(),
        Code::Backspace => "\\C-?".into(),
        Code::Esc | Code::Space => return None,
    })
}

/// zsh's bindkey form (`^R`, `^[f`, `^[[A`), and the other form an arrow or Home/End may come in.
pub fn zsh(k: &Key) -> Vec<String> {
    let Some(r) = readline(k) else { return vec![] };
    let mut s = String::new();
    let mut it = r.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\\' {
            s.push(c);
            continue;
        }
        match it.next() {
            Some('e') => s.push_str("^["),
            Some('t') => s.push_str("^I"),
            Some('r') => s.push_str("^M"),
            Some('C') => {
                it.next(); // '-'
                match it.next() {
                    Some('?') => s.push_str("^?"),
                    Some('@') => s.push_str("^@"),
                    Some(x) => {
                        s.push('^');
                        s.push(x.to_ascii_uppercase());
                    }
                    None => {}
                }
            }
            Some(x) => s.push(x),
            None => {}
        }
    }
    let mut out = vec![s.clone()];
    // unmodified arrows, Home and End also arrive as ESC O x (application mode)
    if xmod(k) == 1 {
        if let Some(rest) = s.strip_prefix("^[[").filter(|r| matches!(*r, "A" | "B" | "C" | "D" | "H" | "F")) {
            out.push(format!("^[O{rest}"));
        }
    }
    out
}

/// fish 3's form (`\cr`, `\ef`, `\e\[A`).
pub fn fish(k: &Key) -> Option<String> {
    // Ctrl+Space sends NUL, which fish only takes by its terminfo name
    if k.code == Code::Space && k.ctrl && !k.alt {
        return Some("-k nul".into());
    }
    let r = readline(k)?;
    Some(r.replace("\\C-", "\\c").replace("\\e[", "\\e\\[").replace('~', "\\~"))
}

/// PSReadLine's chord (`Ctrl+r`, `Alt+f`, `UpArrow`, `Shift+Tab`, `F3`).
pub fn powershell(k: &Key) -> String {
    let mut s = String::new();
    if k.ctrl {
        s.push_str("Ctrl+");
    }
    if k.alt {
        s.push_str("Alt+");
    }
    let shifted_letter = matches!(k.code, Code::Char(c) if c.is_ascii_alphabetic()) && k.shift;
    if k.shift && !shifted_letter {
        s.push_str("Shift+");
    }
    s.push_str(&match k.code {
        Code::Char(c) if shifted_letter => c.to_ascii_uppercase().to_string(),
        Code::Char(c) => c.to_string(),
        Code::F(n) => format!("F{n}"),
        Code::Up => "UpArrow".into(),
        Code::Down => "DownArrow".into(),
        Code::Left => "LeftArrow".into(),
        Code::Right => "RightArrow".into(),
        Code::Home => "Home".into(),
        Code::End => "End".into(),
        Code::PageUp => "PageUp".into(),
        Code::PageDown => "PageDown".into(),
        Code::Insert => "Insert".into(),
        Code::Delete => "Delete".into(),
        Code::Backspace => "Backspace".into(),
        Code::Tab => "Tab".into(),
        Code::Enter => "Enter".into(),
        Code::Space => "Spacebar".into(),
        Code::Esc => "Escape".into(),
    });
    s
}

/// nushell's keybinding record fields: (modifier, keycode).
pub fn nu(k: &Key) -> Option<(String, String)> {
    let mut m: Vec<&str> = Vec::new();
    if k.ctrl {
        m.push("control");
    }
    if k.alt {
        m.push("alt");
    }
    let shifted_tab = k.code == Code::Tab && k.shift;
    if k.shift && !shifted_tab {
        m.push("shift");
    }
    let modifier = if m.is_empty() { "none".to_string() } else { m.join("_") };
    let code = match k.code {
        Code::Char(c) => format!("char_{c}"),
        Code::F(n) => format!("f{n}"),
        Code::Up => "up".into(),
        Code::Down => "down".into(),
        Code::Left => "left".into(),
        Code::Right => "right".into(),
        Code::Home => "home".into(),
        Code::End => "end".into(),
        Code::PageUp => "pageup".into(),
        Code::PageDown => "pagedown".into(),
        Code::Insert => "insert".into(),
        Code::Delete => "delete".into(),
        Code::Backspace => "backspace".into(),
        Code::Tab if shifted_tab => "backtab".into(),
        Code::Tab => "tab".into(),
        Code::Enter => "enter".into(),
        Code::Space => "space".into(),
        Code::Esc => "esc".into(),
    };
    Some((modifier, code))
}

/// prompt_toolkit's key names, as a Python tuple's items (`"c-r"`, `"escape", "f"`).
pub fn ptk(k: &Key) -> Option<String> {
    let base = match k.code {
        Code::Char(c) if k.ctrl && c.is_ascii_alphabetic() => format!("c-{c}"),
        Code::Char(_) if k.ctrl => return None,
        Code::Char(c) => if k.shift && c.is_ascii_alphabetic() { c.to_ascii_uppercase().to_string() } else { c.to_string() },
        Code::F(n) => format!("{}f{n}", if k.ctrl { "c-" } else if k.shift { "s-" } else { "" }),
        Code::Tab if k.shift => "s-tab".into(),
        Code::Tab => "tab".into(),
        Code::Enter => "enter".into(),
        Code::Space if k.ctrl => "c-space".into(),
        Code::Space => "space".into(),
        Code::Esc => "escape".into(),
        Code::Backspace => "backspace".into(),
        other => {
            let n = match other {
                Code::Up => "up",
                Code::Down => "down",
                Code::Left => "left",
                Code::Right => "right",
                Code::Home => "home",
                Code::End => "end",
                Code::PageUp => "pageup",
                Code::PageDown => "pagedown",
                Code::Insert => "insert",
                _ => "delete",
            };
            format!("{}{n}", if k.ctrl && k.shift { "c-s-" } else if k.ctrl { "c-" } else if k.shift { "s-" } else { "" })
        }
    };
    Some(if k.alt { format!("\"escape\", \"{base}\"") } else { format!("\"{base}\"") })
}

/// The shell lines that bind the map's shell keys, for `reman init <shell>` (`__KEYS__`).
pub fn bindings(shell: &str, map: &Map) -> String {
    // (action, the shell's function for it)
    let fns: &[(&str, &str)] = match shell {
        "powershell" | "pwsh" => &[("find_here", "FindHere"), ("find_all", "FindAll"), ("tab", "Tab"), ("fix", "Fix"), ("next", "Next"), ("hold", "Hold")],
        "zsh" => &[("find_here", "__reman_find_here"), ("find_all", "__reman_find_all"), ("tab", "__reman_tab"), ("fix", "__reman_insert_fix"), ("next", "__reman_nextup"), ("hold", "__reman_accept")],
        "bash" | "cmd" | "clink" => &[("find_here", "__reman_find folder"), ("find_all", "__reman_find all"), ("fix", "__reman_insert_fix"), ("next", "__reman_nextup")],
        "fish" => &[("find_here", "__reman_find folder"), ("find_all", "__reman_find all"), ("tab", "__reman_tab"), ("fix", "__reman_insert_fix"), ("next", "__reman_nextup"), ("hold", "__reman_enter")],
        "nu" | "nushell" => &[("find_here", "__reman_find folder"), ("find_all", "__reman_find all"), ("fix", "__reman_insert_fix"), ("next", "__reman_nextup")],
        "xonsh" => &[("find_here", "_here"), ("find_all", "_all"), ("fix", "_fix"), ("next", "_next")],
        _ => &[],
    };
    let mut out: Vec<String> = Vec::new();
    match shell {
        "powershell" | "pwsh" => {
            let pairs: Vec<String> = fns.iter().flat_map(|(a, f)| map.get(a).iter().map(move |k| format!("@('{}', '{f}')", powershell(k)))).collect();
            out.push(format!("$global:__RemanFixHint = '{}'; $global:__RemanNextKey = '{}'", fix_hint(map).replace('\'', "''"), next_key(map).replace('\'', "''")));
            out.push(format!("__RemanBindKeys @({})", pairs.join(", ")));
        }
        "zsh" => {
            out.push(format!("__reman_fix_hint={}", sq(&fix_hint(map))));
            // the keys reman had before are given back first (an open shell taking a change)
            out.push("__reman_unbind_all".into());
            for (a, f) in fns {
                for k in map.get(a) {
                    for seq in zsh(k) {
                        // what the key did before (Alt-F's forward-word, Alt-N's history search), for when there's nothing to do
                        out.push(format!("__reman_bind {} {f}", sq(&seq)));
                    }
                }
            }
        }
        "bash" => {
            out.push(format!("__reman_fix_hint={}", sq(&fix_hint(map))));
            out.push("__reman_unbind_all".into());
            for (a, f) in fns {
                for k in map.get(a) {
                    if let Some(seq) = readline(k) {
                        out.push(format!("__reman_bindx {} {}", sq(&seq), sq(f)));
                    }
                }
            }
            if map.on("tab") {
                out.push("complete -o nospace -E -F __reman_tab_empty 2>/dev/null && __reman_tab_on=1".into());
            }
        }
        "cmd" | "clink" => {
            let names = [("find_here", "reman_find_folder"), ("find_all", "reman_find_all"), ("fix", "reman_insert_fix"), ("next", "reman_nextup")];
            for (a, f) in names {
                for k in map.get(a) {
                    if let Some(seq) = readline(k) {
                        out.push(format!("rl.setbinding([[\"{seq}\"]], [[\"luafunc:{f}\"]])"));
                    }
                }
            }
        }
        "fish" => {
            out.push(format!("set -g __reman_fix_hint {}", sq(&fix_hint(map))));
            out.push("__reman_unbind_all".into());
            for (a, f) in fns {
                for k in map.get(a) {
                    if let Some(seq) = fish(k) {
                        out.push(format!("__reman_bindf {seq} '{f}'"));
                    }
                }
            }
        }
        "nu" | "nushell" => {
            out.push("$env.config.keybindings = ($env.config.keybindings? | default [] | append [".into());
            for (a, f) in fns {
                for k in map.get(a) {
                    if let Some((m, c)) = nu(k) {
                        // Up keeps moving through an open menu
                        let event = if *a == "find_here" && c == "up" && m == "none" {
                            format!("{{until: [{{send: menuup}}, {{send: executehostcommand, cmd: \"{f}\"}}]}}")
                        } else {
                            format!("{{send: executehostcommand, cmd: \"{f}\"}}")
                        };
                        out.push(format!("  {{name: reman_{a}, modifier: {m}, keycode: {c}, mode: [emacs, vi_insert, vi_normal], event: {event}}}"));
                    }
                }
            }
            out.push("])".into());
        }
        "xonsh" => {
            for (a, f) in fns {
                for k in map.get(a) {
                    if let Some(keys) = ptk(k) {
                        // Up opens the finder on one line with no menu open; elsewhere Up as usual
                        let filt = if *a == "find_here" && keys == "\"up\"" { ", filter=one_line" } else { "" };
                        out.push(format!("    bindings.add({keys}{filt})({f})"));
                    }
                }
            }
        }
        _ => {}
    }
    out.join("\n")
}

/// A word for a POSIX shell, single-quoted.
fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// What a message about the fix key says: `Alt+F inserts`, or how to get it without a key.
/// What a message names the next-step key as.
pub fn next_key(map: &Map) -> String {
    map.label("next").unwrap_or_else(|| "it".into())
}

pub fn fix_hint(map: &Map) -> String {
    match map.label("fix") {
        Some(l) => format!("{l} inserts"),
        None => "reman fixes lists it".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(s: &str) -> Key {
        Key::parse(s).unwrap()
    }

    #[test]
    fn parses_and_labels() {
        assert_eq!(k("ctrl+r").label(), "Ctrl+R");
        assert_eq!(k("Alt+f").label(), "Alt+F");
        assert_eq!(k("UpArrow").label(), "Up");
        assert_eq!(k("Shift+Tab").label(), "Shift+Tab");
        assert_eq!(k("f3").label(), "F3");
        assert_eq!(k("Ctrl+O").short(), "^O");
        assert!(Key::parse("Hyper+x").is_err());
        assert!(Key::parse("Ctrl+F13").is_err());
        assert!(k("x").types() && !k("Ctrl+x").types());
    }

    #[test]
    fn in_each_shells_words() {
        assert_eq!(readline(&k("Ctrl+R")).unwrap(), "\\C-r");
        assert_eq!(readline(&k("Alt+F")).unwrap(), "\\ef");
        assert_eq!(readline(&k("Up")).unwrap(), "\\e[A");
        assert_eq!(readline(&k("Ctrl+Up")).unwrap(), "\\e[1;5A");
        assert_eq!(readline(&k("F5")).unwrap(), "\\e[15~");
        assert_eq!(readline(&k("Shift+Tab")).unwrap(), "\\e[Z");
        assert_eq!(zsh(&k("Ctrl+R")), ["^R"]);
        assert_eq!(zsh(&k("Alt+N")), ["^[n"]);
        assert_eq!(zsh(&k("Up")), ["^[[A", "^[OA"]);
        assert_eq!(fish(&k("Up")).unwrap(), "\\e\\[A");
        assert_eq!(fish(&k("Ctrl+Space")).unwrap(), "-k nul");
        assert_eq!(fish(&k("F5")).unwrap(), "\\e\\[15\\~");
        assert_eq!(powershell(&k("Ctrl+r")), "Ctrl+r");
        assert_eq!(powershell(&k("Up")), "UpArrow");
        assert_eq!(nu(&k("Ctrl+Alt+R")).unwrap(), ("control_alt".into(), "char_r".into()));
        assert_eq!(nu(&k("Shift+Tab")).unwrap(), ("none".into(), "backtab".into()));
        assert_eq!(ptk(&k("Alt+F")).unwrap(), "\"escape\", \"f\"");
        assert_eq!(ptk(&k("Ctrl+R")).unwrap(), "\"c-r\"");
    }

    #[test]
    fn presets_and_changes() {
        let none = BTreeMap::new();
        let std = Map::of(None, &none);
        assert_eq!(std.label("find_here").as_deref(), Some("Up"));
        assert!(std.on("tab") && std.on("hold"));
        let gentle = Map::of(Some("gentle"), &none);
        assert!(!gentle.on("find_here") && !gentle.on("tab") && !gentle.on("hold") && gentle.on("find_all"));
        let mut ch = BTreeMap::new();
        ch.insert("find_here".to_string(), vec!["Ctrl+Space".to_string()]);
        ch.insert("runs".to_string(), vec![]);
        ch.insert("tab".to_string(), vec!["F9".to_string()]);
        let m = Map::of(None, &ch);
        assert_eq!(m.label("find_here").as_deref(), Some("Ctrl+Space"));
        assert!(!m.on("runs"), "an empty list is off");
        assert_eq!(m.label("tab").as_deref(), Some("Tab"), "a fixed action is its key or nothing");
    }

    #[test]
    fn refuses_and_warns() {
        let m = Map::of(None, &BTreeMap::new());
        assert!(check(&m, "find_all", &k("x")).unwrap().0, "a typing key");
        assert!(check(&m, "find_all", &k("Ctrl+C")).unwrap().0, "the shell's own");
        assert!(check(&m, "runs", &k("Ctrl+P")).unwrap().0, "taken by pin");
        assert!(check(&m, "runs", &k("Esc")).unwrap().0);
        let (refused, why) = check(&m, "find_all", &k("Ctrl+K")).unwrap();
        assert!(!refused && why.contains("VS Code"), "{why}");
        assert!(check(&m, "find_all", &k("Ctrl+Space")).is_none());
        // the shell's own history keys: taken, with what they did; the preset's Up says nothing
        let (refused, why) = check(&m, "find_here", &k("Down")).unwrap();
        assert!(!refused && why == "Down no longer goes forward through history in your shell", "{why}");
        assert!(check(&m, "find_here", &k("Up")).is_none());
    }

    #[test]
    fn writes_each_shells_bindings() {
        let m = Map::of(None, &BTreeMap::new());
        let ps = bindings("powershell", &m);
        assert!(ps.contains("@('UpArrow', 'FindHere')") && ps.contains("@('Enter', 'Hold')"), "{ps}");
        let z = bindings("zsh", &m);
        assert!(z.contains("__reman_fix_hint='Alt+F inserts'\n__reman_unbind_all") &&z.contains("__reman_bind '^[[A' __reman_find_here") && z.contains("__reman_bind '^[OA' __reman_find_here"), "{z}");
        let b = bindings("bash", &Map::of(Some("gentle"), &BTreeMap::new()));
        assert!(!b.contains("\\e[A") && !b.contains("complete -o nospace -E") && b.contains("__reman_bindx '\\C-r' '__reman_find all'"), "{b}");
        assert_eq!(sq("a'b"), "'a'\\''b'");
        let n = bindings("nu", &m);
        assert!(n.contains("keycode: up") && n.contains("menuup"), "{n}");
        let x = bindings("xonsh", &m);
        assert!(x.contains("bindings.add(\"up\", filter=one_line)(_here)") && x.contains("bindings.add(\"escape\", \"f\")(_fix)"), "{x}");
    }
}
