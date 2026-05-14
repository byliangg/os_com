# ext4 PageCache 集成 Phase 4 Milestone 记录

首次更新时间：2026-05-11（Asia/Shanghai）

当前状态（2026-05-14）：

- Step 0：代码审计完成，baseline benchmark 待补
- Step 1：默认关闭的 ext4 per-inode PageCache state 骨架已接入，`cargo check` 通过
- Step 2：gated buffered read 已走 PageCache，`cargo check` 通过
- Step 3：buffered write 已改为 journaled metadata prepare + dirty PageCache write；基础 writeback 已接入；sparse direct write allocation、truncate partial EOF zeroing、unlink 后数据块回收已修复
- Step 4：fsync drain / syncfs drain / direct read evict / truncate writeback+resize 已接入；minimal fallocate 支持已补齐 `Allocate` / `ZeroRange` / `PunchHoleKeepSize`；shared VMO mmap dirty tracking 已补齐
- Step 5：mmap/direct/buffered coherency 已收口：修复 stale JBD2 checkpoint metadata home-write 覆盖复用后的 regular-file data block，并避免空 metadata transaction TID 让 fsync 等待不存在的 commit
- Step 6：`pagecache_phase4` upstream xfstests 验收集已落地；最新 full list `9 PASS / 0 FAIL / 4 NOTRUN`，有效样本 pass rate `100.00%`
- Step 6 守底回归接手状态：`phase3_base_guard` / `phase4_good` / `phase6_good` / `jbd_phase1` 已通过；Phase 2 concurrency `unlink_while_open` 与 `jbd_phase3_fsync_flush` NOTRUN 退化已修复并复跑通过；JBD2 crash matrix 已恢复 `18/18 PASS`，Phase 3 host-crash fsync matrix 已恢复 `4/4 PASS`
- Step 7：可选性能 hardening，暂不作为 correctness 退场前置

阶段目标：把 ext4 regular-file buffered I/O 与 mmap 接入 Asterinas `PageCache` / `Vmo`，替换当前 non-O_DIRECT `Vec` 直通读写路径，并把 ext4 自研 direct-read cache 从默认 correctness / benchmark 口径中隔离。Phase 4 继续沿用“先 correctness，后性能”的边界。

## Phase 3 收口基线

| 测试项 | Phase 3 收口结果 | Phase 4 要求 |
|--------|------------------|--------------|
| `phase3_base_guard` | `10 PASS / 0 FAIL / 6 NOTRUN / 24 STATIC_BLOCKED` | 不回退 |
| `phase4_good` | `12 PASS / 0 FAIL / 6 NOTRUN / 22 STATIC_BLOCKED` | 不回退 |
| `phase6_good` | `25 PASS / 0 FAIL / 0 NOTRUN / 26 STATIC_BLOCKED` | 不回退 |
| `jbd_phase1` | `6 PASS / 0 FAIL / 6 NOTRUN` | 不回退 |
| JBD2 crash matrix | `18/18 PASS` | 不回退 |
| Phase 2 concurrency | `7/7 PASS`，`workers=4 rounds=8 seed=78` | 不回退 |
| `jbd_phase3_fsync_flush` | 默认 2G scratch `11 PASS / 1 NOTRUN / 0 FAIL` | 不回退 |
| Phase 3 12G scratch `generic/048` | `PASS` | 不回退或明确资源前置 |
| Phase 3 host-crash fsync matrix | `4/4 PASS` | 不回退 |
| fio O_DIRECT read | `127.06%` | 单独记录，不能和 PageCache 指标混算 |
| fio O_DIRECT write | `39.18%` | 后续性能 hardening blocker，不阻塞 Phase 4 correctness 规划 |

## Phase 4 验收口径

- `PASS` 必须同时满足 runner 成功、严格关键词扫描为空、数据校验通过；
- PageCache 指标与 O_DIRECT 指标分开统计；
- 默认综合 fio 使用 `EXT4_DIRECT_READ_CACHE=0` / `ext4fs.direct_read_cache=0`；
- `PageCache` 只作为 buffered I/O / mmap / writeback 机制，不为 O_DIRECT 提供数据缓存；
- mixed buffered/direct 测试必须覆盖 dirty page writeback 与 discard 顺序；
- `fsync` / `fdatasync` 必须先 drain dirty PageCache，再执行 Phase 3 force-commit + device flush；
- ext4 self-developed direct-read cache 如保留，只能作为 opt-in O_DIRECT mapping cache 实验，不作为 Phase 4 默认能力；
- Phase 4 不维护自研 PageCache smoke/coherency 测试集，新增 correctness 验收只使用 upstream xfstests `pagecache_phase4` list。

### PageCache Phase 4 upstream xfstests 验收集

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

