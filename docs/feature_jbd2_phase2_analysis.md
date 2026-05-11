# Asterinas ext4 JBD2 功能实现 Phase 2 — 并发正确性问题分析

首次更新时间：2026-04-24（Asia/Shanghai）

## 目标与赛题映射

Phase 1 已经完成 JBD2 事务管理、日志刷盘、checkpoint、全量 recovery 与多场景 crash 验证。按照 `赛题要求.md` 的优秀档标准，Phase 2 的核心剩余目标是：

- 支持多文件并发基本读写，且无数据错乱、无数据丢失；
- 在更大范围 xfstests 核心功能验证中达到通过率 >= 95%；
- 保持多场景崩溃恢复数据一致性 100%；
- 在 correctness 稳定后，再恢复或提升并发吞吐，守住 Linux ext4 对照 90% 级别性能目标。

本阶段原则：**先证明并发 correctness，再拆锁做性能**。当前 Phase 1 的全局串行化保证了稳定性，但也掩盖了多文件并发读写的真实竞争路径。

## Phase 1 当前基线

| 项目 | Phase 1 收口结果 |
|------|------------------|
| `phase3_base_guard` | `10 PASS / 0 FAIL / 6 NOTRUN / 24 STATIC_BLOCKED` |
| `phase4_good` | `12 PASS / 0 FAIL / 6 NOTRUN / 22 STATIC_BLOCKED` |
| `phase6_good` | `25 PASS / 0 FAIL / 0 NOTRUN / 26 STATIC_BLOCKED` |
| `jbd_phase1` | `6 PASS / 0 FAIL / 6 NOTRUN`，有效样本通过率 100% |
| JBD2 crash matrix | `9/9 PASS` |
| dirty journal recovery | `jbd2_probe write-probe-tx -> recover -> e2fsck -fn` PASS |
| fio O_DIRECT | read `93.49%`，write `87.01%`（JBD2 Phase 1 收口后守底） |

Phase 2 的所有实现都必须以“不回退上述基线”为前提。

## 当前串行化与共享状态盘点

### 1. 全局 ext4_rs runtime 锁

代码位置：`asterinas/kernel/src/fs/ext4/fs.rs`

- `EXT4_RS_RUNTIME_LOCK: Mutex<()>` 目前包住 `run_ext4()`、`run_ext4_noerr()`、`run_journaled_ext4()` 等高层 ext4_rs 调用。
- 直接原因是 ext4_rs 内部使用全局 `runtime_block_size()` / `set_runtime_block_size()`，例如：
  - `kernel/libs/ext4_rs/src/ext4_defs/consts.rs`
  - `kernel/libs/ext4_rs/src/ext4_defs/block.rs`
  - `kernel/libs/ext4_rs/src/ext4_defs/direntry.rs`
  - `kernel/libs/ext4_rs/src/ext4_defs/extents.rs`
  - `kernel/libs/ext4_rs/src/ext4_impls/ext4.rs`
- 该锁当前还顺带串行化了 allocator、extent tree、目录变更、JBD2 handle 生命周期等大量状态，因此一旦缩小或删除，会同时暴露其他隐藏竞争。

风险结论：`runtime_block_size` 必须改为 per-filesystem/per-operation 显式上下文，否则多挂载点、mkfs/remount、并发 xfstests 都可能读到错误块大小。删除 `EXT4_RS_RUNTIME_LOCK` 前，必须先补齐其他共享状态的真实同步。

### 2. `Ext4Fs::inner: Mutex<Ext4>`

代码位置：`asterinas/kernel/src/fs/ext4/fs.rs`

- 当前每个挂载点有一个 `inner: Mutex<Ext4>`，保护 ext4_rs 的 superblock、block group descriptor、system zone、inode/extent/dir 操作等内部状态。
- 因为外层还有 `EXT4_RS_RUNTIME_LOCK`，Phase 1 实际是“全系统 ext4 串行 + 单 fs 串行”。
- 如果 Phase 2 先删除全局锁、保留 `inner`，可以得到“不同 ext4 挂载点并发，同一 fs 串行”的中间态。
- 真正支持多文件并发写时，单一 `inner` 会成为主要瓶颈，需要进一步拆成 per-inode、per-directory、per-block-group/bitmap 与 journal runtime 的组合锁。

