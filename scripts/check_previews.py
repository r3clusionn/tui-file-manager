"""Compares fm's previews with Python's own readers on real files: zip listings (names and sizes)
with zipfile, image format and size with Pillow.

    cargo build --release --example preview
    python scripts/check_previews.py ROOT [ROOT ...]

Every .zip, .jar, .docx, .xlsx, .pptx, .nupkg, .whl, .vsix, .png, .jpg, .jpeg, .gif, .bmp, .webp and .ico file
under the roots is checked. Files Python itself cannot read are counted and skipped.
"""
import itertools
import os
import re
import subprocess
import sys
import zipfile

from PIL import Image

sys.stdout.reconfigure(encoding="utf-8")
ZIPS = {".zip", ".jar", ".docx", ".xlsx", ".pptx", ".nupkg", ".whl", ".vsix"}
IMAGES = {".png", ".jpg", ".jpeg", ".gif", ".bmp", ".webp", ".ico"}
EXE = os.path.join(os.path.dirname(__file__), "..", "target", "release", "examples", "preview")
LIMIT = int(os.environ.get("CHECK_LIMIT", "4000"))


def walk(roots):
    for root in roots:
        for d, dirs, files in os.walk(root):
            for f in files:
                ext = os.path.splitext(f)[1].lower()
                if ext in ZIPS or ext in IMAGES:
                    yield os.path.join(d, f), ext


def fm_previews(paths):
    out = subprocess.run([EXE], input="\n".join(paths), capture_output=True, text=True, encoding="utf-8").stdout
    result, cur = {}, None
    for line in out.split("\n"):
        if line.startswith("== "):
            cur = line[3:]
            result[cur] = []
        elif cur is not None and line:
            tone, _, text = line.partition("\t")
            result[cur].append((tone, text))
    return result


def human(n):
    units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"]
    if n < 1024:
        return f"{n} B"
    v, u = float(n), 0
    while v >= 1024 and u < len(units) - 1:
        v /= 1024
        u += 1
    # Rust's {:.1} and {:.0} round half to even on the binary value, like Python's format.
    return f"{v:.1f} {units[u]}" if v < 10 else f"{v:.0f} {units[u]}"


def main():
    files = list(itertools.islice(walk(sys.argv[1:]), LIMIT))
    got = fm_previews([p for p, _ in files])
    stats = {"zip ok": 0, "zip bad": 0, "zip unreadable": 0, "image ok": 0, "image bad": 0, "image unreadable": 0}
    for path, ext in files:
        lines = got.get(path, [])
        if ext in ZIPS:
            try:
                with zipfile.ZipFile(path) as z:
                    want = [f"{i.filename}  {human(i.file_size)}" for i in z.infolist()]
            except Exception:
                stats["zip unreadable"] += 1
                continue
            body = [t for tone, t in lines if tone in ("Plain", "Dir")]
            head = [t for tone, t in lines if tone == "Header"]
            n = len(want)
            ok = f"{n} entries" in head and body == want[:1000]
            stats["zip ok" if ok else "zip bad"] += 1
            if not ok:
                print("ZIP MISMATCH", path, head[:2], body[:3], want[:3])
        else:
            try:
                with Image.open(path) as im:
                    fmt, (w, h) = im.format, im.size
            except Exception:
                stats["image unreadable"] += 1
                continue
            head = next((t for tone, t in lines if tone == "Header"), "")
            m = re.match(r"(\w+) image(?:, (\d+) x (\d+))?", head)
            name = {"JPEG": "JPEG", "PNG": "PNG", "GIF": "GIF", "BMP": "BMP", "WEBP": "WebP", "MPO": "JPEG", "ICO": "ICO"}.get(fmt, fmt)
            ok = bool(m) and m.group(1) == name and m.group(2) is not None and (int(m.group(2)), int(m.group(3))) == (w, h)
            stats["image ok" if ok else "image bad"] += 1
            if not ok:
                print("IMAGE MISMATCH", path, repr(head), fmt, w, h)
    print(stats)
    sys.exit(1 if stats["zip bad"] or stats["image bad"] else 0)


main()
