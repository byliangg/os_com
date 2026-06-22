# Asterinas ext4 PageCache 集成 Phase 4 — 计划

首次更新时间：2026-05-11（Asia/Shanghai）

## 阶段状态

Phase 4 已启动规划。本阶段承接已经收口的 JBD2 Phase 3：`fsync` / `fdatasync` / block flush / shutdown durability 语义已经结束，普通 O_DIRECT write 性能不再阻塞功能阶段退场，后续单独 hardening。

Phase 4 的目标是把 ext4 regular-file 的 buffered I/O 与 mmap 接入 Asterinas 统一 `PageCache` / `Vmo` 系统，并退役或隔离 ext4 当前自研 cache 路径。这里的关键边界是：

- `PageCache` 只服务 buffered I/O、mmap 与 page-cache writeback；
- O_DIRECT 仍然必须绕过 `PageCache`，但要和 `PageCache` 做重叠范围的 flush / discard 一致性协议；
- 当前 `DirectReadCache` 是 O_DIRECT extent mapping / speculative read 优化，不是 Linux PageCache 的等价物；Phase 4 需要把它从默认 correctness / buffered 路径中移走，是否保留为 opt-in 直接 I/O 性能实验另行决定。

## 目标

在 ext4 已有 JBD2 ordered mode 与 Phase 3 fsync 持久化语义之上，完成 Asterinas PageCache 集成：

1. regular-file buffered `read` / `write` 改走 `PageCache.pages().read/write`，不再每次在 `Ext4Inode` 中分配 `Vec` 后直通 `ext4_rs` 同步读写；
2. `Ext4Inode::page_cache()` 返回共享 `Arc<Vmo>`，让 `InodeHandle::mappable()` 能对 ext4 regular-file 暴露文件映射；
3. 新增 ext4-specific `PageCacheBackend`，负责 page idx 到 ext4 logical block / physical block 的映射、sparse hole zero-fill、dirty page writeback 与错误传播；
4. `fsync` / `fdatasync` 先 drain dirty page cache，再执行 Phase 3 已建立的 inode -> TID force commit + device flush 语义；
5. O_DIRECT read / write、truncate、unlink、rename 与 `PageCache` 建立明确失效协议，避免 buffered 与 direct 混用读到旧数据；
6. 关闭或移除默认自研 direct-read cache / speculative direct read 口径；如短期保留，必须是 opt-in 且只作为 O_DIRECT 性能实验，不计作 PageCache 集成成果；
7. 以 correctness-first 方式推进，先保证 phase3/phase4/phase6/JBD/crash/concurrency 不回退，再看 lmbench / buffered workload / mmap 与 O_DIRECT cache-off 性能。

## 当前代码审计锚点

本计划基于 2026-05-11 对当前代码的审计，开始实现前需再次确认：

| 路径 | 当前观察 |
|------|----------|
| `kernel/src/fs/utils/page_cache.rs` | `PageCache` 封装 `Vmo` + `PageCacheManager`；`evict_range()` 写回 dirty page，`discard_range()` 丢弃 cache page；`PageCacheBackend` 需要实现 `read_page_async`、`write_page_async`、`npages` |
| `kernel/src/fs/ext2/inode.rs` | ext2 `InodeInner` 持有 `PageCache`，`PageCache::with_capacity(num_page_bytes, Arc::downgrade(&block_manager))` 是主要参考；buffered read/write 走 `page_cache.pages()` |
| `kernel/src/fs/ext2/impl_for_vfs/inode.rs` | ext2 `Inode::page_cache()` 返回 `Some(self.page_cache())`；`sync_all` / `sync_data` 先 inode sync，再 `block_device().sync()` |
| `kernel/src/fs/ext2/inode.rs` | ext2 O_DIRECT read/write 会对重叠 page cache range 做 discard；ext4 不能简单照抄 direct read 的 discard，需要先处理 dirty page coherency |
| `kernel/src/fs/inode_handle.rs` | `mappable()` 已经通过 `inode.page_cache()` 暴露 `Vmo`；ext4 实现 `page_cache()` 后 mmap 入口可复用 VFS 现有逻辑 |
| `kernel/src/fs/ext4/inode.rs` | ext4 `read_at` / `write_at` 当前非 O_DIRECT 路径分配 `Vec`，调用 `Ext4Fs::read_at` / `write_at`，未使用 VFS PageCache |
| `kernel/src/fs/ext4/inode.rs` | ext4 目前没有覆盖 `Inode::page_cache()`，regular-file mmap 无共享 PageCache VMO |
| `kernel/src/fs/ext4/fs.rs` | `Ext4Fs::make_inode()` 每次创建新的 `Ext4Inode` wrapper；Phase 4 需要按 inode number 共享同一个 PageCache state，而不是每个 path wrapper 一个 cache |
| `kernel/src/fs/ext4/fs.rs` | `inode_direct_read_cache: Mutex<BTreeMap<u32, DirectReadCache>>`、pending speculative direct read 与 mapping cache 只服务 O_DIRECT read；默认口径已经可通过 `ext4fs.direct_read_cache=0` 关闭 |
| `kernel/src/fs/ext4/fs.rs` | `fsync_regular_file()` 已按 Phase 3 建立 inode -> TID force commit；接入 PageCache 后必须先 writeback dirty pages，再 force commit metadata |
| `kernel/libs/ext4_rs/src/ext4_impls/file.rs` | 当前有 `ext4_read_at` / `ext4_write_at` / `ext4_prepare_write_at` / `ext4_plan_direct_read` 等能力；PageCacheBackend 应优先复用结构化 mapping API，而不是解析日志或手写偏移字符串 |
| `benchmark/benchmark.md` | 2026-05-09 起 6-test 默认 `EXT4_DIRECT_READ_CACHE=0`，cache-on 结果只作历史对照；Phase 4 需要延续 cache-off 默认口径 |

