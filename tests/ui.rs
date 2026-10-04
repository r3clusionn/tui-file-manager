//! The file manager driven by keys, as a user would, on temporary folders.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use filemgr::app::{run_script, App, Effect};
use filemgr::config::{Config, Key};

fn app(dir: &Path) -> App {
    App::new(dir, Config::default()).unwrap()
}

fn keys(app: &mut App, k: &str) -> String {
    run_script(app, k, 100, 20).unwrap().plain()
}

fn tree() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    fs::create_dir_all(p.join("src/bin")).unwrap();
    fs::create_dir(p.join("docs")).unwrap();
    fs::write(p.join("src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(p.join("file10.txt"), "ten").unwrap();
    fs::write(p.join("file9.txt"), "nine!").unwrap();
    fs::write(p.join("File1.txt"), vec![b'x'; 2000]).unwrap();
    fs::write(p.join(".hidden"), "").unwrap();
    d
}

#[test]
fn listing_is_natural_with_folders_first_and_hidden_files_hidden() {
    let d = tree();
    let mut a = app(d.path());
    assert_eq!(a.names(), ["docs", "src", "File1.txt", "file9.txt", "file10.txt"]);
    keys(&mut a, ".");
    assert_eq!(a.names(), ["docs", "src", ".hidden", "File1.txt", "file9.txt", "file10.txt"]);
    keys(&mut a, ".,s");
    // Size, largest first (folders still first).
    assert_eq!(a.names(), ["docs", "src", "File1.txt", "file9.txt", "file10.txt"]);
    keys(&mut a, "S");
    // Reversed: smallest first, and folders stay first (in reverse order too).
    assert_eq!(a.names(), ["src", "docs", "file10.txt", "file9.txt", "File1.txt"]);
}

#[test]
fn entering_and_leaving_folders_keeps_the_cursor() {
    let d = tree();
    let mut a = app(d.path());
    keys(&mut a, "j,l");
    assert_eq!(a.cwd, d.path().join("src"));
    assert_eq!(a.names(), ["bin", "main.rs"]);
    keys(&mut a, "j,h");
    assert_eq!(a.cwd, d.path());
    assert_eq!(a.current().unwrap().name, "src", "back on the folder we came from");
    keys(&mut a, "l");
    assert_eq!(a.current().unwrap().name, "main.rs", "the cursor is remembered per folder");
    keys(&mut a, "-");
    assert_eq!(a.cwd, d.path(), "back goes to the previous folder");
}

#[test]
fn filter_narrows_as_you_type() {
    let d = tree();
    let mut a = app(d.path());
    keys(&mut a, "/,text:file");
    assert_eq!(a.names(), ["File1.txt", "file9.txt", "file10.txt"]);
    keys(&mut a, "text:1");
    assert_eq!(a.names(), ["File1.txt", "file10.txt"]);
    keys(&mut a, "esc,/,text:F");
    assert_eq!(a.names(), ["File1.txt"], "a capital letter makes the filter case-sensitive");
    keys(&mut a, "esc,/,text:*9*,enter");
    assert_eq!(a.names(), ["file9.txt"]);
    let screen = keys(&mut a, "");
    assert!(screen.contains("filter: *9*"), "{screen}");
    keys(&mut a, "space,esc");
    assert_eq!(a.names().len(), 5);
    assert_eq!(a.selected().len(), 1, "Esc clears the filter before the selection");
    keys(&mut a, "esc");
    assert!(a.selected().is_empty());
}

#[test]
fn create_rename_and_the_cursor_follows() {
    let d = tree();
    let mut a = app(d.path());
    keys(&mut a, "a,text:notes/2026/todo.md,enter");
    assert!(d.path().join("notes/2026/todo.md").is_file());
    assert_eq!(a.current().unwrap().name, "notes");
    keys(&mut a, "a,text:build/,enter");
    assert!(d.path().join("build").is_dir());
    keys(&mut a, "G,r,end,ctrl-u,text:first.txt,enter");
    assert!(d.path().join("first.txt").exists());
    assert_eq!(a.current().unwrap().name, "first.txt");
    // Renaming onto an existing name is refused and says so.
    let s = keys(&mut a, "r,end,ctrl-u,text:file9.txt,enter");
    assert!(s.contains("already exists"), "{s}");
    assert!(d.path().join("first.txt").exists());
    // The rename prompt starts with the cursor before the extension.
    let s = keys(&mut a, "r,text:-v2,enter");
    assert!(s.contains("renamed to first-v2.txt"), "{s}");
}

#[test]
fn copy_cut_paste_and_selection() {
    let d = tree();
    let mut a = app(d.path());
    // Select the three text files and copy them into docs.
    keys(&mut a, "/,text:.txt,enter,ctrl-a,y,esc,g,l,p");
    assert_eq!(a.cwd, d.path().join("docs"));
    assert_eq!(a.names(), ["File1.txt", "file9.txt", "file10.txt"]);
    assert!(a.message().starts_with("copied 3 items"), "{}", a.message());
    // Paste again: numbered copies, originals untouched.
    keys(&mut a, "p");
    assert_eq!(a.names().len(), 6);
    assert!(a.names().contains(&"file9 (1).txt"));
    // Cut one back to the parent.
    keys(&mut a, "g,x,h,p");
    assert!(d.path().join("File1 (1).txt").exists());
    assert!(!d.path().join("docs/File1.txt").exists() || !d.path().join("docs/File1 (1).txt").exists());
    let screen = keys(&mut a, "");
    assert!(!screen.contains("[cut"), "the clipboard is empty after pasting a cut: {screen}");
}

#[test]
fn delete_asks_first() {
    let d = tree();
    let mut a = app(d.path());
    keys(&mut a, "G,D");
    let s = keys(&mut a, "");
    assert!(s.contains("delete file10.txt permanently? (y/n)"), "{s}");
    keys(&mut a, "n");
    assert!(d.path().join("file10.txt").exists());
    keys(&mut a, "D,y");
    assert!(!d.path().join("file10.txt").exists());
    assert_eq!(a.names(), ["docs", "src", "File1.txt", "file9.txt"]);
    assert_eq!(a.current().unwrap().name, "file9.txt");
}

#[test]
fn find_lists_matches_below_and_jumps_to_one() {
    let d = tree();
    fs::write(d.path().join("src/bin/tool.rs"), "").unwrap();
    fs::create_dir(d.path().join(".git")).unwrap();
    fs::write(d.path().join(".git/config.rs"), "").unwrap();
    let mut a = app(d.path());
    let s = keys(&mut a, "f,text:*.rs,enter");
    assert!(s.contains("'*.rs': 2 found"), "{s}");
    assert!(!s.contains("config.rs"), "hidden folders are skipped: {s}");
    // Depth first: src/main.rs, then src/bin/tool.rs.
    keys(&mut a, "G,enter");
    assert_eq!(a.cwd, d.path().join("src").join("bin"));
    assert_eq!(a.current().unwrap().name, "tool.rs");
}

#[test]
fn bookmarks_are_saved_and_survive_a_restart() {
    let d = tree();
    let cfgdir = tempfile::tempdir().unwrap();
    let cfg = Config { dir: Some(cfgdir.path().to_path_buf()), ..Config::default() };
    let mut a = App::new(&d.path().join("src"), cfg).unwrap();
    keys(&mut a, "m,s,h");
    drop(a);
    let cfg = Config { dir: Some(cfgdir.path().to_path_buf()), ..Config::default() };
    let mut b = App::new(d.path(), cfg).unwrap();
    keys(&mut b, "',s");
    assert_eq!(b.cwd, d.path().join("src"));
    keys(&mut b, "',',");
    assert_eq!(b.cwd, d.path(), "'' goes back");
    let s = keys(&mut b, "',z");
    assert!(s.contains("no bookmark 'z'"), "{s}");
}

#[test]
fn goto_accepts_relative_paths_files_and_completion() {
    let d = tree();
    let mut a = app(&d.path().join("src/bin"));
    keys(&mut a, ":,ctrl-u,text:../..,enter");
    assert_eq!(a.cwd, d.path());
    keys(&mut a, ":,ctrl-u,text:src/main.rs,enter");
    assert_eq!(a.cwd, d.path().join("src"));
    assert_eq!(a.current().unwrap().name, "main.rs");
    // Tab completes a unique folder name.
    keys(&mut a, ":,ctrl-u,text:../do,tab,enter");
    assert_eq!(a.cwd, d.path().join("docs"));
    let s = keys(&mut a, ":,ctrl-u,text:nowhere,enter");
    assert!(s.contains("nowhere"), "{s}");
    assert_eq!(a.cwd, d.path().join("docs"));
}

#[test]
fn enter_on_a_file_asks_to_open_it() {
    let d = tree();
    let mut a = app(d.path());
    keys(&mut a, "G");
    a.handle_key(Key::Enter);
    assert_eq!(a.effect.take(), Some(Effect::Open(d.path().join("file10.txt"))));
    a.handle_key(Key::Char('e'));
    assert_eq!(a.effect.take(), Some(Effect::Edit(d.path().join("file10.txt"))));
}

#[test]
fn slow_previews_do_not_hold_up_keys() {
    let d = tree();
    // Every preview takes 300 ms, as on a slow network share.
    let mut a = App::with_preview_delay(d.path(), Config::default(), Duration::from_millis(300)).unwrap();
    let start = Instant::now();
    for _ in 0..4 {
        a.handle_key(Key::Down);
        let s = a.render(100, 20).plain();
        assert!(s.contains("loading..."), "{s}");
    }
    let took = start.elapsed();
    assert!(took < Duration::from_millis(100), "four keys and frames took {took:?}");
    // The preview for where the cursor stopped arrives without more keys.
    a.settle();
    let s = a.render(100, 20).plain();
    assert!(s.contains("ten"), "{s}");
}

#[test]
fn changes_made_elsewhere_show_up() {
    let d = tree();
    let mut a = app(d.path());
    a.render(100, 20);
    fs::write(d.path().join("new-from-outside.txt"), "hi").unwrap();
    let end = Instant::now() + Duration::from_secs(5);
    while !a.names().contains(&"new-from-outside.txt") && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(100));
        a.tick();
    }
    assert!(a.names().contains(&"new-from-outside.txt"));
}

