//! Directory entries: reading a directory, sorting it and matching names against a filter.

use std::cmp::Ordering;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Dir,
    File,
    /// A symbolic link (or junction); `to_dir` says whether its target is a directory.
    Link {
        to_dir: bool,
        broken: bool,
    },
    Other,
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub kind: Kind,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub hidden: bool,
    pub readonly: bool,
    pub executable: bool,
}

impl Entry {
    /// True for directories and links to directories: the entries you can enter.
    pub fn is_dir(&self) -> bool {
        matches!(self.kind, Kind::Dir | Kind::Link { to_dir: true, .. })
    }

    pub fn extension(&self) -> &str {
        if self.is_dir() {
            return "";
        }
        match self.name.rfind('.') {
            Some(i) if i > 0 => &self.name[i + 1..],
            _ => "",
        }
    }

    /// Builds the entry for one path from its metadata (not following a final link).
    pub fn from_path(path: &Path) -> io::Result<Entry> {
        let meta = fs::symlink_metadata(path)?;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
        Ok(Entry::from_meta(name, path.to_path_buf(), &meta))
    }

    fn from_meta(name: String, path: PathBuf, meta: &fs::Metadata) -> Entry {
        let ft = meta.file_type();
        let mut size = meta.len();
        let kind = if ft.is_symlink() || is_junction(meta) {
            match fs::metadata(&path) {
                Ok(target) => {
                    if target.is_file() {
                        size = target.len();
                    }
                    Kind::Link { to_dir: target.is_dir(), broken: false }
                }
                Err(_) => Kind::Link { to_dir: false, broken: true },
            }
        } else if ft.is_dir() {
            Kind::Dir
        } else if ft.is_file() {
            Kind::File
        } else {
            Kind::Other
        };
        if matches!(kind, Kind::Dir | Kind::Link { to_dir: true, .. }) {
            size = 0;
        }
        let hidden = name.starts_with('.') || attr_hidden(meta);
        let executable = kind == Kind::File && is_executable(&name, meta);
        Entry {
            name,
            path,
            kind,
            size,
            modified: meta.modified().ok(),
            hidden,
            readonly: meta.permissions().readonly(),
            executable,
        }
    }
}

#[cfg(windows)]
fn attr_hidden(meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    meta.file_attributes() & 0x2 != 0
}

#[cfg(not(windows))]
fn attr_hidden(_: &fs::Metadata) -> bool {
    false
}

#[cfg(windows)]
fn is_junction(meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    // FILE_ATTRIBUTE_REPARSE_POINT on a directory that std did not already report as a symlink.
    meta.file_attributes() & 0x400 != 0 && meta.is_dir()
}

#[cfg(not(windows))]
fn is_junction(_: &fs::Metadata) -> bool {
    false
}

#[cfg(windows)]
fn is_executable(name: &str, _: &fs::Metadata) -> bool {
    let lower = name.to_ascii_lowercase();
    [".exe", ".bat", ".cmd", ".com", ".ps1", ".msi"].iter().any(|e| lower.ends_with(e))
}

#[cfg(unix)]
fn is_executable(_: &str, meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(any(windows, unix)))]
fn is_executable(_: &str, _: &fs::Metadata) -> bool {
    false
}

/// Reads every entry of a directory. Entries whose metadata cannot be read are kept as `Other`
/// with no size, so a listing never silently loses names.
pub fn read_dir(dir: &Path) -> io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for item in fs::read_dir(dir)? {
        let item = item?;
        let name = item.file_name().to_string_lossy().into_owned();
        let path = item.path();
        // On Windows DirEntry::metadata comes from the directory listing itself (no extra open).
        match item.metadata() {
            Ok(meta) => out.push(Entry::from_meta(name, path, &meta)),
            Err(_) => out.push(Entry {
                hidden: name.starts_with('.'),
                name,
                path,
                kind: Kind::Other,
                size: 0,
                modified: None,
                readonly: false,
                executable: false,
            }),
        }
    }
    Ok(out)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SortKey {
    Name,
    Size,
    Modified,
    Extension,
}