风险结论：`inner` 可以作为 Phase 2 早期 correctness 保护，但不能作为最终并发模型。

### 3. JBD2 runtime / journal / checkpoint 状态

代码位置：

- `kernel/src/fs/ext4/fs.rs`
- `kernel/libs/ext4_rs/src/ext4_impls/jbd2/journal.rs`

共享状态：

- `jbd2_runtime: Arc<Mutex<Option<JournalRuntime>>>`
- `jbd2_journal: Mutex<Option<Jbd2Journal>>`
- `jbd2_checkpoint_lock: Mutex<()>`
- `JournalRuntime::{running, prev_running, committing, checkpoint_list, active_handles}`

当前关键风险：

- `JournalRuntime::active_handles` 是 `VecDeque<JournalHandle>`，`record_metadata_write()` 和 `mark_active_handle_requires_data_sync()` 默认使用 `front_mut()`。
- `JournalHandle` 当前只有 `transaction_id`，没有唯一 handle id；`remove_active_handle()` 按 `transaction_id` 查找 active handle。
- Phase 1 因为全局串行，一个时刻只有一个真实活跃高层操作，FIFO front 基本等价于当前 handle。
- Phase 2 一旦允许多个 handle 重叠，metadata write 可能被记到错误 transaction 或错误 handle 上，形成提交顺序、data-before-metadata、recovery 可见性错误。
- 多个并发 handle 通常会复用同一个 running transaction。若 `stop_handle(B)` 只按 transaction id 匹配，可能摘掉队首的 `A`，导致 `unregister_handle()` 使用错误的 reserved blocks 和 modified blocks 统计。
- `overlay_metadata_read()` 会按 `running -> prev_running -> committing -> checkpoint_list` 查找最新 metadata 镜像；并发下需要明确读者看到哪个 transaction 的视图，以及是否允许读取尚未提交的其他操作 metadata。
- commit/checkpoint 会持有 runtime、journal、checkpoint 相关锁并执行块设备 I/O，后续拆锁时必须制定固定锁顺序，避免 `runtime -> journal -> device` 与 `inner -> runtime` 交叉死锁。
- JBD2 ordered mode 的 data-before-metadata 约束是 transaction 级别：一个 transaction 内只要有任一 handle 标记 data sync，commit 前就必须确保该 transaction 内所有相关 dirty data block 都已落盘，不能只等待触发 fsync 的单个文件。
- 同一 running transaction 内的多个 handle 共享 metadata 视图，`overlay_metadata_read()` 不按 handle id 过滤；跨 transaction 的可见性按 `running -> prev_running -> committing -> checkpoint_list` 顺序读取最新已知 metadata，不能跨过未完成 commit 看到 future transaction。
- `JournalTransaction::register_handle()` 当前会累加 `reserved_blocks`，但没有明确 transaction credit 上限和 admission control。并发 handle 共享 running transaction 后，需要审计 reserved credit 是否超过 journal/transaction 容量，并在超限时 rotate transaction 或让新 handle 等待。

风险结论：在允许真正并发 handle 之前，必须为 `JournalHandle` 引入唯一 id / generation，并把“当前 handle”从全局 FIFO 改成显式 handle token / operation context。`remove_active_handle()`、data sync 标记和 metadata write 都必须按 handle id 匹配，而不是按 transaction id 或队首推断。

### 4. operation allocated block guard

代码位置：`kernel/libs/ext4_rs/src/ext4_impls/alloc_guard.rs`

当前实现：