## 设计原则

1. **PageCache 只接 buffered / mmap**：不能为了 O_DIRECT 性能把 PageCache 当作直接 I/O 数据缓存；O_DIRECT 的定义是绕过页缓存。
2. **共享 per-inode cache state**：`Ext4Inode` 是可重复创建的 VFS wrapper，PageCache 必须挂在 `Ext4Fs` 的 per-inode state 上，所有 path / fd / mmap 共享同一份 `Vmo`。
3. **先分配元数据，再 dirty page**：buffered write 涉及扩展文件、分配 extent、更新时间或 inode size 时，应在 JBD2 handle 中完成 metadata 准备和 TID 记录，再把用户数据写入 PageCache。
4. **ordered mode 不打折**：`fsync` / `fdatasync` 必须先 `evict_range(0..file_size)` 写回 dirty data block，再 force commit 对应 metadata TID，最后执行 device flush。
5. **混合 direct/buffered 要有强一致性**：O_DIRECT read 前要写回重叠 dirty PageCache；O_DIRECT write 成功后要丢弃或更新重叠 PageCache；truncate / unlink / rename 需要 resize、zero-fill 或 invalidate 对应 cache。
6. **sparse 文件读零**：read page 时未映射 hole 必须填零，不能把未初始化 frame 暴露给用户或 mmap。
7. **EOF tail 明确清零**：partial-page write、truncate shrink / extend、hole punch 后要保证 EOF 后半页不会把旧数据写回磁盘。
8. **不盲目复制 ext2**：ext2 是结构参考，但 ext4 有 JBD2、extent、sparse、shutdown 和 Phase 3 fsync 语义，PageCache 协议必须按 ext4 需求补齐。
9. **feature flag 分阶段打开**：实现初期建议加 `ext4fs.page_cache=0/1` 或等价开关，默认可先关闭；通过核心回归后再切默认。
10. **先 correctness，后性能**：lmbench / buffered I/O 提升是目标之一，但不能牺牲 crash recovery、fsync durability 或 direct/buffered coherency。

## 验收标准

### 固定回归不能回退

| 测试项 | 最低要求 |
|--------|----------|
| `phase3_base_guard` | 不回退，严格关键词扫描为空 |
| `phase4_good` | 不回退，严格关键词扫描为空 |
| `phase6_good` | `25/25 PASS`，严格关键词扫描为空 |
| `jbd_phase1` | 有效样本通过率 100% |
| JBD2 crash matrix | 默认场景 100% PASS |
| Phase 2 concurrency baseline | `7/7 PASS`，严格关键词扫描为空 |
| `jbd_phase3_fsync_flush` | 不回退；默认 2G scratch 口径仍保持 0 FAIL |
| Phase 3 host-crash fsync matrix | 4/4 PASS |

### 新增 Phase 4 验收

Phase 4 不再新增自研 PageCache smoke/coherency 测试集。新增 correctness 验收统一使用 upstream xfstests 子集：

