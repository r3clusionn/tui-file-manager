//! Keys, actions, the configuration file and bookmarks.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::entry::{SortKey, SortOrder};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    Char(char),
    Ctrl(char),
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
    Enter,
    Esc,
    Backspace,
    Delete,
    Tab,
    F(u8),
}

/// Parses a key name as used by `--keys` and the `[keys]` table: a single character, or `esc`,
/// `enter`, `tab`, `space`, `comma`, `up`, `down`, `left`, `right`, `pgup`, `pgdn`, `home`,
/// `end`, `backspace`, `delete`, `f1` to `f12`, or `ctrl-x`.
pub fn parse_key(s: &str) -> Option<Key> {
    let mut chars = s.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return Some(Key::Char(c));
    }
    let t = s.trim();
    let lower = t.to_ascii_lowercase();
    if let Some(c) = lower.strip_prefix("ctrl-") {
        let mut cs = c.chars();
        if let (Some(c), None) = (cs.next(), cs.next()) {
            return Some(Key::Ctrl(c));
        }
    }
    if let Some(n) = lower.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
        if (1..=12).contains(&n) {
            return Some(Key::F(n));
        }
    }
    Some(match lower.as_str() {
        "up" => Key::Up,
        "down" => Key::Down,
        "left" => Key::Left,
        "right" => Key::Right,
        "pageup" | "pgup" => Key::PageUp,
        "pagedown" | "pgdn" => Key::PageDown,
        "home" => Key::Home,
        "end" => Key::End,
        "enter" | "return" => Key::Enter,
        "esc" | "escape" => Key::Esc,
        "backspace" | "bs" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "tab" => Key::Tab,
        "space" => Key::Char(' '),
        "comma" => Key::Char(','),
        _ => return None,
    })
}

pub fn key_name(k: Key) -> String {
    match k {
        Key::Char(' ') => "space".into(),
        Key::Char(c) => c.to_string(),
        Key::Ctrl(c) => format!("ctrl-{c}"),
        Key::Up => "up".into(),
        Key::Down => "down".into(),
        Key::Left => "left".into(),
        Key::Right => "right".into(),
        Key::PageUp => "pgup".into(),
        Key::PageDown => "pgdn".into(),
        Key::Home => "home".into(),
        Key::End => "end".into(),
        Key::Enter => "enter".into(),
        Key::Esc => "esc".into(),
        Key::Backspace => "backspace".into(),
        Key::Delete => "delete".into(),
        Key::Tab => "tab".into(),
        Key::F(n) => format!("f{n}"),
    }
}

macro_rules! actions {
    ($($v:ident $name:literal $help:literal [$($key:literal),*];)*) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum Action { $($v,)* }

        impl Action {
            pub const ALL: &'static [Action] = &[$(Action::$v,)*];

            pub fn name(self) -> &'static str {
                match self { $(Action::$v => $name,)* }
            }

            pub fn help(self) -> &'static str {
                match self { $(Action::$v => $help,)* }
            }

            pub fn parse(s: &str) -> Option<Action> {
                match s { $($name => Some(Action::$v),)* _ => None }
            }

            fn default_keys(self) -> &'static [&'static str] {
                match self { $(Action::$v => &[$($key),*],)* }
            }
        }
    };
}

