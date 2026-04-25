# Asterinas ext4 JBD2 功能实现 Phase 2 — 计划

首次更新时间：2026-04-24（Asia/Shanghai）

## 目标

在 JBD2 Phase 1 完成的基础上，实现优秀档剩余能力：**多文件并发基本读写正确性**，并把验证范围扩展到更接近“核心功能全量验证”的 xfstests 口径。

阶段目标按优先级排序：

1. correctness：并发 create/read/write/truncate/unlink/rename/fsync 不出现数据错乱、丢失、重复块分配、metadata 不一致；
2. recovery：并发 workload + crash 后仍保持数据一致性 100%；
3. coverage：扩展 xfstests core 子集，通过率 >= 95%；
4. performance：在正确性稳定后缩小串行化，恢复并发吞吐，fio 守住 90% 级别目标。

## 设计原则

1. **先 correctness，再性能**：任何拆锁都必须被新增并发测试覆盖。
2. **先显式状态，再并发执行**：把全局变量、隐式 current handle、operation-global guard 改成 per-fs/per-operation/per-handle 状态。
3. **先多文件并发，再同文件复杂并发**：Phase 2 可以保守串行同 inode 写侧，优先让不同 inode/不同目录并发。
4. **拆锁分阶段推进**：先把 `runtime_block_size` 显式化，但不立即改变并发模型；只有 JBD2 handle、alloc guard、inode/目录锁、allocator 协议都就位后，才能缩小 `EXT4_RS_RUNTIME_LOCK`。
5. **JBD2 语义不让步**：metadata 必须归属正确 transaction，ordered mode 的 data-before-metadata 约束不能因并发破坏。
6. **日志证据优先**：每个 step 的 milestone 必须写入测试命令、通过率、日志路径和严格关键词扫描结论。
7. **每步可独立落地**：每个 step 都要能单独 commit / PR，并独立跑通固定回归；若发现死锁或数据错误，优先回退到上一层保守串行策略。

## 验收标准

### 固定回归不能回退

| 测试项 | 最低要求 |
|--------|----------|
| `phase3_base_guard` | `10/10 PASS`，严格关键词扫描为空 |
| `phase4_good` | `12/12 PASS`，严格关键词扫描为空 |
| `phase6_good` | `25/25 PASS`，严格关键词扫描为空 |
| `jbd_phase1` | 有效样本通过率 100% |
| JBD2 crash matrix | `9/9 PASS` |
| dirty journal recovery | `jbd2_probe recover + e2fsck -fn` PASS |

### 新增 Phase 2 验收

- `feature_jbd2_phase2_concurrency` 自研并发测试集通过：
  - 多文件并发写后逐文件校验内容；
  - 多 reader / writer 混合读写后校验无旧 mapping、无越界数据；
  - 并发 create/unlink 后目录集合合法；
  - 并发 rename 后源/目标路径状态合法；
  - write/truncate/fsync 混合后 size 与内容合法；
  - 并发 workload crash 后 verify 100%。
- xfstests core 扩展集通过率 >= 95%，失败项必须有 blocked/excluded 原因。
- freeze、shutdown、remount、device-mapper crash 注入等环境/挂载生命周期类用例不作为 Phase 2 必过范围；若纳入 xfstests 列表，必须用 blocked/excluded 明确原因。
- fio O_DIRECT：read >= 90%，write 优先恢复到 >= 90%；若阶段中间低于 90%，必须解释是 correctness 临时开销还是真实回退。

## Step 0：建立 Phase 2 基线与并发测试资产

**状态：** 已完成（baseline 已建立；仅作为全局串行 fence 下的不回退基线）
**目标：** 先有能复现并发问题的测试，再开始拆锁。

### 方案

1. 新增 `feature_jbd2_phase2` xfstests/case list 或 runner mode，独立于 `jbd_phase1`。
2. 新增自研并发测试程序/脚本，建议覆盖：
   - `multi_file_write_verify`：N 个进程/线程分别写 N 个文件，落盘后逐文件校验 pattern；
   - `multi_file_read_write`：多个 writer 与 reader 交错，reader 校验已 fsync 文件的内容；
   - `create_unlink_churn`：并发 create/unlink 同目录不同文件名，最终目录集合可预测；
   - `rename_churn`：同目录与跨目录 rename，校验不存在双重可见或全部消失；
   - `write_truncate_fsync`：写、truncate、fsync 混合，校验 size、hole、extent mapping；
   - `concurrent_crash_prepare/verify`：并发 workload 中注入 crash，mount 后验证一致性。
