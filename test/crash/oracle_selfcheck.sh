#!/bin/bash

# SPDX-License-Identifier: MPL-2.0
#
# Calibrates the data oracle (oracle.py): a judge that cannot turn red is
# worthless. Complements selfcheck.sh (which calibrates the e2fsck judge).
#
# Stage 1 — OFFLINE red-path legs (seconds, no QEMU, no root): fabricated
# console logs + mke2fs -d images pin every judgement mechanism that the
# recorded-run legs cannot exercise deterministically:
#   O1 GREEN     fabricated two-checkpoint run judges green (check + final),
#                including the directory entry-set digest round-trip
#                (guest-side algorithm mirrored by the fabricator, host side
#                recomputed from debugfs ls -p);
#   O2 RED       marker-contiguity break (fsynced marker 1's bytes wiped
#                while marker 2 is in force) reds `check`;
#   O3 RED       vacuity guard: final image lacking a checkpoint marker
#                reds `final` (while `check` legitimately stays green);
#   O4 EXIT 3    a dropped console line makes the workload UNAVAIL and
#                `final` returns 3 (unjudged), never 0;
#   O5 EXIT 3    a garbled (checksum-corrupt) console line likewise;
#   O6 GREEN/RED post-last-checkpoint revocation (sentinel-tagged x row) is
#                kept by collect and suppresses the stale assertion
#                (old-or-new window); without the x row the same image reds
#                — proves the retention is load-bearing;
#   O7 RED       fsync(dir) entry-set violation: a promised dirent removed
#                behind the fs's back reds the directory digest assertion
#                (the j-lang1 shape: fsync(dir) is the ONLY persistence).
#
# Stage 2 — RECORDED-RUN legs (QEMU boot via run_matrix.sh):
#   GREEN  a real recorded run (4 workloads incl. write/fsync/sync/falloc)
#          must sweep green through every FLUSH point WITH the oracle, and
#          the final image must pass the vacuity-guarded `final` verdict;
#   RED-1  an fsynced file removed behind the fs's back (debugfs -w rm)
#          must turn the oracle red (fsynced-entity-missing);
#   RED-2  an fsynced file's data block overwritten with the 0x52 dye
#          must turn the oracle red (content mismatch + stale-dye block).
#
#   oracle_selfcheck.sh [jlang-dir]   (default /root/jlang-seq1; run from
#                                      the repo root, inside the container)
#   X4_OFFLINE_ONLY=1                 run only stage 1 (no QEMU needed)
#
# Exit 0 = all verdicts came out as they must.

set -eu

JLANG_DIR=${1:-/root/jlang-seq1}
HERE=$(dirname "$(readlink -f "$0")")
REPO=$(readlink -f "$HERE/../..")
BUILD=$REPO/test/initramfs/build

# ------------------------------------------------- stage 1: offline legs

OFF=$(mktemp -d "${TMPDIR:-/dev/shm}/oracle-offline-XXXXXX")
WORK=$(mktemp /dev/shm/oracle-selfcheck-XXXXXX.img)
BAD=$(mktemp /dev/shm/oracle-selfcheck-bad-XXXXXX.img)
trap 'rm -rf "$OFF" "$WORK" "$BAD"' EXIT

PYTHONDONTWRITEBYTECODE=1 OFF="$OFF" HERE="$HERE" python3 - <<'PYEOF'
import hashlib, os, shutil, subprocess, sys

OFF = os.environ["OFF"]
HERE = os.environ["HERE"]
sys.path.insert(0, HERE)
# Cross-implementation anchor: markers come from the GENERATOR's formula
# (jlang2sh), and oracle.py must find them — a drift between the two
# implementations reds the green leg here.
from jlang2sh import marker_string

ORACLE = os.path.join(HERE, "oracle.py")
WL = "wl1"


def md5(b):
    return hashlib.md5(b).hexdigest()


def oline(payload):
    return "X4ORACLE|%s|%s" % (payload, md5(payload.encode()))


def write_log(path, payloads):
    with open(path, "w") as f:
        for p in payloads:
            f.write(oline(p) + "\n")


def oracle(*args):
    r = subprocess.run(
        [sys.executable, ORACLE, *args],
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
    )
    return r.returncode, r.stdout


def gate(name, cond, detail=""):
    if not cond:
        print("oracle-selfcheck(offline): FAIL — %s\n%s" % (name, detail),
              file=sys.stderr)
        sys.exit(1)
    print("oracle-selfcheck(offline): %s (as it must)" % name)


def build_img(path, files):
    seed = os.path.join(OFF, "seed")
    shutil.rmtree(seed, ignore_errors=True)
    os.makedirs(os.path.join(seed, "wd_" + WL))
    for name, content in files.items():
        with open(os.path.join(seed, "wd_" + WL, name), "wb") as f:
            f.write(content)
    with open(path, "wb") as f:
        f.truncate(16 << 20)
    subprocess.run(
        ["mke2fs", "-F", "-q", "-t", "ext4", "-b", "4096", "-I", "256",
         "-O", "has_journal,extent,filetype,^metadata_csum,^dir_index,"
         "^64bit,^flex_bg,^inline_data,^resize_inode,^uninit_bg",
         "-d", seed, path],
        check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )


