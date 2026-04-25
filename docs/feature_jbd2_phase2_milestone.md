# ext4 JBD2 功能实现 Phase 2 Milestone 记录

首次更新时间：2026-04-24（Asia/Shanghai）

当前状态：Phase 2 Step 0/1/2/3/4/4.5 已完成，准备进入 Step 5

## Phase 1 收口基线

| 测试项 | Phase 1 结果 | Phase 2 要求 |
|--------|--------------|--------------|
| `phase3_base_guard` | `10 PASS / 0 FAIL / 6 NOTRUN / 24 STATIC_BLOCKED` | 不回退 |
| `phase4_good` | `12 PASS / 0 FAIL / 6 NOTRUN / 22 STATIC_BLOCKED` | 不回退 |
| `phase6_good` | `25 PASS / 0 FAIL / 0 NOTRUN / 26 STATIC_BLOCKED` | 不回退 |
| `jbd_phase1` | `6 PASS / 0 FAIL / 6 NOTRUN` | 不回退 |
| JBD2 crash matrix | `9/9 PASS` | 不回退，并扩展并发 crash |
| dirty journal recovery | PASS | 不回退 |
| fio O_DIRECT | read `93.49%`，write `87.01%` | read >= 90%，write 优先恢复 >= 90% |

## Phase 2 验收口径

- `PASS` 必须同时满足 runner 成功、严格关键词扫描为空、数据校验通过。
- 固定回归集必须写明 `几/几`，不能只写百分比。
- 并发测试必须校验内容、size、目录项集合、fsync 后持久性；不能只看程序退出码。
- crash 测试必须区分 committed / uncommitted 语义，verify 阶段必须独立重挂载检查。
- Step 0/3/4 已记录的 Phase 2 并发 baseline 均运行在 `EXT4_RS_RUNTIME_LOCK` 仍作为全局 safety fence 的模型下，只能作为不回退基线；真实内核并发 correctness 需在 Step 4.5 补齐上下文语义并于 Step 7 拆锁后重新验证。

## Step 0：建立 Phase 2 基线与并发测试资产

**状态：** 已完成
**对应 analysis：** G10
**目标摘要：** 建立可重复运行的多文件并发读写、目录 churn、rename、write/truncate/fsync 与并发 crash 测试资产。

### 改动概要

- 新增 `ext4_phase2` syscall 测试套件，包含一个 host 编译后注入 initramfs 的 C helper 和一个 shell runner。
- 新增 `jbd_phase2_concurrency` Docker/phase runner mode，独立于 `jbd_phase1` 和 xfstests 主列表。
- 当前 baseline case 覆盖：
  - `multi_file_write_verify`
  - `multi_file_read_write`
  - `create_unlink_churn`
  - `rename_churn`
  - `write_truncate_fsync`
- case 支持 `EXT4_PHASE2_SEED`、`EXT4_PHASE2_WORKERS`、`EXT4_PHASE2_ROUNDS`、`EXT4_PHASE2_CASES`；失败时输出 `EXT4_PHASE2_FAIL`，包含 case/seed/workers/rounds 和原因。
- host runner 对日志执行严格关键词扫描，命中 `panic`、`BUG`、`logical block not mapped`、`Extentindex not found`、`ext4 write_at failed` 等关键错误时失败。

### 涉及文件

- `asterinas/test/initramfs/src/syscall/ext4_phase2/phase2_concurrency.c`
- `asterinas/test/initramfs/src/syscall/ext4_phase2/run_ext4_phase2_concurrency.sh`
- `asterinas/test/initramfs/src/syscall/run_syscall_test.sh`
- `asterinas/tools/ext4/prepare_phase4_part3_initramfs.sh`
- `asterinas/tools/ext4/run_phase4_part3.sh`
- `asterinas/tools/ext4/run_phase4_in_docker.sh`

### 功能回归

| 测试项 | 结果 |
|--------|------|
| C helper host smoke | PASS（5/5，`workers=3 rounds=3 seed=42`） |
| 脚本语法检查 | PASS（`bash -n` / `sh -n`） |
| Phase 2 并发测试 smoke | PASS（5/5，`workers=2 rounds=2 seed=42`，日志：`asterinas/benchmark/logs/jbd_phase2_concurrency_20260424_100852.log`） |
| Phase 2 并发测试 baseline | PASS（5/5，`workers=4 rounds=8 seed=1`，日志：`asterinas/benchmark/logs/jbd_phase2_concurrency_20260424_100946.log`） |
| `phase3_base_guard` | PASS（10/10，日志：`asterinas/benchmark/logs/phase3_base_guard_20260424_101240.log`） |
| `phase4_good` | PASS（12/12，日志：`asterinas/benchmark/logs/phase4_good_20260424_101240.log`） |
| `phase6_good` | PASS（25/25，日志：`asterinas/benchmark/logs/phase6_good_20260424_101240.log`） |
| `jbd_phase1` | PASS（6/6 有效样本，6 NOTRUN，日志：`asterinas/benchmark/logs/jbd_phase1_20260424_104734.log`） |
| crash matrix | PASS（9/9，日志：`asterinas/benchmark/logs/crash/phase4_part3_crash_summary_20260424_110351.tsv`） |
| strict keyword scan | PASS（上述 phase3/phase4/phase6/jbd_phase1/phase2/crash summary 均为空） |