actions! {
    Down "down" "Move down" ["j", "down"];
    Up "up" "Move up" ["k", "up"];
    Parent "parent" "Go to the parent folder" ["h", "left", "backspace"];
    Open "open" "Enter a folder, or open a file with its program" ["l", "right", "enter"];
    Top "top" "First entry" ["g", "home"];
    Bottom "bottom" "Last entry" ["G", "end"];
    PageDown "page_down" "Page down" ["pgdn", "ctrl-d"];
    PageUp "page_up" "Page up" ["pgup", "ctrl-u"];
    Back "back" "Previous folder" ["-"];
    Home "home" "Home folder" ["~"];
    Goto "goto" "Go to a path (Tab completes)" [":"];
    Filter "filter" "Filter this folder as you type (* and ? are wildcards)" ["/"];
    Find "find" "Find by name in this folder and below" ["f"];
    Select "select" "Select or unselect, then move down" ["space"];
    SelectAll "select_all" "Select everything shown" ["ctrl-a"];
    Invert "invert_selection" "Invert the selection" ["v"];
    Copy "copy" "Copy the selection (or the entry) for pasting" ["y"];
    Cut "cut" "Cut the selection (or the entry) for pasting" ["x"];
    Paste "paste" "Paste here (copies run in the background)" ["p"];
    Trash "trash" "Move to the trash or Recycle Bin" ["d"];
    Delete "delete" "Delete permanently (asks first)" ["D"];
    Rename "rename" "Rename" ["r"];
    Create "create" "New file, or folder if the name ends with /" ["a"];
    Edit "edit" "Edit in $EDITOR" ["e"];
    SetBookmark "bookmark" "Bookmark this folder under the next key" ["m"];
    JumpBookmark "jump" "Jump to the bookmark under the next key" ["'"];
    Bookmarks "bookmarks" "List bookmarks" ["b"];
    ToggleHidden "toggle_hidden" "Show or hide hidden files" ["."];
    SortNext "sort" "Sort by name, size, time, extension" ["s"];
    SortReverse "sort_reverse" "Reverse the sort" ["S"];
    TogglePreview "toggle_preview" "Show or hide the preview" ["i"];
    PreviewDown "preview_down" "Scroll the preview down" ["J"];
    PreviewUp "preview_up" "Scroll the preview up" ["K"];
    Reload "reload" "Read the folder again" ["ctrl-r", "f5"];
    CancelJob "cancel" "Cancel the running copy, move or delete" ["ctrl-c"];
    Clear "clear" "Clear the filter, or else the selection" ["esc"];
    Help "help" "This help" ["?", "f1"];
    Quit "quit" "Quit" ["q"];
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct File {
    layout: Layout,
    view: View,
    keys: BTreeMap<String, String>,
    editor: Option<String>,
    preview_threads: Option<usize>,
}

/// Column layout: relative widths of the parent, current and preview columns. A zero hides that
/// column. `info` picks what is shown after each name: "size", "modified", both or neither.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Layout {
    pub columns: [u16; 3],
    pub info: Vec<String>,
    /// Draw a vertical line between columns.
    pub borders: bool,
}

impl Default for Layout {
    fn default() -> Layout {
        Layout { columns: [1, 3, 4], info: vec!["size".into()], borders: true }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct View {
    show_hidden: bool,
    sort: String,
    reverse: bool,
    dirs_first: bool,
}

impl Default for View {
    fn default() -> View {
        View { show_hidden: false, sort: "name".into(), reverse: false, dirs_first: true }
    }
}

#[derive(Debug)]
pub struct Config {
    pub layout: Layout,
    pub show_hidden: bool,
    pub order: SortOrder,
    pub keys: HashMap<Key, Action>,
    pub editor: Option<String>,
    pub preview_threads: usize,
    /// Where bookmarks are saved; `None` keeps them in memory only.
    pub dir: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Config {
        let mut keys = HashMap::new();
        for &a in Action::ALL {
            for k in a.default_keys() {
                keys.insert(parse_key(k).expect("default key names parse"), a);
            }
        }
        Config {
            layout: Layout::default(),
            show_hidden: false,
            order: SortOrder::default(),
            keys,
            editor: None,
            preview_threads: 4,
            dir: None,
        }
    }
}

impl Config {
    /// Parses a configuration file's text. Errors name the bad key or value.
    pub fn parse(text: &str) -> Result<Config, String> {
        let f: File = toml::from_str(text).map_err(|e| e.to_string())?;
        let mut c = Config::default();
        if f.layout.columns[1] == 0 {
            return Err("layout.columns: the middle (current folder) column cannot be 0".into());
        }
        for i in &f.layout.info {
            if i != "size" && i != "modified" {
                return Err(format!("layout.info: unknown '{i}' (use \"size\" and \"modified\")"));
            }
        }
        c.layout = f.layout;
        c.show_hidden = f.view.show_hidden;
        c.order = SortOrder {
            key: SortKey::parse(&f.view.sort).ok_or_else(|| format!("view.sort: unknown '{}'", f.view.sort))?,
            reverse: f.view.reverse,
            dirs_first: f.view.dirs_first,
        };
        for (k, a) in &f.keys {
            let key = parse_key(k).ok_or_else(|| format!("keys: unknown key name '{k}'"))?;
            if a == "none" {
                c.keys.remove(&key);
            } else {
                let action = Action::parse(a).ok_or_else(|| format!("keys: unknown action '{a}' for '{k}'"))?;
                c.keys.insert(key, action);
            }
        }
        c.editor = f.editor;
        if let Some(n) = f.preview_threads {
            c.preview_threads = n.clamp(1, 64);
        }
        Ok(c)
    }

    /// Loads `config.toml` from `dir` if it exists; a missing file means the defaults.
    pub fn load(dir: &Path) -> Result<Config, String> {
        let path = dir.join("config.toml");
        let mut c = match fs::read_to_string(&path) {
            Ok(t) => Config::parse(&t).map_err(|e| format!("{}: {e}", path.display()))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Config::default(),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        c.dir = Some(dir.to_path_buf());
        Ok(c)
    }

    /// The keys bound to an action, for the help screen.
    pub fn keys_for(&self, a: Action) -> Vec<String> {
        let mut v: Vec<String> = self.keys.iter().filter(|(_, &b)| b == a).map(|(&k, _)| key_name(k)).collect();
        v.sort_by_key(|s| (s.len() > 1, s.clone()));
        v
    }
}

/// The configuration directory: `$FM_CONFIG_DIR`, else `%APPDATA%\fm` on Windows, else
/// `$XDG_CONFIG_HOME/fm` or `~/.config/fm`.
pub fn config_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("FM_CONFIG_DIR") {
        return Some(PathBuf::from(d));
    }
    if cfg!(windows) {
        return std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("fm"));
    }
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(x).join("fm"));
    }
    home_dir().map(|h| h.join(".config").join("fm"))
}

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from)
}

