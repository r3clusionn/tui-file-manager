//! Prints the preview fm would show for each path, read one per line from standard input.
//! `scripts/check_previews.py` uses it to compare previews with Python's zipfile and Pillow.
//!
//!     dir /s /b *.zip | cargo run --release --example preview

use std::io::{self, BufRead, Write};
use std::path::Path;

use filemgr::entry::SortOrder;
use filemgr::preview::{build, Options};

fn main() {
    let opts = Options { width: 200, show_hidden: true, order: SortOrder::default() };
    let out = io::stdout();
    let mut out = out.lock();
    for line in io::stdin().lock().lines() {
        let path = line.expect("stdin");
        if path.is_empty() {
            continue;
        }
        let p = build(Path::new(&path), &opts);
        writeln!(out, "== {path}").unwrap();
        for l in &p.lines {
            writeln!(out, "{:?}\t{}", l.tone, l.text).unwrap();
        }
    }
}
