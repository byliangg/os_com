See [AGENTS.md](AGENTS.md).

Current ext4 context: JBD2 Phase 1/2/3, PageCache Phase 4, and performance Phase 5 are all complete (guard regressions all green). Phase 5 closed out the O_DIRECT read/write floor (75–123%), fixed the two SQLite crash bugs (A1 allocator underflow, B checkpoint OOM), and landed the overwrite fast-path (SQLite 4773→2022s, 2.36×). The current line is **feature_sqlite_phase6**: optimizing SQLite real-application *append / new-allocation* writes (INSERT new blocks / CREATE INDEX / VACUUM), with **delalloc (delayed allocation)** as the prior main line and a profile-first methodology.

Honest cache-off baseline (NOT the old cache-on `read 127% / write 39%`, which must not be used for the defense). All numbers `direct=1, nj=1`, drop-caches fair口径, median-of-N, with `ext4fs.extent_map_cache` + inode metadata cache active:

| bs | read | write |
|----|-----:|------:|
| 4K | 86.38% | 75.54% |
| 16K | 84.42% | 75.78% |
| 64K | 86.89% | 84.09% |
| 256K | 94.81% | 121.07% |
| 1M | 122.94% | 88.28% |

Before optimization small blocks sat at 16–24% read and write 4K=20% / 1M=63%. Four ext4-domain optimizations got us here: (1) extent mapping plan cache, (2) whole-file coverage for random reads, (3) relatime atime throttling, (4) **inode metadata cache** (the big win — `get_inode_ref` was reloading the inode block from device on every `stat`). The ext4-domain per-op fixed overhead is now exhausted; the remaining gap is the Asterinas virtio device round-trip (platform layer, common across FS — confirmed by ext2 hitting the same 82–85% ceiling on the same platform).

Phase 6 references (current):
- Plan: `feature_sqlite_phase6_plan.md` / `docs/feature_sqlite_phase6_plan.md`
- Milestone: `feature_sqlite_phase6_milestone.md` / `docs/feature_sqlite_phase6_milestone.md` (starting baseline 2.97%, guard gates, SQLite re-measure)
- Starting evidence: `sqlite_benchmark_report.md` / `docs/sqlite_benchmark_report.md`
- Phase 6 target path: SQLite append/new-allocation writes still take the slow journaled-allocation path in `write_at_page_cache` (`kernel/src/fs/ext4/fs.rs`) — every newly-allocated 4KB page runs a full `run_journaled_ext4(JournaledOp::Write)` (JBD2 handle + `EXT4_RS_RUNTIME_LOCK` + `ext4_map_blocks` alloc + meta cache clear) plus per-transaction fsync plus per-4KB bio. The overwrite fast-path (`write_range_fully_mapped` → `touch_mtime_ctime` + page_cache.write) already bypasses this for in-place rewrites. **Profile-first** (Phase 5 lesson: the un-profiled writeback-batching attempt only got 3% and was reverted).

Phase 5 references (complete):
- Plan/Milestone: `feature_perf_phase5_plan.md`, `feature_perf_phase5_milestone.md` (full read/write table, before→after)
- Baseline evidence: `fio_direct_parameter_sweep_report.md`, `fio_direct_senior_feedback_response.md`
- Main ext4 code: `kernel/src/fs/ext4/fs.rs` (`inode_meta_cache` + `meta_cache_generation` gen-guard, `inode_extent_map_cache`, `stat`, `run_journaled_ext4` single-chokepoint invalidation, `JOURNAL_CHECKPOINT_MAX_DEPTH`, overwrite fast-path, `DirectReadProfileStats`/`DirectWriteProfileStats`, `sync`), `kernel/src/fs/ext4/inode.rs` (cached `type_()` stat), `kernel/libs/ext4_rs/src/ext4_impls/extents.rs` (A1 zero-extent fix), `kernel/comps/block/src/bio.rs` (`[block-profile]` dumps)

Phase 5 remaining open items (virtio/platform territory, align with advisor): concurrent read nj>1 lock degradation, bio_copy, symlink/512B-align small gaps, ext2 4K O_DIRECT write hang (reference-impl quirk, not ours), A2 (page_cache=0 legacy Vec path corruption — page_cache=1 unaffected, low priority, may be cleaned in Phase 6).