/// Bookmarks: one character to one folder, saved as `c<TAB>path` lines in `bookmarks`.
#[derive(Default)]
pub struct Bookmarks {
    pub marks: BTreeMap<char, PathBuf>,
    file: Option<PathBuf>,
}

impl Bookmarks {
    pub fn load(dir: Option<&Path>) -> Bookmarks {
        let file = dir.map(|d| d.join("bookmarks"));
        let mut marks = BTreeMap::new();
        if let Some(text) = file.as_ref().and_then(|f| fs::read_to_string(f).ok()) {
            for line in text.lines() {
                let mut chars = line.chars();
                if let (Some(c), Some('\t')) = (chars.next(), chars.next()) {
                    marks.insert(c, PathBuf::from(chars.as_str()));
                }
            }
        }
        Bookmarks { marks, file }
    }

    pub fn set(&mut self, c: char, path: &Path) -> io::Result<()> {
        self.marks.insert(c, path.to_path_buf());
        let Some(file) = &self.file else { return Ok(()) };
        if let Some(d) = file.parent() {
            fs::create_dir_all(d)?;
        }
        let text: String = self.marks.iter().map(|(c, p)| format!("{c}\t{}\n", p.display())).collect();
        // Write a temporary file and rename it, so a crash never leaves half a bookmarks file.
        let tmp = file.with_extension("tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_default_key_is_bound_once() {
        let mut seen = HashMap::new();
        for &a in Action::ALL {
            for k in a.default_keys() {
                let key = parse_key(k).unwrap();
                assert!(seen.insert(key, a).is_none(), "{k} bound twice");
                assert_eq!(parse_key(&key_name(key)), Some(key));
            }
        }
    }

    #[test]
    fn config_overrides() {
        let c = Config::parse(
            "[layout]\ncolumns = [0, 1, 1]\ninfo = [\"modified\"]\n[view]\nsort = \"size\"\nreverse = true\n[keys]\nq = \"none\"\nctrl-q = \"quit\"\n",
        )
        .unwrap();
        assert_eq!(c.layout.columns, [0, 1, 1]);
        assert_eq!(c.order.key, SortKey::Size);
        assert!(c.order.reverse);
        assert!(!c.keys.contains_key(&Key::Char('q')));
        assert_eq!(c.keys[&Key::Ctrl('q')], Action::Quit);
        assert_eq!(c.keys[&Key::Char('j')], Action::Down);
    }

    #[test]
    fn config_errors_say_what_is_wrong() {
        assert!(Config::parse("[keys]\nq = \"fly\"").unwrap_err().contains("unknown action 'fly'"));
        assert!(Config::parse("[view]\nsort = \"colour\"").unwrap_err().contains("view.sort"));
        assert!(Config::parse("[layout]\ncolumns = [1, 0, 1]").unwrap_err().contains("middle"));
        assert!(Config::parse("[viwe]\n").is_err());
    }

    #[test]
    fn bookmarks_persist() {
        let d = tempfile::tempdir().unwrap();
        let mut b = Bookmarks::load(Some(d.path()));
        b.set('w', Path::new("/work/project")).unwrap();
        b.set('\u{e9}', Path::new("/tmp/with space")).unwrap();
        let b2 = Bookmarks::load(Some(d.path()));
        assert_eq!(b2.marks[&'w'], Path::new("/work/project"));
        assert_eq!(b2.marks[&'\u{e9}'], Path::new("/tmp/with space"));
    }
}
