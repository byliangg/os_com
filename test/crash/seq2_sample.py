#!/usr/bin/env python3
"""seq-2 stratified sampler (P8b T2).

The full seq-2 corpus is 241,720 two-op workloads. A full-fleet crash sweep
(strict e2fsck + accounting + metadata_csum + durability oracle) costs
~3.4s/point (measured, P8b_milestone T2), which puts an exhaustive sweep at
~71 days single-container — infeasible. So P8b sweeps a STRATIFIED SAMPLE that
covers the op-family cross-section instead (kickoff decision 1's sanctioned
fallback + P8_plan/P8b_plan sampling delegation).

"Family" = the frozenset of core mutating ops a workload's `# run` section
uses (write/falloc/truncate/link/unlink/remove/rename/creat/mkdir/rmdir).
Markers (fsync/sync/checkpoint/open/close/opendir) are in every workload and
do not define a family. We keep only CONVERTIBLE workloads (jlang2sh's
supported op + falloc-mode set, mirrored from seq2_stats.py), then sample each
family proportionally to its convertible size with a floor of 1 so no family
is dropped, trimming/topping-up to the target.

Deterministic: within-family order is by md5(name), so the same target always
yields the same sample (reproducibility for the T7 evidence pack). No RNG.

    seq2_sample.py <corpus-dir> <target-count> <out-list>

Writes the chosen workload names (one per line) to <out-list> and prints a
census (total / unconvertible / convertible / families / sampled) to stdout.
"""
import collections
import hashlib
import os
import sys

SUPPORTED = {
    "open", "creat", "opendir", "close", "mkdir", "rmdir", "write",
    "truncate", "link", "unlink", "remove", "rename", "fsync", "fdatasync",
    "sync", "checkpoint", "falloc",
}
FALLOC_OK = {"0", "FALLOC_FL_KEEP_SIZE", "FALLOC_FL_PUNCH_HOLE|FALLOC_FL_KEEP_SIZE"}
FAMILY_OPS = {
    "write", "falloc", "truncate", "link", "unlink", "remove", "rename",
    "creat", "mkdir", "rmdir",
}


def classify(path):
    """Return (convertible, family frozenset) for one workload file."""
    allops = set()
    modes = set()
    fam = set()
    in_run = False
    with open(path) as f:
        for raw in f:
            line = raw.strip()
            if line.startswith("# run"):
                in_run = True
                continue
            if not in_run or not line or line.startswith("#"):
                continue
            parts = line.split()
            op = parts[0]
            allops.add(op)
            if op == "falloc" and len(parts) > 2:
                modes.add(parts[2])
            if op == "open" and "O_CREAT" in line:
                fam.add("creat")
            elif op in FAMILY_OPS:
                fam.add(op)
    convertible = allops <= SUPPORTED and modes <= FALLOC_OK
    return convertible, frozenset(fam)


def main():
    corpus, target, out = sys.argv[1], int(sys.argv[2]), sys.argv[3]
    buckets = collections.defaultdict(list)
    total = unconv = 0
    for name in os.listdir(corpus):
        total += 1
        ok, fam = classify(os.path.join(corpus, name))
        if not ok:
            unconv += 1
            continue
        buckets[fam].append(name)

    conv = sum(len(v) for v in buckets.values())
    # Proportional allocation with a floor of 1 per family.
    sample = []
    for fam, names in buckets.items():
        names.sort(key=lambda n: hashlib.md5(n.encode()).hexdigest())
        k = max(1, round(target * len(names) / conv))
        sample.extend(names[:k])
    # Trim or top up to hit the target as closely as the floor allows.
    if len(sample) > target:
        sample.sort(key=lambda n: hashlib.md5(n.encode()).hexdigest())
        sample = sample[:target]
    else:
        chosen = set(sample)
        leftovers = []
        for fam, names in sorted(buckets.items(), key=lambda kv: -len(kv[1])):
            for n in names:
                if n not in chosen:
                    leftovers.append(n)
        for n in leftovers:
            if len(sample) >= target:
                break
            sample.append(n)

    sample.sort(key=lambda n: int(n.replace("j-lang", "")) if n.replace("j-lang", "").isdigit() else 0)
    with open(out, "w") as f:
        for n in sample:
            f.write(n + "\n")

    fam_sizes = sorted((len(v) for v in buckets.values()), reverse=True)
    print(f"total={total} unconvertible={unconv} convertible={conv} "
          f"families={len(buckets)} sampled={len(sample)} -> {out}")
    print(f"family sizes: max={fam_sizes[0]} median={fam_sizes[len(fam_sizes)//2]} "
          f"min={fam_sizes[-1]}")


if __name__ == "__main__":
    main()
