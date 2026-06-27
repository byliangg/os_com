# ext4 PageCache 集成记录

| xfstests ID | 验收覆盖点 | 结果 | 日志 |
|-------------|------------|------|------|
| `generic/091` | fsx O_DIRECT 小块与并发 buffered I/O | PASS：collapse/insert/unshare 正确返回 `EOPNOTSUPP` 供 fsx 探测禁用；sparse direct write 预分配 stale data 与 zero/punch mmap 可见性已修复 | `benchmark/logs/pagecache_phase4_20260512_025549.log` |
| `generic/130` | buffered/direct coherency、hole、direct EOF zeroing | PASS：truncate shrink partial EOF block stale data 已修复；prepare-write 新分配块清零后 full list PASS | `benchmark/logs/pagecache_phase4_20260512_030905.log`, `benchmark/logs/pagecache_phase4_20260512_040825.log` |
| `generic/133` | 同一文件并发 buffered/direct 读写 | PASS：zero-link inode cleanup 后 unlink 释放数据块，ENOSPC 已消除 | `benchmark/logs/pagecache_phase4_20260512_032624.log`, `benchmark/logs/pagecache_phase4_20260512_040825.log` |
| `generic/208` | AIO DIO read-cache invalidation race | NOTRUN：`aio-dio-invalidate-failure` 未构建 | `benchmark/logs/pagecache_phase4_20260512_040825.log` |
| `generic/209` | sync DIO 对 readahead/page cache 的 invalidation | NOTRUN：`aio-dio-invalidate-readahead` 未构建 | `benchmark/logs/pagecache_phase4_20260512_040825.log` |
| `generic/247` | direct I/O 与 mmap writer race | PASS（single + full list） | `benchmark/logs/pagecache_phase4_20260512_031754.log`, `benchmark/logs/pagecache_phase4_20260512_040825.log` |
| `generic/263` | fsx direct I/O 与 sub-block buffered I/O 混合 | PASS：shared VMO dirty tracking、fallocate 后 evict、空 metadata transaction 不再记录 inode TID 后 clean run 已通过 | `benchmark/logs/pagecache_phase4_20260513_091148.log`, `benchmark/logs/pagecache_phase4_20260513_091938.log` |
| `generic/366` | direct read/write 与 buffered write 混合 hang 回归 | NOTRUN：O_DIRECT 512-byte alignment 不支持 | `benchmark/logs/pagecache_phase4_20260512_040825.log` |
| `generic/412` | direct I/O + buffered write + truncate into hole 持久化 | PASS（full list） | `benchmark/logs/pagecache_phase4_20260512_040825.log` |
| `generic/418` | buffered/direct 混用的显式 pagecache invalidation | PASS：`generic/247 -> generic/418` stale data 根因为旧 checkpoint metadata home-write 覆盖复用后的 data block；buffered/PageCache writeback 与 O_DIRECT write 均 revoke 对应 mapped data block 的 checkpoint metadata 后 clean 序列与 full list 已通过 | `benchmark/logs/pagecache_phase4_20260513_091558.log`, `benchmark/logs/pagecache_phase4_20260513_091938.log` |
| `generic/469` | truncate-down 后 page cache EOF 之后清零 | PASS：truncate 前 dirty PageCache writeback 已修复；minimal fallocate 解除 NOTRUN | `benchmark/logs/pagecache_phase4_20260512_033122.log`, `benchmark/logs/pagecache_phase4_20260512_040825.log` |
| `generic/749` | mmap EOF partial-page zero-fill 与 SIGBUS 边界 | PASS：PageCache 数据校验与 mmap EOF SIGBUS 边界均已修复 | `benchmark/logs/pagecache_phase4_20260512_023459.log` |
| `generic/751` | page-cache truncation + writeback 压力 | NOTRUN：guest 缺少 `/sys/kernel/debug` debugfs | `benchmark/logs/pagecache_phase4_20260512_040825.log` |


## 代码审计

### 代码审计结论

| 主题 | 结论 | 证据 |
|------|------|------|
| ext2 PageCache 结构 | `InodeInner` 持有 `PageCache`，通过 `PageCache::with_capacity(num_page_bytes, Arc::downgrade(&inode_impl.block_manager))` 接入 backend | `kernel/src/fs/ext2/inode.rs` |
| ext2 VFS 暴露 | `Inode::page_cache()` 返回 `Some(self.page_cache())`，`sync_all/sync_data` 在 inode sync 后做 `block_device().sync()` | `kernel/src/fs/ext2/impl_for_vfs/inode.rs` |
| ext2 backend | `InodeBlockManager impl PageCacheBackend`，page idx 直接对应 file block idx，读写经 `read_block_async/write_block_async` | `kernel/src/fs/ext2/inode.rs` |
| ext2 direct I/O coherency | direct read/write 会对重叠 `page_cache` range 做 discard；ext4 接入时必须区分 dirty page writeback，不能照抄成无条件丢弃 | `kernel/src/fs/ext2/inode.rs` |
| Asterinas PageCache 能力 | `evict_range` 写回 dirty page，`discard_range` 丢弃 page，`commit_overwrite` 支持覆盖写不预读旧页 | `kernel/src/fs/utils/page_cache.rs` |
| VFS mmap 入口 | `InodeHandle::mappable()` 仅在 `inode.page_cache()` 返回 `Some(Vmo)` 时可用 | `kernel/src/fs/inode_handle.rs` |
| ext4 buffered read | 非 O_DIRECT 当前分配 `Vec`，调用 `Ext4Fs::read_at()` / `ext4.ext4_read_at()`，不走 PageCache | `kernel/src/fs/ext4/inode.rs`, `kernel/src/fs/ext4/fs.rs` |
| ext4 buffered write | 非 O_DIRECT 当前把用户数据复制到 `Vec`，调用 `Ext4Fs::write_at()` / `ext4.ext4_write_at()`，数据同步直写 | `kernel/src/fs/ext4/inode.rs`, `kernel/src/fs/ext4/fs.rs` |
| ext4 page_cache 暴露 | `Ext4Inode` 当前没有覆盖 `Inode::page_cache()`，regular-file mmap 不能复用 PageCache | `kernel/src/fs/ext4/inode.rs` |
| ext4 inode wrapper 生命周期 | `Ext4Fs::make_inode()` 每次创建新的 `Ext4Inode`，所以 PageCache 必须挂到 `Ext4Fs` per-inode state，而不是 wrapper 字段 | `kernel/src/fs/ext4/fs.rs` |
| ext4 自研 cache | `DirectReadCache` 是 O_DIRECT mapping/speculative read 优化，默认可由 `ext4fs.direct_read_cache=0` 关闭；不是 PageCache 替代品 | `kernel/src/fs/ext4/fs.rs`, `benchmark/benchmark.md` |
| Phase 3 fsync 前置 | `fsync_regular_file()` 已基于 inode -> TID force commit；接入 PageCache 后需要先 drain dirty page | `kernel/src/fs/ext4/fs.rs` |