### 性能结果

| 测试项 | Asterinas | Linux | 比值 | 结论 |
|--------|----------:|------:|-----:|------|
| fio read | 待运行 | 待运行 | 待运行 | 待记录 |
| fio write | 待运行 | 待运行 | 待运行 | 待记录 |

### 验收项

- [x] 并发测试资产可一键运行
- [x] 当前串行模型 baseline 已记录
- [x] strict log scan 已接入
- [x] 每个并发 case 支持固定 seed、重复轮次、hash 校验与失败最小复现参数输出

## Step 1：锁/状态可观测性与锁顺序文档化

**状态：** 已完成
**对应 analysis：** G1-G17
**目标摘要：** 明确锁顺序，增加低噪声统计，支持后续拆锁定位。

### 改动概要

- 新增锁顺序与同步原语约定文档，固定 Phase 2 拆锁前必须遵守的顺序：
  - VFS / per-inode / per-dir
  - cache 全局 map
  - ext4_rs coordination / allocator block group
  - `jbd2_runtime -> jbd2_journal -> jbd2_checkpoint_lock`
  - block device I/O
- 文档明确 JBD2 handle 可见性、ordered-mode transaction 级 data drain、rename 锁序、多 block group 锁序、buffered I/O 范围与回退策略。
- 新增 `JournalRuntimeDebugStats`，记录 started/finished handle、最大 active handle、最大 running TX handle/reserved/metadata、rotation、commit/checkpoint、overlay metadata read/write 统计。
- 新增 `OperationAllocGuardDebugStats`，记录 `OP_ALLOCATED_BLOCKS` clear/reserve/contains 调用、累计 reserved block 与单次操作峰值。
- 在 Asterinas ext4 层统计 `EXT4_RS_RUNTIME_LOCK` acquire 次数、平均/最大等待时间、平均/最大持有时间，并以低频 debug 日志输出 Phase 2 汇总。

### 涉及文件

- `feature_jbd2_phase2_lock_order.md`
- `asterinas/docs/feature_jbd2_phase2_lock_order.md`
- `feature_jbd2_phase2_plan.md`
- `feature_jbd2_phase2_milestone.md`
- `asterinas/kernel/libs/ext4_rs/src/ext4_impls/alloc_guard.rs`
- `asterinas/kernel/libs/ext4_rs/src/ext4_impls/jbd2/journal.rs`
- `asterinas/kernel/libs/ext4_rs/src/simple_interface/mod.rs`
- `asterinas/kernel/src/fs/ext4/fs.rs`

### 功能回归

| 测试项 | 结果 |
|--------|------|
| `cargo check -p ext4_rs` | PASS（仅既有 `error_in_core` stable warning） |
| `cargo test -p ext4_rs ext4_impls::jbd2::journal --lib` | PASS（6/6） |
| shell/gcc 轻量检查 | PASS（runner `bash -n` / `sh -n`，Phase 2 helper static build） |
| Phase 2 并发 smoke | PASS（5/5，`workers=2 rounds=2 seed=44`，日志：`asterinas/benchmark/logs/jbd_phase2_concurrency_20260424_135914.log`） |
| Phase 2 并发 baseline | PASS（5/5，`workers=4 rounds=8 seed=1`，日志：`asterinas/benchmark/logs/jbd_phase2_concurrency_20260424_135657.log`） |
| 固定回归集 | Step 1 未改变事务语义；复用 Step 0 全量固定回归结果 |
| `cargo fmt --check` | BLOCKED：当前 nightly toolchain 未安装 `rustfmt` |
| `cargo check -p aster-kernel` | BLOCKED：本机工作区缺少 `acpi` / `x86_64` / `tdx_guest` 等架构依赖；Docker/QEMU smoke 已覆盖真实内核编译 |

### 验收项

- [x] 锁顺序已写入文档
- [x] handle / transaction / allocator / checkpoint 关键统计可观测
- [x] cache 全局 map 锁顺序已覆盖
- [x] `jbd2_runtime -> jbd2_journal -> checkpoint -> block I/O` 方向已固定，反向路径已消除或记录
- [x] 同步原语选型已明确，禁止持 spin lock 做阻塞 I/O
- [x] 最大/平均 active handle 并发度可观测
- [x] 后台 commit/checkpoint 若进入实现，需要重新审计锁序

