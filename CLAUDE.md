See [AGENTS.md](AGENTS.md).

Current ext4 context: JBD2 Phase 1, Phase 2, and Phase 3 are complete. Phase 4 is now the PageCache integration line: replace ext4 regular-file buffered `Vec` read/write with Asterinas `PageCache` / `Vmo`, expose `Inode::page_cache()` for mmap, and isolate the old self-developed direct-read cache from the default correctness/benchmark path.

Phase 3 closed state:
- `jbd_phase3_fsync_flush`: default 2G scratch `11 PASS / 1 NOTRUN / 0 FAIL`
- 12G scratch `generic/048`: PASS
- host-crash fsync matrix: 4/4 PASS
- ordinary O_DIRECT fio: read 127.06% PASS, write 39.18% parked as later performance hardening

Phase 4 starting points:
- Plan: `docs/feature_pagecache_phase4_plan.md`
- Milestone: `docs/feature_pagecache_phase4_milestone.md`
- Main references: `kernel/src/fs/ext2/inode.rs`, `kernel/src/fs/ext2/impl_for_vfs/inode.rs`, `kernel/src/fs/utils/page_cache.rs`
- Main ext4 targets: `kernel/src/fs/ext4/inode.rs`, `kernel/src/fs/ext4/fs.rs`, and if needed `kernel/libs/ext4_rs/src/ext4_impls/file.rs`

Important boundary: `PageCache` is for buffered I/O, mmap, and writeback. O_DIRECT must still bypass PageCache, but O_DIRECT read/write must flush or discard overlapping PageCache ranges correctly. The existing `DirectReadCache` is an O_DIRECT mapping/speculative-read optimization, not a PageCache replacement.
