# test/crash/protocol — hand-picked protocol-point crash corpus (P8a-a6)

Workloads targeting journal-protocol windows the ACE seq-1 sweep does not
aim at, plus the CrashMonkey/ACE known-bug sequences as regression seeds.
Everything here runs through the ordinary matrix driver:

```
test/crash/run_matrix.sh test/crash/protocol                    # whole library
test/crash/run_matrix.sh --journal-size 4 test/crash/protocol \
    proto3-ckpt-tail proto4-multidesc.sh ...                    # tiny-journal round
```

J-lang files are converted by `jlang2sh.py --oracle` (which accepts the
`sleep`/`symlink` extensions documented in its header); `*.sh` files are
raw guest workloads taken verbatim (no oracle instrumentation — every
structural judge still runs on their FLUSH points).

## Protocol-point families

| workload | window under test |
|---|---|
| `proto1-revoke-reuse` | revoke + freed-block reuse before the next commit (P7b): rmdir frees a dir block (revoke record), file data immediately reuses it; replay must honor PASS_REVOKE, stale bytes must not resurface |
| `proto2-commit-batch` | group-commit batch boundaries (P7c-c2): back-to-back fsyncs of two files keep landing commit requests on an in-flight pipeline (T_LOCKED seat, request/batch merge) |
| `proto3-ckpt-tail` | checkpoint tail publication: wraps a `-J size=4` journal (~>1024 journaled blocks + an unlink wave) so lazy checkpoint must evict and write final locations mid-session — the traffic that gives walcheck.py a non-empty judged surface (a4 finding) |
| `proto4-multidesc.sh` | multi-descriptor transaction intermediates (P7a): a 4500-inode uncommitted batch spills one 4K descriptor (>254 tag3 tags). Lands on DEFAULT journals (measured: one 2-descriptor commit, seq 333, in the library round) — NOT under `--journal-size 4`, whose quarter trigger fires on reserved worst-case credits before 254 actual blocks accumulate (all 222 tiny-round commits were single-desc). Verify per recording by counting jbd2 type-1 blocks per committed sequence in the write log (see the file header) |

## Age-commit specials (ledger `age-commit-crash-coverage-gap`)

| workload | role |
|---|---|
| `age1-pending-create` | uncommitted write dwells 7s > the 5s age window; its span must show 3 journal commits (leading data fsync + age commit + marker fsync) |
| `age2-namespace-batch` | same law for an uncommitted namespace batch |
| `age0-warmup` / `age9-control` | no-sleep control twins (leading twin absorbs mount noise; trailing twin proves spans carry no stray commits) — 2 commits each (data fsync + marker) |

The law is stated in JBD2 commit blocks, not FLUSH barriers (this kernel
emits 2-3 barriers per commit). Every age file opens with one data-file
fsync so the corpus satisfies the sweep's 2-FLUSH-per-workload density
gate. Evidence reader/gate: `../age_evidence.py <log> --assert
age0-warmup:2 --assert age1-pending-create:3 --assert
age2-namespace-batch:3 --assert age9-control:2` (proven 2026-07-10: age
spans held seq 8,9,10 / 11,12,13 with seq 9 and 12 requested by nobody —
the age trigger by elimination; controls exactly 2).

## ACE known-bug sequences (crashmonkey/ace/ace.py:24-190)

ace.py encodes 28 known-bug sequences (its own tally line says 26). The 22
expressible in our converter subset are checked in as `ace*` J-lang files,
each header citing its number and original test. Excluded, with reasons:

| # | bug | why not |
|---|---|---|
| 2 | btrfs_rename_special_file | needs `mknod` + fsync of a FIFO; opening a FIFO to fsync it blocks without a peer (xfs_io would hang the guest) |
| 3, 4 | new_bug1_btrfs / new_bug2_f2fs | FALLOC_FL_ZERO_RANGE — kernel rejects with EOPNOTSUPP (converter refuses fzero, run_matrix counts the skip) |
| 8 | generic_066 | xattrs — Non-goal (2026-07-10 user ruling, A2_ledger §4) |
| 13 | ext4_direct_write | `dwrite` (O_DIRECT) outside the converter subset |
| 28 | generic_325 | mmapwrite/msync — mmap writeback is the known P9 debt |

Degradations noted in headers: `ace15-generic041` reduced 3000→200 links
(still >1 dir block); `ace19-generic106` cannot drop caches.