- `static OP_ALLOCATED_BLOCKS: Mutex<BTreeSet<Ext4Fsblk>>`
- `start_jbd2_handle()` / `finish_jbd2_handle()` 前后调用 `clear_operation_allocated_blocks()`
- allocator 通过 `reserve_operation_allocated_block(s)` 与 `is_operation_allocated_block()` 追踪本操作刚分配的块。

风险：

- 这是全局状态，不区分 filesystem、inode、transaction、handle 或 task。
- Phase 1 串行下它近似等价于 operation-local set。
- Phase 2 并发下，A 操作 clear 可能清掉 B 操作的记录，A/B 分配集合也可能互相污染，进一步造成重复分配检测失效或误判。

风险结论：必须改为 handle-local 或 operation context-local 状态，不能保留全局 `OP_ALLOCATED_BLOCKS` 语义。

### 5. allocator、bitmap、block group 与 superblock 计数

代码位置：

- `kernel/libs/ext4_rs/src/ext4_impls/balloc.rs`
- `kernel/libs/ext4_rs/src/ext4_impls/ialloc.rs`
- `kernel/libs/ext4_rs/src/ext4_defs/block_group.rs`
- `kernel/libs/ext4_rs/src/ext4_defs/super_block.rs`

风险：

- block bitmap read-modify-write、inode bitmap read-modify-write、block group free counters、superblock free counters必须保持原子一致。
- Phase 1 曾经在 `generic/013` 中暴露过 metadata block 双重分配、extent leaf 被错误复用、bitmap 可见性等问题；当前依赖 JBD2 overlay 与全局串行收口。
- 并发写不同文件时，多个 allocator 同时扫描同一 block group，若没有 per-block-group 锁或原子分配协议，可能分配同一 pblock。
- 单次操作可能触碰多个 block group，例如 inode 所在 group、data block 目标 group、fallback 分配 group；若不同任务以不同顺序获取 per-bg 锁，会出现 G3 等 G7、G7 等 G3 的经典死锁。
- allocator 修改 metadata 时必须与 JBD2 handle 绑定，否则 crash recovery 后可能出现 bitmap、inode extent tree、superblock counter 三者不一致。

风险结论：并发分配前必须先定义 block group 级锁或 allocator transaction 锁，并把 bitmap/counter 修改纳入同一 handle。多 block group 操作必须按 group number 升序取锁，不能在持锁后反向补锁。

### 6. inode extent tree、size 与 truncate/write 竞争

代码位置：

- `kernel/src/fs/ext4/inode.rs`
- `kernel/src/fs/ext4/fs.rs`
- `kernel/libs/ext4_rs/src/ext4_impls/extents.rs`
- `kernel/libs/ext4_rs/src/ext4_impls/file.rs`
- `kernel/libs/ext4_rs/src/ext4_impls/inode.rs`

风险：

- 同一 inode 的 write、append、truncate、fallocate/extent 修改不能并发无序执行。
- write 扩展 extent tree 与 truncate shrink/free blocks 之间必须互斥，否则读路径可能拿到已经释放的 pblock，或者 truncate 后被旧 write 恢复 size/extent。
- 多文件并发目标不要求同一文件无限并发写，但必须保证同 inode 读写/truncate 的 POSIX 可接受语义，不能数据错乱。
- direct read cache 依赖 extent mapping，写/truncate 后必须可靠失效。

风险结论：应引入 per-inode 写侧序列化，先支持不同 inode 并发；同 inode 并发可保守串行。

### 7. 目录项缓存与目录变更

代码位置：`kernel/src/fs/ext4/fs.rs`

共享状态：

- `dir_entry_cache: Mutex<BTreeMap<u32, DirEntryCache>>`

风险：

- create/unlink/rename/rmdir 会修改 parent directory block 与 dentry cache。
- rename 可能同时涉及 old parent、new parent、源 inode、目标 inode；锁顺序不固定会死锁。
- cache 只保护内存 map，不自动保证 disk directory entry、inode link count、JBD2 transaction 三者一致。

