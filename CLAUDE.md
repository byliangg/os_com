See [AGENTS.md](AGENTS.md).

Current ext4/JBD2 context: Phase 1 and Phase 2 are complete; Phase 3 is in progress (Step 0-4 main line done; Step 4c / Step 5 / Step 6 pending).

Phase 3 highlights:
- Step 0: Docker mode `jbd_phase3_fsync_flush` + xfstests Tier 1 list + godown shim fail-fast + xfs_io truncate/fpunch
- Step 1: Observation points (fsync JBD2 state, BlockDevice::sync, virtio flush branches)
- Step 2: `RamInode::sync_all/sync_data` → `aster_block::lookup().sync()` (raw `/dev/vda` fsync 302ns no-op → 1597us real flush)
- Step 3: virtio FLUSH bit check + `flush()` branch inversion fix
- Step 4a-1: `Ext4Inode::sync_all/sync_data` end with `block_device().sync()` (mirror ext2 VFS pattern; ext4 fsync 49us → 2374us, with Linux 1884us same order)
- Step 4a-2: inode→TID map + `force_commit_for_tid` + `WaitQueue` (Linux `jbd2_journal_force_commit_nested` equivalent; replaces "two commit_pending" hack)
- Step 4b: `EXT4_IOC_SHUTDOWN` ioctl with NOLOGFLUSH/LOGFLUSH/DEFAULT + shutdown state machine + needs_recovery SB write
- Step 4d: Tier 1 xfstests **9 PASS / 1 NOTRUN / 2 FAIL**; phase3/phase4/phase6/jbd_phase1/jbd_phase2_concurrency regressions all pass at Phase 2 baseline.

Remaining work (parked):
- generic/049 (journal space pressure)
- generic/392 (fdatasync atime-only distinction)
- Step 4c (commit-block-pre PREFLUSH for strict ordered-mode barrier)
- crash matrix truncate_append regression vs Phase 2 baseline
- fio O_DIRECT write ratio recovery to ≥ 90%

Before changing fsync/flush behavior, read `docs/feature_jbd2_phase3_pretest.md`, `docs/feature_jbd2_phase3_plan.md`, and `docs/feature_jbd2_phase3_milestone.md`.
