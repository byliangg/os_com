See [AGENTS.md](AGENTS.md).

Current ext4 context: JBD2 Phase 1/2/3 and PageCache Phase 4 are all complete (guard regressions all green). The current line is **feature_perf_phase5**: a performance-optimization phase driven by latency attribution. The read-side optimization has now landed and is closed out.

Honest cache-off baseline (NOT the old cache-on `read 127% / write 39%`, which must not be used for the defense). All numbers `direct=1, nj=1`, drop-caches fair口径, median-of-N, with `ext4fs.extent_map_cache` + inode metadata cache active:

| bs | read | write |
|----|-----:|------:|
| 4K | 86.38% | 75.54% |
| 16K | 84.42% | 75.78% |
| 64K | 86.89% | 84.09% |
| 256K | 94.81% | 121.07% |
| 1M | 122.94% | 88.28% |

Before optimization small blocks sat at 16–24% read and write 4K=20% / 1M=63%. Four ext4-domain optimizations got us here: (1) extent mapping plan cache, (2) whole-file coverage for random reads, (3) relatime atime throttling, (4) **inode metadata cache** (the big win — `get_inode_ref` was reloading the inode block from device on every `stat`). The ext4-domain per-op fixed overhead is now exhausted; the remaining gap is the Asterinas virtio device round-trip (platform layer, common across FS — confirmed by ext2 hitting the same 82–85% ceiling on the same platform).

Phase 5 references:
- Plan: `feature_perf_phase5_plan.md`
- Milestone: `feature_perf_phase5_milestone.md` (full read/write table, before→after, open items)
- Baseline evidence: `fio_direct_parameter_sweep_report.md`, `fio_direct_senior_feedback_response.md`
- Main ext4 code: `kernel/src/fs/ext4/fs.rs` (`inode_meta_cache` + `meta_cache_generation` gen-guard, `inode_extent_map_cache`, `stat`, `run_journaled_ext4` single-chokepoint invalidation, `DirectReadProfileStats`/`DirectWriteProfileStats`, `sync`), `kernel/src/fs/ext4/inode.rs` (cached `type_()` stat in `read_at`/`write_at`), `kernel/comps/block/src/bio.rs` (`[block-profile]` dumps)

Remaining open items (virtio/platform territory, align with advisor): concurrent read nj>1 lock degradation, bio_copy, symlink/512B-align small gaps, ext2 4K O_DIRECT write hang (reference-impl quirk, not ours).