def wipe_marker(img, ckpt):
    """Zero the magic prefix of every on-disk copy of marker <ckpt>."""
    m = marker_string(WL, ckpt).encode()
    with open(img, "r+b") as f:
        data = f.read()
        pos, n = 0, 0
        while True:
            pos = data.find(m, pos)
            if pos < 0:
                break
            f.seek(pos)
            f.write(b"\x00" * 8)
            n += 1
            pos += 1
    assert n > 0, "marker %d not found in %s" % (ckpt, img)


def dir_digest(names):
    """The GUEST-side algorithm: ls -A | grep -v ^ckpt_ | sort | md5sum."""
    keep = sorted((n for n in names if not n.startswith("ckpt_")),
                  key=lambda s: s.encode())
    return md5("".join(n + "\n" for n in keep).encode())


FOO = b"\x22" * 8192
mark1 = marker_string(WL, 1) + "\n"
mark2 = marker_string(WL, 2) + "\n"

# --- O1: green round trip (files + directory entry-set digest) ---
base = os.path.join(OFF, "base.img")
build_img(base, {"foo": FOO, "ckpt_1": mark1.encode(), "ckpt_2": mark2.encode()})
log = os.path.join(OFF, "good.log")
write_log(log, [
    "%s|1|D|1|f|8192|%s|foo" % (WL, md5(FOO)),
    "%s|2|D|1|d|2|%s|." % (WL, dir_digest(["foo"])),
    "%s|3|C|1" % WL,
    "%s|4|D|2|f|8192|%s|foo" % (WL, md5(FOO)),
    "%s|5|D|2|d|2|%s|." % (WL, dir_digest(["foo"])),
    "%s|6|C|2" % WL,
    "%s|7|E" % WL,
])
table = os.path.join(OFF, "good.table")
rc, out = oracle("collect", log, table)
gate("collect accepts the fabricated run", rc == 0 and "1 workloads available" in out, out)
rc, out = oracle("check", table, base)
gate("O1 green: fabricated image judged green (2 live assertions)",
     rc == 0 and "2 assertions ok" in out, out)
rc, out = oracle("final", table, "1", base)
gate("O1 green: final verdict green", rc == 0, out)

# --- O2: marker contiguity red ---
bad = os.path.join(OFF, "bad.img")
shutil.copyfile(base, bad)
wipe_marker(bad, 1)
rc, out = oracle("check", table, bad)
gate("O2 red: broken marker contiguity reds check",
     rc == 1 and "contiguity" in out, out)

# --- O3: vacuity-final red (check legitimately green) ---
shutil.copyfile(base, bad)
wipe_marker(bad, 2)
rc, out = oracle("check", table, bad)
gate("O3: prefix without marker 2 still greens check", rc == 0, out)
rc, out = oracle("final", table, "1", bad)
gate("O3 red: final image lacking marker 2 reds final",
     rc == 1 and "lacks marker" in out, out)

# --- O4: dropped console line -> UNAVAIL -> final exit 3 ---
with open(log) as f:
    lines = f.read().splitlines()
droplog = os.path.join(OFF, "drop.log")
with open(droplog, "w") as f:
    f.write("\n".join(lines[:3] + lines[4:]) + "\n")
dtable = os.path.join(OFF, "drop.table")
rc, out = oracle("collect", droplog, dtable)
gate("O4: dropped line marks the workload UNAVAIL", rc == 0 and "UNAVAIL" in out, out)
rc, out = oracle("final", dtable, "1", base)
gate("O4 exit 3: final on an UNAVAIL table is unjudged", rc == 3, out)

# --- O5: garbled (checksum-corrupt) line -> UNAVAIL -> final exit 3 ---
garblog = os.path.join(OFF, "garb.log")
with open(garblog, "w") as f:
    f.write("\n".join(lines[:3] + [lines[3][:-4] + "beef"] + lines[4:]) + "\n")
gtable = os.path.join(OFF, "garb.table")
rc, out = oracle("collect", garblog, gtable)
gate("O5: garbled line marks the workload UNAVAIL",
     rc == 0 and "UNAVAIL" in out and "1 garbled" in out, out)
rc, out = oracle("final", gtable, "1", base)
gate("O5 exit 3: final on a garbled table is unjudged", rc == 3, out)

# --- O6: post-last-checkpoint revocation kept and load-bearing ---
NEW = b"\x23" * 4096  # foo was modified AFTER checkpoint 1; disk holds new
img_s = os.path.join(OFF, "sentinel.img")
build_img(img_s, {"foo": NEW, "ckpt_1": mark1.encode()})
slog = os.path.join(OFF, "sentinel.log")
write_log(slog, [
    "%s|1|D|1|f|8192|%s|foo" % (WL, md5(FOO)),
    "%s|2|C|1" % WL,
    "%s|3|D|2|x|-|-|foo" % WL,  # revocation tagged past the last checkpoint
    "%s|4|E" % WL,
])
stable = os.path.join(OFF, "sentinel.table")
rc, out = oracle("collect", slog, stable)
with open(stable) as f:
    ttext = f.read()