最近一次 full list（2026-05-13）：`benchmark/logs/pagecache_phase4_20260513_091938.log`，结果 `9 PASS / 0 FAIL / 4 NOTRUN`，有效样本 pass rate `100.00%`。最近一次 clean 最小序列：`benchmark/logs/pagecache_phase4_20260513_091558.log`，`generic/247 PASS` 后 `generic/418 PASS`。

2026-05-13 补充诊断：

- `cargo check -p aster-kernel --target x86_64-unknown-none` 通过，仅有既有 warning；
- 单跑 `generic/418` PASS，说明 `418` 本身不是无条件失败；
- 早期 `generic/247,generic/418` clean 序列稳定复现，`418` BAD OUT 多次出现 `Expected: 0x1/0x2, got 0xa`；
- 后续 checkpoint 诊断确认：PageCache writeback 已写出正确数据，但稍后 JBD2 checkpoint 仍会把旧 metadata block 写回同一物理块；该物理块已被释放并复用为 regular-file data block，导致 O_DIRECT read 读到旧内容；
- 修复点：对 PageCache writeback、buffered write prepare 后映射、O_DIRECT write 映射执行 `revoke_checkpoint_metadata_block()`，从 checkpoint list 删除这些 data block 上残留的 metadata home-write；
- 额外修复：`finish_jbd2_handle()` 只在成功且 `modified_blocks > 0` 时记录 inode TID，避免空 handle 生成的 TID 让 `generic/263` fsync 等待不存在的 commit；
- 中间日志 `benchmark/logs/pagecache_phase4_20260513_090842.log` 证明只处理 PageCache writeback 不够，O_DIRECT write 也必须 revoke checkpoint metadata；最终 clean `generic/263`、clean `generic/247,generic/418` 与 full list 均通过。

执行入口：`XFSTESTS_MODE=pagecache_phase4` 或 `PHASE4_DOCKER_MODE=pagecache_phase4 tools/ext4/run_phase4_in_docker.sh`。该模式必须显式传入 `ext4fs.page_cache=1`，守底回归默认保持 PageCache 关闭。`pagecache_phase4_excluded.tsv` 默认为空，list 内所有 case 都属于验收范围；`NOTRUN` 只能记录工具/内核能力缺口，不作为静态排除规避。

## Step 0：代码审计与基线固化

**状态：** 进行中（2026-05-11）
**目标摘要：** 在改代码前，固定 ext2 PageCache 参考、ext4 当前自研 cache 路径、buffered/direct 分流和 benchmark 口径。

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

### 待固化 baseline

| 项目 | 命令 / 入口 | 预期记录 |
|------|-------------|----------|
| 6-test cache-off 综合 fio | `EXT4_DIRECT_READ_CACHE=0 ./asterinas/test/initramfs/src/benchmark/fio/run_6test_summary.sh` | Asterinas / Linux / ratio 与日志路径 |
| 普通 O_DIRECT 守底 fio | `./asterinas/test/initramfs/src/benchmark/fio/run_ext4_summary.sh` | read/write 与 Phase 3 结果对比 |
| lmbench regression | `PHASE4_DOCKER_MODE=part3_full ... run_phase4_in_docker.sh` | 8/8 或失败原因 |
| PageCache/VFS microbench | `test/initramfs/src/benchmark/lmbench/vfs_read_pagecache_bw` 或等价入口 | ext4 接入前后对比 |
| `pagecache_phase4` upstream xfstests | `PHASE4_DOCKER_MODE=pagecache_phase4 ... run_phase4_in_docker.sh` | 13 个上游 PageCache/direct-buffered/mmap case，目标 `FAIL=0` |
| Phase 3 fsync/flush | `PHASE4_DOCKER_MODE=jbd_phase3_fsync_flush ...` | 0 FAIL |
| host-crash fsync matrix | `PHASE4_DOCKER_MODE=jbd_phase3_host_crash ...` | 4/4 PASS |

### 改动概要

本 step 已新增 Phase 4 plan/milestone 文档，并完成后续 Step 1 的代码审计准备。benchmark baseline 尚未复跑。

### 涉及文件

| 文件 | 类型 |
|------|------|
| `docs/feature_pagecache_phase4_plan.md` | 新建 |
| `docs/feature_pagecache_phase4_milestone.md` | 新建 |
| `README.md` | 更新阶段说明与文档索引 |
| `AGENTS.md` / `CLAUDE.md` | 更新当前阶段指引 |
| `environment.md` / `docs/environment.md` | 更新 Phase 4 环境口径 |

### 验收项

- [x] ext2 PageCache 参考路径已记录
- [x] ext4 buffered I/O 直通路径已记录
- [x] ext4 self-developed direct-read cache 边界已记录
- [x] Phase 4 PageCache / O_DIRECT 边界已明确
- [ ] baseline 命令与日志路径已补齐
- [x] feature flag 策略已落到代码或文档

## Step 1：建立 ext4 per-inode PageCache state

**状态：** 部分完成（2026-05-11）
**目标摘要：** 给 ext4 regular-file 建立共享 `PageCache` / `Vmo`，初期不强制切换主要 I/O 行为。

