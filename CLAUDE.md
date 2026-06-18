See [AGENTS.md](AGENTS.md).

Current ext4 context: JBD2 Phase 1/2/3, PageCache Phase 4, performance Phase 5, and SQLite Phase 6 are complete (guard regressions all green). Phase 6 closed out the SQLite real-application write line at 234.9s = Linux 21.92% (7.4× from the 2.97% starting point), with fio O_DIRECT guard floors preserved. The current line is **feature_fixerror_phase7** on branch `feature-fixerror-phase-7`: fix all official xfstests errors exposed by the new runner.

Honest cache-off baseline (NOT the old cache-on `read 127% / write 39%`, which must not be used for the defense). All numbers `direct=1, nj=1`, drop-caches fair口径, median-of-N, with `ext4fs.extent_map_cache` + inode metadata cache active:

| bs | read | write |
|----|-----:|------:|
| 4K | 86.38% | 75.54% |
| 16K | 84.42% | 75.78% |
| 64K | 86.89% | 84.09% |
| 256K | 94.81% | 121.07% |
| 1M | 122.94% | 88.28% |

Before optimization small blocks sat at 16–24% read and write 4K=20% / 1M=63%. Four ext4-domain optimizations got us here: (1) extent mapping plan cache, (2) whole-file coverage for random reads, (3) relatime atime throttling, (4) **inode metadata cache** (the big win — `get_inode_ref` was reloading the inode block from device on every `stat`). The ext4-domain per-op fixed overhead is now exhausted; the remaining gap is the Asterinas virtio device round-trip (platform layer, common across FS — confirmed by ext2 hitting the same 82–85% ceiling on the same platform).

Phase 7 references (current):
- Plan: `feature_fixerror_phase7_plan.md` / `docs/feature_fixerror_phase7_plan.md`
- Milestone: `feature_fixerror_phase7_milestone.md` / `docs/feature_fixerror_phase7_milestone.md`
- Starting evidence: `benchmark/logs/official_20260612_082930.log`
- Starting result: official full run completed 47 cases (35 PASS / 12 FAIL) and then stopped during `generic/320` with `Failed to allocate a large slot` / heap allocation error. Completed FAILs: `ext4/042`, `generic/030`, `generic/074`, `generic/141`, `generic/246`, `generic/248`, `generic/249`, `generic/257`, `generic/273`, `generic/275`, `generic/309`, `generic/313`.
- Method: first make the official full run complete, then fix failures by bad-output/root-cause clusters. Do not expand excludes casually; if a case seems outside the contest requirements, check `/home/lby/os_com_codex/赛题要求.md` and ask for human confirmation.

Phase 6 references (complete):
- Plan: `feature_sqlite_phase6_plan.md` / `docs/feature_sqlite_phase6_plan.md`
- Milestone: `feature_sqlite_phase6_milestone.md` / `docs/feature_sqlite_phase6_milestone.md`
- Starting evidence: `sqlite_benchmark_report.md` / `docs/sqlite_benchmark_report.md`

Phase 5 references (complete):
- Plan/Milestone: `feature_perf_phase5_plan.md`, `feature_perf_phase5_milestone.md` (full read/write table, before→after)
- Baseline evidence: `fio_direct_parameter_sweep_report.md`, `fio_direct_senior_feedback_response.md`
- Main ext4 code: `kernel/src/fs/ext4/fs.rs` (`inode_meta_cache` + `meta_cache_generation` gen-guard, `inode_extent_map_cache`, `stat`, `run_journaled_ext4` single-chokepoint invalidation, `JOURNAL_CHECKPOINT_MAX_DEPTH`, overwrite fast-path, `DirectReadProfileStats`/`DirectWriteProfileStats`, `sync`), `kernel/src/fs/ext4/inode.rs` (cached `type_()` stat), `kernel/libs/ext4_rs/src/ext4_impls/extents.rs` (A1 zero-extent fix), `kernel/comps/block/src/bio.rs` (`[block-profile]` dumps)

Phase 5 remaining open items (virtio/platform territory, align with advisor): concurrent read nj>1 lock degradation, bio_copy, symlink/512B-align small gaps, ext2 4K O_DIRECT write hang (reference-impl quirk, not ours), A2 (page_cache=0 legacy Vec path corruption — page_cache=1 unaffected, low priority, may be cleaned in Phase 6).
