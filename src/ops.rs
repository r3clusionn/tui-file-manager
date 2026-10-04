//! File operations: copy, move, delete and move to the trash, run one job at a time on a
//! background thread with progress reports and cancellation; plus the quick ones (rename, create)
//! that run directly.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Copy,
    Move,
    Delete,
    Trash,
}

impl Op {
    pub fn verb(self) -> &'static str {
        match self {
            Op::Copy => "copying",
            Op::Move => "moving",
            Op::Delete => "deleting",
            Op::Trash => "moving to trash",
        }
    }

    pub fn noun(self) -> &'static str {
        match self {
            Op::Copy => "copy",
            Op::Move => "move",
            Op::Delete => "delete",
            Op::Trash => "move to the trash",
        }
    }

    /// "copied 3 items", "moved 1 item to the trash".
    pub fn summary(self, items: &str) -> String {
        match self {
            Op::Copy => format!("copied {items}"),
            Op::Move => format!("moved {items}"),
            Op::Delete => format!("deleted {items}"),
            Op::Trash => format!("moved {items} to the trash"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Request {
    pub op: Op,
    pub sources: Vec<PathBuf>,
    /// The directory to copy or move into; unused for deletes.
    pub dest: PathBuf,
}

#[derive(Clone, Debug, Default)]
pub struct Progress {
    pub bytes: u64,
    pub total_bytes: u64,
    pub items: u64,
    pub total_items: u64,
    pub current: String,
}

#[derive(Clone, Debug)]
pub struct Outcome {
    pub op: Op,
    /// Top-level items that were handled (the new paths for copy and move).
    pub done: Vec<PathBuf>,
    pub errors: Vec<String>,
    pub cancelled: bool,
    pub bytes: u64,
    pub elapsed: Duration,
}

pub enum Event {
    Progress(Op, Progress),
    Done(Outcome),
}

/// The background job runner.
pub struct Jobs {
    tx: Sender<Request>,
    rx: Receiver<Event>,
    cancel: Arc<AtomicBool>,
    pending: usize,
    pub current: Option<(Op, Progress)>,
}

impl Default for Jobs {
    fn default() -> Jobs {
        Jobs::new()
    }
}

impl Jobs {
    pub fn new() -> Jobs {
        let (tx, job_rx) = mpsc::channel::<Request>();
        let (ev_tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let c = Arc::clone(&cancel);
        thread::spawn(move || {
            for req in job_rx {
                c.store(false, Ordering::SeqCst);
                let op = req.op;
                let tx = ev_tx.clone();
                let mut last: Option<Instant> = None;
                let out = run(&req, &c, &mut |p: &Progress| {
                    // A report every 50 ms is plenty for a status line.
                    if last.is_none_or(|t| t.elapsed() >= Duration::from_millis(50)) {
                        last = Some(Instant::now());
                        let _ = tx.send(Event::Progress(op, p.clone()));
                    }
                });
                if ev_tx.send(Event::Done(out)).is_err() {
                    return;
                }
            }
        });
        Jobs { tx, rx, cancel, pending: 0, current: None }
    }

    pub fn submit(&mut self, req: Request) {
        self.pending += 1;
        if self.current.is_none() {
            self.current = Some((req.op, Progress::default()));
        }
        let _ = self.tx.send(req);
    }

    pub fn busy(&self) -> bool {
        self.pending > 0
    }

    /// Cancels the running job. Files already finished stay; a file half copied is removed.
    pub fn cancel(&self) {
        if self.busy() {
            self.cancel.store(true, Ordering::SeqCst);
        }
    }

    /// Returns finished jobs, updating `current` from progress reports.
    pub fn poll(&mut self) -> Vec<Outcome> {
        let mut out = Vec::new();
        while let Ok(ev) = self.rx.try_recv() {
            self.handle(ev, &mut out);
        }
        out
    }

    /// Waits until every submitted job has finished, or `timeout` passes.
    pub fn wait(&mut self, timeout: Duration) -> Vec<Outcome> {
        let end = Instant::now() + timeout;
        let mut out = Vec::new();
        while self.pending > 0 {
            match self.rx.recv_timeout(end.saturating_duration_since(Instant::now())) {
                Ok(ev) => self.handle(ev, &mut out),
                Err(_) => break,
            }
        }
        out
    }

    fn handle(&mut self, ev: Event, out: &mut Vec<Outcome>) {
        match ev {
            Event::Progress(op, p) => self.current = Some((op, p)),
            Event::Done(o) => {
                self.pending -= 1;
                self.current = if self.pending > 0 { Some((o.op, Progress::default())) } else { None };
                out.push(o);
            }
        }
    }
}

/// Runs one request to completion on the calling thread.
pub fn run(req: &Request, cancel: &AtomicBool, report: &mut dyn FnMut(&Progress)) -> Outcome {
    let start = Instant::now();
    let mut ctx = Ctx { cancel, report, p: Progress::default(), errors: Vec::new() };
    let mut done = Vec::new();
    match req.op {
        Op::Copy | Op::Move => {
            if req.op == Op::Copy {
                for s in &req.sources {
                    ctx.count(s);
                }
            }
            for src in &req.sources {
                if ctx.cancelled() {
                    break;
                }
                if let Some(d) = transfer(req.op, src, &req.dest, &mut ctx) {
                    done.push(d);
                }
            }
        }
        Op::Delete => {
            for s in &req.sources {
                ctx.count(s);
            }
            for src in &req.sources {
                if ctx.cancelled() {
                    break;
                }
                match remove_tree(src, &mut ctx) {
                    Ok(()) => done.push(src.clone()),
                    Err(e) => ctx.errors.push(format!("{}: {e}", src.display())),
                }
            }
        }
        Op::Trash => {
            ctx.p.total_items = req.sources.len() as u64;
            // One call for all items: on Windows the shell does them as one operation.
            match trash::delete_all(&req.sources) {
                Ok(()) => done.extend(req.sources.iter().cloned()),
                Err(e) => ctx.errors.push(e.to_string()),
            }
            ctx.p.items = ctx.p.total_items;
        }
    }
    let cancelled = cancel.load(Ordering::SeqCst);
    Outcome { op: req.op, done, errors: ctx.errors, cancelled, bytes: ctx.p.bytes, elapsed: start.elapsed() }
}

struct Ctx<'a> {
    cancel: &'a AtomicBool,
    report: &'a mut dyn FnMut(&Progress),
    p: Progress,
    errors: Vec<String>,
}

impl Ctx<'_> {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// Adds a tree's files and bytes to the totals (without following links).
    fn count(&mut self, path: &Path) {
        let Ok(meta) = fs::symlink_metadata(path) else { return };
        self.p.total_items += 1;
        if meta.is_dir() {
            if let Ok(rd) = fs::read_dir(path) {
                for e in rd.flatten() {
                    self.count(&e.path());
                }
            }
        } else if meta.is_file() {
            self.p.total_bytes += meta.len();
        }
    }

    fn item_done(&mut self) {
        self.p.items += 1;
        (self.report)(&self.p);
    }
}

/// Copies or moves one top-level item into `dest_dir`, returning where it ended up.
fn transfer(op: Op, src: &Path, dest_dir: &Path, ctx: &mut Ctx) -> Option<PathBuf> {
    let Some(name) = src.file_name() else {
        ctx.errors.push(format!("{}: cannot {} a root", src.display(), if op == Op::Copy { "copy" } else { "move" }));
        return None;
    };
    if is_inside(dest_dir, src) {
        ctx.errors.push(format!("{}: cannot put a folder inside itself", src.display()));
        return None;
    }
    if op == Op::Move && src.parent().is_some_and(|p| same_path(p, dest_dir)) {
        return Some(src.to_path_buf()); // already there
    }
    let dst = unique_path(dest_dir, &name.to_string_lossy());
    ctx.p.current = name.to_string_lossy().into_owned();
    if op == Op::Move {
        match fs::rename(src, &dst) {
            Ok(()) => {
                ctx.item_done();
                return Some(dst);
            }
            Err(e) if is_cross_device(&e) => {
                // Different volume: copy, then delete the source if everything arrived.
                ctx.count(src);
                let before = ctx.errors.len();
                copy_tree(src, &dst, ctx);
                if ctx.errors.len() == before && !ctx.cancelled() {
                    if let Err(e) = remove_tree(
                        src,
                        &mut Ctx { cancel: ctx.cancel, report: &mut |_| {}, p: Progress::default(), errors: Vec::new() },
                    ) {
                        ctx.errors.push(format!("{}: copied, but removing the original failed: {e}", src.display()));
                    }
                    return Some(dst);
                }
                return None;
            }
            Err(e) => {
                ctx.errors.push(format!("{}: {e}", src.display()));
                return None;
            }
        }
    }
    let before = ctx.errors.len();
    copy_tree(src, &dst, ctx);
    (ctx.errors.len() == before && !ctx.cancelled()).then_some(dst)
}

fn copy_tree(src: &Path, dst: &Path, ctx: &mut Ctx) {
    if ctx.cancelled() {
        return;
    }
    let meta = match fs::symlink_metadata(src) {
        Ok(m) => m,
        Err(e) => return ctx.errors.push(format!("{}: {e}", src.display())),
    };
    if meta.file_type().is_symlink() {
        if let Err(e) = copy_link(src, dst) {
            ctx.errors.push(format!("{}: copying the link failed: {e}", src.display()));
        }
        return ctx.item_done();
    }
    if meta.is_dir() {
        if let Err(e) = fs::create_dir(dst) {
            return ctx.errors.push(format!("{}: {e}", dst.display()));
        }
        ctx.item_done();
        let entries = match fs::read_dir(src) {
            Ok(rd) => rd,
            Err(e) => return ctx.errors.push(format!("{}: {e}", src.display())),
        };
        for e in entries {
            match e {
                Ok(e) => copy_tree(&e.path(), &dst.join(e.file_name()), ctx),
                Err(e) => ctx.errors.push(format!("{}: {e}", src.display())),
            }
            if ctx.cancelled() {
                return;
            }
        }
        return;
    }
    ctx.p.current = src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    match copy_file(src, dst, ctx) {
        Ok(()) => ctx.item_done(),
        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
        Err(e) => ctx.errors.push(format!("{}: {e}", src.display())),
    }
}

#[cfg(unix)]
fn copy_link(src: &Path, dst: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(fs::read_link(src)?, dst)
}

/// On Windows a link is made as a file link or a folder link, and its own attributes say which
/// (even when the target is missing). Unix has one kind.
#[cfg(windows)]
fn is_dir_link(meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    meta.file_attributes() & 0x10 != 0 // FILE_ATTRIBUTE_DIRECTORY
}

#[cfg(not(windows))]
fn is_dir_link(_: &fs::Metadata) -> bool {
    false
}

#[cfg(windows)]
fn copy_link(src: &Path, dst: &Path) -> io::Result<()> {
    let target = fs::read_link(src)?;
    if is_dir_link(&fs::symlink_metadata(src)?) {
        std::os::windows::fs::symlink_dir(target, dst)
    } else {
        std::os::windows::fs::symlink_file(target, dst)
    }
}

/// Copies one file, reporting bytes as they go. On Windows this is `CopyFileExW` (which keeps
/// attributes, times and alternate streams and uses the system's fast paths); elsewhere a 1 MiB
/// buffered loop. Either way a cancelled copy leaves no partial file.
fn copy_file(src: &Path, dst: &Path, ctx: &mut Ctx) -> io::Result<()> {
    #[cfg(windows)]
    {
        win_copy(src, dst, ctx)
    }
    #[cfg(not(windows))]
    {
        buffered_copy(src, dst, ctx)
    }
}

#[cfg_attr(windows, allow(dead_code))]
fn buffered_copy(src: &Path, dst: &Path, ctx: &mut Ctx) -> io::Result<()> {
    let mut from = fs::File::open(src)?;
    let meta = from.metadata()?;
    let mut to = fs::OpenOptions::new().write(true).create_new(true).open(dst)?;
    let result = (|| {
        let mut buf = vec![0u8; 1 << 20];
        // Report after the first chunk, then every 50 ms.
        let mut last: Option<Instant> = None;
        loop {
            let n = match from.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            to.write_all(&buf[..n])?;
            ctx.p.bytes += n as u64;
            if ctx.cancelled() {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            if last.is_none_or(|t| t.elapsed() >= Duration::from_millis(50)) {
                last = Some(Instant::now());
                (ctx.report)(&ctx.p);
            }
        }
        to.set_permissions(meta.permissions())?;
        if let Ok(t) = meta.modified() {
            to.set_modified(t)?;
        }
        Ok(())
    })();
    if result.is_err() {
        drop(to);
        let _ = fs::remove_file(dst);
    }
    result
}

#[cfg(windows)]
fn win_copy(src: &Path, dst: &Path, ctx: &mut Ctx) -> io::Result<()> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{CopyFileExW, COPY_FILE_FAIL_IF_EXISTS};

    struct Data<'a, 'b> {
        ctx: &'a mut Ctx<'b>,
        base: u64,
        last: Option<Instant>,
    }

    unsafe extern "system" fn progress(
        _total: i64,
        transferred: i64,
        _stream_size: i64,
        _stream_done: i64,
        _stream: u32,
        _reason: u32,
        _src: *mut c_void,
        _dst: *mut c_void,
        data: *const c_void,
    ) -> u32 {
        // SAFETY: `data` is the `Data` passed to CopyFileExW below, alive for the whole call.
        let d = unsafe { &mut *(data as *mut Data) };
        d.ctx.p.bytes = d.base + transferred as u64;
        // Report after the first chunk, then every 50 ms. (The first call, for the start of the
        // stream, has moved no bytes yet.)
        if transferred > 0 && d.last.is_none_or(|t| t.elapsed() >= Duration::from_millis(50)) {
            d.last = Some(Instant::now());
            (d.ctx.report)(&d.ctx.p);
        }
        if d.ctx.cancelled() {
            1 // PROGRESS_CANCEL: the system deletes the partial file
        } else {
            0 // PROGRESS_CONTINUE
        }
    }

    let wide = |p: &Path| p.as_os_str().encode_wide().chain([0]).collect::<Vec<u16>>();
    let (s, d) = (wide(src), wide(dst));
    let base = ctx.p.bytes;
    let mut data = Data { ctx, base, last: None };
    // SAFETY: both strings are NUL-terminated and outlive the call; `data` is only used by the
    // callback during the call.
    let ok = unsafe {
        CopyFileExW(
            s.as_ptr(),
            d.as_ptr(),
            Some(progress),
            &mut data as *mut Data as *const c_void,
            std::ptr::null_mut(),
            COPY_FILE_FAIL_IF_EXISTS,
        )
    };
    if ok == 0 {
        let e = io::Error::last_os_error();
        // ERROR_REQUEST_ABORTED: we cancelled it.
        if e.raw_os_error() == Some(1235) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        return Err(e);
    }
    Ok(())
}

/// Deletes a file, a link (never what it points at) or a whole directory tree. Read-only files
/// are deleted too, as a file manager's delete is expected to do.
fn remove_tree(path: &Path, ctx: &mut Ctx) -> io::Result<()> {
    if ctx.cancelled() {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
    }
    let meta = fs::symlink_metadata(path)?;
    let ft = meta.file_type();
    if ft.is_dir() && !ft.is_symlink() {
        for e in fs::read_dir(path)? {
            remove_tree(&e?.path(), ctx)?;
        }
        retry_writable(path, || fs::remove_dir(path))?;
    } else if ft.is_symlink() && is_dir_link(&meta) {
        // A directory symlink or junction on Windows is removed as a directory (which removes the
        // link, never what it points at).
        fs::remove_dir(path)?;
    } else {
        ctx.p.bytes += if ft.is_file() { meta.len() } else { 0 };
        retry_writable(path, || fs::remove_file(path))?;
    }
    ctx.p.current = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    ctx.item_done();
    Ok(())
}

fn retry_writable(path: &Path, f: impl Fn() -> io::Result<()>) -> io::Result<()> {
    match f() {
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            let mut perm = fs::symlink_metadata(path)?.permissions();
            if !perm.readonly() {
                return Err(e);
            }
            #[allow(clippy::permissions_set_readonly_false)]
            perm.set_readonly(false);
            fs::set_permissions(path, perm)?;
            f()
        }
        r => r,
    }
}

fn is_cross_device(e: &io::Error) -> bool {
    // ERROR_NOT_SAME_DEVICE on Windows, EXDEV on Unix.
    e.raw_os_error() == Some(if cfg!(windows) { 17 } else { 18 }) || e.kind() == io::ErrorKind::CrossesDevices
}

/// A name in `dir` that does not exist yet: `name`, else `name (1)`, `name (2)` and so on, with
/// the number before the extension ("report (1).txt").
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    if fs::symlink_metadata(&p).is_err() {
        return p;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 && !dir.join(name).is_dir() => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (1..).map(|n| dir.join(format!("{stem} ({n}){ext}"))).find(|p| fs::symlink_metadata(p).is_err()).unwrap()
}

fn norm(p: &Path) -> String {
    let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let s = abs.to_string_lossy().trim_end_matches(['/', '\\']).to_string();
    if cfg!(windows) {
        s.replace('/', "\\").to_lowercase()
    } else {
        s
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    norm(a) == norm(b)
}

/// True if `inner` is `outer` or lies under it.
fn is_inside(inner: &Path, outer: &Path) -> bool {
    let (i, o) = (norm(inner), norm(outer));
    let sep = if cfg!(windows) { '\\' } else { '/' };
    i == o || i.starts_with(&format!("{o}{sep}"))
}

/// Renames within the same directory. Refuses to replace another file; on Windows a change of
/// case only ("a.txt" to "A.txt") is allowed.
pub fn rename(path: &Path, new_name: &str) -> io::Result<PathBuf> {
    check_name(new_name)?;
    let dst = path.parent().unwrap_or(Path::new("")).join(new_name);
    let case_only =
        cfg!(windows) && path.file_name().map(|n| n.to_string_lossy().to_lowercase()) == Some(new_name.to_lowercase());
    if !case_only && fs::symlink_metadata(&dst).is_ok() {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("{new_name} already exists")));
    }
    fs::rename(path, &dst)?;
    Ok(dst)
}

