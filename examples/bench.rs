//! Measurements for the README.
//!
//!     cargo run --release --example bench -- dir N        open, draw and filter a folder of N files
//!     cargo run --release --example bench -- copy SRC DEST   copy SRC into the folder DEST with fm's job code

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use filemgr::app::{run_script, App};
use filemgr::config::{Config, Key};
use filemgr::ops::{self, Op, Request};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("dir") => dir(args.get(1).and_then(|n| n.parse().ok()).unwrap_or(100_000)),
        Some("copy") => copy(Path::new(&args[1]), Path::new(&args[2])),
        _ => eprintln!("usage: bench dir N | bench copy SRC DEST"),
    }
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

fn dir(n: usize) {
    let root = std::env::temp_dir().join(format!("fm-bench-{n}"));
    if !root.join("done").exists() {
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("big")).unwrap();
        for i in 0..n {
            fs::write(root.join("big").join(format!("file{i}.dat")), []).unwrap();
        }
        fs::write(root.join("done"), "").unwrap();
    }
    let big = root.join("big");
    // Open: list, sort and draw the first frame (the parent column lists the bench root).
    let mut opens = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        let mut app = App::new(&big, Config::default()).unwrap();
        app.render(160, 50);
        opens.push(t.elapsed());
    }
    let mut app = App::new(&big, Config::default()).unwrap();
    app.render(160, 50);
    app.settle();
    // Key and frame: move the cursor and draw; previews are cached after the first visit.
    let mut frames = Vec::new();
    for i in 0..400 {
        let t = Instant::now();
        app.handle_key(if i % 200 < 100 { Key::Down } else { Key::Up });
        app.render(160, 50);
        frames.push(t.elapsed());
    }
    // Filtering: each typed character filters all entries again.
    let mut filters = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        run_script(&mut app, "/,text:9999,esc", 160, 50).unwrap();
        filters.push(t.elapsed() / 6);
    }
    println!("folder of {n} files");
    println!("  open (read, sort, first frame): median {:.1} ms of 5", median(opens).as_secs_f64() * 1e3);
    println!(
        "  key + frame (160x50): median {:.3} ms, worst {:.3} ms of 400",
        median(frames.clone()).as_secs_f64() * 1e3,
        frames.iter().max().unwrap().as_secs_f64() * 1e3
    );
    println!("  filter, per key: median {:.1} ms of 5", median(filters).as_secs_f64() * 1e3);
}

fn copy(src: &Path, dest: &Path) {
    let cancel = AtomicBool::new(false);
    let req = Request { op: Op::Copy, sources: vec![PathBuf::from(src)], dest: dest.to_path_buf() };
    let t = Instant::now();
    let o = ops::run(&req, &cancel, &mut |_| {});
    let secs = t.elapsed().as_secs_f64();
    assert!(o.errors.is_empty(), "{:?}", o.errors);
    println!("{secs:.3}");
}
