//! File operations against the real file system.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use filemgr::ops::{self, Op, Outcome, Request};

fn run(op: Op, sources: &[PathBuf], dest: &Path) -> Outcome {
    let cancel = AtomicBool::new(false);
    ops::run(&Request { op, sources: sources.to_vec(), dest: dest.to_path_buf() }, &cancel, &mut |_| {})
}

/// Every file and folder under `root`, as relative path to content (None for folders).
fn tree(root: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
    let mut out = BTreeMap::new();
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Option<Vec<u8>>>) {
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
            if p.is_dir() {
                out.insert(rel, None);
                walk(root, &p, out);
            } else {
                out.insert(rel, Some(fs::read(&p).unwrap()));
            }
        }
    }
    walk(root, root, &mut out);
    out
}

fn sample(root: &Path) {
    fs::create_dir_all(root.join("src/deep/er")).unwrap();
    fs::write(root.join("README.md"), "hello").unwrap();
    fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
    fs::write(root.join("src/deep/er/data.bin"), (0..=255u8).cycle().take(3_000_000).collect::<Vec<_>>()).unwrap();
    fs::create_dir(root.join("empty")).unwrap();
}

#[test]
fn copy_keeps_content_times_and_read_only() {
    let d = tempfile::tempdir().unwrap();
    let src = d.path().join("proj");
    sample(&src);
    let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_500_000_000);
    fs::File::options().write(true).open(src.join("README.md")).unwrap().set_modified(old).unwrap();
    let mut perm = fs::metadata(src.join("src/main.rs")).unwrap().permissions();
    perm.set_readonly(true);
    fs::set_permissions(src.join("src/main.rs"), perm).unwrap();
    let dest = d.path().join("out");
    fs::create_dir(&dest).unwrap();

    let o = run(Op::Copy, std::slice::from_ref(&src), &dest);
    assert!(o.errors.is_empty(), "{:?}", o.errors);
    assert_eq!(o.done, [dest.join("proj")]);
    assert_eq!(o.bytes, 5 + 12 + 3_000_000);
    assert_eq!(tree(&src), tree(&dest.join("proj")));
    assert_eq!(fs::metadata(dest.join("proj/README.md")).unwrap().modified().unwrap(), old);
    assert!(fs::metadata(dest.join("proj/src/main.rs")).unwrap().permissions().readonly());

    // Copying again into the same place makes a numbered copy instead of overwriting.
    let o = run(Op::Copy, std::slice::from_ref(&src), &dest);
    assert_eq!(o.done, [dest.join("proj (1)")]);
    // And deleting a tree with a read-only file in it works.
    let o = run(Op::Delete, &[dest.join("proj"), dest.join("proj (1)")], Path::new(""));
    assert!(o.errors.is_empty(), "{:?}", o.errors);
    assert_eq!(fs::read_dir(&dest).unwrap().count(), 0);
}

#[test]
fn refuses_to_copy_or_move_a_folder_into_itself() {
    let d = tempfile::tempdir().unwrap();
    let src = d.path().join("a");
    sample(&src);
    let before = tree(d.path());
    for op in [Op::Copy, Op::Move] {
        let o = run(op, std::slice::from_ref(&src), &src.join("src"));
        assert_eq!(o.errors.len(), 1);
        assert!(o.errors[0].contains("inside itself"), "{:?}", o.errors);
        let o = run(op, std::slice::from_ref(&src), &src);
        assert_eq!(o.errors.len(), 1);
    }
    assert_eq!(tree(d.path()), before);
}

#[test]
fn moving_into_the_same_folder_does_nothing() {
    let d = tempfile::tempdir().unwrap();
    fs::write(d.path().join("x"), "1").unwrap();
    let o = run(Op::Move, &[d.path().join("x")], d.path());
    assert!(o.errors.is_empty());
    assert_eq!(o.done, [d.path().join("x")]);
    assert_eq!(fs::read_dir(d.path()).unwrap().count(), 1);
}