### 改动概要

1. 新增 `ext4fs.page_cache` feature flag，默认关闭。
2. 在 `Ext4Fs` 中新增 `inode_page_caches`，按 ino 保存共享 `Ext4PageCacheState`。
3. 新增 `Ext4PageCacheState` / `Ext4PageCacheBackend` 骨架：
   - `PageCache::with_capacity()` 初始容量来自 inode size；
   - backend `npages()` 返回当前 inode size 对应页数；
   - backend read/write 当前同步复用 ext4_rs 直通读写能力，作为 Step 1 skeleton；
   - 后续 Step 2/3 再替换为 extent mapping + bio writeback。
4. `Ext4Inode::page_cache()` 在 regular-file 且 `ext4fs.page_cache=1` 时返回共享 `Arc<Vmo>`；默认关闭时返回 `None`，保持 Phase 3 行为。

### 涉及文件

| 文件 | 类型 |
|------|------|
| `kernel/src/fs/ext4/fs.rs` | 修改：feature flag、per-inode PageCache state、backend skeleton |
| `kernel/src/fs/ext4/inode.rs` | 修改：gated `Inode::page_cache()` |

### 验收项

- [x] 同一 ino 多个 `Ext4Inode` wrapper 共享同一 PageCache state（通过 `Ext4Fs::inode_page_caches` 设计保证）
- [x] regular-file `Inode::page_cache()` 在 `ext4fs.page_cache=1` 时返回 `Some(Arc<Vmo>)`
- [x] 非 regular-file 不暴露 PageCache
- [x] 默认关闭时 Phase 3 行为不变
- [ ] `pagecache_phase4` upstream xfstests 有明确结果

### 验证

```bash
VDSO_LIBRARY_DIR="$(pwd)/benchmark/assets/linux_vdso" \
CARGO_TARGET_DIR=/tmp/os_com_codex_phase4_kernel_target \
cargo check -p aster-kernel --target x86_64-unknown-none
```

结果：通过。当前仅有既有 warning：`JOURNALED_SMALL_WRITE_MAX_BYTES` 与 `commit_pending_jbd2_transactions` unused。

备注：`cargo fmt --check --package aster-kernel` 暴露全仓多处既有格式差异（包括未触碰文件），本 step 未执行全仓格式化，避免引入无关 diff。

## Step 2：接入 buffered read 与 read-page backend

**状态：** 部分完成（2026-05-11）
**目标摘要：** non-O_DIRECT read 改走 `PageCache.pages().read()`，backend 负责读盘 / hole zero-fill。

### 改动概要

1. `Ext4Inode::read_at()` 在 `ext4fs.page_cache=1` 且非 O_DIRECT 时进入 `Ext4Fs::read_at_page_cache()`。
2. `read_at_page_cache()` 按 inode size 裁剪读取长度，用 `VmWriter::limit(read_len)` 限制写入范围，然后调用共享 `Vmo::read()`。
3. read path 保留现有 atime 更新策略。
4. `Ext4PageCacheBackend::read_page_async()` 当前同步复用 ext4_rs `ext4_read_at()`，并用零初始化 page buffer 兜底 hole / EOF。

### 涉及文件

| 文件 | 类型 |
|------|------|
| `kernel/src/fs/ext4/inode.rs` | 修改：gated buffered read 分流 |
| `kernel/src/fs/ext4/fs.rs` | 修改：`read_at_page_cache()` 与 PageCache state resize |

### 功能结果

| case | 结果 | 日志 |
|------|------|------|
| normal buffered read | 待补 | 待补 |
| cross-page read | 待补 | 待补 |
| EOF read | 待补 | 待补 |
| sparse hole read | 待补 | 待补 |
| `pagecache_phase4` mmap/read 相关 case | 待补 | 待补 |

### 验收项

- [x] buffered read 在 `ext4fs.page_cache=1` 时不再走 `Ext4Fs::read_at()` 数据直通路径
- [x] EOF 裁剪由 inode size + writer limit 保证；hole 由零初始化 backend buffer 兜底
- [x] atime 行为沿用现有 `touch_atime()`
- [ ] phase3/phase4/phase6 smoke 不回退

## Step 3：接入 buffered write、dirty page writeback 与 JBD2 metadata 准备

**状态：** 部分完成（2026-05-11）
**目标摘要：** non-O_DIRECT write 改走 PageCache，同时保证 extent allocation、inode size、mtime/ctime 与 TID 追踪正确。

### 改动概要

