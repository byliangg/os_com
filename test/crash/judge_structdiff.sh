#!/bin/bash

# SPDX-License-Identifier: MPL-2.0
#
# Structural-diff forensics judge: dump a normalized manifest of the whole
# namespace (path, type, mode, links, size, blockcount, symlink target,
# optionally a content md5) via debugfs, and diff it against a reference.
#
#   judge_structdiff.sh [-m] <image>              print the manifest
#   judge_structdiff.sh [-m] <image> <reference>  diff; exit 1 on any delta
#
# <reference> is either another image (its manifest is generated the same
# way) or a previously saved manifest file. Timestamps are deliberately
# excluded so that a replayed crash image can be compared against a golden
# tree. -m adds md5 of regular-file contents (debugfs cat; slower).
#
# This is the second-level attribution tool for suspicious crash points
# (P8_plan §2.5 leg ①): not meant to run at every FLUSH point, but to
# answer "WHAT diverged" once judge.sh / the oracles flag a state. It sees
# namespace and inode-field damage (lost entries, wrong link counts, size
# truncation, clobbered file type) that accounting/csum judges ignore.
#
# Needs debugfs on PATH. Read-only. Exit 0 = manifests identical (or
# manifest printed), 1 = difference found, 2 = usage/debugfs failure.

set -u

MD5=0
if [ "${1:-}" = "-m" ]; then
    MD5=1
    shift
fi
if [ $# -lt 1 ] || [ $# -gt 2 ]; then
    echo "usage: judge_structdiff.sh [-m] <image> [reference]" >&2
    exit 2
fi

manifest() { # $1 = image, output on stdout
    MD5="$MD5" python3 - "$1" <<'PYEOF'
import os, re, subprocess, sys

img = sys.argv[1]
want_md5 = os.environ.get("MD5") == "1"


ENV = {**os.environ, "LC_ALL": "C"}  # English "Inode:"/"Type:" tokens


def debugfs(req):
    r = subprocess.run(
        ["debugfs", "-R", req, img], capture_output=True, text=True, env=ENV
    )
    return r.stdout


# BFS the namespace with `ls -p` (machine format: /ino/mode/uid/gid/name/size/).
entries = []  # (path, ino, mode_octal)
queue = [("/", 2)]
seen_dirs = {2}
while queue:
    path, ino = queue.pop(0)
    out = debugfs("ls -p <%d>" % ino)
    for line in out.splitlines():
        if not line.startswith("/"):
            continue
        parts = line.split("/")
        if len(parts) < 7:
            continue
        _, ino_s, mode, _uid, _gid, name = parts[:6]
        if name in (".", "..", "") or not ino_s.isdigit():
            continue
        child = int(ino_s)
        if child == 0:  # empty dirent slot
            continue
        cpath = (path.rstrip("/") or "") + "/" + name
        entries.append((cpath, child, mode))
        if mode.startswith("04") and child not in seen_dirs:
            seen_dirs.add(child)
            queue.append((cpath, child))

entries.append(("/", 2, "040000"))

# One batched debugfs run for all stats.
inos = sorted({e[1] for e in entries})
cmds = "".join("stat <%d>\n" % i for i in inos)
r = subprocess.run(
    ["debugfs", "-f", "/dev/stdin", img],
    input=cmds,
    capture_output=True,
    text=True,
    env=ENV,
)
stats = {}
cur = None
for line in r.stdout.splitlines():
    m = re.match(r"debugfs:\s+stat <(\d+)>", line)
    if m:
        cur = int(m.group(1))
        stats[cur] = {}
        continue
    if cur is None:
        continue
    st = stats[cur]
    m = re.match(r"Inode: \d+\s+Type: (\S+)\s+Mode:\s+(\S+)", line)
    if m:
        st["type"], st["mode"] = m.group(1), m.group(2)
    m = re.search(r"\bSize: (\d+)", line)
    if m and "size" not in st:
        st["size"] = m.group(1)
    m = re.match(r"Links: (\d+)\s+Blockcount: (\d+)", line)
    if m:
        st["links"], st["blocks"] = m.group(1), m.group(2)
    m = re.match(r'Fast link dest: "(.*)"', line)
    if m:
        st["target"] = m.group(1)

lines = []
for path, ino, mode in entries:
    st = stats.get(ino, {})
    cols = [
        path,
        "type=%s" % st.get("type", "?"),
        "mode=%s" % mode,
        "links=%s" % st.get("links", "?"),
        "size=%s" % st.get("size", "?"),
        "blocks=%s" % st.get("blocks", "?"),
    ]
    if "target" in st:
        cols.append("target=%s" % st["target"])
    if want_md5 and st.get("type") == "regular":
        c = subprocess.run(
            ["debugfs", "-R", "cat <%d>" % ino, img], capture_output=True, env=ENV
        )
        import hashlib

        cols.append("md5=%s" % hashlib.md5(c.stdout).hexdigest())
    lines.append("\t".join(cols))

sys.stdout.write("\n".join(sorted(lines)) + "\n")
PYEOF
}

is_ext_image() { # magic 0xEF53 little-endian at offset 1080
    [ "$(dd if="$1" bs=1 skip=1080 count=2 2>/dev/null | od -An -tx1 | tr -d ' \n')" = "53ef" ]
}

if [ $# -eq 1 ]; then
    if ! is_ext_image "$1"; then
        echo "judge_structdiff: $1 is not an ext image" >&2
        exit 2
    fi
    manifest "$1"
    exit $?
fi

# Two arguments: the image and the reference may arrive in either order —
# judge.sh's oracle-hook wiring appends the crash image LAST, while the
# documented CLI order puts it FIRST. Probe both; manifesting a text file
# as if it were an image would degenerate into a guaranteed (false) diff.
ACTUAL=$1
REF=$2
if ! is_ext_image "$ACTUAL" && is_ext_image "$REF"; then
    ACTUAL=$2
    REF=$1
fi
if ! is_ext_image "$ACTUAL"; then
    echo "judge_structdiff: neither $1 nor $2 is an ext image" >&2
    exit 2
fi

WORK=$(mktemp -d "${TMPDIR:-/tmp}/structdiff-XXXXXX")
trap 'rm -rf "$WORK"' EXIT
manifest "$ACTUAL" >"$WORK/actual" || exit 2
if is_ext_image "$REF"; then
    manifest "$REF" >"$WORK/ref" || exit 2
else
    # LC_ALL=C: match python's bytewise sorted(); locale collation (e.g.
    # zh_CN) orders paths differently and turns identical trees into diffs.
    LC_ALL=C sort "$REF" >"$WORK/ref"
fi

if diff -u "$WORK/ref" "$WORK/actual"; then
    echo "judge_structdiff: no structural difference"
    exit 0
fi
echo "judge_structdiff: structural difference between $ACTUAL and $REF" >&2
exit 1