## Step 2：显式化 ext4_rs 全局 `runtime_block_size`

**状态：** 已完成
**对应 analysis：** G1
**目标摘要：** 将 block size 改为显式上下文；本 step 不删除、不缩空 `EXT4_RS_RUNTIME_LOCK`，不改变并发模型。

### 改动概要

- 移除 ext4_rs 全局 `RUNTIME_BLOCK_SIZE`、`runtime_block_size()`、`set_runtime_block_size()`，`Ext4::open()` 不再写全局 block size 状态。
- `Block::load()` 改为接收显式 `block_size` 参数；superblock 读取使用固定 `BLOCK_SIZE`，其他 metadata/data block 读取使用当前 `Ext4` superblock 的 block size。
- `Ext4DirEntryTail::copy_to_slice()`、`ExtentNode::load_from_data()` / `load_from_data_mut()` 同步改为显式 block size，避免内部继续依赖全局上下文。
- Asterinas ext4 wrapper 中删除 `sync_runtime_block_size()` 调用点，但 `EXT4_RS_RUNTIME_LOCK` 仍保留为 Phase 2 correctness safety fence。
- strict-scan 回归中发现 `phase6_good` 会打印可恢复的 directory logical-block unmapped 日志；`dir_add_entry()` 现在对未映射目录逻辑块跳过/追加，不再把目录空洞当成致命错误输出。

### 涉及文件

- `asterinas/kernel/libs/ext4_rs/src/ext4_defs/consts.rs`
- `asterinas/kernel/libs/ext4_rs/src/ext4_defs/block.rs`
- `asterinas/kernel/libs/ext4_rs/src/ext4_defs/direntry.rs`
- `asterinas/kernel/libs/ext4_rs/src/ext4_defs/extents.rs`
- `asterinas/kernel/libs/ext4_rs/src/ext4_defs/block_group.rs`
- `asterinas/kernel/libs/ext4_rs/src/ext4_impls/ext4.rs`
- `asterinas/kernel/libs/ext4_rs/src/ext4_impls/file.rs`
- `asterinas/kernel/libs/ext4_rs/src/ext4_impls/inode.rs`
- `asterinas/kernel/libs/ext4_rs/src/ext4_impls/dir.rs`
- `asterinas/kernel/libs/ext4_rs/src/ext4_impls/extents.rs`
- `asterinas/kernel/libs/ext4_rs/src/ext4_impls/balloc.rs`
- `asterinas/kernel/libs/ext4_rs/src/ext4_impls/jbd2/device.rs`
- `asterinas/kernel/libs/ext4_rs/src/simple_interface/mod.rs`
- `asterinas/kernel/src/fs/ext4/fs.rs`

### 功能回归

| 测试项 | 结果 |
|--------|------|
| `cargo check -p ext4_rs` | PASS（warning only：`error_in_core` stable） |
| `cargo test -p ext4_rs --lib` | PASS（21/21） |
| `runtime_block_size` 残留扫描 | PASS（`rg runtime_block_size/sync_runtime_block_size/set_runtime_block_size` 无命中） |
| `phase3_base_guard` | PASS（10/10，日志：`asterinas/benchmark/logs/phase3_base_guard_20260424_142753.log`，严格关键词扫描为空） |
| `phase4_good` | PASS（12/12，日志：`asterinas/benchmark/logs/phase4_good_20260424_142753.log`，严格关键词扫描为空） |
| `phase6_good` | PASS（25/25，日志：`asterinas/benchmark/logs/phase6_good_20260424_150407.log`，严格关键词扫描为空） |
| `jbd_phase1` | PASS（6/6 有效样本，6 NOTRUN 为环境静态跳过，日志：`asterinas/benchmark/logs/jbd_phase1_20260424_152114.log`，严格关键词扫描为空） |
| Phase 2 并发 baseline | PASS（5/5，`workers=4 rounds=8 seed=1`，日志：`asterinas/benchmark/logs/jbd_phase2_concurrency_20260424_142529.log`，严格关键词扫描为空） |
| crash matrix | PASS（9/9，summary：`asterinas/benchmark/logs/crash/phase4_part3_crash_summary_20260424_153829.tsv`，verify 日志严格关键词扫描为空） |

### 验收项

- [x] `runtime_block_size()` 调用点已迁移
- [x] 不再依赖跨 ext4 挂载点的全局 block size 状态
- [x] `EXT4_RS_RUNTIME_LOCK` 仍作为 safety fence 保留
- [x] 固定回归不回退

## Step 3：JBD2 handle-local operation context

