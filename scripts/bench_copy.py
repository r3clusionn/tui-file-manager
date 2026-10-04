"""Times fm's copy job against robocopy and xcopy on the same files, in rotated rounds.

    cargo build --release --examples
    python scripts/bench_copy.py WORKDIR [ROUNDS]

WORKDIR gets a 2 GiB file and a tree of 10,000 files (4 to 60 KiB each); each tool copies them to
a fresh folder next to them, which is deleted afterwards. All tools see the same (cached) source.
"""
import os
import random
import shutil
import statistics
import subprocess
import sys
import time

work = os.path.abspath(sys.argv[1])
rounds = int(sys.argv[2]) if len(sys.argv) > 2 else 3
BENCH = os.path.join(os.path.dirname(__file__), "..", "target", "release", "examples", "bench.exe")
os.makedirs(work, exist_ok=True)

big = os.path.join(work, "src-big", "big.bin")
tree = os.path.join(work, "src-tree", "tree")
if not os.path.exists(big):
    os.makedirs(os.path.dirname(big))
    with open(big, "wb") as f:
        for _ in range(2048):
            f.write(os.urandom(1 << 20))
if not os.path.exists(tree):
    r = random.Random(1)
    for i in range(10_000):
        d = os.path.join(tree, f"d{i % 100:02}", f"e{i % 7}")
        os.makedirs(d, exist_ok=True)
        with open(os.path.join(d, f"f{i}.dat"), "wb") as f:
            f.write(r.randbytes(r.randint(4, 60) * 1024))


def fm(src, dest):
    out = subprocess.run([BENCH, "copy", src, dest], capture_output=True, text=True, check=True)
    return float(out.stdout.strip())


def timed(cmd, ok_codes=(0,)):
    t = time.perf_counter()
    p = subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    took = time.perf_counter() - t
    assert p.returncode in ok_codes, (cmd, p.returncode)
    return took


def robocopy(src, dest):
    # robocopy copies a folder's contents; exit codes below 8 are success.
    name = os.path.basename(src)
    if os.path.isdir(src):
        return timed(["robocopy", src, os.path.join(dest, name), "/E", "/NJH", "/NJS", "/NFL", "/NDL", "/NP"], ok_codes=range(8))
    return timed(["robocopy", os.path.dirname(src), dest, name, "/NJH", "/NJS", "/NFL", "/NDL", "/NP"], ok_codes=range(8))


def xcopy(src, dest):
    name = os.path.basename(src)
    if os.path.isdir(src):
        return timed(["xcopy", src, os.path.join(dest, name) + "\\", "/E", "/I", "/Q", "/H", "/K", "/Y"])
    return timed(["xcopy", src, dest + "\\", "/Q", "/H", "/K", "/Y"])


tools = {"fm": fm, "robocopy": robocopy, "xcopy": xcopy}
results = {(t, s): [] for t in tools for s in ("big", "tree")}
for rnd in range(rounds):
    order = list(tools)[rnd % 3:] + list(tools)[: rnd % 3]
    for case, src in (("big", big), ("tree", tree)):
        for t in order:
            dest = os.path.join(work, f"dest-{t}")
            shutil.rmtree(dest, ignore_errors=True)
            os.makedirs(dest)
            took = tools[t](src, dest)
            # Check the copy is complete before trusting the time.
            if case == "big":
                assert os.path.getsize(os.path.join(dest, "big.bin")) == 2 << 30
            else:
                n = sum(len(f) for _, _, f in os.walk(os.path.join(dest, "tree")))
                assert n == 10_000, (t, n)
            results[(t, case)].append(took)
            shutil.rmtree(dest)
            print(f"round {rnd + 1} {case:4} {t:9} {took:.2f} s", flush=True)

print()
for case in ("big", "tree"):
    for t in tools:
        v = results[(t, case)]
        print(f"{case:4} {t:9} median {statistics.median(v):.2f} s  (runs: {', '.join(f'{x:.2f}' for x in v)})")
