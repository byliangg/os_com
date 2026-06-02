See [AGENTS.md](AGENTS.md).

Current ext4 context: JBD2 Phase 1/2/3 and PageCache Phase 4 are all complete (guard regressions all green). The current line is **feature_perf_phase5**: a performance-optimization phase driven by latency attribution. Goal: lift O_DIRECT write (honest cache-off guard `direct=1, bs=1M, nj=1` = 51.60%) toward 90%, fix the small-block (4K/16K) per-request overhead in the ext4 direct path, and the O_DIRECT read multi-job degradation.

Honest cache-off baseline (NOT the old cache-on `read 127% / write 39%`, which must not be used for the defense):
- O_DIRECT read `bs=1M nj=1`: 101.91% (passing)
- O_DIRECT write `bs=1M nj=1`: 51.60% (main blocker)
- O_DIRECT write `bs=1M nj=4` (ext4j): 95.01% (multi-job can pass)
- O_DIRECT write/read `bs=4K nj=1` (ext4j): 20.59% / 11.38% (small-block ext4 itself is weak)

Phase 5 starting points:
- Plan: `feature_perf_phase5_plan.md`
- Milestone: `feature_perf_phase5_milestone.md`
- Baseline evidence: `fio_direct_parameter_sweep_report.md`, `fio_direct_senior_feedback_response.md`
- Main ext4 targets: `kernel/src/fs/ext4/fs.rs` (`DirectWriteProfileStats` / `DirectReadProfileStats` / `Ext4RsRuntimeLockStats` / `JournaledOpProfileStats`, `write_direct_at`, `read_direct_at`, `sync`), `kernel/comps/block/src/bio.rs` (`[block-profile]` write/read-bio stats)

Key boundary: the profiling infrastructure is already built end-to-end across four layers (FS / virtio / lock / JBD2, ns-level, gated by `ext4fs.phase2_profile=1`). **Do not rebuild it** — the missing piece is a final dump (force-print full cumulative summary in `sync()`/unmount) plus harvesting the breakdown table over a 1M + 4K/16K matrix. Three bottlenecks: (1) large-block single-job ceiling lives in block/virtio (ext4 ≈ raw, not ext4's fault); (2) small-block per-request overhead lives in ext4 (best "we optimized ext4" story); (3) read multi-job degradation is lock contention. JBD2 and the large-block ext4 path are cleared of suspicion by the data. The 1M positioning (virtio vs ext4 credit) needs alignment with the advisor for the defense framing.