3. 新增 strict log scan 入口，复用 Phase 1 关键词。
4. 所有并发 case 必须支持固定 seed、重复轮次和内容 hash 校验；失败时输出最小复现参数、inode 列表、handle id / transaction id 序列、commit tid 序列与相关日志切片。
5. 记录当前 Phase 1 串行模型下的并发测试结果，作为拆锁前基线。

### 验收

- Phase 2 并发测试资产可在 Docker 内一键运行；
- 在当前代码上至少得到一个可信 baseline；
- 每个 case 至少支持 `SEED=`、`ROUNDS=`、`WORKERS=` 等确定性参数；
- milestone 记录日志路径、PASS/FAIL、失败样本解释。

## Step 1：锁/状态可观测性与锁顺序文档化

**状态：** 已完成
**目标：** 让后续拆锁可以定位死锁、长尾与错误归属。

### 方案

1. 在 ext4/JBD2 热点路径增加低噪声 debug 统计：
   - handle start/stop 与 transaction id；
   - 最大并发 active handle 数、平均 active handle 数；
   - allocator block group 与分配数量；
   - commit/checkpoint 耗时；
   - direct read cache hit/miss 与失效；
   - per-op metadata block 数。
2. 写入锁顺序约定：
   - VFS/inode/dir 锁；
   - cache 全局 map 锁：`dir_entry_cache`、`inode_direct_read_cache`、timestamp caches；
   - ext4_rs inner 或未来 per-inode/per-block-group 锁；
   - JBD2 runtime；
   - JBD2 journal device/ring；
   - checkpoint lock；
   - block device I/O。
3. 约定 cache 全局 map 锁只做短临界区，排在 per-inode/per-dir 资源锁之后、JBD2 runtime 之前；禁止持有 cache map 锁进入块设备 I/O。
4. 固定 JBD2 锁序：`jbd2_runtime` 必须先于 `jbd2_journal` 获取；commit/checkpoint 路径若已经持有 journal 锁又需要修改 runtime，必须先释放 journal 锁再回头获取 runtime，或把 runtime 状态修改移出 journal critical section。
5. 同步原语选型先定规矩：可能阻塞、等待 I/O 或跨调度点的锁使用可睡眠/阻塞 mutex；`spin::Mutex` 只用于极短、不阻塞、不做 I/O 的内存状态。
6. Phase 2 默认 commit/checkpoint 仍是 inline 路径；若 Step 8 引入后台线程，必须重新审计锁顺序、唤醒条件与 shutdown/unmount 交互。
7. 检查现有路径是否存在反向获取，尤其是 `inner -> jbd2_runtime -> jbd2_journal` 与 checkpoint/sync 路径。

### 验收

- 文档记录当前锁顺序；
- 并发测试失败时能定位到 transaction、inode、block group 或 cache 层。

## Step 2：显式化 ext4_rs `runtime_block_size`

**状态：** 待开始
**目标：** 移除全局 block size 变量的语义风险，但暂不删除 `EXT4_RS_RUNTIME_LOCK`，不改变并发模型。

### 方案

1. 将 `runtime_block_size()` 调用点改为显式 block size 参数或 `Ext4`/context 字段：
   - block load/sync；
   - direntry tail/checksum；
   - extent block load/checksum；
   - superblock mount/open 流程。
2. 删除或废弃 `set_runtime_block_size()` 全局语义；若短期保留兼容 shim，必须确保它不再影响真实解析路径。
3. 明确保留 `EXT4_RS_RUNTIME_LOCK` 作为 safety fence，本 step 不缩空、不删除它，也不放开同 fs 并发写。
4. 验证不同挂载点/不同测试镜像不再依赖全局 block size 状态。

### 验收