风险结论：目录操作需要固定锁顺序，建议以 inode number 排序获取 parent dir locks；cache 更新必须在 journaled mutation 成功后提交，失败路径要回滚或失效。

### 8. direct read cache 与 pending prefetch

代码位置：`kernel/src/fs/ext4/fs.rs`

共享状态：

- `inode_direct_read_cache: Mutex<BTreeMap<u32, DirectReadCache>>`
- `PendingDirectRead` 中包含 `BioWaiter`

风险：

- read cache 存储 logical-to-physical mapping 和 pending bio。
- 并发 write/truncate 可能改变 mapping，但 pending read 仍可能复制旧 pblock 数据。
- 当前 cache lock 只保护 map 本身，不表达“mapping generation”或“正在被写侧失效”。

风险结论：Phase 2 correctness 阶段可先在任何写/truncate/rename影响数据可见性时粗粒度失效对应 inode cache；性能阶段再补 generation check。

### 9. PageCache / buffered I/O 协议

代码位置：

- `kernel/src/fs/ext4/inode.rs`
- `kernel/src/fs/ext2/`（PageCache 集成参考）
- `kernel/src/fs/utils/page_cache.rs`

现状与风险：

- ext4 的 `InodeIo::read_at()` / `write_at()` 区分 O_DIRECT 与 buffered 路径；当前 ext4 buffered 路径直接调用 `fs.read_at()` / `fs.write_at()`，不像 ext2 那样暴露完整 `page_cache()`。
- lmbench 与普通 buffered I/O 仍会走非 O_DIRECT 路径；Phase 2 选择不引入 ext4 PageCache，把 PageCache 接入留作 Phase 3 / 性能深化项。
- Phase 2 需要审计的是当前 buffered 直通 `fs.read_at()` / `fs.write_at()` 的并发正确性：写/truncate/unlink/rename 与 direct I/O cache 之间不能产生旧 mapping 或旧数据。
- 如果未来把 ext2 风格 PageCache 接入 ext4，必须重新定义 PageCache 与 direct bio、truncate、unlink、rename、fsync、JBD2 ordered mode 的顺序关系。

风险结论：Phase 2 不新增 ext4 PageCache；测试和文档必须显式说明 buffered 路径仍直通 fs 层，其并发语义由 fs 层锁、JBD2 ordered mode 与 direct read cache 失效共同保证。

### 10. timestamp cache

代码位置：`kernel/src/fs/ext4/fs.rs`

共享状态：

- `inode_atime_cache`
- `inode_ctime_cache`
- `inode_mtime_ctime_cache`

风险：

- 这些 cache 通过“同一秒内少写 metadata”减少日志压力。
- 并发下，如果 cache 更新先于 journaled inode metadata 成功提交，失败路径可能导致后续合法时间戳更新被跳过。
- 与 fsync、crash durability 的可见性也需要明确。

风险结论：正确性阶段应保守处理：metadata 变更失败时失效 cache，关键 mutation 不依赖 timestamp cache 判定完成。

### 11. fsync、group commit 与 orphan inode

代码位置：

- `kernel/src/fs/ext4/inode.rs`
- `kernel/src/fs/ext4/fs.rs`
- `kernel/libs/ext4_rs/src/ext4_impls/ext4.rs`

风险：

- `fsync_regular_file()` 触发 JBD2 commit/checkpoint 时可能采用 group commit 语义；这本身是合理的，但测试不能假设 fsync 只提交当前 inode 的 metadata。
- 正确验收应关注“调用 fsync 前该文件已完成的写入必须 durable”，而不是“其他文件修改不能一起变 durable”。
- 当前 unlink 逻辑对 nlink 降到 0 的文件/目录采取“不立即释放 inode bitmap”的保守策略，注释中明确提到尚无 close-time orphan cleanup。
- 并发 unlink-while-open 是 xfstests 常考路径；如果没有 `s_last_orphan` / orphan list 或等价 cleanup，必须防止 inode 复用腐败，并记录可能的空间泄漏边界。

