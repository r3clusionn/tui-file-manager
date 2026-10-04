//! The file manager: a state machine fed with keys and a function that draws the screen. Neither
//! touches a terminal, so both are tested directly and `run_script` replays a key sequence.
//! Slow work (previews, copies, searches) runs on other threads; the app only polls for results.

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf, MAIN_SEPARATOR};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::config::{self, Action, Bookmarks, Config, Key};
use crate::entry::{self, human_size, Entry, Kind, Matcher};
use crate::find::{self, Search};
use crate::ops::{self, Jobs, Op, Outcome, Request};
use crate::preview::{self, Preview, Previewer, Tone};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    Default,
    Blue,
    Cyan,
    Green,
    Yellow,
    Red,
    Grey,
    Magenta,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Style {
    pub fg: Color,
    pub bold: bool,
    pub reverse: bool,
}

impl Style {
    pub const PLAIN: Style = Style { fg: Color::Default, bold: false, reverse: false };

    const fn fg(fg: Color) -> Style {
        Style { fg, bold: false, reverse: false }
    }

    const fn bold(self) -> Style {
        Style { bold: true, ..self }
    }

    const fn reverse(self) -> Style {
        Style { reverse: true, ..self }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Screen {
    pub lines: Vec<Vec<Span>>,
}

impl Screen {
    pub fn plain(&self) -> String {
        self.lines
            .iter()
            .map(|l| l.iter().map(|s| s.text.as_str()).collect::<String>().trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The screen with ANSI colours, for replaying a script into a terminal or an image.
    pub fn ansi(&self) -> String {
        let mut out = String::new();
        for (i, line) in self.lines.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            for s in line {
                let mut codes = Vec::new();
                if s.style.bold {
                    codes.push("1");
                }
                if s.style.reverse {
                    codes.push("7");
                }
                codes.push(match s.style.fg {
                    Color::Default => "",
                    Color::Blue => "94",
                    Color::Cyan => "36",
                    Color::Green => "32",
                    Color::Yellow => "33",
                    Color::Red => "31",
                    Color::Grey => "90",
                    Color::Magenta => "35",
                });
                codes.retain(|c| !c.is_empty());
                if codes.is_empty() {
                    out.push_str(&s.text);
                } else {
                    let _ = write!(out, "\x1b[{}m{}\x1b[0m", codes.join(";"), s.text);
                }
            }
        }
        out
    }
}

/// Something the app wants done outside itself: the terminal loop does it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Open with the system's default program.
    Open(PathBuf),
    /// Edit in the terminal with `$EDITOR`.
    Edit(PathBuf),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PromptKind {
    Rename(PathBuf),
    Create,
    Goto,
    Find,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Mode {
    Normal,
    Filter,
    Prompt(PromptKind),
    ConfirmDelete(Vec<PathBuf>),
    Results,
    Help,
    Bookmarks,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pending {
    SetBookmark,
    JumpBookmark,
}

/// A line of text being typed, with a cursor.
#[derive(Default)]
struct Input {
    text: Vec<char>,
    pos: usize,
}

impl Input {
    fn set(&mut self, s: &str) {
        self.text = s.chars().collect();
        self.pos = self.text.len();
    }

    fn string(&self) -> String {
        self.text.iter().collect()
    }

    /// Applies an editing key. Returns false for keys it does not handle.
    fn edit(&mut self, key: Key) -> bool {
        match key {
            Key::Char(c) => {
                self.text.insert(self.pos, c);
                self.pos += 1;
            }
            Key::Backspace if self.pos > 0 => {
                self.pos -= 1;
                self.text.remove(self.pos);
            }
            Key::Delete if self.pos < self.text.len() => {
                self.text.remove(self.pos);
            }
            Key::Left => self.pos = self.pos.saturating_sub(1),
            Key::Right => self.pos = (self.pos + 1).min(self.text.len()),
            Key::Home | Key::Ctrl('a') => self.pos = 0,
            Key::End | Key::Ctrl('e') => self.pos = self.text.len(),
            Key::Ctrl('u') => {
                self.text.drain(..self.pos);
                self.pos = 0;
            }
            Key::Ctrl('w') => {
                let mut i = self.pos;
                while i > 0 && self.text[i - 1] == ' ' {
                    i -= 1;
                }
                while i > 0 && !matches!(self.text[i - 1], ' ' | '/' | '\\') {
                    i -= 1;
                }
                self.text.drain(i..self.pos);
                self.pos = i;
            }
            Key::Backspace | Key::Delete => {}
            _ => return false,
        }
        true
    }
}

struct Message {
    text: String,
    error: bool,
}

pub struct App {
    /// The folder shown. Empty means the list of drives (Windows).
    pub cwd: PathBuf,
    entries: Vec<Entry>,
    view: Vec<usize>,
    cursor: usize,
    top: usize,
    parent: Vec<Entry>,
    remembered: HashMap<PathBuf, String>,
    history: Vec<PathBuf>,
    selected: BTreeSet<PathBuf>,
    clipboard: Option<(Op, Vec<PathBuf>)>,
    filter: String,
    mode: Mode,
    pending: Option<Pending>,
    input: Input,
    message: Option<Message>,
    pub cfg: Config,
    bookmarks: Bookmarks,
    previewer: Previewer,
    jobs: Jobs,
    search: Option<Search>,
    found_cursor: usize,
    found_top: usize,
    help_top: usize,
    preview_on: bool,
    preview_scroll: usize,
    stamp: Option<SystemTime>,
    last_check: Instant,
    rows: usize,
    pub effect: Option<Effect>,
    pub quit: bool,
}

impl App {
    /// Opens `dir` (or, if it is a file, its folder with the file under the cursor).
    pub fn new(start: &Path, cfg: Config) -> Result<App, String> {
        Self::with_preview_delay(start, cfg, Duration::ZERO)
    }

    /// As [`App::new`], with a delay added to every preview (to test slow storage).
    pub fn with_preview_delay(start: &Path, cfg: Config, delay: Duration) -> Result<App, String> {
        let bookmarks = Bookmarks::load(cfg.dir.as_deref());
        let previewer = Previewer::new(cfg.preview_threads, delay);
        let mut app = App {
            cwd: PathBuf::new(),
            entries: Vec::new(),
            view: Vec::new(),
            cursor: 0,
            top: 0,
            parent: Vec::new(),
            remembered: HashMap::new(),
            history: Vec::new(),
            selected: BTreeSet::new(),
            clipboard: None,
            filter: String::new(),
            mode: Mode::Normal,
            pending: None,
            input: Input::default(),
            message: None,
            cfg,
            bookmarks,
            previewer,
            jobs: Jobs::new(),
            search: None,
            found_cursor: 0,
            found_top: 0,
            help_top: 0,
            preview_on: true,
            preview_scroll: 0,
            stamp: None,
            last_check: Instant::now(),
            rows: 20,
            effect: None,
            quit: false,
        };
        let abs = std::path::absolute(start).map_err(|e| format!("{}: {e}", start.display()))?;
        let meta = fs::metadata(&abs).map_err(|e| format!("{}: {e}", start.display()))?;
        if meta.is_dir() {
            app.load(abs, None)?;
        } else {
            let name = abs.file_name().map(|n| n.to_string_lossy().into_owned());
            app.load(abs.parent().map(Path::to_path_buf).unwrap_or_default(), name)?;
        }
        Ok(app)
    }

    pub fn current(&self) -> Option<&Entry> {
        self.view.get(self.cursor).map(|&i| &self.entries[i])
    }

    pub fn message(&self) -> &str {
        self.message.as_ref().map(|m| m.text.as_str()).unwrap_or("")
    }

    /// Names in the current view, in order (for tests).
    pub fn names(&self) -> Vec<&str> {
        self.view.iter().map(|&i| self.entries[i].name.as_str()).collect()
    }

    pub fn selected(&self) -> Vec<PathBuf> {
        self.selected.iter().cloned().collect()
    }

    fn in_drives(&self) -> bool {
        self.cwd.as_os_str().is_empty()
    }

    fn info(&mut self, text: impl Into<String>) {
        self.message = Some(Message { text: text.into(), error: false });
    }

    fn error(&mut self, text: impl Into<String>) {
        self.message = Some(Message { text: text.into(), error: true });
    }

    /// Shows an error from outside the app (the terminal loop failing to open a file).
    pub fn handle_error(&mut self, text: String) {
        self.error(text);
    }

    /// Reads `dir` and shows it, with the cursor on `select` (a name) if given, else where it was
    /// the last time this folder was shown.
    fn load(&mut self, dir: PathBuf, select: Option<String>) -> Result<(), String> {
        let mut entries = if dir.as_os_str().is_empty() {
            drives()
        } else {
            entry::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?
        };
        entry::sort(&mut entries, self.cfg.order);
        let prev = self.current().map(|e| e.name.clone());
        if let Some(name) = &prev {
            self.remembered.insert(self.cwd.clone(), name.clone());
        }
        let changed = dir != self.cwd;
        self.parent = match parent_of(&dir) {
            Some(p) if p.as_os_str().is_empty() => drives(),
            Some(p) => entry::read_dir(&p).unwrap_or_default(),
            None => Vec::new(),
        };
        entry::sort(&mut self.parent, self.cfg.order);
        self.stamp = dir_stamp(&dir);
        self.cwd = dir;
        // The view indexes the old entries; drop it before they are replaced.
        self.view.clear();
        self.entries = entries;
        if changed {
            self.filter.clear();
            self.top = 0;
            self.preview_scroll = 0;
        }
        let want = select.or_else(|| if changed { self.remembered.get(&self.cwd).cloned() } else { prev });
        self.rebuild(want.as_deref());
        Ok(())
    }

    /// Goes to `dir`, remembering where we were for `back`.
    fn go(&mut self, dir: PathBuf, select: Option<String>) {
        let old = self.cwd.clone();
        match self.load(dir, select) {
            Ok(()) => {
                if old != self.cwd {
                    self.history.push(old);
                    if self.history.len() > 100 {
                        self.history.remove(0);
                    }
                }
            }
            Err(e) => self.error(e),
        }
    }

    /// Recomputes the visible entries (hidden files, filter) and keeps the cursor on `select`, or
    /// on the same entry as before, or as close to its old position as possible.
    fn rebuild(&mut self, select: Option<&str>) {
        let keep = select.map(str::to_string).or_else(|| self.current().map(|e| e.name.clone()));
        let old = self.cursor;
        let m = Matcher::new(&self.filter);
        let show_hidden = self.cfg.show_hidden;
        self.view = (0..self.entries.len())
            .filter(|&i| {
                let e = &self.entries[i];
                (show_hidden || !e.hidden || keep.as_deref() == Some(e.name.as_str()) && select.is_some()) && m.matches(&e.name)
            })
            .collect();
        self.cursor = keep
            .and_then(|k| self.view.iter().position(|&i| self.entries[i].name == k))
            .unwrap_or_else(|| old.min(self.view.len().saturating_sub(1)));
    }

    fn resort(&mut self) {
        entry::sort(&mut self.entries, self.cfg.order);
        entry::sort(&mut self.parent, self.cfg.order);
        self.rebuild(None);
    }

    /// Reads the folder again, keeping cursor, filter and selection; selected paths that no
    /// longer exist are dropped.
    pub fn reload(&mut self) {
        let dir = self.cwd.clone();
        let name = self.current().map(|e| e.name.clone());
        if let Err(e) = self.load(dir, name) {
            // The folder itself went away: climb to the nearest one that still exists.
            let mut p = self.cwd.clone();
            while let Some(up) = parent_of(&p) {
                if up.as_os_str().is_empty() || up.is_dir() {
                    let _ = self.load(up, None);
                    break;
                }
                p = up;
            }
            self.error(e);
        }
        self.selected.retain(|p| fs::symlink_metadata(p).is_ok());
        self.previewer.clear();
    }

    /// Polls background work and watches the folder for changes. Returns true if the screen may
    /// have changed. The terminal loop calls this between key presses.
    pub fn tick(&mut self) -> bool {
        let mut changed = self.previewer.collect();
        let progress_before = self.jobs.current.as_ref().map(|(_, p)| (p.bytes, p.items));
        for o in self.jobs.poll() {
            self.finished(o);
            changed = true;
        }
        changed |= self.jobs.current.as_ref().map(|(_, p)| (p.bytes, p.items)) != progress_before;
        if let Some(s) = &mut self.search {
            changed |= s.poll();
        }
        if self.last_check.elapsed() >= Duration::from_millis(1000) {
            self.last_check = Instant::now();
            if !self.in_drives() && dir_stamp(&self.cwd) != self.stamp {
                self.reload();
                changed = true;
            }
        }
        changed
    }

    /// True while anything runs in the background.
    pub fn busy(&self) -> bool {
        self.jobs.busy() || self.previewer.is_waiting() || self.search.as_ref().is_some_and(|s| !s.finished)
    }

    /// Waits for background work to finish (used when replaying keys, so screens are repeatable).
    pub fn settle(&mut self) {
        for o in self.jobs.wait(Duration::from_secs(600)) {
            self.finished(o);
        }
        if let Some(s) = &mut self.search {
            s.wait(Duration::from_secs(600));
        }
        self.previewer.wait(Duration::from_secs(60));
    }

    fn finished(&mut self, o: Outcome) {
        if matches!(o.op, Op::Move | Op::Delete | Op::Trash) {
            for p in &o.done {
                self.selected.remove(p);
            }
        }
        let n = o.done.len();
        let items = if n == 1 { "1 item".to_string() } else { format!("{n} items") };
        let mut text = if o.cancelled { format!("{} cancelled after {items}", o.op.noun()) } else { o.op.summary(&items) };
        if o.op == Op::Copy && o.bytes > 0 && !o.cancelled {
            let secs = o.elapsed.as_secs_f64().max(1e-3);
            let _ = write!(text, ", {} in {:.1} s ({}/s)", human_size(o.bytes), secs, human_size((o.bytes as f64 / secs) as u64));
        }
        let first_new = o.done.first().filter(|p| matches!(o.op, Op::Copy | Op::Move) && p.parent() == Some(&self.cwd)).cloned();
        let name = first_new.and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
        let dir = self.cwd.clone();
        if !self.in_drives() {
            match self.load(dir, name) {
                Ok(()) => self.previewer.clear(),
                Err(_) => self.reload(),
            }
        }
        if o.errors.is_empty() {
            self.info(text);
        } else {
            let more = if o.errors.len() > 1 { format!(" (and {} more)", o.errors.len() - 1) } else { String::new() };
            self.error(format!("{text}; error: {}{more}", o.errors[0]));
        }
    }

    /// The selection if there is one, else the entry under the cursor.
    fn targets(&self) -> Vec<PathBuf> {
        if !self.selected.is_empty() {
            return self.selected.iter().cloned().collect();
        }
        self.current().map(|e| vec![e.path.clone()]).unwrap_or_default()
    }

    fn move_to(&mut self, i: usize) {
        let i = i.min(self.view.len().saturating_sub(1));
        if i != self.cursor {
            self.preview_scroll = 0;
        }
        self.cursor = i;
    }

    pub fn handle_key(&mut self, key: Key) {
        if !matches!(self.mode, Mode::Normal) || self.pending.is_some() {
            self.handle_modal(key);
            return;
        }
        self.message = None;
        if let Some(&action) = self.cfg.keys.get(&key) {
            self.act(action);
        }
    }

    fn handle_modal(&mut self, key: Key) {
        if let Some(p) = self.pending.take() {
            let Key::Char(c) = key else { return };
            match p {
                Pending::SetBookmark => {
                    if self.in_drives() {
                        return self.error("a bookmark needs a folder");
                    }
                    let cwd = self.cwd.clone();
                    match self.bookmarks.set(c, &cwd) {
                        Ok(()) => self.info(format!("bookmark '{c}' set to {}", cwd.display())),
                        Err(e) => self.error(format!("saving bookmarks: {e}")),
                    }
                }
                Pending::JumpBookmark => self.jump(c),
            }
            return;
        }
        match self.mode.clone() {
            Mode::Normal => {}
            Mode::Help => match key {
                Key::Char('j') | Key::Down => self.help_top += 1,
                Key::Char('k') | Key::Up => self.help_top = self.help_top.saturating_sub(1),
                _ => self.mode = Mode::Normal,
            },
            Mode::Bookmarks => {
                self.mode = Mode::Normal;
                if let Key::Char(c) = key {
                    self.jump(c);
                }
            }
            Mode::ConfirmDelete(paths) => {
                self.mode = Mode::Normal;
                if key == Key::Char('y') || key == Key::Char('Y') {
                    let n = paths.len();
                    self.jobs.submit(Request { op: Op::Delete, sources: paths, dest: PathBuf::new() });
                    self.info(format!("deleting {n} item{}", if n == 1 { "" } else { "s" }));
                } else {
                    self.info("nothing deleted");
                }
            }
            Mode::Filter => match key {
                Key::Enter => {
                    self.mode = Mode::Normal;
                }
                Key::Esc => {
                    self.mode = Mode::Normal;
                    self.filter.clear();
                    self.rebuild(None);
                }
                Key::Down | Key::Up | Key::PageDown | Key::PageUp => {
                    if let Some(&a) = self.cfg.keys.get(&key) {
                        self.act(a);
                    }
                }
                k => {
                    if self.input.edit(k) {
                        self.filter = self.input.string();
                        self.rebuild(None);
                        self.top = 0;
                    }
                }
            },
            Mode::Prompt(kind) => match key {
                Key::Esc => self.mode = Mode::Normal,
                Key::Enter => {
                    self.mode = Mode::Normal;
                    let text = self.input.string();
                    self.submit(kind, text);
                }
                Key::Tab if kind == PromptKind::Goto => self.complete(),
                k => {
                    self.message = None;
                    self.input.edit(k);
                }
            },
            Mode::Results => self.results_key(key),
        }
    }

    fn results_key(&mut self, key: Key) {
        let n = self.search.as_ref().map_or(0, |s| s.hits.len());
        let page = self.rows.saturating_sub(1).max(1);
        match key {
            Key::Char('j') | Key::Down => self.found_cursor = (self.found_cursor + 1).min(n.saturating_sub(1)),
            Key::Char('k') | Key::Up => self.found_cursor = self.found_cursor.saturating_sub(1),
            Key::PageDown | Key::Ctrl('d') => self.found_cursor = (self.found_cursor + page).min(n.saturating_sub(1)),
            Key::PageUp | Key::Ctrl('u') => self.found_cursor = self.found_cursor.saturating_sub(page),
            Key::Char('g') | Key::Home => self.found_cursor = 0,
            Key::Char('G') | Key::End => self.found_cursor = n.saturating_sub(1),
            Key::Enter | Key::Char('l') | Key::Right => {
                let hit = self.search.as_ref().and_then(|s| s.hits.get(self.found_cursor)).map(|h| h.path.clone());
                if let Some(path) = hit {
                    self.mode = Mode::Normal;
                    if let Some(s) = &self.search {
                        s.cancel();
                    }
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
                    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
                    // A hidden match stays visible when we land on it (see `rebuild`).
                    self.go(dir, name);
                }
            }
            Key::Esc | Key::Char('q') | Key::Char('h') | Key::Left => {
                self.mode = Mode::Normal;
                if let Some(s) = &self.search {
                    s.cancel();
                }
            }
            _ => {}
        }
    }

    fn jump(&mut self, c: char) {
        if c == '\'' {
            return self.act(Action::Back);
        }
        match self.bookmarks.marks.get(&c).cloned() {
            Some(p) => self.go(p, None),
            None => self.error(format!("no bookmark '{c}'")),
        }
    }

    fn act(&mut self, a: Action) {
        let n = self.view.len();
        let page = self.rows.saturating_sub(1).max(1);
        match a {
            Action::Down => self.move_to((self.cursor + 1).min(n.saturating_sub(1))),
            Action::Up => self.move_to(self.cursor.saturating_sub(1)),
            Action::PageDown => self.move_to(self.cursor + page),
            Action::PageUp => self.move_to(self.cursor.saturating_sub(page)),
            Action::Top => self.move_to(0),
            Action::Bottom => self.move_to(n.saturating_sub(1)),
            Action::Parent => {
                if let Some(p) = parent_of(&self.cwd) {
                    let name = if self.cwd.file_name().is_some() {
                        self.cwd.file_name().map(|n| n.to_string_lossy().into_owned())
                    } else {
                        Some(drive_name(&self.cwd))
                    };
                    self.go(p, name);
                }
            }
            Action::Open => {
                let Some(e) = self.current().cloned() else { return };
                if e.is_dir() {
                    self.go(e.path.clone(), None);
                } else if matches!(e.kind, Kind::Link { broken: true, .. }) {
                    self.error(format!("{}: broken link", e.name));
                } else {
                    self.effect = Some(Effect::Open(e.path.clone()));
                }
            }
            Action::Back => match self.history.pop() {
                Some(p) => {
                    if let Err(e) = self.load(p, None) {
                        self.error(e);
                    }
                }
                None => self.info("no earlier folder"),
            },
            Action::Home => match config::home_dir() {
                Some(h) => self.go(h, None),
                None => self.error("no home folder"),
            },
            Action::Goto => {
                let mut start = if self.in_drives() { String::new() } else { self.cwd.display().to_string() };
                if !start.is_empty() && !start.ends_with(['/', '\\']) {
                    start.push(MAIN_SEPARATOR);
                }
                self.prompt(PromptKind::Goto, &start);
            }
            Action::Filter => {
                let f = self.filter.clone();
                self.input.set(&f);
                self.mode = Mode::Filter;
            }
            Action::Find => {
                if self.in_drives() {
                    return self.error("choose a drive first");
                }
                self.prompt(PromptKind::Find, "");
            }
            Action::Select => {
                if let Some(e) = self.current() {
                    let p = e.path.clone();
                    if !self.selected.remove(&p) {
                        self.selected.insert(p);
                    }
                    self.move_to((self.cursor + 1).min(n.saturating_sub(1)));
                }
            }
            Action::SelectAll => {
                let shown: Vec<PathBuf> = self.view.iter().map(|&i| self.entries[i].path.clone()).collect();
                if shown.iter().all(|p| self.selected.contains(p)) {
                    for p in &shown {
                        self.selected.remove(p);
                    }
                } else {
                    self.selected.extend(shown);
                }
            }
            Action::Invert => {
                for &i in &self.view {
                    let p = &self.entries[i].path;
                    if !self.selected.remove(p) {
                        self.selected.insert(p.clone());
                    }
                }
            }
            Action::Copy | Action::Cut => {
                let t = self.targets();
                if t.is_empty() || self.in_drives() {
                    return;
                }
                let op = if a == Action::Copy { Op::Copy } else { Op::Move };
                let what = if t.len() == 1 { name_of(&t[0]) } else { format!("{} items", t.len()) };
                self.info(format!("{} {what}: paste with p", if op == Op::Copy { "copied" } else { "cut" }));
                self.clipboard = Some((op, t));
                self.selected.clear();
            }
            Action::Paste => {
                let Some((op, paths)) = self.clipboard.clone() else { return self.info("nothing to paste") };
                if self.in_drives() {
                    return self.error("choose a folder first");
                }
                if op == Op::Move {
                    self.clipboard = None;
                }
                let n = paths.len();
                self.jobs.submit(Request { op, sources: paths, dest: self.cwd.clone() });
                self.info(format!("{} {n} item{}", op.verb(), if n == 1 { "" } else { "s" }));
            }
            Action::Trash => {
                let t = self.targets();
                if t.is_empty() || self.in_drives() {
                    return;
                }
                let n = t.len();
                self.jobs.submit(Request { op: Op::Trash, sources: t, dest: PathBuf::new() });
                self.info(format!("moving {n} item{} to the trash", if n == 1 { "" } else { "s" }));
            }
            Action::Delete => {
                let t = self.targets();
                if t.is_empty() || self.in_drives() {
                    return;
                }
                self.mode = Mode::ConfirmDelete(t);
            }
            Action::Rename => {
                let Some(e) = self.current().cloned() else { return };
                if self.in_drives() {
                    return;
                }
                self.prompt(PromptKind::Rename(e.path.clone()), &e.name);
                // Put the cursor before the extension, where a rename usually changes things.
                if let Some(dot) = e.name.rfind('.').filter(|&i| i > 0 && !e.is_dir()) {
                    self.input.pos = e.name[..dot].chars().count();
                }
            }
            Action::Create => {
                if self.in_drives() {
                    return self.error("choose a folder first");
                }
                self.prompt(PromptKind::Create, "");
            }
            Action::Edit => {
                if let Some(e) = self.current().filter(|e| !e.is_dir()) {
                    self.effect = Some(Effect::Edit(e.path.clone()));
                }
            }
            Action::SetBookmark => self.pending = Some(Pending::SetBookmark),
            Action::JumpBookmark => self.pending = Some(Pending::JumpBookmark),
            Action::Bookmarks => self.mode = Mode::Bookmarks,
            Action::ToggleHidden => {
                self.cfg.show_hidden = !self.cfg.show_hidden;
                self.rebuild(None);
                self.info(if self.cfg.show_hidden { "showing hidden files" } else { "hiding hidden files" });
            }
            Action::SortNext => {
                self.cfg.order.key = self.cfg.order.key.next();
                self.resort();
                self.info(format!("sorted by {}", self.cfg.order.key.name()));
            }
            Action::SortReverse => {
                self.cfg.order.reverse = !self.cfg.order.reverse;
                self.resort();
            }
            Action::TogglePreview => self.preview_on = !self.preview_on,
            Action::PreviewDown => self.preview_scroll += page / 2,
            Action::PreviewUp => self.preview_scroll = self.preview_scroll.saturating_sub(page / 2),
            Action::Reload => {
                self.reload();
                self.info("reloaded");
            }
            Action::CancelJob => {
                if self.jobs.busy() {
                    self.jobs.cancel();
                    self.info("cancelling");
                }
            }
            Action::Clear => {
                // One step at a time: the filter first, then the selection.
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.rebuild(None);
                } else {
                    self.selected.clear();
                }
            }
            Action::Help => {
                self.help_top = 0;
                self.mode = Mode::Help;
            }
            Action::Quit => self.quit = true,
        }
    }

    fn prompt(&mut self, kind: PromptKind, text: &str) {
        self.message = None;
        self.input.set(text);
        self.mode = Mode::Prompt(kind);
    }

    fn submit(&mut self, kind: PromptKind, text: String) {
        match kind {
            PromptKind::Rename(path) => {
                if text.is_empty() || Some(text.as_str()) == path.file_name().and_then(|n| n.to_str()) {
                    return;
                }
                match ops::rename(&path, &text) {
                    Ok(new) => {
                        if self.selected.remove(&path) {
                            self.selected.insert(new);
                        }
                        let dir = self.cwd.clone();
                        let _ = self.load(dir, Some(text.clone()));
                        self.previewer.clear();
                        self.info(format!("renamed to {text}"));
                    }
                    Err(e) => self.error(format!("rename: {e}")),
                }
            }
            PromptKind::Create => {
                if text.is_empty() {
                    return;
                }
                match ops::create(&self.cwd, &text) {
                    Ok(p) => {
                        // Select the top-level item that now holds what was created.
                        let rel = p.strip_prefix(&self.cwd).map(Path::to_path_buf).unwrap_or(p);
                        let first = rel.components().next().map(|c| c.as_os_str().to_string_lossy().into_owned());
                        let dir = self.cwd.clone();
                        let _ = self.load(dir, first);
                        self.info(format!("created {text}"));
                    }
                    Err(e) => self.error(format!("create: {e}")),
                }
            }
            PromptKind::Goto => {
                let path = self.resolve(&text);
                match fs::metadata(&path) {
                    Ok(m) if m.is_dir() => self.go(path, None),
                    Ok(_) => {
                        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
                        self.go(path.parent().map(Path::to_path_buf).unwrap_or_default(), name);
                    }
                    Err(e) => self.error(format!("{}: {e}", path.display())),
                }
            }
            PromptKind::Find => {
                if text.is_empty() {
                    return;
                }
                self.search = Some(Search::start(&self.cwd, &text, self.cfg.show_hidden));
                self.found_cursor = 0;
                self.found_top = 0;
                self.mode = Mode::Results;
            }
        }
    }

    /// Expands `~` and makes a typed path absolute against the current folder.
    fn resolve(&self, text: &str) -> PathBuf {
        let t = text.trim();
        let p = match t.strip_prefix('~') {
            Some(rest) if rest.is_empty() || rest.starts_with(['/', '\\']) => {
                config::home_dir().unwrap_or_default().join(rest.trim_start_matches(['/', '\\']))
            }
            _ => PathBuf::from(t),
        };
        let p = if p.is_absolute() || self.in_drives() { p } else { self.cwd.join(p) };
        // Normalise "." and ".." without touching the disk (links stay as typed, like a shell).
        let mut out = PathBuf::new();
        for c in p.components() {
            match c {
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    if out.file_name().is_some() {
                        out.pop();
                    }
                }
                c => out.push(c),
            }
        }
        out
    }

    /// Tab in the goto prompt: completes the last part of the path to a folder name, as far as
    /// all candidates agree.
    fn complete(&mut self) {
        let text = self.input.string();
        let split = text.rfind(['/', '\\']).map(|i| i + 1).unwrap_or(0);
        let (dir_part, partial) = text.split_at(split);
        let dir = self.resolve(if dir_part.is_empty() { "." } else { dir_part });
        let Ok(rd) = fs::read_dir(&dir) else { return };
        let fold = cfg!(windows);
        let low = |s: &str| if fold { s.to_lowercase() } else { s.to_string() };
        let mut names: Vec<String> = rd
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| {
                low(n).starts_with(&low(partial)) && (self.cfg.show_hidden || !n.starts_with('.') || partial.starts_with('.'))
            })
            .collect();
        names.sort_by(|a, b| entry::natural_cmp(a, b));
        match names.len() {
            0 => self.error("no match"),
            1 => self.input.set(&format!("{dir_part}{}{MAIN_SEPARATOR}", names[0])),
            _ => {
                let mut common: Vec<char> = names[0].chars().collect();
                for n in &names[1..] {
                    let k = common
                        .iter()
                        .zip(n.chars())
                        .take_while(|(a, b)| if fold { a.to_lowercase().eq(b.to_lowercase()) } else { **a == *b })
                        .count();
                    common.truncate(k);
                }
                let common: String = common.into_iter().collect();
                if common.chars().count() > partial.chars().count() {
                    self.input.set(&format!("{dir_part}{common}"));
                }
                let shown: Vec<&str> = names.iter().take(8).map(String::as_str).collect();
                let more = if names.len() > 8 { " ..." } else { "" };
                self.info(format!("{}{more}", shown.join("  ")));
            }
        }
    }

    /// Draws the whole screen.
    pub fn render(&mut self, width: usize, height: usize) -> Screen {
        self.tick();
        let mut lines = Vec::with_capacity(height);
        if height == 0 || width == 0 {
            return Screen { lines };
        }
        let body = height.saturating_sub(2);
        self.rows = body.max(1);
        lines.push(self.title_line(width));
        match self.mode {
            Mode::Help => self.help_body(width, body, &mut lines),
            Mode::Results => self.results_body(width, body, &mut lines),
            Mode::Bookmarks => self.bookmarks_body(width, body, &mut lines),
            _ => self.columns(width, body, &mut lines),
        }
        // In a prompt the status line is taken, so a message (completion candidates, an error)
        // goes on the line above it.
        if let (Mode::Prompt(_) | Mode::Filter, Some(m), Some(last)) = (&self.mode, &self.message, lines.last_mut()) {
            if body > 0 {
                let style = if m.error { Style::fg(Color::Red).bold() } else { Style::fg(Color::Grey) };
                *last = vec![Span { text: fit(&m.text, width), style }];
            }
        }
        if height >= 2 {
            lines.push(self.status_line(width));
        }
        lines.truncate(height);
        Screen { lines }
    }

    fn title_line(&self, width: usize) -> Vec<Span> {
        let path = if self.in_drives() { "Drives".to_string() } else { self.cwd.display().to_string() };
        let mut right = String::new();
        if let Some((op, p)) = &self.jobs.current {
            let pct =
                (p.bytes * 100).checked_div(p.total_bytes).or_else(|| (p.items * 100).checked_div(p.total_items)).unwrap_or(0);
            let _ = write!(right, "{} {pct}% {} ", op.verb(), p.current);
        }
        if !self.selected.is_empty() {
            let _ = write!(right, "{} selected ", self.selected.len());
        }
        if let Some((op, paths)) = &self.clipboard {
            let _ = write!(right, "[{} {}] ", if *op == Op::Copy { "copy" } else { "cut" }, paths.len());
        }
        let right = right.trim_end();
        let rw = right.width();
        let left_w = width.saturating_sub(if rw > 0 { rw + 2 } else { 0 });
        let mut spans = vec![Span { text: fit_left(&path, left_w), style: Style::fg(Color::Blue).bold() }];
        if rw > 0 && width > rw + 2 {
            spans.push(Span { text: format!("  {right}"), style: Style::fg(Color::Yellow) });
        }
        pad(spans, width)
    }

    fn status_line(&self, width: usize) -> Vec<Span> {
        if let Some(p) = self.pending {
            let t = match p {
                Pending::SetBookmark => "bookmark this folder as: (press a key)",
                Pending::JumpBookmark => "jump to bookmark: (press a key, ' for the previous folder)",
            };
            return pad(vec![Span { text: fit(t, width), style: Style::PLAIN.bold() }], width);
        }
        match &self.mode {
            Mode::Prompt(kind) => {
                let label = match kind {
                    PromptKind::Rename(_) => "rename: ",
                    PromptKind::Create => "new (end with / for a folder): ",
                    PromptKind::Goto => "go to: ",
                    PromptKind::Find => "find: ",
                };
                return self.input_line(label, width);
            }
            Mode::Filter => return self.input_line("filter: ", width),
            Mode::Results => {
                let hit = self.search.as_ref().and_then(|s| s.hits.get(self.found_cursor));
                let t = hit.map(|h| h.path.display().to_string()).unwrap_or_default();
                let n = self.search.as_ref().map_or(0, |s| s.hits.len());
                let right = format!("{}/{n}", if n == 0 { 0 } else { self.found_cursor + 1 });
                let lw = width.saturating_sub(right.width() + 2);
                return pad(
                    vec![
                        Span { text: fit_left(&t, lw), style: Style::fg(Color::Grey) },
                        Span { text: format!("  {right}"), style: Style::PLAIN },
                    ],
                    width,
                );
            }
            Mode::ConfirmDelete(paths) => {
                let what = if paths.len() == 1 { name_of(&paths[0]) } else { format!("{} items", paths.len()) };
                let t = format!("delete {what} permanently? (y/n)");
                return pad(vec![Span { text: fit(&t, width), style: Style::fg(Color::Red).bold() }], width);
            }
            _ => {}
        }
        let mut right = String::new();
        if !self.filter.is_empty() {
            let _ = write!(right, "filter: {}  ", self.filter);
        }
        let o = self.cfg.order;
        let _ = write!(right, "{}{}  ", o.key.name(), if o.reverse { " rev" } else { "" });
        if !self.cfg.show_hidden {
            let hidden = self.entries.iter().filter(|e| e.hidden).count();
            if hidden > 0 {
                let _ = write!(right, "{hidden} hidden  ");
            }
        }
        let _ = write!(right, "{}/{}", if self.view.is_empty() { 0 } else { self.cursor + 1 }, self.view.len());
        let (left, style) = match &self.message {
            Some(m) => (m.text.clone(), if m.error { Style::fg(Color::Red).bold() } else { Style::PLAIN }),
            None => (self.current().map(describe).unwrap_or_default(), Style::fg(Color::Grey)),
        };
        let rw = right.width();
        if rw + 2 > width {
            return pad(vec![Span { text: fit(&left, width), style }], width);
        }
        let lw = width - rw - 2;
        pad(vec![Span { text: fit(&left, lw), style }, Span { text: format!("  {right}"), style: Style::PLAIN }], width)
    }

    fn input_line(&self, label: &str, width: usize) -> Vec<Span> {
        let lw = label.width().min(width);
        let room = width.saturating_sub(lw + 1).max(1);
        // Scroll the text so the cursor stays visible.
        let mut start = 0;
        while self.input.text[start..self.input.pos].iter().map(|c| c.width().unwrap_or(0)).sum::<usize>() >= room {
            start += 1;
        }
        let mut spans = vec![Span { text: fit(label, lw), style: Style::PLAIN.bold() }];
        let before: String = self.input.text[start..self.input.pos].iter().collect();
        spans.push(Span { text: before, style: Style::PLAIN });
        let at = self.input.text.get(self.input.pos).map(|c| c.to_string()).unwrap_or_else(|| " ".into());
        spans.push(Span { text: at, style: Style::PLAIN.reverse() });
        let used: usize = spans.iter().map(|s| s.text.width()).sum();
        if self.input.pos < self.input.text.len() {
            let after: String = self.input.text[self.input.pos + 1..].iter().collect();
            spans.push(Span { text: fit_left_cut(&after, width.saturating_sub(used)), style: Style::PLAIN });
        }
        pad(spans, width)
    }

    /// Widths of the three columns for this screen width (0 = hidden).
    fn column_widths(&self, width: usize) -> [usize; 3] {
        let mut r = self.cfg.layout.columns.map(|x| x as usize);
        if !self.preview_on {
            r[2] = 0;
        }
        if width < 60 {
            r[0] = 0;
        }
        if width < 30 {
            r[2] = 0;
        }
        let active = r.iter().filter(|&&x| x > 0).count();
        let avail = width.saturating_sub(active.saturating_sub(1));
        let total: usize = r.iter().sum();
        let mut w = [0; 3];
        let mut used = 0;
        let last = (0..3).rev().find(|&i| r[i] > 0).unwrap_or(1);
        for i in 0..3 {
            if r[i] == 0 {
                continue;
            }
            w[i] = if i == last { avail - used } else { avail * r[i] / total };
            used += w[i];
        }
        w
    }

    fn columns(&mut self, width: usize, rows: usize, lines: &mut Vec<Vec<Span>>) {
        let w = self.column_widths(width);
        // Keep the cursor on screen.
        if self.cursor < self.top {
            self.top = self.cursor;
        }
        if self.cursor >= self.top + rows {
            self.top = self.cursor + 1 - rows;
        }
        self.top = self.top.min(self.view.len().saturating_sub(rows));
        let parent_rows = if w[0] > 0 { self.parent_column(w[0], rows) } else { Vec::new() };
        let current_rows = self.current_column(w[1], rows);
        let preview_rows = if w[2] > 0 { self.preview_column(w[2], rows) } else { Vec::new() };
        let sep = Span { text: if self.cfg.layout.borders { "│" } else { " " }.into(), style: Style::fg(Color::Grey) };
        for y in 0..rows {
            let mut line = Vec::new();
            let mut first = true;
            for (i, col) in [&parent_rows, &current_rows, &preview_rows].into_iter().enumerate() {
                if w[i] == 0 {
                    continue;
                }
                if !first {
                    line.push(sep.clone());
                }
                first = false;
                match col.get(y) {
                    Some(spans) => line.extend(spans.iter().cloned()),
                    None => line.push(Span { text: " ".repeat(w[i]), style: Style::PLAIN }),
                }
            }
            lines.push(line);
        }
    }

    fn parent_column(&self, w: usize, rows: usize) -> Vec<Vec<Span>> {
        let me = if self.cwd.file_name().is_some() {
            self.cwd.file_name().map(|n| n.to_string_lossy().into_owned())
        } else {
            Some(drive_name(&self.cwd))
        };
        let shown: Vec<&Entry> =
            self.parent.iter().filter(|e| self.cfg.show_hidden || !e.hidden || Some(&e.name) == me.as_ref()).collect();
        let at = shown.iter().position(|e| Some(&e.name) == me.as_ref());
        let top = at.map(|i| i.saturating_sub(rows / 2)).unwrap_or(0).min(shown.len().saturating_sub(rows));
        shown
            .iter()
            .enumerate()
            .skip(top)
            .take(rows)
            .map(|(i, e)| {
                let mut st = kind_style(e);
                if Some(i) == at {
                    st = st.reverse();
                }
                vec![Span { text: fit(&format!(" {}", e.name), w), style: st }]
            })
            .collect()
    }

    fn current_column(&self, w: usize, rows: usize) -> Vec<Vec<Span>> {
        if self.view.is_empty() {
            let t = if !self.filter.is_empty() {
                format!(" nothing matches '{}'", self.filter)
            } else if self.entries.is_empty() {
                " empty".into()
            } else {
                " only hidden files (. shows them)".into()
            };
            return vec![vec![Span { text: fit(&t, w), style: Style::fg(Color::Grey) }]];
        }
        let info = &self.cfg.layout.info;
        let show_size = info.iter().any(|s| s == "size");
        let show_time = info.iter().any(|s| s == "modified");
        let cut: BTreeSet<&PathBuf> = match &self.clipboard {
            Some((Op::Move, p)) => p.iter().collect(),
            _ => BTreeSet::new(),
        };
        let mut out = Vec::with_capacity(rows);
        for (row, &i) in self.view.iter().enumerate().skip(self.top).take(rows) {
            let e = &self.entries[i];
            let sel = self.selected.contains(&e.path);
            let mut right = String::new();
            if show_size && !e.is_dir() && e.kind != Kind::Other {
                right.push_str(&human_size(e.size));
            }
            if show_time {
                if let Some(t) = e.modified {
                    if !right.is_empty() {
                        right.push_str("  ");
                    }
                    right.push_str(&entry::format_time(t));
                }
            }
            if right.width() + 12 > w {
                right.clear();
            }
            let marker = if sel { "+" } else { " " };
            // Marker, name, two spaces, the info and a one-column margin before the border.
            let name_w = w.saturating_sub(right.width() + if right.is_empty() { 0 } else { 3 } + 1);
            let mut name = e.name.clone();
            if e.is_dir() {
                name.push(MAIN_SEPARATOR);
            }
            if let Kind::Link { broken: true, .. } = e.kind {
                name.push_str(" (broken)");
            }
            let mut text = format!("{marker}{}", fit(&name, name_w));
            if !right.is_empty() {
                let _ = write!(text, "  {right} ");
            }
            let mut st = if sel { Style::fg(Color::Yellow).bold() } else { kind_style(e) };
            if cut.contains(&e.path) {
                st = Style::fg(Color::Grey);
            }
            if row == self.cursor {
                st = st.reverse();
            }
            out.push(vec![Span { text: fit(&text, w), style: st }]);
        }
        out
    }

    fn preview_column(&mut self, w: usize, rows: usize) -> Vec<Vec<Span>> {
        let Some(e) = self.current().cloned() else { return Vec::new() };
        let opts = preview::Options {
            width: w.saturating_sub(1).min(u16::MAX as usize) as u16,
            show_hidden: self.cfg.show_hidden,
            order: self.cfg.order,
        };
        let p: Option<Arc<Preview>> = self.previewer.get(&e.path, e.size, e.modified, opts);
        let Some(p) = p else {
            return vec![vec![Span { text: fit(" loading...", w), style: Style::fg(Color::Grey) }]];
        };
        self.preview_scroll = self.preview_scroll.min(p.lines.len().saturating_sub(1));
        p.lines
            .iter()
            .skip(self.preview_scroll)
            .take(rows)
            .map(|l| {
                let style = match l.tone {
                    Tone::Plain => Style::PLAIN,
                    Tone::Header => Style::fg(Color::Cyan).bold(),
                    Tone::Dim => Style::fg(Color::Grey),
                    Tone::Dir => Style::fg(Color::Blue).bold(),
                    Tone::Link => Style::fg(Color::Cyan),
                    Tone::Exec => Style::fg(Color::Green),
                    Tone::Error => Style::fg(Color::Red),
                };
                vec![Span { text: fit(&format!(" {}", l.text), w), style }]
            })
            .collect()
    }

    fn help_body(&mut self, width: usize, rows: usize, lines: &mut Vec<Vec<Span>>) {
        let mut text: Vec<(String, Style)> = vec![("Keys (q or Esc closes, j and k scroll)".into(), Style::PLAIN.bold())];
        for &a in Action::ALL {
            let keys = self.cfg.keys_for(a).join(", ");
            if keys.is_empty() {
                continue;
            }
            text.push((format!("  {keys:<22} {}", a.help()), Style::PLAIN));
        }
        text.push((String::new(), Style::PLAIN));
        text.push((
            "In prompts: Enter accepts, Esc cancels, ctrl-u clears, ctrl-w deletes a word.".into(),
            Style::fg(Color::Grey),
        ));
        let cfg = match &self.cfg.dir {
            Some(d) => format!("Configuration: {}", d.join("config.toml").display()),
            None => "Configuration: none (built-in defaults)".into(),
        };
        text.push((cfg, Style::fg(Color::Grey)));
        self.help_top = self.help_top.min(text.len().saturating_sub(rows));
        for (t, st) in text.iter().skip(self.help_top).take(rows) {
            lines.push(vec![Span { text: fit(t, width), style: *st }]);
        }
        while lines.len() < rows + 1 {
            lines.push(vec![Span { text: " ".repeat(width), style: Style::PLAIN }]);
        }
    }

    fn results_body(&mut self, width: usize, rows: usize, lines: &mut Vec<Vec<Span>>) {
        let Some(s) = &self.search else { return };
        let state = if s.finished { format!("done in {:.2} s", s.elapsed.as_secs_f64()) } else { "searching".into() };
        let limit = if s.hits.len() >= find::LIMIT { format!(" (stopped at {})", find::LIMIT) } else { String::new() };
        let head = format!(
            "'{}': {} found{limit}, {} folders, {state}  (Enter goes there, Esc returns)",
            s.pattern,
            s.hits.len(),
            s.dirs_scanned
        );
        lines.push(vec![Span { text: fit(&head, width), style: Style::fg(Color::Cyan) }]);
        let rows = rows.saturating_sub(1);
        let n = s.hits.len();
        self.found_cursor = self.found_cursor.min(n.saturating_sub(1));
        if self.found_cursor < self.found_top {
            self.found_top = self.found_cursor;
        }
        if self.found_cursor >= self.found_top + rows {
            self.found_top = self.found_cursor + 1 - rows;
        }
        for (i, h) in s.hits.iter().enumerate().skip(self.found_top).take(rows) {
            let rel = h.path.strip_prefix(&s.root).unwrap_or(&h.path).display().to_string();
            let mut st = if h.is_dir { Style::fg(Color::Blue).bold() } else { Style::PLAIN };
            if i == self.found_cursor {
                st = st.reverse();
            }
            lines.push(vec![Span { text: fit(&format!(" {rel}"), width), style: st }]);
        }
        while lines.len() < rows + 2 {
            lines.push(vec![Span { text: " ".repeat(width), style: Style::PLAIN }]);
        }
    }

    fn bookmarks_body(&self, width: usize, rows: usize, lines: &mut Vec<Vec<Span>>) {
        lines.push(vec![Span {
            text: fit("Bookmarks (press a key to jump, Esc closes; m sets one)", width),
            style: Style::PLAIN.bold(),
        }]);
        if self.bookmarks.marks.is_empty() {
            lines.push(vec![Span { text: fit("  none yet", width), style: Style::fg(Color::Grey) }]);
        }
        for (c, p) in self.bookmarks.marks.iter().take(rows.saturating_sub(1)) {
            lines.push(vec![Span { text: fit(&format!("  {c}  {}", p.display()), width), style: Style::PLAIN }]);
        }
        while lines.len() < rows + 1 {
            lines.push(vec![Span { text: " ".repeat(width), style: Style::PLAIN }]);
        }
    }
}

fn kind_style(e: &Entry) -> Style {
    match e.kind {
        Kind::Dir => Style::fg(Color::Blue).bold(),
        Kind::Link { broken: true, .. } => Style::fg(Color::Red),
        Kind::Link { .. } => Style::fg(Color::Cyan),
        Kind::Other => Style::fg(Color::Magenta),
        Kind::File if e.executable => Style::fg(Color::Green),
        Kind::File if e.hidden => Style::fg(Color::Grey),
        Kind::File => Style::PLAIN,
    }
}

/// The status line text for an entry: what it is, size, time, flags.
fn describe(e: &Entry) -> String {
    let mut s = match e.kind {
        Kind::Dir => "folder".to_string(),
        Kind::File => human_size(e.size),
        Kind::Link { to_dir: true, .. } => "link to a folder".to_string(),
        Kind::Link { broken: true, .. } => "broken link".to_string(),
        Kind::Link { .. } => format!("link, {}", human_size(e.size)),
        Kind::Other => "special file".to_string(),
    };
    if let Some(t) = e.modified {
        let _ = write!(s, "  {}", entry::format_time(t));
    }
    if e.readonly {
        s.push_str("  read-only");
    }
    if e.hidden {
        s.push_str("  hidden");
    }
    s
}

fn name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}

/// The parent folder; for a drive root on Windows that is the drive list (an empty path).
fn parent_of(p: &Path) -> Option<PathBuf> {
    if p.as_os_str().is_empty() {
        return None;
    }
    match p.parent() {
        Some(up) => Some(up.to_path_buf()),
        None if cfg!(windows) => Some(PathBuf::new()),
        None => None,
    }
}

/// "C:" for "C:\".
fn drive_name(p: &Path) -> String {
    p.display().to_string().trim_end_matches(['\\', '/']).to_string()
}

fn dir_stamp(p: &Path) -> Option<SystemTime> {
    fs::metadata(p).and_then(|m| m.modified()).ok()
}

#[cfg(windows)]
fn drives() -> Vec<Entry> {
    // SAFETY: no arguments; returns a bit mask of drive letters.
    let mask = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
    (0..26)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| {
            let name = format!("{}:", (b'A' + i as u8) as char);
            Entry {
                path: PathBuf::from(format!("{name}\\")),
                name,
                kind: Kind::Dir,
                size: 0,
                modified: None,
                hidden: false,
                readonly: false,
                executable: false,
            }
        })
        .collect()
}