1. `Ext4Inode::write_at()` 在 `ext4fs.page_cache=1` 且非 O_DIRECT 时进入 `Ext4Fs::write_at_page_cache()`。
2. `ext4_rs::prepare_write_at()` 已放宽为任意 offset/len：按触达 logical block 分配缺失 blocks，并在同一 helper 中更新 inode size。
3. `write_at_page_cache()` 现在先在 journaled handle 中执行 metadata prepare 与 mtime/ctime 更新，再把用户数据写入共享 VMO，让 dirty PageCache 负责后续 writeback。
4. `Ext4PageCacheBackend::write_page_async()` 走内部 `write_page_cache_data_at()`，避免 writeback 递归进入 `Ext4Inode::write_at()`；writeback 不再刷新 mtime/ctime，时间戳保持 buffered write 发生时刻。
5. direct write 与 truncate 后会 discard 已存在 PageCache state 的重叠范围或全量 cache，避免 PageCache read/mmap 读到旧页。
6. 当前 writeback 仍复用 ext4_rs `ext4_write_at()` 写 home data blocks；后续 Step 7 再评估 extent-aware/data-only bio 合并。
7. sparse O_DIRECT write 在分配新 extent 但 inode size 不变时，也会写回 inode metadata，避免 remount 后 extent 丢失。
8. truncate shrink 会在移除 extent 前清零 partial EOF block，避免 EOF 后 stale data 被 direct/pagecache 混用路径读出。

### 涉及文件

| 文件 | 类型 |
|------|------|
| `kernel/src/fs/ext4/inode.rs` | 修改：gated buffered write 分流 |
| `kernel/src/fs/ext4/fs.rs` | 修改：metadata prepare + dirty PageCache write、PageCache discard、backend writeback helper |
| `kernel/libs/ext4_rs/src/ext4_impls/file.rs` | 修改：`prepare_write_at()` 支持非 block-aligned buffered write range |
| `kernel/libs/ext4_rs/src/simple_interface/mod.rs` | 修改：暴露 minimal fallocate helper |

### 功能结果

| case | 结果 | 日志 |
|------|------|------|
| overwrite write | 待补 | 待补 |
| append / extend write | 待补 | 待补 |
| partial page write | 待补 | 待补 |
| ENOSPC handling | 待补 | 待补 |
| `generic/130` | PASS | `benchmark/logs/pagecache_phase4_20260512_030905.log` |
| `generic/418` | 单例 PASS，full list 仍 FAIL | `benchmark/logs/pagecache_phase4_20260512_043823.log`, `benchmark/logs/pagecache_phase4_20260512_044244.log` |
| `pagecache_phase4` mmap/writeback 相关 case | 部分通过，见 Step 6 xfstests 表 | 见下文 |

### 验收项

- [x] buffered write 不再走 `Ext4Fs::write_at()` 数据直写路径
- [x] dirty PageCache writeback 基础路径已接入
- [x] JBD2 metadata TID 追踪通过 `JournaledOp::Write { ino, len }` 保持
- [ ] crash matrix 不回退

## Step 4：fsync / truncate / direct I/O coherency 收口

**状态：** 部分完成（2026-05-11）
**目标摘要：** 将 PageCache 与 Phase 3 fsync、truncate、O_DIRECT 混用规则闭环。

### 改动概要

1. `Ext4Inode::sync_all()` / `sync_data()` 在 regular-file fsync 前调用 `Ext4Fs::sync_page_cache_for_inode()`。
2. `sync_page_cache_for_inode()` 只处理已存在的 PageCache state，不会为默认关闭或未触发 PageCache 的 inode 创建新 state。
3. O_DIRECT read 入口先 `evict_page_cache_range()`，防止绕过 dirty PageCache 直接读盘。
4. O_DIRECT write 成功后 discard 重叠 PageCache。
5. truncate 在 ext4 truncate/reset 前先 drain 已存在 dirty PageCache，成功后 discard 已存在 state，并 resize 到新的 inode size。
6. `FileSystem::sync()` 会 drain 当前 ext4 superblock 下全部已存在 PageCache state，再执行 JBD2 / device flush；该修复消除了 `generic/749` 的 mmap writeback checksum mismatch。
7. 新增 minimal fallocate 支持：`Allocate` / `AllocateKeepSize` / `ZeroRange` / `ZeroRangeKeepSize` / `PunchHoleKeepSize`；`CollapseRange` / `InsertRange` / `Unshare` 仍返回 `EOPNOTSUPP`。
8. 当前普通 buffered write 已进入 dirty PageCache，`fsync` / `fdatasync` drain 会触发 backend writeback；writeback 的 bio 合并与 data-only mapping helper 仍留作 hardening。

### 涉及文件

| 文件 | 类型 |
|------|------|
| `kernel/src/fs/ext4/inode.rs` | 修改：fsync/fdatasync 前 drain PageCache |
| `kernel/src/fs/ext4/fs.rs` | 修改：PageCache evict/discard/resize/sync-all helper、direct read evict、truncate writeback+resize、minimal fallocate dispatch |
| `kernel/src/fs/inode_handle.rs` | 修改：允许 O_DIRECT fd 进入 fallocate，unsupported mode 由具体 FS 返回 |
| `test/initramfs/src/syscall/xfstests/testcases/pagecache_phase4.list` | 新增：upstream xfstests PageCache 验收清单 |
| `test/initramfs/src/syscall/xfstests/blocked/pagecache_phase4_excluded.tsv` | 新增：默认空的 PageCache 验收静态排除表 |

