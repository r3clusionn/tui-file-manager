"""Writes the small image and zip files in tests/fixtures, with Pillow and zipfile, and prints
what Pillow and zipfile say about each one (the expected values in tests/preview.rs).

    python scripts/make_fixtures.py
"""
import io
import sys
import os
import struct
import zipfile
import zlib

from PIL import Image

sys.stdout.reconfigure(encoding="utf-8")
OUT = os.path.join(os.path.dirname(__file__), "..", "tests", "fixtures")
os.makedirs(OUT, exist_ok=True)


def save(img, name, **kw):
    path = os.path.join(OUT, name)
    img.save(path, **kw)
    with Image.open(path) as check:
        print(f"{name}: {check.format} {check.size[0]}x{check.size[1]} mode={check.mode}")


def gradient(w, h, mode):
    img = Image.new("RGB", (w, h))
    img.putdata([((x * 37) % 256, (y * 53) % 256, ((x + y) * 11) % 256) for y in range(h) for x in range(w)])
    return img.convert(mode)


save(gradient(7, 5, "RGBA"), "rgba.png")
save(gradient(9, 4, "P"), "palette.png")
save(gradient(6, 6, "I;16"), "grey16.png")
save(gradient(33, 17, "RGB"), "baseline.jpg", quality=80)
save(gradient(40, 24, "RGB"), "progressive.jpg", quality=80, progressive=True)
save(gradient(16, 8, "L"), "grey.jpg")
save(gradient(10, 6, "CMYK"), "cmyk.jpg")
save(gradient(12, 9, "P"), "small.gif")
save(gradient(13, 11, "RGB"), "rgb24.bmp")
save(gradient(21, 14, "RGB"), "lossy.webp", quality=70)
save(gradient(19, 15, "RGB"), "lossless.webp", lossless=True)
alpha = gradient(17, 12, "RGBA")
alpha.putalpha(gradient(17, 12, "L"))
save(alpha, "alpha.webp", quality=70)

# Zip with UTF-8 names (the encoder sets flag bit 11 for non-ASCII names) and a directory.
with zipfile.ZipFile(os.path.join(OUT, "utf8.zip"), "w", zipfile.ZIP_DEFLATED) as z:
    z.writestr("docs/", b"")
    z.writestr("docs/résumé.txt", b"x" * 1000)
    z.writestr("日本.txt", b"hello")
    z.comment = b"a comment that sits after the end record"

# A name in code page 437: write an ASCII placeholder and patch the byte in both headers.
buf = io.BytesIO()
with zipfile.ZipFile(buf, "w") as z:
    z.writestr("caf_.txt", b"abc")
data = buf.getvalue().replace(b"caf_.txt", b"caf\x82.txt")
open(os.path.join(OUT, "cp437.zip"), "wb").write(data)


# A zip64 archive built by hand: the central directory entry stores 0xFFFFFFFF sizes with the real
# ones in a zip64 extra field, and the end of central directory is the zip64 record + locator.
def zip64():
    name = b"big.bin"
    payload = b"z" * 300
    crc = zlib.crc32(payload)
    local = struct.pack("<IHHHHHIIIHH", 0x04034B50, 45, 0, 0, 0, 0, crc, 0xFFFFFFFF, 0xFFFFFFFF, len(name), 20)
    local += name + struct.pack("<HHQQ", 1, 16, len(payload), len(payload)) + payload
    extra = struct.pack("<HHQQQ", 1, 24, len(payload), len(payload), 0)
    cd = struct.pack("<IHHHHHHIIIHHHHHII", 0x02014B50, 45, 45, 0, 0, 0, 0, crc, 0xFFFFFFFF, 0xFFFFFFFF, len(name), len(extra), 0, 0, 0, 0, 0xFFFFFFFF)
    cd += name + extra
    cd_off = len(local)
    z64_off = cd_off + len(cd)
    z64 = struct.pack("<IQHHIIQQQQ", 0x06064B50, 44, 45, 45, 0, 0, 1, 1, len(cd), cd_off)
    loc = struct.pack("<IIQI", 0x07064B50, 0, z64_off, 1)
    eocd = struct.pack("<IHHHHIIH", 0x06054B50, 0, 0, 0xFFFF, 0xFFFF, 0xFFFFFFFF, 0xFFFFFFFF, 0)
    return local + cd + z64 + loc + eocd


open(os.path.join(OUT, "zip64.zip"), "wb").write(zip64())

for name in ["utf8.zip", "cp437.zip", "zip64.zip"]:
    with zipfile.ZipFile(os.path.join(OUT, name)) as z:
        print(name, [(i.filename, i.file_size) for i in z.infolist()])
        assert z.testzip() is None