#[cfg(not(windows))]
fn drives() -> Vec<Entry> {
    Vec::new()
}

/// Cuts or pads `s` to exactly `w` columns, marking a cut with "~" at the end.
pub fn fit(s: &str, w: usize) -> String {
    let total = s.width();
    if total <= w {
        return format!("{s}{}", " ".repeat(w - total));
    }
    if w == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if used + cw > w - 1 {
            break;
        }
        out.push(c);
        used += cw;
    }
    out.push('~');
    used += 1;
    out.push_str(&" ".repeat(w - used));
    out
}

/// Like [`fit`] but keeps the end of the text (for paths: the folder you are in matters most).
fn fit_left(s: &str, w: usize) -> String {
    let total = s.width();
    if total <= w {
        return format!("{s}{}", " ".repeat(w - total));
    }
    if w == 0 {
        return String::new();
    }
    let mut tail: Vec<char> = Vec::new();
    let mut used = 1;
    for c in s.chars().rev() {
        let cw = c.width().unwrap_or(0);
        if used + cw > w {
            break;
        }
        tail.push(c);
        used += cw;
    }
    let mut out = String::from("~");
    out.extend(tail.into_iter().rev());
    out.push_str(&" ".repeat(w - used));
    out
}

/// Cuts without padding.
fn fit_left_cut(s: &str, w: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if used + cw > w {
            break;
        }
        out.push(c);
        used += cw;
    }
    out
}