gate("O6: collect keeps the sentinel-tagged x row",
     rc == 0 and "DECL\t%s\t2\tx" % WL in ttext, ttext)
rc, out = oracle("check", stable, img_s)
gate("O6 green: old-or-new state after the revocation is not asserted",
     rc == 0, out)
write_log(slog, [
    "%s|1|D|1|f|8192|%s|foo" % (WL, md5(FOO)),
    "%s|2|C|1" % WL,
    "%s|3|E" % WL,
])
rc, out = oracle("collect", slog, stable)
rc, out = oracle("check", stable, img_s)
gate("O6 red: without the revocation the same image reds (control)",
     rc == 1, out)

# --- O7: fsync(dir) entry-set violation (dir digest is the ONLY promise) ---
dlog = os.path.join(OFF, "dir.log")
write_log(dlog, [
    "%s|1|D|1|d|2|%s|." % (WL, dir_digest(["foo"])),
    "%s|2|C|1" % WL,
    "%s|3|E" % WL,
])
dtable2 = os.path.join(OFF, "dir.table")
rc, out = oracle("collect", dlog, dtable2)
rc, out = oracle("check", dtable2, base)
gate("O7 green: intact directory entry set judged green", rc == 0, out)
shutil.copyfile(base, bad)
subprocess.run(
    ["debugfs", "-w", "-R", "rm /wd_%s/foo" % WL, bad],
    check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
)
rc, out = oracle("check", dtable2, bad)
gate("O7 red: lost promised dirent reds the entry-set digest",
     rc == 1 and "ENTRY SET" in out, out)

print("oracle-selfcheck(offline): all offline legs OK")
PYEOF

if [ "${X4_OFFLINE_ONLY:-0}" = 1 ]; then
    echo "oracle-selfcheck: offline legs OK (X4_OFFLINE_ONLY=1, skipping recorded-run legs)"
    exit 0
fi

# -------------------------------------------- stage 2: recorded-run legs

# The victim below is j-lang134's foo: 32768 bytes of 0x22, written and
# fsynced before checkpoint 1, untouched afterwards — so it must be intact
# in every state where marker 1 is in force, including the final image.
VICTIM_WL=j-lang134
VICTIM=foo
WORKLOADS=(j-lang2 j-lang110 j-lang134 j-lang136)

cd "$REPO"
bash "$HERE/run_matrix.sh" "$JLANG_DIR" "${WORKLOADS[@]}"
echo "oracle-selfcheck: recorded run swept green with the oracle (as it must)"

TABLE=$BUILD/oracle.table
cp --sparse=always "$BUILD/xfstests_test.img" "$WORK"
e2fsck -fy "$WORK" > /dev/null 2>&1 || true

if ! python3 "$HERE/oracle.py" final "$TABLE" "${#WORKLOADS[@]}" "$WORK"; then
    echo "oracle-selfcheck: FAIL — oracle reddened the good final image" >&2
    exit 1
fi
echo "oracle-selfcheck: good final image judged green (as it must)"

# RED-1: unlink the fsynced victim behind the filesystem's back.
cp --sparse=always "$WORK" "$BAD"
debugfs -w -R "rm /wd_$VICTIM_WL/$VICTIM" "$BAD" > /dev/null 2>&1
if python3 "$HERE/oracle.py" check "$TABLE" "$BAD"; then
    echo "oracle-selfcheck: FAIL — oracle greened a deleted fsynced file" >&2
    exit 1
fi
echo "oracle-selfcheck: deleted fsynced file judged red (as it must)"

# RED-2: resurface the 0x52 dye through the victim's first data block.
BLK=$(debugfs -R "blocks /wd_$VICTIM_WL/$VICTIM" "$WORK" 2>/dev/null | tr ' ' '\n' | grep -m1 .)
[ -n "$BLK" ] || { echo "oracle-selfcheck: cannot locate victim data block" >&2; exit 1; }
cp --sparse=always "$WORK" "$BAD"
tr '\0' '\122' < /dev/zero | dd of="$BAD" bs=4096 count=1 seek="$BLK" conv=notrunc status=none
OUT=$(python3 "$HERE/oracle.py" check "$TABLE" "$BAD") && {
    echo "oracle-selfcheck: FAIL — oracle greened a 0x52-revived data block" >&2
    exit 1
}
if ! echo "$OUT" | grep -q "STALE 0x52"; then
    echo "oracle-selfcheck: FAIL — red, but the stale-dye detector did not fire:" >&2
    echo "$OUT" >&2
    exit 1
fi
echo "oracle-selfcheck: 0x52 dye revival judged red with stale-dye finding (as it must)"

echo "oracle-selfcheck: oracle calibration OK"