- 入口：`XFSTESTS_MODE=pagecache_phase4`，或 Docker 入口 `PHASE4_DOCKER_MODE=pagecache_phase4 tools/ext4/run_phase4_in_docker.sh`；该模式必须显式传入 `ext4fs.page_cache=1`，守底回归默认保持 PageCache 关闭；
- 用例清单：`test/initramfs/src/syscall/xfstests/testcases/pagecache_phase4.list`；
- 静态排除：`test/initramfs/src/syscall/xfstests/blocked/pagecache_phase4_excluded.tsv`，默认空文件，表示 list 内所有 case 都属于验收范围；
- 通过标准：runner 成功、`FAIL=0`、严格关键词扫描为空；`NOTRUN` 只能作为工具/内核能力缺口记录，不得用作静态排除规避。

| xfstests ID | Phase 4 覆盖点 |
|-------------|----------------|
| `generic/091` | fsx O_DIRECT 小块与并发 buffered I/O |
| `generic/130` | buffered/direct coherency、hole、direct EOF zeroing |
| `generic/133` | 同一文件并发 buffered/direct 读写 |
| `generic/208` | AIO DIO read-cache invalidation race |
| `generic/209` | sync DIO 对 readahead/page cache 的 invalidation |
| `generic/247` | direct I/O 与 mmap writer race |
| `generic/263` | fsx direct I/O 与 sub-block buffered I/O 混合 |
| `generic/366` | direct read/write 与 buffered write 混合 hang 回归 |
| `generic/412` | direct I/O + buffered write + truncate into hole 持久化 |
| `generic/418` | buffered/direct 混用的显式 pagecache invalidation |
| `generic/469` | truncate-down 后 page cache EOF 之后清零 |
| `generic/749` | mmap EOF partial-page zero-fill 与 SIGBUS 边界 |
| `generic/751` | page-cache truncation + writeback 压力 |

实现层验收仍需满足：共享 `Arc<Vmo>`、buffered read/write 走 PageCache、dirty writeback 先于 JBD2 metadata commit、`fsync/fdatasync` drain dirty PageCache、O_DIRECT 与 truncate 的 PageCache coherency、默认 benchmark 不依赖自研 direct-read cache。上述语义以 `pagecache_phase4` 上游用例作为退场证据。

## Step 0：代码审计与基线固化

**状态：** 进行中
**目标：** 在改代码前，把 ext2 PageCache 模式、ext4 自研 cache 路径、buffered/direct 分流和 benchmark 口径固定下来。

### 方案

1. 记录 ext2 参考实现：
   - `InodeInner { inode_impl, page_cache }`；
   - `PageCacheBackend for InodeBlockManager`；
   - `read_at/write_at/sync_data/direct I/O discard` 的调用顺序；
   - `impl_for_vfs/inode.rs` 中 `page_cache()` / `sync_all()` / `sync_data()`。
2. 记录 ext4 当前路径：
   - non-O_DIRECT `Ext4Inode::read_at/write_at` 的 `Vec` 拷贝；
   - `Ext4Fs::read_at/write_at` 的 ext4_rs 直通；
   - `DirectReadCache` / pending speculative direct read 的 O_DIRECT-only 作用域；
   - `make_inode()` 重建 wrapper 导致 PageCache 必须下沉到 per-inode state。
3. 固化 baseline：
   - 6-test cache-off 综合 fio；
   - 普通 O_DIRECT read/write；
   - lmbench regression 与 VFS/page-cache 类 microbenchmark；
   - `pagecache_phase4` upstream xfstests 验收集（当前预期 ext4 未接入时部分失败或 NOTRUN）；
   - phase3 fsync/flush 与 host-crash matrix。
4. 明确 feature flag 策略与默认值。

### 验收

- milestone 写入代码审计结论；
- baseline 命令、日志路径与 cache 开关写清；
- 明确 `pagecache_phase4` 是 Phase 4 新增 upstream xfstests 验收集，Phase 3/2 项只作为守底回归；
- 未修改内核实现。

## Step 1：建立 ext4 per-inode PageCache state

**状态：** 部分完成（2026-05-11：默认关闭的骨架已接入）
**目标：** 给 ext4 regular-file 建立共享 PageCache 容器，但暂不切换主要 I/O 行为。

### 方案