impl SortKey {
    pub fn parse(s: &str) -> Option<SortKey> {
        Some(match s {
            "name" => SortKey::Name,
            "size" => SortKey::Size,
            "modified" | "mtime" | "time" => SortKey::Modified,
            "extension" | "ext" => SortKey::Extension,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            SortKey::Name => "name",
            SortKey::Size => "size",
            SortKey::Modified => "modified",
            SortKey::Extension => "extension",
        }
    }

    pub fn next(self) -> SortKey {
        match self {
            SortKey::Name => SortKey::Size,
            SortKey::Size => SortKey::Modified,
            SortKey::Modified => SortKey::Extension,
            SortKey::Extension => SortKey::Name,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SortOrder {
    pub key: SortKey,
    pub reverse: bool,
    pub dirs_first: bool,
}

impl Default for SortOrder {
    fn default() -> SortOrder {
        SortOrder { key: SortKey::Name, reverse: false, dirs_first: true }
    }
}

/// Sorts entries. Size and time sort largest and newest first; ties fall back to the name, so the
/// order is total and does not depend on the order the directory was read in.
pub fn sort(entries: &mut [Entry], order: SortOrder) {
    entries.sort_by(|a, b| {
        if order.dirs_first && a.is_dir() != b.is_dir() {
            return if a.is_dir() { Ordering::Less } else { Ordering::Greater };
        }
        let primary = match order.key {
            SortKey::Name => Ordering::Equal,
            SortKey::Size => b.size.cmp(&a.size),
            SortKey::Modified => b.modified.cmp(&a.modified),
            SortKey::Extension => natural_cmp(a.extension(), b.extension()),
        };
        let o = primary.then_with(|| natural_cmp(&a.name, &b.name));
        if order.reverse {
            o.reverse()
        } else {
            o
        }
    });
}

/// Compares names the way people read them: runs of digits by value ("file9" before "file10"),
/// letters without regard to case, then the exact text as a tiebreak so different names never
/// compare equal.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut x, mut y) = (a.as_bytes(), b.as_bytes());
    while !x.is_empty() && !y.is_empty() {
        if x[0].is_ascii_digit() && y[0].is_ascii_digit() {
            let (nx, rx) = split_digits(x);
            let (ny, ry) = split_digits(y);
            let (tx, ty) = (trim_zeros(nx), trim_zeros(ny));
            // A longer run without leading zeros is the larger number; same length compares as text.
            let o = tx.len().cmp(&ty.len()).then_with(|| tx.cmp(ty));
            if o != Ordering::Equal {
                return o;
            }
            x = rx;
            y = ry;
            continue;
        }
        // Compare one character at a time, folding ASCII case. Multi-byte UTF-8 compares by its
        // bytes, which orders code points correctly.
        let (cx, cy) = (x[0].to_ascii_lowercase(), y[0].to_ascii_lowercase());
        if cx != cy {
            return cx.cmp(&cy);
        }
        x = &x[1..];
        y = &y[1..];
    }
    x.len().cmp(&y.len()).then_with(|| a.cmp(b))
}

fn split_digits(s: &[u8]) -> (&[u8], &[u8]) {
    let n = s.iter().take_while(|c| c.is_ascii_digit()).count();
    s.split_at(n)
}

fn trim_zeros(s: &[u8]) -> &[u8] {
    let n = s.iter().take_while(|&&c| c == b'0').count();
    &s[n..]
}

/// A name filter: case-insensitive unless the pattern has a capital letter ("smart case"), a plain
/// substring unless it contains `*` or `?`, in which case it is a glob over the whole name.
#[derive(Clone, Debug)]
pub struct Matcher {
    pattern: Vec<char>,
    glob: bool,
    fold: bool,
}

impl Matcher {
    pub fn new(pattern: &str) -> Matcher {
        let fold = !pattern.chars().any(char::is_uppercase);
        let glob = pattern.contains(['*', '?']);
        let pattern = if fold { pattern.to_lowercase().chars().collect() } else { pattern.chars().collect() };
        Matcher { pattern, glob, fold }
    }

    pub fn is_empty(&self) -> bool {
        self.pattern.is_empty()
    }

    pub fn matches(&self, name: &str) -> bool {
        let name: Vec<char> = if self.fold { name.to_lowercase().chars().collect() } else { name.chars().collect() };
        if self.glob {
            glob_match(&self.pattern, &name)
        } else {
            self.pattern.is_empty() || name.windows(self.pattern.len()).any(|w| w == self.pattern.as_slice())
        }
    }
}

/// Matches `*` (any run, including empty) and `?` (one character) with the usual greedy
/// backtracking, which is linear in practice and never exponential.
fn glob_match(p: &[char], s: &[char]) -> bool {
    let (mut pi, mut si) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while si < s.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, si));
            pi += 1;
        } else if let Some((sp, ss)) = star {
            pi = sp + 1;
            si = ss + 1;
            star = Some((sp, ss + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// "1.4 KiB" style sizes, at most four significant characters before the unit.
pub fn human_size(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if v < 10.0 {
        format!("{v:.1} {}", UNITS[u])
    } else {
        format!("{:.0} {}", v, UNITS[u])
    }
}

/// Formats a time as local "YYYY-MM-DD HH:MM". The offset from UTC is taken from the system once.
pub fn format_time(t: SystemTime) -> String {
    let secs = match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    } + utc_offset_secs();
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", rem / 3600, rem % 3600 / 60)
}

/// Howard Hinnant's days-to-civil algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(windows)]
fn utc_offset_secs() -> i64 {
    use std::sync::OnceLock;
    use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
    static OFFSET: OnceLock<i64> = OnceLock::new();
    *OFFSET.get_or_init(|| {
        // SAFETY: plain out-parameter call.
        let mut tz: TIME_ZONE_INFORMATION = unsafe { std::mem::zeroed() };
        let r = unsafe { GetTimeZoneInformation(&mut tz) };
        let bias = match r {
            2 => tz.Bias + tz.DaylightBias, // TIME_ZONE_ID_DAYLIGHT
            0xFFFF_FFFF => 0,
            _ => tz.Bias + tz.StandardBias,
        };
        -(bias as i64) * 60
    })
}

#[cfg(not(windows))]
fn utc_offset_secs() -> i64 {
    // Without a time zone library, read the offset that `date` reports once (e.g. "+0200").
    use std::sync::OnceLock;
    static OFFSET: OnceLock<i64> = OnceLock::new();
    *OFFSET.get_or_init(|| {
        let out = std::process::Command::new("date").arg("+%z").output().ok();
        let s = out.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
        if s.len() == 5 {
            let sign = if s.starts_with('-') { -1 } else { 1 };
            let h: i64 = s[1..3].parse().unwrap_or(0);
            let m: i64 = s[3..5].parse().unwrap_or(0);
            sign * (h * 3600 + m * 60)
        } else {
            0
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_order() {
        let mut v = vec!["file10", "File2", "file1", "file02", "a", "B", "file", "file1b", "x100", "x99"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, ["a", "B", "file", "file1", "file1b", "File2", "file02", "file10", "x99", "x100"]);
    }

    #[test]
    fn huge_numbers_do_not_overflow() {
        assert_eq!(natural_cmp("a99999999999999999999999", "a100000000000000000000000"), Ordering::Less);
        assert_eq!(natural_cmp("a007", "a7"), Ordering::Less); // same value, text tiebreak
    }

    #[test]
    fn natural_cmp_is_a_total_order() {
        // Pseudo-random strings from a small alphabet so digits, case and prefixes collide often.
        let alphabet: Vec<char> = "aAb0019.".chars().collect();
        let mut seed = 0x9e3779b97f4a7c15u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let words: Vec<String> =
            (0..300).map(|_| (0..next() % 6).map(|_| alphabet[(next() % alphabet.len() as u64) as usize]).collect()).collect();
        for a in &words {
            for b in &words {
                let ab = natural_cmp(a, b);
                assert_eq!(ab, natural_cmp(b, a).reverse(), "{a:?} {b:?}");
                assert_eq!(ab == Ordering::Equal, a == b, "{a:?} {b:?}");
                for c in words.iter().take(40) {
                    if ab == Ordering::Less && natural_cmp(b, c) == Ordering::Less {
                        assert_eq!(natural_cmp(a, c), Ordering::Less, "{a:?} {b:?} {c:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn matcher() {
        assert!(Matcher::new("read").matches("README.md"));
        assert!(!Matcher::new("Read").matches("README.md"));
        assert!(Matcher::new("*.rs").matches("main.rs"));
        assert!(!Matcher::new("*.rs").matches("main.rsx"));
        assert!(Matcher::new("m?in*").matches("Main.rs"));
        assert!(Matcher::new("*a*b*c*").matches("xxaxxbxxcxx"));
        assert!(!Matcher::new("*a*b*c*").matches("xxcxxbxxaxx"));
        assert!(Matcher::new("").matches("anything"));
        assert!(Matcher::new("ü").matches("Über"));
    }

    #[test]
    fn sizes() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(1536), "1.5 KiB");
        assert_eq!(human_size(10 * 1024 * 1024), "10 MiB");
        assert_eq!(human_size(u64::MAX), "16384 PiB");
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }
}