**状态：** 已完成
**对应 analysis：** G2、G11、G14、G17
**目标摘要：** 并发 metadata write / stop / data-sync 必须归属正确 handle/transaction，不再依赖 `active_handles.front_mut()` 或 transaction id 匹配。

### 改动概要

- 为 `JournalHandle` 引入 per-runtime 单调 `handle_id: u64`，`JournalHandleSummary` 同步带出 handle id，transaction id 不再承担唯一 handle 身份。
- `JournalRuntime::remove_active_handle()` 改为按 handle id 精确匹配，修复多个 handle 复用同一 running transaction 时 `stop_handle(B)` 误摘队首 `A` 的语义 bug。
- metadata write 改为 `record_metadata_write_for_handle(handle_id, ...)`；Asterinas `JournalIoBridge` 通过当前 handle id context 将 metadata 写归属到显式 handle，不再使用 `active_handles.front_mut()` 推断。
- data-sync 标记改为按 handle id 精确标记；direct write prepare 路径使用当前 handle id 标记 ordered-mode 数据同步需求。
- 同一 running transaction 内 metadata overlay 继续共享 TX 视图，不按 handle id 过滤；跨 TX 仍沿用 `running -> prev_running -> committing -> checkpoint_list` 顺序。
- 新增 transaction 级累计 admitted credit 统计；当 idle running TX 的累计 credit 加新 handle 会超过 soft limit 时，先 rotate 到 `prev_running`，新 handle 进入新 TX。

### 涉及文件

- `asterinas/kernel/libs/ext4_rs/src/ext4_impls/jbd2/handle.rs`
- `asterinas/kernel/libs/ext4_rs/src/ext4_impls/jbd2/journal.rs`
- `asterinas/kernel/libs/ext4_rs/src/ext4_impls/jbd2/transaction.rs`
- `asterinas/kernel/src/fs/ext4/fs.rs`

### 功能回归

| 测试项 | 结果 |
|--------|------|
| `cargo check -p ext4_rs` | PASS（warning only：`error_in_core` stable） |
| `cargo test -p ext4_rs --lib` | PASS（24/24） |
| JBD2 handle 交错单测 | PASS（新增/覆盖 `stop_handle_matches_unique_handle_id_not_transaction_id`、`data_sync_mark_targets_unique_handle_id`、`credit_admission_rotates_idle_transaction_before_overflow`） |
| `front_mut` / transaction-id 摘除残留扫描 | PASS（JBD2 runtime 无 `front_mut`，无按 transaction id 摘除 active handle） |
| Phase 2 并发 smoke | PASS（5/5，`workers=2 rounds=2 seed=45`，日志：`asterinas/benchmark/logs/jbd_phase2_concurrency_20260425_013615.log`，严格关键词扫描为空） |
| Phase 2 并发 baseline | PASS（5/5，`workers=4 rounds=8 seed=1`，日志：`asterinas/benchmark/logs/jbd_phase2_concurrency_20260425_013827.log`，严格关键词扫描为空） |
| crash matrix | PASS（9/9，summary：`asterinas/benchmark/logs/crash/phase4_part3_crash_summary_20260425_013931.tsv`，verify 日志严格关键词扫描为空） |
| `phase3_base_guard` | PASS（10/10，日志：`asterinas/benchmark/logs/phase3_base_guard_20260425_014131.log`，严格关键词扫描为空） |
| `phase4_good` | PASS（12/12，日志：`asterinas/benchmark/logs/phase4_good_20260425_014131.log`，严格关键词扫描为空） |
| `phase6_good` | PASS（25/25，日志：`asterinas/benchmark/logs/phase6_good_20260425_014131.log`，严格关键词扫描为空） |
| `jbd_phase1` | PASS（6/6 有效样本，6 NOTRUN 为环境静态跳过，日志：`asterinas/benchmark/logs/jbd_phase1_20260425_021742.log`，严格关键词扫描为空） |

### 验收项

- [x] metadata write 通过显式 context 归属 transaction
- [x] `JournalHandle` 有唯一 handle id / generation
- [x] `remove_active_handle()` 按 handle id 精确匹配
- [x] data sync 标记按 handle id 生效
- [x] 同一 running transaction 内 metadata overlay 不按 handle id 过滤
- [x] transaction credit/admission 有 rotate 或 wait 策略
- [x] ordered mode crash 语义不回退

## Step 4：operation allocated block guard 本地化

**状态：** 已完成
**对应 analysis：** G3
**目标摘要：** 将 `OP_ALLOCATED_BLOCKS` 从全局集合迁移为 operation-local / handle-local 状态。

### 改动概要

