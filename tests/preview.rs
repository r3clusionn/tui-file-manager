//! Previews of the fixture files. The expected values are what Pillow and Python's zipfile report
//! for the same files (`scripts/make_fixtures.py` prints them).

use std::fs;
use std::path::Path;

use filemgr::entry::SortOrder;
use filemgr::preview::{build, Options, Tone};

fn opts() -> Options {
    Options { width: 80, show_hidden: false, order: SortOrder::default() }
}

fn header(name: &str) -> String {
    let p = build(&Path::new("tests/fixtures").join(name), &opts());
    p.lines.iter().find(|l| l.tone == Tone::Header).map(|l| l.text.clone()).unwrap_or_default()
}

fn body(name: &str) -> Vec<String> {
    let p = build(&Path::new("tests/fixtures").join(name), &opts());
    p.lines.iter().filter(|l| matches!(l.tone, Tone::Plain | Tone::Dir)).map(|l| l.text.clone()).collect()
}

#[test]
fn image_sizes_match_pillow() {
    assert_eq!(header("rgba.png"), "PNG image, 7 x 5, 8-bit RGBA");
    assert_eq!(header("palette.png"), "PNG image, 9 x 4, 8-bit palette");
    assert_eq!(header("grey16.png"), "PNG image, 6 x 6, 16-bit grey");
    assert_eq!(header("baseline.jpg"), "JPEG image, 33 x 17, baseline, 3 components");
    assert_eq!(header("progressive.jpg"), "JPEG image, 40 x 24, progressive, 3 components");
    assert_eq!(header("grey.jpg"), "JPEG image, 16 x 8, baseline, 1 components");
    assert_eq!(header("cmyk.jpg"), "JPEG image, 10 x 6, baseline, 4 components");
    assert_eq!(header("small.gif"), "GIF image, 12 x 9");
    assert_eq!(header("rgb24.bmp"), "BMP image, 13 x 11, 24-bit");
    assert_eq!(header("lossy.webp"), "WebP image, 21 x 14");
    assert_eq!(header("lossless.webp"), "WebP image, 19 x 15");
    assert_eq!(header("alpha.webp"), "WebP image, 17 x 12");
}

#[test]
fn jpeg_frame_header_after_large_metadata() {
    // Put 100 KiB of APP segments before the frame header, beyond the 64 KiB sample.
    let jpg = fs::read("tests/fixtures/baseline.jpg").unwrap();
    let mut big = jpg[..2].to_vec();
    for _ in 0..2 {
        big.extend([0xff, 0xe2, 0xc8, 0x02]);
        big.extend(vec![0u8; 0xc800]);
    }
    big.extend(&jpg[2..]);
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("big.jpg");
    fs::write(&p, big).unwrap();
    let h = build(&p, &opts()).lines.into_iter().find(|l| l.tone == Tone::Header).unwrap().text;
    assert_eq!(h, "JPEG image, 33 x 17, baseline, 3 components");
}

#[test]
fn zip_listings_match_zipfile() {
    assert_eq!(header("utf8.zip"), "Zip archive");
    assert_eq!(body("utf8.zip"), ["docs/  0 B", "docs/r\u{e9}sum\u{e9}.txt  1000 B", "\u{65e5}\u{672c}.txt  5 B"]);
    assert_eq!(body("cp437.zip"), ["caf\u{e9}.txt  3 B"]);
    assert_eq!(body("zip64.zip"), ["big.bin  300 B"]);
}

#[test]
fn zip_with_data_in_front() {
    // A self-extracting archive is a program with a zip appended; offsets inside the zip are
    // relative to where the zip starts.
    let zip = fs::read("tests/fixtures/utf8.zip").unwrap();
    let mut sfx = b"MZ".to_vec();
    sfx.extend(vec![0x90; 5000]);
    sfx.extend(&zip);
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("setup.exe");
    fs::write(&p, sfx).unwrap();
    let lines = build(&p, &opts()).lines;
    assert!(lines.iter().any(|l| l.text == "3 entries (self-extracting archive)"), "{lines:?}");
}

#[test]
fn text_binary_and_directories() {
    let d = tempfile::tempdir().unwrap();
    fs::write(d.path().join("a.txt"), "line one\n\tindented\n").unwrap();
    fs::write(d.path().join("b.bin"), [0u8, 1, 2, 3, 0x41, 0x42]).unwrap();
    fs::write(d.path().join("empty"), "").unwrap();
    fs::create_dir(d.path().join("sub")).unwrap();
    fs::write(d.path().join(".dot"), "").unwrap();
    let text = build(&d.path().join("a.txt"), &opts()).lines;
    assert_eq!(text.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), ["line one", "    indented"]);
    let bin = build(&d.path().join("b.bin"), &opts()).lines;
    assert_eq!(bin[0].text, "00000000  00 01 02 03 41 42                                ....AB");
    assert_eq!(build(&d.path().join("empty"), &opts()).lines[0].text, "empty file");
    let dir = build(d.path(), &opts()).lines;
    let names: Vec<&str> = dir.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(names, ["sub", "a.txt", "b.bin", "empty"]);
    assert_eq!(dir[0].tone, Tone::Dir);
    let all = build(d.path(), &Options { show_hidden: true, ..opts() }).lines;
    assert_eq!(all.len(), 5);
}

#[test]
fn a_missing_file_is_an_error_line() {
    let p = build(Path::new("tests/fixtures/does-not-exist"), &opts());
    assert_eq!(p.lines.len(), 1);
    assert_eq!(p.lines[0].tone, Tone::Error);
}