#[test]
fn a_folder_removed_under_us_climbs_to_its_parent() {
    let d = tree();
    let mut a = app(&d.path().join("src/bin"));
    fs::remove_dir_all(d.path().join("src")).unwrap();
    a.reload();
    assert_eq!(a.cwd, d.path());
}

#[test]
fn config_can_rebind_keys_and_hide_columns() {
    let d = tree();
    let cfg =
        Config::parse("[keys]\nq = \"none\"\nctrl-q = \"quit\"\nn = \"down\"\n[layout]\ncolumns = [0, 1, 0]\nborders = false\n")
            .unwrap();
    let mut a = App::new(d.path(), cfg).unwrap();
    keys(&mut a, "n,q");
    assert!(!a.quit);
    assert_eq!(a.current().unwrap().name, "src");
    let screen = run_script(&mut a, "", 40, 6).unwrap().plain();
    assert!(!screen.contains('│'), "{screen}");
    assert!(screen.lines().nth(1).unwrap().starts_with(" docs"), "{screen}");
    keys(&mut a, "ctrl-q");
    assert!(a.quit);
}

#[test]
fn the_screen() {
    let d = tree();
    let mut a = app(&d.path().join("src"));
    let s = run_script(&mut a, "j", 60, 6).unwrap().plain();
    let lines: Vec<&str> = s.lines().collect();
    let sep = std::path::MAIN_SEPARATOR;
    assert_eq!(lines.len(), 6);
    assert!(lines[0].ends_with(&format!("{sep}src")), "{s}");
    // Parent column, current folder (cursor on main.rs), preview of main.rs.
    assert_eq!(lines[1], format!(" docs  │ bin{sep}                │ fn main() {{}}"));
    assert_eq!(lines[2], " src   │ main.rs        13 B │");
    assert_eq!(lines[3], " File1~│                     │");
    assert!(lines[5].starts_with("13 B  20"), "{s}");
    assert!(lines[5].ends_with("name  2/2"), "{s}");
}
