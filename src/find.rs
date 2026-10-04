//! Recursive search by name on a background thread. Results arrive in batches while the walk goes
//! on, so the first matches show up at once in a big tree, and leaving the results cancels it.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::entry::Matcher;

/// At most this many results are kept.
pub const LIMIT: usize = 10_000;

pub struct Hit {
    pub path: PathBuf,
    pub is_dir: bool,
}

enum Msg {
    Hits(Vec<Hit>, u64),
    Done(u64),
}

pub struct Search {
    pub root: PathBuf,
    pub pattern: String,
    pub hits: Vec<Hit>,
    pub dirs_scanned: u64,
    pub finished: bool,
    pub elapsed: Duration,
    rx: Receiver<Msg>,
    stop: Arc<AtomicBool>,
    start: Instant,
}

impl Search {
    /// Starts walking `root`. Hidden entries are skipped (and not descended into) unless
    /// `show_hidden`; links are listed but never followed, so a loop of links cannot trap the walk.
    pub fn start(root: &Path, pattern: &str, show_hidden: bool) -> Search {
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let s = Arc::clone(&stop);
        let matcher = Matcher::new(pattern);
        let root_buf = root.to_path_buf();
        thread::spawn(move || {
            let mut stack = vec![root_buf];
            let mut batch = Vec::new();
            let mut found = 0usize;
            let mut dirs = 0u64;
            let mut last = Instant::now();
            while let Some(dir) = stack.pop() {
                if s.load(Ordering::Relaxed) || found >= LIMIT {
                    break;
                }
                dirs += 1;
                let Ok(rd) = fs::read_dir(&dir) else { continue };
                let mut subdirs = Vec::new();
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let Ok(ft) = e.file_type() else { continue };
                    let meta = e.metadata().ok();
                    if !show_hidden && (name.starts_with('.') || meta.as_ref().is_some_and(hidden_attr)) {
                        continue;
                    }
                    let is_dir = ft.is_dir() && !ft.is_symlink();
                    if matcher.matches(&name) && found < LIMIT {
                        found += 1;
                        batch.push(Hit { path: e.path(), is_dir });
                    }
                    if is_dir && !meta.as_ref().is_some_and(reparse) {
                        subdirs.push(e.path());
                    }
                }
                // Depth first, but visiting a directory's children in name order.
                subdirs.sort_by(|a, b| b.cmp(a));
                stack.extend(subdirs);
                if !batch.is_empty() && last.elapsed() >= Duration::from_millis(30) {
                    last = Instant::now();
                    if tx.send(Msg::Hits(std::mem::take(&mut batch), dirs)).is_err() {
                        return;
                    }
                }
            }
            let _ = tx.send(Msg::Hits(batch, dirs));
            let _ = tx.send(Msg::Done(dirs));
        });
        Search {
            root: root.to_path_buf(),
            pattern: pattern.to_string(),
            hits: Vec::new(),
            dirs_scanned: 0,
            finished: false,
            elapsed: Duration::ZERO,
            rx,
            stop,
            start: Instant::now(),
        }
    }

    /// Takes the results that have arrived. Returns true if anything changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(m) = self.rx.try_recv() {
            changed = true;
            self.apply(m);
        }
        changed
    }

    /// Blocks until the walk has finished, or `timeout` passes.
    pub fn wait(&mut self, timeout: Duration) {
        let end = Instant::now() + timeout;
        while !self.finished {
            match self.rx.recv_timeout(end.saturating_duration_since(Instant::now())) {
                Ok(m) => self.apply(m),
                Err(_) => return,
            }
        }
    }

    fn apply(&mut self, m: Msg) {
        match m {
            Msg::Hits(h, d) => {
                self.hits.extend(h);
                self.dirs_scanned = d;
            }
            Msg::Done(d) => {
                self.dirs_scanned = d;
                self.finished = true;
                self.elapsed = self.start.elapsed();
            }
        }
    }

    pub fn cancel(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for Search {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(windows)]
fn hidden_attr(m: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    m.file_attributes() & 0x2 != 0
}

#[cfg(not(windows))]
fn hidden_attr(_: &fs::Metadata) -> bool {
    false
}

#[cfg(windows)]
fn reparse(m: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    m.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
fn reparse(_: &fs::Metadata) -> bool {
    false
}