### 功能结果

| case | 结果 | 日志 |
|------|------|------|
| buffered write + fsync + remount | 待补 | 待补 |
| buffered write + O_DIRECT read | 待补 | 待补 |
| O_DIRECT write + buffered read | 待补 | 待补 |
| truncate shrink + cached read | PASS：`generic/130` / `generic/469` 覆盖到关键场景 | `benchmark/logs/pagecache_phase4_20260511_121722.log`, `benchmark/logs/pagecache_phase4_20260511_163603.log` |
| truncate extend + hole read | 待补 | 待补 |
| mmap/truncate/pagecache writeback upstream cases | 部分通过：`generic/247` / `generic/749` PASS | `benchmark/logs/pagecache_phase4_20260511_162025.log`, `benchmark/logs/pagecache_phase4_20260512_023459.log` |

### 2026-05-11 进展记录

1. minimal fallocate 支持解除 `generic/469` / `generic/749` 的 `xfs_io falloc -k` NOTRUN；`generic/469` 已从真实失败修到 PASS。
2. truncate 前 drain dirty PageCache，修复 fallocate + write + truncate + mapread 读到全零的问题。
3. syncfs / `FileSystem::sync()` drain 所有 ext4 PageCache state，修复 `generic/749` 中 mmap 写入后 checksum mismatch。
4. `generic/749` 当前剩余问题缩小为 VM mmap EOF 边界信号语义：测试期望 SIGBUS，当前仍收到 SIGSEGV；一次 VM 层 ENXIO->SIGBUS 实验会导致重复 page fault 和超大日志，已撤回，日志 `benchmark/logs/pagecache_phase4_20260511_164853.log` 不作为有效结果。
5. `generic/091` 初始失败中的 `FALLOC_FL_COLLAPSE_RANGE` 已通过返回 `EOPNOTSUPP` 让 fsx 探测禁用；后续暴露的 sparse direct write stale data 与 zero/punch mmap 可见性问题已在 2026-05-12 修复。

### 2026-05-12 进展记录

1. VM shared VMO-backed mapping 在访问超过 VMO page-rounded size 时返回 `ENXIO`，exception 层将该类 page fault 转换为 `SIGBUS/BUS_ADRERR`。
2. `FaultSignal` 增加同步 fault 标记；signal pending/dispatch 不再静默忽略被设置为 `SIG_IGN` 的同步 fault，而是按默认 fatal action 处理，避免返回用户态后反复触发同一 page fault。
3. `generic/749` 单例通过：`1 PASS / 0 FAIL`，日志 `benchmark/logs/pagecache_phase4_20260512_023459.log`。
4. `generic/091` 继续推进后暴露两个数据一致性问题：direct write sparse 预分配会映射未清零物理块；zero/punch 使用旧逐块 helper 时 mmap read 可见旧页尾部数据。当前修复为写路径禁止额外预分配、新分配块先清零，zero/punch 复用 `write_at` 分块写零。
5. `generic/091` 单例通过：`1 PASS / 0 FAIL`，日志 `benchmark/logs/pagecache_phase4_20260512_025549.log`。

### 验收项

- [x] `fsync` / `fdatasync` 先 drain 已存在 PageCache，再 force commit + final flush
- [x] O_DIRECT read 前会 evict 重叠 PageCache
- [x] O_DIRECT write 后会 discard stale cached page
- [x] truncate 前会 drain dirty PageCache，truncate 后会 discard + resize 已存在 PageCache state
- [x] syncfs 会 drain 已存在 PageCache state
- [x] minimal fallocate 支持已覆盖 `generic/469` 依赖路径
- [ ] unlink / rename 与 PageCache 生命周期一致
- [ ] Phase 3 fsync/flush 回归不回退
- [x] `pagecache_phase4` upstream xfstests `FAIL=0`

## Step 5：退役或隔离自研 direct-read cache

**状态：** 已完成（2026-05-13 correctness 收口）
**目标摘要：** 将 ext4 自研 O_DIRECT read cache 从默认口径中移出，必要时保留 opt-in 性能实验。

### 改动概要

1. PageCache 模式下默认绕开 self-developed direct-read mapping cache，避免 O_DIRECT read 与 buffered/PageCache dirty state 混用时引入额外状态源。
2. PageCache writeback、buffered write metadata prepare 后映射、O_DIRECT write 映射均会 revoke checkpoint list 中同物理块上的 stale metadata home-write。
3. JBD2 checkpoint revoke 只删除 checkpoint metadata buffer，不影响已提交事务 TID 与后续 metadata recovery；用于处理 freed metadata block 被复用为 regular-file data block 后的 home-write 顺序问题。
4. inode fsync TID 只记录非空 metadata transaction，避免 overwrite/mtime 同秒等空 handle 让 fsync 等待一个不会 commit 的 TID。