1. 在 `Ext4Fs` 中新增 per-inode PageCache state map，例如 `inode_page_caches` / `inode_cache_states`：
   - key 为 ino；
   - value 持有 `PageCache`、`PageCacheBackend`、ino、弱引用 fs、必要的 size/generation 状态；
   - 多个 `Ext4Inode` wrapper 通过 ino 共享同一个 value。
2. 新增 ext4 PageCacheBackend：
   - `npages()` 返回当前 inode size 对应的页数，而不是仅返回已分配 extent 数；
   - 确认 `PAGE_SIZE == ext4_rs::BLOCK_SIZE` 的当前假设；若未来不等，则 backend 要支持一页多块或一块多页。
3. 在 `Ext4Inode` 中实现 regular-file `page_cache()`：
   - 非 regular-file 返回 `None`；
   - regular-file 返回共享 `Arc<Vmo>`；
   - 初期可用 `ext4fs.page_cache=1` 控制是否暴露。
4. 验收不新增自研 smoke；共享 VMO 与 mmap 入口行为通过代码审计和 `pagecache_phase4` upstream xfstests 验证。

### 当前落地

- 已新增 `ext4fs.page_cache` feature flag，默认 `false`；
- 已在 `Ext4Fs` 中新增 per-inode PageCache state map；
- 已新增同步型 `Ext4PageCacheBackend` 骨架，当前复用 ext4_rs 直通读写能力，后续 Step 2/3 再替换为结构化 mapping + ordered writeback；
- 已在 `Ext4Inode::page_cache()` 中按 regular-file + feature flag 暴露共享 `Vmo`；
- 默认关闭时 non-O_DIRECT read/write 行为保持 Phase 3 状态。

### 验收

- ext4 regular-file 有共享 `PageCache` / `Vmo`；
- 多 wrapper 不会各自创建独立 cache；
- 代码可编译，默认关闭时行为与 Phase 3 相同；
- 文档记录 feature flag 默认值与风险。

## Step 2：接入 buffered read 与 read-page backend

**状态：** 部分完成（2026-05-11：gated buffered read 已走 PageCache）
**目标：** 让 ext4 buffered read 走 PageCache，并保证 sparse / EOF / atime 语义。

### 方案

1. 实现 `Ext4PageCacheBackend::read_page_async(idx, frame)`：
   - 通过 `idx * PAGE_SIZE` 计算 file offset；
   - 使用 `ext4_plan_direct_read` 或新增结构化 helper 获取逻辑块到物理块映射；
   - 对 hole / EOF 之外区域 zero-fill；
   - 对连续物理块尽量合并 bio；
   - 错误统一映射到 `EIO` / ext4 现有错误。
2. `Ext4Inode::read_at` 非 O_DIRECT 路径改为：
   - 根据 inode size 裁剪 read_len；
   - 从 `page_cache.pages().read(offset, writer)` 读取；
   - 保留 atime 降频更新策略或接入现有 atime cache。
3. mmap read 与 EOF 行为由 `generic/247`、`generic/749`、`generic/751` 覆盖。

### 当前落地

- 在 `ext4fs.page_cache=1` 时，non-O_DIRECT read 已改走 `Ext4Fs::read_at_page_cache()`；
- `read_at_page_cache()` 按当前 inode size 裁剪 `read_len`，用 `VmWriter::limit()` 避免越过 EOF 写用户 buffer；
- read 后保留现有 atime 策略；
- PageCache backend read 目前仍同步复用 ext4_rs `ext4_read_at()`，hole / EOF 由零初始化 buffer + read_len 裁剪兜底；后续再替换为 extent mapping + bio 合并。

### 验收

- buffered read 不再调用 `Ext4Fs::read_at()` 的数据直通路径；
- normal read、跨页 read、EOF read、hole read 均正确；
- phase3/phase4/phase6 smoke 不回退；
- 记录读路径性能与 PageCache 命中/未命中观测。

## Step 3：接入 buffered write、dirty page writeback 与 JBD2 metadata 准备

**状态：** 部分完成（2026-05-11：dirty-cache write 基础路径已接入，writeback hardening 待完成）
**目标：** 让 buffered write 进入 PageCache，同时保证 block allocation、inode size、mtime/ctime 与 JBD2 TID 追踪正确。

### 方案

1. 设计 buffered write 的 metadata 准备路径：
   - 覆盖写：确认目标 logical block 已映射；必要时仍走 `ext4_prepare_write_at` 统一处理 hole；
   - 扩展写：在 JBD2 handle 内分配 extent、更新 inode size 与时间戳；
   - ENOSPC 时不得把无法持久化的 bytes 留成 dirty PageCache 假成功。