- `rg runtime_block_size` 只剩兼容 shim 或为 0；
- phase3/phase4/phase6/jbd_phase1/crash 全部不回退；
- Phase 2 并发测试在“仍保留全局串行 fence”的模型下不回退；
- milestone 明确记录：本 step 只完成纯重构，不作为删除 `EXT4_RS_RUNTIME_LOCK` 的验收。

## Step 3：JBD2 handle-local operation context

**状态：** 待开始
**目标：** 让并发 metadata write 归属正确 handle/transaction。

### 方案

1. 引入显式 `JournalOperationContext` 或等价 token，包含：
   - transaction id；
   - 新增唯一 handle id / generation（不能只使用 transaction id，可用 per-runtime 单调 `u64`）；
   - reserved blocks；
   - data_sync_required；
   - operation-local allocated blocks。
2. metadata writer 不再从 `active_handles.front_mut()` 推断当前 handle，而是通过 context 记录 metadata write。
3. `remove_active_handle()` 必须改为按唯一 handle id 精确匹配，禁止按 transaction id 摘除。
4. `mark_active_jbd2_handle_requires_data_sync()` 改成按 handle id 标记。
5. 明确 metadata 可见性：同一 running transaction 内多个 handle 共享 metadata view，`overlay_metadata_read()` 不按 handle id 过滤；跨 transaction 仍按 `running -> prev_running -> committing -> checkpoint_list` 的顺序找最新 buffer。
6. 审计 transaction credit/admission：多 handle 共用 running transaction 时，`reserved_blocks` 累加不能无限增长；达到容量或阈值时必须 rotate running transaction 或让新 handle 等待。
7. `JournalRuntime::active_handles` 保留为统计/生命周期管理，不再作为 current-handle source of truth。
8. 增加单测：同一 running transaction 下两个 handle 交错写不同 metadata block，并按相反顺序 stop，最终 handle/transaction 记账正确。

### 验收

- 并发 handle 交错单测通过；
- `stop_handle()` 不会在同一 transaction 多 handle 下摘错对象；
- metadata write 不再依赖 FIFO front；
- 同一 running transaction 的 handle 可以看到共享 metadata overlay；
- transaction credit 超阈值时有明确 rotate/wait 策略；
- ordered mode data-before-metadata 约束仍通过 crash 测试。

## Step 4：operation allocated block guard 本地化

**状态：** 待开始
**目标：** 修复 allocator guard 的全局共享风险。

### 方案

1. 将 `OP_ALLOCATED_BLOCKS` 从 static 全局集合迁移到 operation context 或 journal handle。
2. allocator 查询“本操作刚分配块”时通过 context 访问。
3. 对无 journal 模式提供显式临时 context，避免回退到全局变量。
4. 增加并发 allocator 测试：多个文件同时扩展，校验无重复 pblock。

### 验收

- `alloc_guard.rs` 不再维护跨操作全局集合；
- 并发写不同文件不出现重复物理块；
- `generic/013`、`generic/014` 不回退。

## Step 4.5：补齐 Step 3/4 的真实并发上下文语义

**状态：** 已完成
**目标：** 在进入 per-inode/per-directory 锁之前，消除 Step 3/4 中仍依赖 `EXT4_RS_RUNTIME_LOCK` 兜底的 single-slot current context，避免 Step 7 拆锁时暴露 metadata 归属错乱、allocator guard 串扰和 JBD2 read-side 瓶颈。

### 背景

Step 3/4 已移除 `active_handles.front_mut()` 与全局 `OP_ALLOCATED_BLOCKS` 等历史风险，但当前集成层仍存在两个“当前操作”单槽状态：

- Asterinas ext4 bridge 的 `jbd2_current_handle_id: Arc<Mutex<Option<u64>>>`；
- ext4_rs `LocalOperationAllocGuard.current_operation: Mutex<u64>`。

在 `EXT4_RS_RUNTIME_LOCK` 仍全局串行时，这两个 single-slot 不会被真实并发覆盖；一旦 Step 7 缩小全局锁，多 handle / 多 operation 交错会把 T1 的 metadata write 或 allocated block guard 操作路由到 T2。Step 4.5 的验收标准是让 Step 3/4 的 correctness 不再依赖全局串行 fence。