### 涉及文件

| 文件 | 类型 |
|------|------|
| `kernel/src/fs/ext4/fs.rs` | 修改：PageCache/direct write mapped data block checkpoint revoke；非空 metadata transaction 才记录 inode TID |
| `kernel/libs/ext4_rs/src/ext4_impls/jbd2/journal.rs` | 修改：新增 checkpoint metadata block revoke helper 与单元测试 |
| `kernel/libs/ext4_rs/src/ext4_impls/jbd2/transaction.rs` | 修改：支持从 checkpoint transaction 删除指定 metadata block |

### 验收项

- [x] PageCache 模式绕开 direct-read mapping cache
- [x] dirty PageCache / O_DIRECT write 与 JBD2 checkpoint home-write 顺序已收口
- [x] `generic/263` / `generic/418` clean 序列不再复现 stale data 或 fsync hang
- [ ] cache-off 性能结果已记录

## Step 6：全量回归与 benchmark

**状态：** 守底回归完成，benchmark 待补（2026-05-14：PageCache xfstests `FAIL=0`，Phase 2 / Phase 3 fsync-flush / crash / host-crash 均已恢复）
**目标摘要：** 完成 Phase 4 退场验证与文档更新。

### 功能回归

| 测试项 | 结果 | 日志 |
|--------|------|------|
| `phase3_base_guard` | PASS：`pass_rate=100.00%` | `benchmark/logs/phase3_base_guard_20260513_121817.log` |
| `phase4_good` | PASS：`pass_rate=100.00%` | `benchmark/logs/phase4_good_20260513_123012.log` |
| `phase6_good` | PASS：`25 PASS / 0 FAIL` | `benchmark/logs/phase6_good_20260513_124232.log` |
| `jbd_phase1` | PASS：有效样本 `100.00%` | `benchmark/logs/jbd_phase1_20260513_130709.log` |
| JBD2 crash matrix | PASS：`18/18`，两轮 9 场景全通过；`truncate_append` crash 注入点已修正为命中最终 append write | `benchmark/logs/crash/phase4_part3_crash_summary_20260514_043248.tsv` |
| Phase 2 concurrency baseline | PASS：`7 PASS / 0 FAIL`，`workers=4 rounds=8 seed=78`；`unlink_while_open` short read 已修复 | `benchmark/logs/jbd_phase2_concurrency_20260514_034441.log` |
| `jbd_phase3_fsync_flush` | PASS：`11 PASS / 0 FAIL / 1 NOTRUN`，仅 `generic/048` 因 2G scratch 缺少 10GB 空间 NOTRUN，恢复历史守底口径 | `benchmark/logs/jbd_phase3_fsync_durability_20260514_034641.log` |
| Phase 3 host-crash fsync matrix | PASS：`4/4`，fsync size / fdatasync metadata / rename+dst dir fsync / concurrent fsync 均通过 | `benchmark/logs/crash/phase4_part3_crash_summary_20260514_043536.tsv` |

### 2026-05-14 接手诊断

1. 当前无残留 Docker / QEMU 测试进程；上一轮 `cargo check -p aster-kernel --target x86_64-unknown-none` 通过，仅有既有 warning。
2. Phase 2 concurrency 历史守底参数为 `workers=4 rounds=8 seed=78`，要求 `7/7 PASS`；本轮 full run 与 `unlink_while_open` 单项 clean run 都出现 `short read path=/ext4_phase2/phase2/unlink_while_open/open_unlink_00.dat offset=0 remaining=1024`。
3. 根因：2026-05-12 为解决 zero-link inode 数据块释放加入的 `cleanup_unlinked()` 会在 unlink 后尝试 truncate nlink=0 regular file；ext4 `is_dentry_cacheable()` 为 `false`，unlink 路径可能新建 inode wrapper，不能依赖 `Arc::strong_count(&child_inode) == 1` 判断没有 open fd，否则会提前清空仍 open 的 unlinked inode。
4. 修复：VFS `InodeHandle` 在非 `O_PATH` open/Drop 时通知 inode；ext4 以 ino 维护 open handle 计数，`cleanup_unlinked_file()` 仅在 nlink=0 且 open count 为 0 时回收数据块。Drop 路径只更新计数，不做 stat/truncate I/O，避免 dup2 等 atomic 路径触发 `BioWaiter::wait` panic。
5. `jbd_phase3_fsync_flush` 的 `generic/043-049` NOTRUN 退化来自 xfstests `xfs_io` shim 优先 exec 注入的真实 `xfs_io`，而当前 ext4 尚无 native FIEMAP ioctl；含 `help fiemap` / `fiemap` 的命令改为固定走 shim emulation 后恢复到 `11 PASS / 1 NOTRUN / 0 FAIL`，唯一 NOTRUN 为 `generic/048` 10GB scratch 资源前置。
6. `crash_only` 的 `truncate_append` 历史失败不是 JBD2 replay 数据错误，而是测试注入点命中初始化 `dd` 写：prepare 在 `dd 512B` 后即被 `write:after_commit` hold 住，后续 `: > file` 与 append 未执行。初始化改为 `truncate -s 512` 后，`write` hold 命中最终 append，单项与 full matrix 均恢复通过。