2. `Ext4Inode::write_at` 非 O_DIRECT 路径改为：
   - 先在 `Ext4Fs` 中 journaled prepare；
   - resize PageCache 到新 size；
   - 将用户数据写入 `page_cache.pages().write(offset, reader)`；
   - 记录写入长度与 inode TID。
3. 实现 `Ext4PageCacheBackend::write_page_async(idx, frame)`：
   - 复用 prepare 阶段得到的映射或重新查询结构化 mapping；
   - 将 dirty page 写到 home data blocks；
   - partial EOF page 写回前保证 tail zero；
   - 写失败时返回错误，不把 page 标记为 clean。
4. mmap writer 与 dirty writeback 行为由 `generic/247`、`generic/749`、`generic/751` 覆盖；`fsync` / crash 持久化继续由 Phase 3 守底回归覆盖。

### 当前落地

- 在 `ext4fs.page_cache=1` 时，non-O_DIRECT write 走 `Ext4Fs::write_at_page_cache()`；
- `ext4_rs::prepare_write_at()` 已支持任意 offset/len，能够为普通 buffered write 做非对齐 metadata prepare；
- `write_at_page_cache()` 先在 journaled handle 中完成 block allocation / inode size / mtime / ctime，再把用户数据写入共享 VMO 并标记 dirty；
- `Ext4PageCacheBackend::write_page_async()` 通过内部 `write_page_cache_data_at()` 写回 home data blocks，避免递归进入 VFS buffered write；writeback 不再更新 mtime/ctime；
- direct write 与 truncate 成功后也会 discard 已存在的 PageCache state；
- 当前 writeback 仍复用 ext4_rs `ext4_write_at()`，data-only mapping + bio 合并留作后续 hardening。

### 验收

- buffered write 不再调用 `Ext4Fs::write_at()` 的数据直写路径；
- 覆盖写、append、跨页写、partial page、truncate 后写均正确；
- dirty PageCache writeback 可观测，`fsync` 后 dirty pages 清空；
- JBD2 crash matrix 与 Phase 3 host-crash fsync matrix 不回退。

## Step 4：fsync / truncate / direct I/O coherency 收口

**状态：** 待执行
**目标：** 把 PageCache 与 Phase 3 fsync 语义、truncate、O_DIRECT 混用规则收口。

### 方案

1. `sync_all()` / `sync_data()`：
   - regular-file 先 `page_cache.evict_range(0..file_size)`；
   - 再调用 `fsync_regular_file(ino)`；
   - 最后保留 `block_device().sync()`；
   - `fdatasync` 当前继续保守等价，后续若区分 metadata 可单独优化。
2. O_DIRECT read：
   - 对重叠 dirty PageCache 先 writeback / evict，再直接读盘；
   - 不能简单 discard dirty page 后读盘，否则会丢 buffered write。
3. O_DIRECT write：
   - 成功写盘后 discard 重叠 PageCache；
   - 若 block/page 大小不一致或存在 partial overlap，先 writeback 非覆盖脏部分，再 discard 覆盖范围。
4. truncate / resize：
   - shrink：zero EOF tail，resize PageCache，释放 blocks；
   - extend：resize PageCache，hole 读零；
   - 与 JBD2 truncate TID 追踪保持一致。
5. unlink / rename / shutdown：
   - unlink-open 文件 PageCache 生命周期跟随 open inode state；
   - rename 不应误删同 ino PageCache；
   - shutdown 后 dirty PageCache 行为与 Phase 3 shutdown 语义一致，后续 write/fsync 返回 EIO。

### 当前落地

- `sync_all()` / `sync_data()` 已在 regular-file fsync 前调用 `sync_page_cache_for_inode()`，仅对已存在的 PageCache state 做 dirty page writeback，不会为未打开 PageCache 的 inode 创建新 state；
- O_DIRECT read 入口会先 `evict_page_cache_range()`，避免 dirty PageCache 被绕过；
- O_DIRECT write 成功后 discard 重叠 PageCache；
- truncate 成功后 discard 已存在 state 并 resize 到新 size；
- 当前普通 buffered write 已进入 dirty PageCache，`fsync` / `fdatasync` drain 会触发 backend writeback；writeback 的 bio 合并与 data-only mapping helper 仍留作 hardening。

### 验收