### 方案

1. 消除 JBD2 current handle single-slot：
   - 优先将 metadata write 所需 handle id 作为显式上下文下传；
   - 若短期必须保留隐式 current handle，至少改为可嵌套 push/pop guard，且文档标注它仍不是 Step 7 最终形态；
   - `mark_current_jbd2_handle_requires_data_sync()` 同步改为显式 handle id 或 scoped guard 读取，不能被其他 operation 覆盖。
2. 消除 allocator current operation single-slot：
   - 将 `OperationAllocGuard` API 拆成带 operation id 的显式接口，例如 `reserve_for(op_id, block)` / `contains_for(op_id, block)`；
   - ext4_rs allocator 路径必须能从当前 Ext4 operation context 取得稳定 operation id；
   - `clear_current_operation()` 拆成语义明确的 API：切换/重置当前指针与丢弃某 operation 数据不能混用。
3. 整理 lifecycle：
   - 删除 `run_ext4*` / `run_journaled_ext4` 中 begin 后立即 clear 的模式；
   - 嵌套 active handle 路径必须通过测试证明不会清空外层 operation guard；
   - 对无 journal 路径提供明确的临时 operation context。
4. 改造 JBD2 overlay read side：
   - `overlay_reads` / `overlay_hits` 等 debug 计数改为 atomic 或独立统计结构；
   - `JournalRuntime::overlay_metadata_read()` 改回 `&self`；
   - Asterinas bridge 侧 overlay read 使用共享读路径，避免所有 ext4 metadata read 排队到 runtime 写锁。
5. 处理 credit admission 口径：
   - 对超过 soft limit 且 running transaction 仍有 active handle 的场景优先 rotate running TX，让新 handle 进入新 TX；
   - 若 `prev_running` 已占用导致无法 rotate，后续需补 wait/backpressure，不得把该边界误标为拆锁后完整 admission。
6. 补并发与嵌套单测：
   - 两个 handle 交错 metadata write，验证写入归属不被覆盖；
   - 两个 operation 交错 reserve/contains，验证 allocated block set 不串扰；
   - nested guard preserve 分支必须覆盖外层 reserve 后内层 run_ext4 不误清空外层状态；
   - overlay read side 只读路径不需要 runtime 独占写锁。
7. 文档同步：
   - milestone 明确 Step 0/3/4 的并发 baseline 只是“全局串行 fence 下的回归基线”，不是真实内核并发 correctness 证明；
   - Step 5/6/7 的前置条件增加 Step 4.5 通过。

### 验收

- `jbd2_current_handle_id` 不再作为跨 operation 的全局 single-slot current handle source of truth；
- `LocalOperationAllocGuard.current_operation` 不再作为跨 operation 的全局 single-slot current operation source of truth；
- metadata write、data-sync 标记、allocator reserve/contains 在交错 handle/operation 单测下归属正确；
- `overlay_metadata_read()` 为共享读接口，debug stats 不迫使读路径获取 runtime 写锁；
- credit admission 的 rotate/wait/临时放行口径被代码或文档明确覆盖；
- phase3/phase4/phase6/jbd_phase1/crash 与 Phase 2 concurrency baseline 不回退；
- milestone 清楚标注：Step 4.5 通过前不得进入 Step 5 的实现阶段。

## Step 5：per-inode / per-directory correctness 锁

**状态：** 待开始
**目标：** 先以保守锁保护同 inode 与目录变更正确性。
**前置条件：** Step 4.5 已验收通过。

### 方案

1. 为写侧 mutation 引入 per-inode 锁：
   - write 需要分配/改 extent 时加写锁；
   - truncate 加写锁；
   - Step 5 correctness 阶段采用保守同步：read 与同 inode write/truncate 互斥，或只基于持锁期间取得的稳定 mapping snapshot 读取；mapping generation 优化留到 Step 8。