- 移除 ext4_rs 全局 `OP_ALLOCATED_BLOCKS: Mutex<BTreeSet<Ext4Fsblk>>` 与旧 free function API，避免不同 operation 共享同一个 allocated block 临时集合。
- 新增 `OperationAllocGuard` trait 与 `LocalOperationAllocGuard`，以 per-Ext4 实例保存 operation id -> allocated block set，并保留 clear/reserve/contains/max-operation-blocks 调试统计。
- `Ext4` 结构新增 `alloc_guard: Arc<dyn OperationAllocGuard>`；Asterinas `Ext4Fs::open()` 注入同一个 `LocalOperationAllocGuard` 实例，保证同一挂载点内的 allocator guard 可观测且非全局。
- `balloc` 的重复 pblock 防御改为通过 `self.alloc_guard.contains_current_block()` / `reserve_current_block(s)` 访问当前 operation，不再读取全局集合。
- Asterinas ext4 wrapper 将 JBD2 handle id 映射为 alloc operation id；非 journaled 或无 active handle 的路径使用 `next_alloc_operation_id` 生成 operation id，并在正常/错误返回后统一清理。
- 嵌套 active JBD2 handle 路径会保留当前 operation guard，不在内层误清空外层分配状态。
- Phase 2 debug stats 改为读取 `Ext4Fs` 本地 guard stats，后续拆 `EXT4_RS_RUNTIME_LOCK` 时不会丢失 allocator guard 观测。

### 涉及文件

- `asterinas/kernel/libs/ext4_rs/src/ext4_defs/block.rs`
- `asterinas/kernel/libs/ext4_rs/src/ext4_defs/ext4.rs`
- `asterinas/kernel/libs/ext4_rs/src/ext4_impls/alloc_guard.rs`
- `asterinas/kernel/libs/ext4_rs/src/ext4_impls/balloc.rs`
- `asterinas/kernel/libs/ext4_rs/src/ext4_impls/ext4.rs`
- `asterinas/kernel/libs/ext4_rs/src/simple_interface/mod.rs`
- `asterinas/kernel/src/fs/ext4/fs.rs`

### 功能回归

| 测试项 | 结果 |
|--------|------|
| `cargo check -p ext4_rs` | PASS（warning only：`error_in_core` stable） |
| `cargo test -p ext4_rs --lib` | PASS（25/25，新增 `local_guard_keeps_allocated_blocks_per_operation`） |
| 旧全局 guard 符号扫描 | PASS（`OP_ALLOCATED_BLOCKS` / `clear_operation_allocated_blocks` / `operation_alloc_guard_debug_stats` / `is_operation_allocated_block` / `reserve_operation_allocated*` 无命中） |
| Phase 2 并发 smoke | PASS（5/5，`workers=2 rounds=2 seed=46`，日志：`asterinas/benchmark/logs/jbd_phase2_concurrency_20260425_034616.log`，严格关键词扫描为空） |
| Phase 2 并发 baseline | PASS（5/5，`workers=4 rounds=8 seed=1`，日志：`asterinas/benchmark/logs/jbd_phase2_concurrency_20260425_034834.log`，严格关键词扫描为空） |
| `phase3_base_guard` | PASS（10/10，日志：`asterinas/benchmark/logs/phase3_base_guard_20260425_034949.log`，严格关键词扫描为空） |
| `phase4_good` | PASS（12/12，日志：`asterinas/benchmark/logs/phase4_good_20260425_034949.log`，严格关键词扫描为空） |
| `phase6_good` | PASS（25/25，含 `generic/013` / `generic/014`，日志：`asterinas/benchmark/logs/phase6_good_20260425_034949.log`，严格关键词扫描为空） |
| crash matrix | PASS（9/9，summary：`asterinas/benchmark/logs/crash/phase4_part3_crash_summary_20260425_042529.tsv`，verify 日志严格关键词扫描为空） |
| `jbd_phase1` | PASS（6/6 有效样本，6 NOTRUN 为环境静态跳过，日志：`asterinas/benchmark/logs/jbd_phase1_20260425_042715.log`，严格关键词扫描为空） |

### 验收项

- [x] 无跨操作全局 allocated block set
- [x] 并发扩展文件无重复 pblock
- [x] nested active handle 不误清空外层 operation guard
- [x] 固定回归不回退

## Step 4.5：补齐 Step 3/4 的真实并发上下文语义

**状态：** 已完成
**对应 analysis：** G2、G3、G11、G14、G17
**目标摘要：** 消除 Step 3/4 中仍依赖 `EXT4_RS_RUNTIME_LOCK` 兜底的 single-slot current handle / current operation，修复 JBD2 overlay read 独占锁瓶颈，并明确 credit admission 的 rotate/wait 口径。

### 背景与已知缺口