/// Creates a file, or a directory when the name ends with `/` or `\`. Intermediate directories in
/// the name are created as needed ("src/bin/main.rs").
pub fn create(dir: &Path, name: &str) -> io::Result<PathBuf> {
    let is_dir = name.ends_with(['/', '\\']);
    let trimmed = name.trim_end_matches(['/', '\\']);
    for part in trimmed.split(['/', '\\']) {
        check_name(part)?;
    }
    let path = dir.join(trimmed);
    if fs::symlink_metadata(&path).is_ok() {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("{trimmed} already exists")));
    }
    if is_dir {
        fs::create_dir_all(&path)?;
    } else {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::OpenOptions::new().write(true).create_new(true).open(&path)?;
    }
    Ok(path)
}

fn check_name(name: &str) -> io::Result<()> {
    let bad = |m: &str| Err(io::Error::new(io::ErrorKind::InvalidInput, m.to_string()));
    if name.is_empty() || name == "." || name == ".." {
        return bad("not a usable name");
    }
    if name.contains(['/', '\\']) || name.contains('\0') {
        return bad("a name cannot contain / or \\");
    }
    if cfg!(windows) && name.contains(['<', '>', ':', '"', '|', '?', '*']) {
        return bad("a name cannot contain < > : \" | ? or * on Windows");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_names() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("a.txt"), "").unwrap();
        fs::write(d.path().join("a (1).txt"), "").unwrap();
        fs::write(d.path().join(".rc"), "").unwrap();
        fs::create_dir(d.path().join("v1.2")).unwrap();
        assert_eq!(unique_path(d.path(), "a.txt"), d.path().join("a (2).txt"));
        assert_eq!(unique_path(d.path(), ".rc"), d.path().join(".rc (1)"));
        assert_eq!(unique_path(d.path(), "v1.2"), d.path().join("v1.2 (1)"));
        assert_eq!(unique_path(d.path(), "new"), d.path().join("new"));
    }

    #[test]
    fn inside() {
        assert!(is_inside(Path::new("/a/b/c"), Path::new("/a/b")));
        assert!(is_inside(Path::new("/a/b"), Path::new("/a/b/")));
        assert!(!is_inside(Path::new("/a/bc"), Path::new("/a/b")));
    }

    #[test]
    fn names() {
        assert!(check_name("ok.txt").is_ok());
        assert!(check_name("..").is_err());
        assert!(check_name("a/b").is_err());
        assert!(check_name("").is_err());
    }
}