/// Pads a line of spans with spaces to `width` columns.
fn pad(mut spans: Vec<Span>, width: usize) -> Vec<Span> {
    let used: usize = spans.iter().map(|s| s.text.width()).sum();
    if used < width {
        spans.push(Span { text: " ".repeat(width - used), style: Style::PLAIN });
    }
    spans
}

/// Replays comma-separated key names (see [`config::parse_key`]; `text:abc` types the characters
/// of `abc`) and returns the screen that results. Background work is waited for after every key,
/// so the result does not depend on timing. Effects (opening files) are not run; the status line
/// shows what would have been opened.
pub fn run_script(app: &mut App, keys: &str, width: usize, height: usize) -> Result<Screen, String> {
    app.render(width, height);
    app.settle();
    for token in keys.split(',').filter(|t| !t.is_empty()) {
        if let Some(text) = token.strip_prefix("text:") {
            for c in text.chars() {
                app.handle_key(Key::Char(c));
            }
        } else {
            let t = if token.chars().count() == 1 { token } else { token.trim() };
            let key = config::parse_key(t).ok_or_else(|| format!("unknown key '{}'", token.trim()))?;
            app.handle_key(key);
        }
        if let Some(e) = app.effect.take() {
            let (verb, p) = match e {
                Effect::Open(p) => ("open", p),
                Effect::Edit(p) => ("edit", p),
            };
            app.info(format!("would {verb} {}", p.display()));
        }
        app.settle();
        app.render(width, height);
        app.settle();
        if app.quit {
            break;
        }
    }
    Ok(app.render(width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_cuts_by_display_width() {
        assert_eq!(fit("abc", 5), "abc  ");
        assert_eq!(fit("abcdef", 4), "abc~");
        assert_eq!(fit("日本語", 4), "日~ ");
        assert_eq!(fit_left("C:\\Users\\me\\proj", 8), "~me\\proj");
    }

    #[test]
    fn input_editing() {
        let mut i = Input::default();
        i.set("hello world");
        i.edit(Key::Ctrl('w'));
        assert_eq!(i.string(), "hello ");
        i.edit(Key::Home);
        i.edit(Key::Char('>'));
        assert_eq!(i.string(), ">hello ");
        i.edit(Key::End);
        i.edit(Key::Backspace);
        assert_eq!(i.string(), ">hello");
        i.pos = 3;
        i.edit(Key::Ctrl('u'));
        assert_eq!(i.string(), "llo");
    }
}