### Phase 4 新增 upstream xfstests 验收

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
| E3. 6-test cache-off 综合 | 可选综合诊断，待补 | 可选综合诊断，待补 | 可选综合诊断，待补 | `EXT4_DIRECT_READ_CACHE=0 KEEP_LOGS=1 test/initramfs/src/benchmark/fio/run_6test_summary.sh` |

### Phase 4 benchmark 口径（2026-05-14 确认）

1. PageCache 收益只用 buffered I/O 口径观察：`fio` 使用官方工具 `/benchmark/bin/fio`，参数核心为 `direct=0`；不引入自研计时 benchmark。
2. buffered read 拆为 cold / warm：先用 `direct=1` write 准备测试文件，随后同一挂载内 `direct=0` 读第一遍为 cold read，第二遍为 warm read。
3. PageCache A/B 使用同一 workload 对比 `ext4fs.page_cache=0` 与 `ext4fs.page_cache=1`；benchmark harness 新增 `EXT4_PAGE_CACHE` 变量传递该 kcmd 开关。实测显示 PageCache-on warm read 明显命中缓存，但 cold read / buffered write 仍有明显性能 hardening 空间。
4. O_DIRECT fio 仍沿用原官方/历史 `direct=1` 口径，只作为 non-PageCache guard；不得把 O_DIRECT read/write 与 PageCache buffered 收益混算。
5. 新增脚本：
   - `test/initramfs/src/benchmark/fio/ext4_buffered_seq_read_bw/run.sh`
   - `test/initramfs/src/benchmark/fio/ext4_buffered_seq_write_bw/run.sh`
   - `test/initramfs/src/benchmark/fio/run_pagecache_buffered_summary.sh`

### 验收项

- [x] 守底回归 0 FAIL（已恢复 Phase 2 concurrency、`jbd_phase3_fsync_flush`、JBD2 crash matrix 与 Phase 3 host-crash matrix）
- [x] `pagecache_phase4` upstream xfstests `FAIL=0`
- [x] benchmark A-E 结果与日志路径完整（6-test 综合诊断保留为可选补充）
- [ ] README / benchmark / technical_report / core_results 已按需更新
- [ ] Phase 4 收口或遗留项明确

## Step 7：PageCache 性能 hardening（可选）

**状态：** 待执行
**目标摘要：** correctness 收口后，再优化 buffered / mmap / page writeback 性能。

### 候选方向

| 方向 | 状态 | 备注 |
|------|------|------|
| ext4 extent-aware readahead | 待评估 | 与 `PageCacheManager` readahead window 结合 |
| dirty page writeback bio 合并 | 待评估 | 减少单页写回开销 |
| mmap sequential fault 预读 | 待评估 | 看 VFS/VM fault 路径能力 |
| PageCache state 回收 | 待评估 | 避免 inode cache 生命周期导致内存膨胀 |
| correctness lock 缩短 | 待评估 | 先保证语义，再拆锁 |
| O_DIRECT mapping cache 重构 | 待评估 | metadata-only，不能和 PageCache 混用 |

## 变更日志