#[test]
fn cancel_leaves_no_partial_file() {
    let d = tempfile::tempdir().unwrap();
    let big = d.path().join("big.bin");
    fs::write(&big, vec![7u8; 256 << 20]).unwrap();
    fs::write(d.path().join("after.txt"), "x").unwrap();
    let dest = d.path().join("out");
    fs::create_dir(&dest).unwrap();
    let cancel = AtomicBool::new(false);
    let req = Request { op: Op::Copy, sources: vec![big.clone(), d.path().join("after.txt")], dest: dest.clone() };
    // Cancel as soon as the first progress report shows bytes moving.
    let o = ops::run(&req, &cancel, &mut |p| {
        if p.bytes > 0 {
            cancel.store(true, Ordering::SeqCst);
        }
    });
    assert!(o.cancelled);
    assert!(o.bytes < 256 << 20, "the copy finished before the cancel: {}", o.bytes);
    assert_eq!(fs::read_dir(&dest).unwrap().count(), 0, "a partial or later file was left");
}

#[test]
fn move_across_volumes() {
    // Set FM_TEST_OTHER_VOLUME to a folder on a different volume than the temp folder.
    let Some(other) = std::env::var_os("FM_TEST_OTHER_VOLUME") else {
        eprintln!("skipped: FM_TEST_OTHER_VOLUME not set");
        return;
    };
    let target = tempfile::tempdir_in(other).unwrap();
    let d = tempfile::tempdir().unwrap();
    let src = d.path().join("proj");
    sample(&src);
    let want = tree(&src);
    let o = run(Op::Move, std::slice::from_ref(&src), target.path());
    assert!(o.errors.is_empty(), "{:?}", o.errors);
    assert!(!src.exists(), "the source should be gone after a move");
    assert_eq!(tree(&target.path().join("proj")), want);
}

#[cfg(windows)]
fn can_make_links(d: &Path) -> bool {
    std::os::windows::fs::symlink_file("x", d.join("probe")).map(|_| fs::remove_file(d.join("probe")).unwrap()).is_ok()
}

#[cfg(unix)]
fn can_make_links(_: &Path) -> bool {
    true
}

#[test]
fn links_are_copied_and_deleted_as_links() {
    let d = tempfile::tempdir().unwrap();
    if !can_make_links(d.path()) {
        eprintln!("skipped: this account may not create symbolic links");
        return;
    }
    let src = d.path().join("src");
    fs::create_dir_all(src.join("real")).unwrap();
    fs::write(src.join("real/f.txt"), "data").unwrap();
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_dir("real", src.join("dirlink")).unwrap();
        std::os::windows::fs::symlink_file("real\\f.txt", src.join("filelink")).unwrap();
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("real", src.join("dirlink")).unwrap();
        std::os::unix::fs::symlink("real/f.txt", src.join("filelink")).unwrap();
    }
    let dest = d.path().join("dest");
    fs::create_dir(&dest).unwrap();
    let o = run(Op::Copy, std::slice::from_ref(&src), &dest);
    assert!(o.errors.is_empty(), "{:?}", o.errors);
    let copied = dest.join("src");
    assert!(fs::symlink_metadata(copied.join("dirlink")).unwrap().file_type().is_symlink());
    assert_eq!(fs::read_to_string(copied.join("filelink")).unwrap(), "data");
    // Deleting a link to a folder must not delete what is in that folder.
    let o = run(Op::Delete, &[copied.join("dirlink")], Path::new(""));
    assert!(o.errors.is_empty(), "{:?}", o.errors);
    assert!(copied.join("real/f.txt").exists());
    assert!(src.join("real/f.txt").exists());
}