### 新增 upstream xfstests 验收

| 测试项 | 结果 | 日志 |
|--------|------|------|
| `XFSTESTS_MODE=pagecache_phase4` | `9 PASS / 0 FAIL / 4 NOTRUN`，有效样本 pass rate `100.00%` | `benchmark/logs/pagecache_phase4_20260513_091938.log` |
| `generic/091` | PASS | `benchmark/logs/pagecache_phase4_20260512_025549.log` |
| `generic/130` | PASS | `benchmark/logs/pagecache_phase4_20260512_030905.log` |
| `generic/133` | PASS | `benchmark/logs/pagecache_phase4_20260512_032624.log` |
| `generic/208` | NOTRUN：helper 未构建 | `benchmark/logs/pagecache_phase4_20260512_040825.log` |
| `generic/209` | NOTRUN：helper 未构建 | `benchmark/logs/pagecache_phase4_20260512_040825.log` |
| `generic/247` | PASS（single + full list） | `benchmark/logs/pagecache_phase4_20260512_031754.log`, `benchmark/logs/pagecache_phase4_20260512_040825.log` |
| `generic/263` | PASS（clean + full list） | `benchmark/logs/pagecache_phase4_20260513_091148.log`, `benchmark/logs/pagecache_phase4_20260513_091938.log` |
| `generic/366` | NOTRUN：O_DIRECT 512-byte alignment 不支持 | `benchmark/logs/pagecache_phase4_20260512_040825.log` |
| `generic/412` | PASS | `benchmark/logs/pagecache_phase4_20260512_040825.log` |
| `generic/418` | PASS（clean `generic/247,generic/418` + full list） | `benchmark/logs/pagecache_phase4_20260513_091558.log`, `benchmark/logs/pagecache_phase4_20260513_091938.log` |
| `generic/469` | PASS | `benchmark/logs/pagecache_phase4_20260512_033122.log` |
| `generic/749` | PASS | `benchmark/logs/pagecache_phase4_20260512_023459.log` |
| `generic/751` | NOTRUN：debugfs 不可用 | `benchmark/logs/pagecache_phase4_20260512_040825.log` |

### benchmark

| 测试项 | Asterinas | Linux | ratio | 日志 |
|--------|----------:|------:|------:|------|
| A. `lmbench_only` | `8/8 PASS` | N/A | N/A | `benchmark/logs/lmbench/phase4_part3_lmbench_summary_20260514_051539.tsv` |
| B/C. buffered fio cold/warm read A/B | `page_cache=0`: cold 121.0 MB/s, warm 122.0 MB/s；`page_cache=1`: cold 19.9 MB/s, warm 4022.0 MB/s | cold 3948.0 MB/s, warm 7457.0 MB/s | warm read：`page_cache=1` 为 Linux 53.94%，为 `page_cache=0` 的 3296.72% | `benchmark/logs/pagecache_buffered_fio/pagecache_buffered_fio_summary_20260514_130056.tsv` |
| D. buffered fio write A/B | `page_cache=0`: 38.4 MB/s；`page_cache=1`: 10.8 MB/s | 633.0 MB/s | `page_cache=0`: 6.07%；`page_cache=1`: 1.71% | `benchmark/logs/pagecache_buffered_fio/pagecache_buffered_fio_summary_20260514_130056.tsv` |
| E1. fio O_DIRECT read cache-off | 2570 MB/s | 2643 MB/s | 97.24% | `benchmark/logs/fio_ext4_cacheoff_20260514_1345/ext4_seq_read_bw.log` |
| E2. fio O_DIRECT write cache-off | 1706 MB/s | 3158 MB/s | 54.02% | `benchmark/logs/fio_ext4_cacheoff_20260514_1345/ext4_seq_write_bw.log` |




## PageCache 性能 hardening（可选）


### 候选方向

| 方向 | 状态 | 备注 |
|------|------|------|
| ext4 extent-aware readahead | 待评估 | 与 `PageCacheManager` readahead window 结合 |
| dirty page writeback bio 合并 | 待评估 | 减少单页写回开销 |
| mmap sequential fault 预读 | 待评估 | 看 VFS/VM fault 路径能力 |
| PageCache state 回收 | 待评估 | 避免 inode cache 生命周期导致内存膨胀 |
| correctness lock 缩短 | 待评估 | 先保证语义，再拆锁 |
| O_DIRECT mapping cache 重构 | 待评估 | metadata-only，不能和 PageCache 混用 |