| 日期 | Step | 作者 | 变更 |
|------|------|------|------|
| 2026-05-11 | Step 0 | Codex | 新建 Phase 4 PageCache plan/milestone；完成 ext2/ext4 代码审计锚点与阶段边界记录 |
| 2026-05-11 | Step 1 | Codex | 接入默认关闭的 ext4 per-inode PageCache state 骨架与 gated `Ext4Inode::page_cache()`；`cargo check` 通过 |
| 2026-05-11 | Step 3 | Codex | `ext4_rs::prepare_write_at()` 支持非对齐 range；ext4 buffered write 改为 journaled metadata prepare + dirty PageCache，writeback 不再更新时间戳；`ext4_rs` / `aster-kernel` cargo check 通过 |
| 2026-05-11 | Step 6 | Codex | 建立 `pagecache_phase4` upstream xfstests 验收集，明确 Phase 4 不维护自研 PageCache smoke/coherency 测试集 |
| 2026-05-11 | Step 6 | Codex | `pagecache_phase4` runner 自动传入 `ext4fs.page_cache=1`，守底回归继续默认关闭 PageCache |
| 2026-05-11 | Step 3-4 | Codex | 修复 `generic/130` partial EOF stale data、`generic/418` sparse direct allocation metadata 持久化；补齐 dirty PageCache syncfs/truncate drain 与 minimal fallocate，`generic/247` / `generic/469` 已 PASS，`generic/749` 缩小到 VM SIGBUS 边界问题 |
| 2026-05-12 | Step 4 | Codex | 补齐 shared VMO mmap EOF 越界 `SIGBUS` 语义与同步 fault 不可静默忽略处理；`generic/749` 单例 PASS |
| 2026-05-12 | Step 3-4 | Codex | 禁止 ext4_rs write path 额外预分配未写块并对新分配块先清零；zero/punch 改为复用 `write_at` 分块写零；`generic/091` 单例 PASS |
| 2026-05-12 | Step 3-5 | Codex | 修复 `generic/133` ENOSPC：VFS zero-link cleanup 钩子触发 ext4 unlinked regular-file truncate，释放数据块但仍不复用 inode；`generic/133` 单例 PASS |
| 2026-05-12 | Step 4-5 | Codex | shared VMO 写缺页补 dirty tracking；fallocate/zero/punch 后改为保守 evict；fsync PageCache drain 纳入 inode correctness lock；`generic/263` / `generic/418` 单例 PASS，但 full list 仍在这两项复现 stale data |
| 2026-05-12 | Step 6 | Codex | 最新 `pagecache_phase4` full list：`7 PASS / 2 FAIL / 4 NOTRUN`，剩余 blocker 为 direct/buffered/mmap coherency 非确定性失败（`generic/263`, `generic/418`） |
| 2026-05-12 | Step 4-6 | Codex | PageCache full-list evict 改为写回所有 present non-Uninit pages，并在 PageCache 模式禁用 self-developed direct-read mapping cache；`generic/263` full list PASS，最新 `pagecache_phase4` 为 `8 PASS / 1 FAIL / 4 NOTRUN`，仅剩 `generic/418` |
| 2026-05-12 | Step 5-6 | Codex | 继续排查 `generic/418`：单例 `benchmark/logs/pagecache_phase4_20260512_160633.log` PASS；full list `benchmark/logs/pagecache_phase4_20260512_160858.log` 仍 `8 PASS / 1 FAIL / 4 NOTRUN`。已排除 pending/speculative direct-read cache、write-through dirty retention、inode PageCache state 生命周期单点问题；extent-aware PageCache backend 试验导致 `generic/418` 单例 timeout（`benchmark/logs/pagecache_phase4_20260512_162304.log`），已回退 |
| 2026-05-13 | Step 5-6 | Codex | 修复 stale JBD2 checkpoint metadata home-write 覆盖复用后 regular-file data block：PageCache writeback、buffered write 与 O_DIRECT write 均 revoke 对应 mapped data block 的 checkpoint metadata；同时避免空 metadata transaction TID 阻塞 fsync。clean `generic/263`、clean `generic/247,generic/418` 与 full `pagecache_phase4` 已通过，最新 full list `9 PASS / 0 FAIL / 4 NOTRUN` |
| 2026-05-14 | Step 6 | Codex | 接手守底回归：记录 `phase3_base_guard`、`phase4_good`、`phase6_good`、`jbd_phase1` 通过；标记 Phase 2 concurrency `unlink_while_open` 稳定 FAIL 与 `jbd_phase3_fsync_flush` NOTRUN 退化为当前 blocker |
| 2026-05-14 | Step 6 | Codex | 修复 open-unlinked regular-file cleanup：新增 VFS open/close 通知与 ext4 per-ino open handle 计数，避免 cleanup 提前 truncate 仍 open 的 unlinked inode；Phase 2 concurrency 已恢复 `7/7 PASS` |
| 2026-05-14 | Step 6 | Codex | 修复 `jbd_phase3_fsync_flush` NOTRUN 退化：含 `fiemap` 的 `xfs_io` 命令固定走 shim emulation，避免真实 `xfs_io` 因缺 native FIEMAP ioctl 将 `generic/043-049` 判为 NOTRUN；复跑恢复 `11 PASS / 1 NOTRUN / 0 FAIL` |
| 2026-05-14 | Step 6 | Codex | 修正 `truncate_append` crash 场景初始化，避免 `write` hold 命中 setup `dd`；JBD2 crash matrix 恢复 `18/18 PASS`，Phase 3 host-crash fsync matrix 恢复 `4/4 PASS` |
| 2026-05-14 | Step 6 | Codex | 确认 Phase 4 benchmark A-E 最小闭环：`lmbench_only`、buffered fio cold/warm read、buffered fio write、O_DIRECT fio cache-off 守底；新增官方 fio `direct=0` buffered A/B runner，并接入 `EXT4_PAGE_CACHE` benchmark 开关 |
| 2026-05-14 | Step 6 | Codex | 完成 Phase 4 benchmark A-E 实测：lmbench `8/8 PASS`；PageCache-on warm read 4022.0 MB/s（Linux 7457.0 MB/s，page_cache=0 warm 122.0 MB/s）；buffered write 仍慢（10.8 MB/s）；O_DIRECT cache-off read/write 为 97.24% / 54.02% |