2. 为目录 mutation 引入 parent directory 锁：
   - create/unlink/rmdir 锁 parent；
   - rename 先按 inode number 排序锁定 `old_parent` / `new_parent`，再解析目标；
   - 若目标 inode 存在，再按 inode number 排序锁定 `src` / `dst`；若目标不存在，使用明确占位语义，不在持锁后反向补锁；
   - rename 的锁顺序必须覆盖 same-dir、cross-dir、overwrite、目标不存在四种情况。
3. Phase 2 不接入 ext4 PageCache；buffered I/O 仍直通 `fs.read_at()` / `fs.write_at()`，并发语义由 fs 层锁和 JBD2 ordered mode 保证。
4. 所有写/truncate 成功或失败后都失效对应 direct read cache；unlink/rename 覆盖目标需要处理 inode 与 direct read cache 生命周期，避免打开文件、被 unlink 文件或被覆盖目标继续读到错误数据。
5. cache 更新与 journaled mutation 绑定：操作失败时失效 cache，不提交乐观 cache。
6. 明确 ordered-mode 与 fsync group commit 语义：
   - ordered-mode 数据约束以 transaction 为单位，commit 前必须 drain 该 transaction 全部 handle 触及的 dirty data block；
   - fsync 可以顺带提交其他 ready transaction，但对调用者承诺的最小集合是其本人 fsync 前已完成的写入 durable；
   - 测试不得假设“只刷当前文件”。
7. 审计 orphan inode：当前若没有完整 `s_last_orphan` / close-time orphan cleanup，就必须保守禁止 inode 复用并记录空间泄漏边界；新增 unlink-while-open 验收。

### 验收

- 多文件并发写/read verify 通过；
- create/unlink/rename 并发测试通过；
- buffered 直通路径与 direct read cache 不返回过期 mapping 或过期数据；
- unlink-while-open 不发生 inode 复用腐败；
- fsync 并发测试按 group commit 语义通过。

## Step 6：allocator 与 block group 并发协议

**状态：** 待开始
**目标：** 支持不同 inode 同时分配块而不重复、不漏计。

### 方案

1. 引入 per-block-group allocator 锁，保护 bitmap RMW、group free counter、superblock free counter 的一致更新。
2. 多 block group 操作必须按 group number 升序获取 per-bg 锁；不允许在持有某个 bg 锁后回头补更小编号的锁。
3. allocator 扫描时允许不同 block group 并行，同一 block group 串行。
4. metadata block 分配和 data block 分配都走同一保护协议。
5. JBD2 handle 覆盖 bitmap、group descriptor、superblock、inode/extent tree 更新。

### 验收

- 并发大文件写不出现重复 pblock；
- crash 后 e2fsck 不报告 bitmap/counter/extent 不一致；
- allocator 锁不会引入明显死锁或长时间饥饿。

## Step 7：逐步缩小 `EXT4_RS_RUNTIME_LOCK`、`inner: Mutex<Ext4>` 与全局串行路径

**状态：** 待开始
**目标：** 在 Step 3/4/5/6 的 correctness 保护全部通过后，从“同 fs 串行”推进到“不同 inode/目录/allocator 分区并发”。

### 方案

1. 先确认 Step 2 已替换所有 `sync_runtime_block_size()` / `set_runtime_block_size()` 真实调用点，包括 `fs.rs` 中 mount/recovery/run_ext4/run_journaled_ext4 等路径；否则不得放开 `inner` 并发。
2. 再把只读路径从 `inner` 中拆出可并发部分，例如 extent mapping 查询、stat 读取、direct read plan。
3. 只有在 handle-local context、alloc guard 本地化、per-inode/per-dir 锁、per-block-group allocator 协议都通过验收后，才能缩小或删除 `EXT4_RS_RUNTIME_LOCK`。
4. 对写路径按资源加锁：
   - inode extent/size；
   - parent dir；
   - block group allocator；
   - journal runtime。
5. 对仍依赖 `inner` 的路径保守保留锁，并在 milestone 标注剩余原因。
6. 每拆一个路径就跑 Phase 2 并发测试与 Phase 1 固定回归；若出现死锁或数据错误，回退到上一层保守锁范围。

### 验收

- 多文件并发读写真实并行度提升；
- 日志不再显示所有操作长期等待同一个全局锁；
- Phase 1 基线不回退。

## 阶段管理与回退策略