- `pagecache_phase4` upstream xfstests 中 mixed buffered/direct/truncate/mmap 用例 `FAIL=0`；
- truncate + read/write/fsync 组合不读旧数据；
- fdatasync/fsync durability 不回退；
- 关键词扫描无 `logical block not mapped`、`mapped block out of range` 等历史错误。

## Step 5：退役或隔离自研 direct-read cache

**状态：** 待执行
**目标：** 将 Phase 1/2 时代的自研 direct-read cache 从 Phase 4 默认语义与 benchmark 中移出。

### 方案

1. 明确默认：
   - `EXT4_DIRECT_READ_CACHE=0` / `ext4fs.direct_read_cache=0` 作为 Phase 4 默认综合测试口径；
   - README、benchmark、milestone 不再引用 cache-on 读高值作为当前能力。
2. 代码清理候选：
   - 删除或默认禁用 `DirectReadCache`；
   - 删除 pending speculative direct read；
   - 保留最小 direct read mapping plan 路径；
   - 若性能需要保留 opt-in extent mapping cache，必须命名为 O_DIRECT mapping cache，并与 PageCache 清晰隔离。
3. 验证：
   - cache-off read/write 结果可复现；
   - direct/buffered coherency 测试通过；
   - 没有 cache-on 才能通过的 correctness case。

### 验收

- 默认路径不依赖自研 direct-read cache；
- 自研 cache 如保留，仅作为 opt-in 性能实验；
- 文档中 PageCache 与 O_DIRECT mapping cache 的边界清楚。

## Step 6：全量回归与 benchmark

**状态：** 待执行
**目标：** 用 Phase 2/3 守底回归 + Phase 4 新增测试证明 PageCache 集成可退场。

### 方案

1. 功能回归：
   - `phase3_base_guard`
   - `phase4_good`
   - `phase6_good`
   - `jbd_phase1`
   - JBD2 crash matrix
   - Phase 2 concurrency baseline
   - `jbd_phase3_fsync_flush`
   - Phase 3 host-crash fsync matrix
2. Phase 4 新增测试：
   - 只使用 upstream xfstests `pagecache_phase4` list；
   - 不维护自研 PageCache smoke/coherency 测试集；
   - 覆盖 buffered/direct coherency、mmap、truncate、EOF zero-fill、dirty writeback 与 page cache invalidation。
3. benchmark：
   - A. `lmbench_only`：VFS/open/stat/read/write/create/delete/copy 回归，分别记录默认 `page_cache=0` 与 PageCache-on 对照（如需可先跑默认守底）；
   - B. buffered fio cold read：官方 fio 工具、`direct=0`，先用 direct write 准备文件，再读第一遍；
   - C. buffered fio warm read：同一挂载内对同一文件读第二遍，观察 PageCache 命中收益；
   - D. buffered fio write：官方 fio 工具、`direct=0`，观察 dirty PageCache/writeback 路径；
   - E. 原 O_DIRECT fio cache-off 守底：`run_ext4_summary.sh` 与/或 `run_6test_summary.sh`，继续使用 `direct=1` + `EXT4_DIRECT_READ_CACHE=0`，单独记录且不与 PageCache 收益混算。
   - PageCache A/B 默认使用同一套 buffered fio workload，对比 `ext4fs.page_cache=0` 与 `ext4fs.page_cache=1`。
4. 更新：
   - milestone 结果表；
   - README 与 benchmark 快照；
   - technical report / core results（如需要对外提交）。

### 验收

- 守底回归 0 FAIL；
- `pagecache_phase4` upstream xfstests 验收集 `FAIL=0`；
- benchmark 数字和日志路径完整记录；
- Phase 4 plan / milestone 标记收口或明确遗留项。

## Step 7：PageCache 性能 hardening（可选）

**状态：** 待执行
**目标：** correctness 收口后，再做 PageCache 专项性能优化。

候选方向：

1. read-page readahead window 与 ext4 extent 连续性结合；
2. dirty page writeback 批量合并 bio；
3. mmap sequential fault 预读；
4. per-inode PageCache state 生命周期与内存压力回收；
5. 减少 `Ext4Fs` 全局 / per-inode correctness lock 的持有时间；
6. 如果 O_DIRECT read 性能需要恢复，重新设计 metadata-only extent plan cache，而不是复活数据 cache。

本 step 不作为 Phase 4 correctness 退场前置，除非前面步骤引入明显性能退化或内存不可接受。