#[test]
fn rename_and_create() {
    let d = tempfile::tempdir().unwrap();
    let a = ops::create(d.path(), "a.txt").unwrap();
    assert!(a.is_file());
    let nested = ops::create(d.path(), "src/bin/main.rs").unwrap();
    assert!(nested.is_file());
    let dir = ops::create(d.path(), "docs/").unwrap();
    assert!(dir.is_dir());
    assert!(ops::create(d.path(), "a.txt").is_err());
    assert!(ops::create(d.path(), "../escape").is_err());
    fs::write(d.path().join("b.txt"), "keep").unwrap();
    // Never replaces another file.
    assert!(ops::rename(&a, "b.txt").is_err());
    assert_eq!(fs::read_to_string(d.path().join("b.txt")).unwrap(), "keep");
    let c = ops::rename(&a, "c.txt").unwrap();
    assert!(c.exists() && !a.exists());
    if cfg!(windows) {
        // A change of case only is a rename on a case-insensitive file system.
        let upper = ops::rename(&c, "C.txt").unwrap();
        let names: Vec<String> =
            fs::read_dir(d.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert!(names.contains(&"C.txt".to_string()), "{names:?}");
        assert!(upper.exists());
    }
}

#[test]
fn trash_removes_the_item_and_it_can_be_found_in_the_trash() {
    let d = tempfile::tempdir().unwrap();
    let name = format!("fm-trash-test-{}.txt", std::process::id());
    let p = d.path().join(&name);
    fs::write(&p, "to the bin").unwrap();
    let o = run(Op::Trash, std::slice::from_ref(&p), Path::new(""));
    assert!(o.errors.is_empty(), "{:?}", o.errors);
    assert!(!p.exists());
    #[cfg(any(windows, all(unix, not(target_os = "macos"))))]
    {
        // Find our item in the trash and purge it, so the test leaves the real trash as it was.
        let items: Vec<_> = trash::os_limited::list().unwrap().into_iter().filter(|i| i.name == name.as_str()).collect();
        assert_eq!(items.len(), 1, "the item should be in the trash once");
        trash::os_limited::purge_all(items).unwrap();
    }
}

/// Random operations through `ops`, checked after every step against a model of the tree.
#[test]
fn random_operations_match_a_model() {
    let mut seed = 0x2545f4914f6cdd1du64;
    let mut rnd = move |n: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % n as u64) as usize
    };
    for round in 0..12 {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        let mut model: BTreeMap<String, Option<Vec<u8>>> = BTreeMap::new();
        for step in 0..60 {
            let dirs: Vec<String> =
                std::iter::once(String::new()).chain(model.iter().filter(|(_, v)| v.is_none()).map(|(k, _)| k.clone())).collect();
            let all: Vec<String> = model.keys().cloned().collect();
            let join = |dir: &str, name: &str| if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") };
            let pick_dir = dirs[rnd(dirs.len())].clone();
            match rnd(6) {
                0 | 1 => {
                    let name = format!("n{}", rnd(5));
                    let folder = rnd(3) == 0;
                    let rel = join(&pick_dir, &name);
                    let r = ops::create(&root.join(&pick_dir), &format!("{name}{}", if folder { "/" } else { "" }));
                    assert_eq!(r.is_ok(), !model.contains_key(&rel), "round {round} step {step} create {rel}");
                    if r.is_ok() {
                        let content = if folder { None } else { Some(Vec::new()) };
                        model.insert(rel, content);
                    }
                }
                2 if !all.is_empty() => {
                    let item = all[rnd(all.len())].clone();
                    let new_name = format!("r{}", rnd(5));
                    let parent = item.rsplit_once('/').map(|(p, _)| p.to_string()).unwrap_or_default();
                    let target = join(&parent, &new_name);
                    let r = ops::rename(&root.join(&item), &new_name);
                    let should = !model.contains_key(&target) || (target == item && cfg!(windows));
                    assert_eq!(r.is_ok(), should, "round {round} step {step} rename {item} -> {target}");
                    if r.is_ok() && target != item {
                        move_in_model(&mut model, &item, &target);
                    }
                }
                3 | 4 if !all.is_empty() => {
                    let item = all[rnd(all.len())].clone();
                    let op = if rnd(2) == 0 { Op::Copy } else { Op::Move };
                    let into_self = pick_dir == item || pick_dir.starts_with(&format!("{item}/"));
                    let parent = item.rsplit_once('/').map(|(p, _)| p.to_string()).unwrap_or_default();
                    let o = run(op, &[root.join(&item)], &root.join(&pick_dir));
                    if into_self {
                        assert_eq!(o.errors.len(), 1, "round {round} step {step}");
                        continue;
                    }
                    assert!(o.errors.is_empty(), "round {round} step {step}: {:?}", o.errors);
                    if op == Op::Move && parent == pick_dir {
                        continue;
                    }
                    let name = item.rsplit('/').next().unwrap();
                    let target = unique_in_model(&model, &pick_dir, name);
                    assert_eq!(o.done, [root.join(&target)], "round {round} step {step}");
                    if op == Op::Copy {
                        copy_in_model(&mut model, &item, &target);
                    } else {
                        move_in_model(&mut model, &item, &target);
                    }
                }
                _ if !all.is_empty() => {
                    let item = all[rnd(all.len())].clone();
                    let o = run(Op::Delete, &[root.join(&item)], Path::new(""));
                    assert!(o.errors.is_empty());
                    model.retain(|k, _| k != &item && !k.starts_with(&format!("{item}/")));
                }
                _ => {}
            }
            assert_eq!(tree(root), model, "round {round} step {step}");
        }
    }
}