- Step 3 的 handle id 与 Step 4 的 operation id 已按 key 存储，但当前 handle / 当前 operation 仍分别由 `jbd2_current_handle_id: Arc<Mutex<Option<u64>>>` 与 `LocalOperationAllocGuard.current_operation: Mutex<u64>` 表示。
- 在当前全局串行 fence 下，上述 single-slot 不会被真实并发覆盖；Step 7 缩小 `EXT4_RS_RUNTIME_LOCK` 后，metadata write、data-sync 标记、allocator reserve/contains 可能被路由到其他 operation。
- `JournalRuntime::overlay_metadata_read(&mut self, ...)` 为更新 debug stats 获取可变 runtime，导致 ext4 metadata read 侧无法走共享读路径。
- credit admission 原先只覆盖 idle running transaction rotate；active handle 共用 running TX 时会继续累加 credit，需在 Step 4.5 补上 active over-soft-limit rotate 或等价 backpressure。

### 改动概要

- 移除 Asterinas ext4 集成层的 `jbd2_current_handle_id` single-slot；`JournalIoBridge` 不再从全局 current handle 读取 metadata 归属。
- 新增 `JournalOperationMetadataWriter`，`run_journaled_ext4()` 会为每个 JBD2 handle 构造固定 handle id 的 metadata writer，并把 ext4_rs 操作运行在带上下文的 `Ext4` 视图上。
- 移除 `LocalOperationAllocGuard.current_operation` single-slot；guard 内部只保存 `operation_id -> allocated block set`，新增显式 `reserve_block_for_operation()` / `contains_block_for_operation()` / `clear_operation()` API。
- 新增 ext4_rs `OperationScopedAllocGuard`，`run_ext4*` / `run_journaled_ext4()` 为每次 operation 注入固定 operation id 的 alloc guard wrapper。
- 清理 `run_ext4*` / `run_journaled_ext4()` 中 begin 后立即 `clear_current_operation()` 的 lifecycle 反模式；operation 数据只在 `finish_alloc_operation()` 时按 id 清理。
- `JournalRuntime::overlay_metadata_read()` 改为 `&self`；`overlay_reads` / `overlay_hits` 从普通 debug stats 字段迁出为 `AtomicU64`，Asterinas bridge 侧 `jbd2_runtime` 从 `Mutex` 改为 `RwMutex`，overlay read 使用 read guard。
- direct write prepare 路径不再调用 `mark_current_jbd2_handle_requires_data_sync()`；改为以 `JournaledOp::Write { len }` 启动 handle，由 `start_jbd2_handle()` 按 handle id 标记 data-sync。
- 新增 alloc guard 交错 operation 单测，覆盖两个 operation 交错 reserve/contains 不串扰。
- 新增 scoped guard nested 单测，覆盖内层 scoped operation `clear_current_operation()` 不会清空外层 operation guard 状态。
- Step 0/3/4 baseline 注释已同步，明确其不是拆锁后的真实并发证明。
- credit admission 的 soft-limit 判断不再要求 running TX idle；当 active running TX 已记录 metadata 且新 handle 会超过 soft limit 时，先 rotate 到 `prev_running`，新 handle 进入新 running TX。

### 涉及文件

- `asterinas/kernel/src/fs/ext4/fs.rs`
- `asterinas/kernel/libs/ext4_rs/src/ext4_defs/block.rs`
- `asterinas/kernel/libs/ext4_rs/src/ext4_defs/ext4.rs`
- `asterinas/kernel/libs/ext4_rs/src/ext4_impls/alloc_guard.rs`
- `asterinas/kernel/libs/ext4_rs/src/ext4_impls/balloc.rs`
- `asterinas/kernel/libs/ext4_rs/src/ext4_impls/jbd2/journal.rs`
- `asterinas/kernel/libs/ext4_rs/src/ext4_impls/jbd2/transaction.rs`
- `feature_jbd2_phase2_plan.md`
- `feature_jbd2_phase2_milestone.md`
- `feature_jbd2_phase2_lock_order.md`
- `asterinas/docs/feature_jbd2_phase2_plan.md`
- `asterinas/docs/feature_jbd2_phase2_milestone.md`
- `asterinas/docs/feature_jbd2_phase2_lock_order.md`

### 功能回归