风险结论：Step 5 需要把 fsync group commit 预期、unlink-while-open、orphan cleanup/保守不复用策略写入测试与验收。

## Phase 2 主要风险分组

| 编号 | 风险 | 影响 | 优先级 |
|------|------|------|--------|
| G1 | `runtime_block_size` 是全局变量 | 多挂载点/并发 ext4 调用可能解析错误 block size | P0 |
| G2 | `active_handles.front_mut()` 代表当前 handle | 并发 handle 下 metadata 记账串 transaction | P0 |
| G3 | `OP_ALLOCATED_BLOCKS` 是全局集合 | 并发 allocator 记录互相污染，重复分配检测失效 | P0 |
| G4 | allocator bitmap/counter 无细粒度并发协议 | 重复 pblock、free counter 错、crash 后不一致 | P0 |
| G5 | 同 inode write/truncate/extent tree 缺少显式互斥 | 映射丢失、释放后读、size 回退 | P0 |
| G6 | 目录 rename/create/unlink 锁顺序未定义 | dentry/link count 不一致或死锁 | P0 |
| G7 | direct read cache 与写侧失效竞争 | 读到旧 mapping 或旧数据 | P1 |
| G8 | timestamp cache 与 journal 成功语义松耦合 | 时间戳漏写或 crash 后状态异常 | P1 |
| G9 | commit/checkpoint 锁顺序与 I/O 混合 | 高并发下死锁或长尾延迟 | P1 |
| G10 | 当前测试未覆盖真实多文件并发 | 删除全局锁后缺少可信验收 | P0 |
| G11 | active handle 按 transaction id / FIFO 匹配 | 同一 transaction 多 handle 下 stop/metadata/data-sync 归属错误 | P0 |
| G12 | buffered I/O / PageCache 失效协议未定义 | truncate/unlink/rename/direct write 后读到旧数据 | P1 |
| G13 | fsync group commit 与 orphan inode 语义未验收 | fsync 测试预期错误，unlink-while-open 泄漏或复用腐败 | P1 |
| G14 | ordered-mode data drain 误按单文件处理 | 同一 TX 中其他文件 metadata 已提交但 data 未落盘 | P0 |
| G15 | `jbd2_runtime` / `jbd2_journal` 锁序未固定 | commit/checkpoint 与写路径死锁 | P0 |
| G16 | 多 block group 锁序未固定 | allocator 并发死锁 | P1 |
| G17 | 多 handle 共用 TX 时 credit/admission 未审计 | TX 过大、journal 空间不足或 rotation 时机错误 | P1 |

## 初步拆解顺序

1. 先新增并发 correctness 测试，证明当前全局串行模型下测试资产可跑通。
2. 将 `runtime_block_size` 改成显式上下文，但暂不改变并发模型；`EXT4_RS_RUNTIME_LOCK` 先作为 safety fence 保留。
3. 将 JBD2 current handle 改为带唯一 handle id 的显式 token / operation context，禁止依赖 `active_handles.front_mut()` 或 transaction id 匹配。
4. 将 `OP_ALLOCATED_BLOCKS` 改为 operation-local / handle-local。
5. 引入保守的 per-inode 与 parent-directory 锁，先允许不同 inode/不同目录并发，同 inode 写侧串行；同步定义 cache/PageCache 失效协议。
6. 为 allocator 引入 block group 级互斥或等价分配协议。
7. 在 Step 3/4/5/6 的 correctness 保护都通过后，再缩小或删除 `EXT4_RS_RUNTIME_LOCK`，并逐步拆 `inner`。

## 验收口径建议

Phase 2 的 PASS 必须同时满足：

- runner 返回成功；
- 内核日志严格扫描无 ext4/JBD2 核心错误；
- crash/recovery 基线不回退；
- 新增并发测试能检查文件内容、文件大小、目录项集合、link count、fsync 后持久性；
- 对并发测试失败，优先按数据一致性问题处理，而不是只看性能超时。