fn move_in_model(m: &mut BTreeMap<String, Option<Vec<u8>>>, from: &str, to: &str) {
    let moved: Vec<(String, Option<Vec<u8>>)> =
        m.iter().filter(|(k, _)| *k == from || k.starts_with(&format!("{from}/"))).map(|(k, v)| (k.clone(), v.clone())).collect();
    for (k, v) in moved {
        m.remove(&k);
        m.insert(format!("{to}{}", &k[from.len()..]), v);
    }
}

fn copy_in_model(m: &mut BTreeMap<String, Option<Vec<u8>>>, from: &str, to: &str) {
    let copied: Vec<(String, Option<Vec<u8>>)> =
        m.iter().filter(|(k, _)| *k == from || k.starts_with(&format!("{from}/"))).map(|(k, v)| (k.clone(), v.clone())).collect();
    for (k, v) in copied {
        m.insert(format!("{to}{}", &k[from.len()..]), v);
    }
}

/// The model's version of `ops::unique_path`: "name", else "name (1)" and so on (these names
/// have no extension).
fn unique_in_model(m: &BTreeMap<String, Option<Vec<u8>>>, dir: &str, name: &str) -> String {
    let join = |n: &str| if dir.is_empty() { n.to_string() } else { format!("{dir}/{n}") };
    if !m.contains_key(&join(name)) {
        return join(name);
    }
    (1..).map(|i| join(&format!("{name} ({i})"))).find(|p| !m.contains_key(p)).unwrap()
}

#[cfg(windows)]
#[test]
fn deleting_a_junction_keeps_its_target() {
    // Junctions need no privilege, so this runs on every Windows account.
    let d = tempfile::tempdir().unwrap();
    let target = d.path().join("target");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("precious.txt"), "keep me").unwrap();
    let j = d.path().join("junction");
    let st = std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(&j).arg(&target).output().unwrap();
    assert!(st.status.success(), "{st:?}");
    assert_eq!(fs::read_to_string(j.join("precious.txt")).unwrap(), "keep me");
    let o = run(Op::Delete, std::slice::from_ref(&j), Path::new(""));
    assert!(o.errors.is_empty(), "{:?}", o.errors);
    assert!(fs::symlink_metadata(&j).is_err());
    assert_eq!(fs::read_to_string(target.join("precious.txt")).unwrap(), "keep me");
    // Deleting the folder that holds a junction does not follow it either.
    let outer = d.path().join("outer");
    fs::create_dir(&outer).unwrap();
    let st = std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(outer.join("j")).arg(&target).output().unwrap();
    assert!(st.status.success());
    let o = run(Op::Delete, std::slice::from_ref(&outer), Path::new(""));
    assert!(o.errors.is_empty(), "{:?}", o.errors);
    assert!(!outer.exists());
    assert!(target.join("precious.txt").exists());
}