| 测试项 | 结果 |
|--------|------|
| `cargo check -p ext4_rs` | PASS（warning only：`error_in_core` stable） |
| `cargo test -p ext4_rs --lib` | PASS（28/28，新增 `local_guard_interleaved_operations_do_not_share_current_slot`、`credit_admission_rotates_active_transaction_before_overflow`、`scoped_guard_nested_clear_preserves_outer_operation`） |
| `VDSO_LIBRARY_DIR=/home/lby/os_com_codex/asterinas/.local/linux_vdso CARGO_TARGET_DIR=/tmp/os_com_codex_kernel_target cargo check -p aster-kernel --target x86_64-unknown-none` | PASS |
| handle/operation 交错单测 | PASS（operation/scoped wrapper 交错已覆盖；handle 归属通过 scoped metadata writer + 既有 JBD2 handle id 单测覆盖） |
| nested guard preserve 单测 | PASS（新增 `scoped_guard_nested_clear_preserves_outer_operation`） |
| overlay read-side lock/统计单测或扫描 | PASS（代码扫描：`overlay_metadata_read(&mut`、`jbd2_runtime.lock()`、`jbd2_current_handle_id` 无命中；runtime 使用 `RwMutex` read/write） |
| credit admission active-handle over-soft-limit 单测或文档断言 | PASS（新增 `credit_admission_rotates_active_transaction_before_overflow`；active old TX 留在 `prev_running`，new handle 使用新 TX） |
| Phase 2 并发 baseline | PASS（5/5，`workers=4 rounds=8 seed=1`，日志：`asterinas/benchmark/logs/jbd_phase2_concurrency_20260425_054223.log`，严格关键词扫描为空） |
| `phase3_base_guard` | PASS（100.00%，日志：`asterinas/benchmark/logs/phase3_base_guard_20260425_055334.log`，严格关键词扫描为空） |
| `phase4_good` | PASS（100.00%，日志：`asterinas/benchmark/logs/phase4_good_20260425_055334.log`，严格关键词扫描为空） |
| `phase6_good` | PASS（100.00%，日志：`asterinas/benchmark/logs/phase6_good_20260425_061522.log`，严格关键词扫描为空） |
| `jbd_phase1` | PASS（12/12，日志：`asterinas/benchmark/logs/jbd_phase1_20260425_063028.log`，严格关键词扫描为空） |
| crash matrix | PASS（27/27，summary：`asterinas/benchmark/logs/crash/phase4_part3_crash_summary_20260425_064709.tsv`，verify 日志严格关键词扫描为空） |

### 验收项

- [x] `jbd2_current_handle_id` 不再作为跨 operation 的全局 single-slot current handle source of truth
- [x] metadata write 与 data-sync 标记不再依赖全局 current handle slot
- [x] `LocalOperationAllocGuard.current_operation` 不再作为跨 operation 的全局 single-slot current operation source of truth
- [x] allocator reserve/contains 在交错 operation 下不串扰
- [x] nested active handle / nested run_ext4 不误清空外层 operation guard
- [x] `run_ext4*` / `run_journaled_ext4` 不再存在 begin 后立即 clear 的 lifecycle 反模式
- [x] `overlay_metadata_read()` 为共享读接口，debug stats 不迫使读路径获取 runtime 写锁
- [x] credit admission 的 rotate/wait/临时放行边界被代码或文档明确覆盖
- [x] Step 0/3/4 baseline 已标注为全局串行 fence 下的不回退基线
- [x] 固定回归不回退

## Step 5：per-inode / per-directory correctness 锁

**状态：** 待开始
**对应 analysis：** G5、G6、G7、G8、G12、G13
**目标摘要：** 保守保护同 inode 写侧、目录 mutation、buffered 直通路径、direct read cache、fsync 与 orphan inode 语义。
**前置条件：** Step 4.5 已验收通过。

### 改动概要

- 待记录。

### 涉及文件

- 待记录。

### 功能回归

| 测试项 | 结果 |
|--------|------|
| multi-file read/write verify | 待运行 |
| create/unlink churn | 待运行 |
| rename churn | 待运行 |
| write/truncate/fsync | 待运行 |

### 验收项

- [ ] 同 inode write/truncate 串行或有等价保护
- [ ] Step 5 采用保守 read 同步，mapping generation 留到 Step 8
- [ ] rename 多锁顺序固定
- [ ] Phase 2 明确不接入 ext4 PageCache，buffered 路径仍直通 fs 层
- [ ] direct read cache 写侧失效可靠
- [ ] ordered-mode transaction 级 dirty data drain 已验证
- [ ] fsync group commit 语义已写入测试预期
- [ ] unlink-while-open / orphan inode 策略已验证

## Step 6：allocator 与 block group 并发协议

**状态：** 待开始
**对应 analysis：** G4
**目标摘要：** 支持不同 inode 并发分配块，bitmap/counter/extent/JBD2 状态一致。

### 改动概要

- 待记录。

### 涉及文件

- 待记录。

### 功能回归

| 测试项 | 结果 |
|--------|------|
| 并发大文件写 | 待运行 |
| crash + e2fsck | 待运行 |
| 固定回归集 | 待运行 |

### 验收项

- [ ] 同 block group 分配有互斥协议
- [ ] 多 block group 操作按 group number 升序取锁
- [ ] 不同 block group 可并行
- [ ] crash 后 bitmap/counter/extent 一致

## Step 7：逐步缩小 `EXT4_RS_RUNTIME_LOCK`、`inner: Mutex<Ext4>` 与全局串行路径