- 每个 Step 独立落地，提交前至少跑本 Step 目标测试和固定回归 smoke；进入下一 Step 前跑完整固定回归。
- 新并发能力默认可由保守锁/fence 兜底，发现死锁、重复 pblock、metadata 归属错误时，优先恢复上一层锁范围，而不是继续叠补丁。
- `EXT4_RS_RUNTIME_LOCK` 的删除是 Step 7 的结果，不是 Step 2 的结果；任何提前删除都必须同时证明 Step 3/4/5/6 已满足。
- 文档与 milestone 必须记录每次回退原因、日志路径和重新打开并发的条件。

## Step 8：性能恢复与优化

**状态：** 待开始
**目标：** correctness 稳定后，恢复 write ratio 到 90% 级别并提升并发吞吐。

### 方案

1. 对 commit/checkpoint 做批量化与延迟策略复核，避免 correctness 阶段的保守锁导致 per-op flush 回归。
2. direct read cache 引入 generation，减少过度失效。
3. allocator 按 block group 并行，减少热点 group 争用。
4. 评估后台 commit/checkpoint 线程；若引入，必须先补 crash ordering 测试。
5. fio、lmbench、并发 workload 分别记录，避免只优化单一指标。

### 验收

- fio read >= 90%，write 目标 >= 90%；
- 并发 workload 相比全局串行模型有明确吞吐提升；
- crash 与 xfstests 不回退。

## Step 9：文档、报告与最终验收

**状态：** 待开始

### 方案

1. 更新 `feature_jbd2_phase2_milestone.md`，写入每步代码改动、日志路径、测试数据。
2. 更新 README、benchmark、environment 与索引文档。
3. 补充 RustOS/星绽架构下 ext4 并发与日志性能优化研究结论，服务赛题文档完整性与创新性评分。

### 验收

- Phase 2 milestone 完整；
- README 能指导同学克隆后复现；
- 赛题优秀档剩余项均有测试证据。

## 推荐验证命令

所有命令默认在 `/home/lby/os_com_codex/asterinas` 下执行。

```bash
PHASE4_DOCKER_MODE=phase6_with_guard \
ENABLE_KVM=1 \
NETDEV=tap \
VHOST=on \
KLOG_LEVEL=error \
bash tools/ext4/run_phase4_in_docker.sh
```

```bash
PHASE4_DOCKER_MODE=jbd_phase1 \
ENABLE_KVM=1 \
RELEASE_LTO=0 \
XFSTESTS_CASE_TIMEOUT_SEC=1200 \
XFSTESTS_RUN_TIMEOUT_SEC=3600 \
bash tools/ext4/run_phase4_in_docker.sh
```

```bash
PHASE4_DOCKER_MODE=crash_only \
ENABLE_KVM=1 \
NETDEV=tap \
VHOST=on \
CRASH_ROUNDS=1 \
CRASH_HOLD_STAGE=after_commit \
bash tools/ext4/run_phase4_in_docker.sh
```

```bash
BENCH_ENABLE_KVM=1 \
BENCH_ASTER_NETDEV=tap \
BENCH_ASTER_VHOST=on \
bash test/initramfs/src/benchmark/fio/run_ext4_summary.sh
```

Phase 2 新 runner 建立后，应在本节补充 `PHASE4_DOCKER_MODE=jbd_phase2_concurrency` 或等价命令。

### Phase 2 并发 baseline

```bash
PHASE4_DOCKER_MODE=jbd_phase2_concurrency \
ENABLE_KVM=1 \
NETDEV=tap \
VHOST=on \
EXT4_PHASE2_SEED=1 \
EXT4_PHASE2_WORKERS=4 \
EXT4_PHASE2_ROUNDS=8 \
bash tools/ext4/run_phase4_in_docker.sh
```

可用 `EXT4_PHASE2_CASES=multi_file_write_verify,rename_churn` 缩小 case 集合；case 列表使用逗号分隔，避免 kernel cmdline 被空格切开。

锁顺序与同步原语约定见 `feature_jbd2_phase2_lock_order.md`；后续 Step 1/2/3/4/5/6/7 的代码改动必须按该文档检查。