**状态：** 待开始
**对应 analysis：** G1-G10
**目标摘要：** 在 Step 3/4/5/6 的 correctness 保护到位后，把多文件读写从同 fs 串行推进到真实并发。

### 改动概要

- 待记录。

### 涉及文件

- 待记录。

### 功能回归

| 测试项 | 结果 |
|--------|------|
| Phase 2 并发测试 | 待运行 |
| 固定回归集 | 待运行 |
| crash matrix | 待运行 |

### 验收项

- [ ] 多文件并发读写真正绕开单一全局串行瓶颈
- [ ] `sync_runtime_block_size()` / `set_runtime_block_size()` 真实调用点已被 Step 2 替换或证明无竞态
- [ ] 剩余 `inner` 依赖原因已记录
- [ ] 若出现死锁或数据错误，可回退到上一层保守锁范围

## Step 8：性能恢复与优化

**状态：** 待开始
**对应 analysis：** G7、G9
**目标摘要：** 在并发 correctness 稳定后恢复/提升 fio 与并发 workload 性能。

### 改动概要

- 待记录。

### 涉及文件

- 待记录。

### 性能结果

| 测试项 | Asterinas | Linux | 比值 | 结论 |
|--------|----------:|------:|-----:|------|
| fio read | 待运行 | 待运行 | 待运行 | 待记录 |
| fio write | 待运行 | 待运行 | 待运行 | 待记录 |
| 并发 workload | 待运行 | 待运行 | 待运行 | 待记录 |

### 验收项

- [ ] fio read >= 90%
- [ ] fio write 目标 >= 90%
- [ ] 并发 workload 相比全局串行模型有明确提升

## Step 9：文档、报告与最终验收

**状态：** 待开始
**目标摘要：** 汇总 Phase 2 证据，更新 README、benchmark、环境文档与赛题报告材料。

### 改动概要

- 待记录。

### 涉及文件

- 待记录。

### 验收项

- [ ] Phase 2 milestone 完整
- [ ] README 复现说明更新
- [ ] 赛题优秀档剩余项有测试证据

## 变更日志

| 日期 | 变更 | 负责人 | 备注 |
|------|------|--------|------|
| 2026-04-24 | 创建 Phase 2 milestone 模板 | Codex | 依据 Phase 1 模板与赛题优秀档要求 |
| 2026-04-24 | 建立 Step 0 并发测试资产与 `jbd_phase2_concurrency` runner mode | Codex | host smoke 与 Asterinas baseline 均已通过 |
| 2026-04-24 | 完成 Step 0 固定回归 | Codex | phase3/phase4/phase6/jbd_phase1/crash 均不回退，严格扫描为空 |
| 2026-04-24 | 完成 Step 1 锁顺序与低噪声观测点 | Codex | JBD2 runtime、alloc guard、EXT4_RS_RUNTIME_LOCK 统计已接入，Phase 2 smoke 通过 |
| 2026-04-24 | 完成 Step 2 `runtime_block_size` 显式化 | Codex | 全局 block size 状态已移除，phase3/phase4/phase6/jbd_phase1/crash/phase2 baseline 均不回退 |
| 2026-04-25 | 完成 Step 3 JBD2 handle-local operation context | Codex | handle id、显式 metadata context、按 handle id data-sync、credit admission 已接入，固定回归不回退 |
| 2026-04-25 | 完成 Step 4 operation allocated block guard 本地化 | Codex | 全局 `OP_ALLOCATED_BLOCKS` 已移除，handle/operation-local guard 已接入，phase2/phase3/phase4/phase6/jbd_phase1/crash 均不回退 |
| 2026-04-25 | 新增 Step 4.5 修补计划与 milestone 模板 | Codex | 暂缓 Step 5，先补齐 Step 3/4 的真实并发上下文语义 |
| 2026-04-25 | Step 4.5 P0 代码修补 | Codex | 去除 `jbd2_current_handle_id` 与 `LocalOperationAllocGuard.current_operation` single-slot；overlay read 改共享读；ext4_rs 与 aster-kernel check 通过 |
| 2026-04-25 | Step 4.5 active credit admission 修补 | Codex | over-soft-limit 时 active running TX 可 rotate 到 `prev_running`，新增 active-handle credit 单测，`cargo test -p ext4_rs --lib` 27/27 |
| 2026-04-25 | Step 4.5 Docker 固定回归 | Codex | Phase 2 baseline、phase3、phase4、phase6、jbd_phase1、crash matrix 均通过；严格关键词扫描为空 |
| 2026-04-25 | 完成 Step 4.5 nested wrapper 验证 | Codex | `OperationScopedAllocGuard` 下沉到 ext4_rs 并新增 nested scoped guard 单测，`cargo test -p ext4_rs --lib` 28/28 |
