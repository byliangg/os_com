# Asterinas ext4 JBD2 功能实现 Phase 1 — 计划

首次更新时间：2026-04-17（Asia/Shanghai）

## 目标

实现 **block-level JBD2（ordered mode）** 替代现有 CrashJournal，对应优秀档要求：

- 日志刷盘、事务管理、全量崩溃恢复；
- xfstests `jbd_phase1` 子集通过率 ≥ 95%；
- 多场景崩溃恢复测试，数据一致性 100%。

并发读写属 Phase 2 范围，Phase 1 保留 `EXT4_RS_RUNTIME_LOCK` 串行化。

## 设计原则

1. **on-disk 格式与 Linux JBD2 兼容**：`e2fsck` 能识别并恢复我们写出的 journal；
2. **复用 mkfs 已创建的 journal inode**（默认 inode 8），不自定义存储位置；
3. **ordered mode**：metadata 走 journal，data block 先写原位置再提交对应 metadata transaction；
4. **最小改动原则**：ext4_rs 内部 metadata 写路径保留接口不变，通过"拦截层"改写为 journal write；
5. **旧 CrashJournal 在 Phase 1 完成后移除**，避免双轨维护。

## 验收口径

- 文档中的 `PASS` / `通过` 默认指两层条件同时满足：
  - runner / benchmark 返回成功（如 `rc=0`、通过率达标）；
  - 原始内核日志中无新的 ext4/JBD2 相关 panic、assert、`ext4 write_at failed`、`logical block not mapped`、`Extentindex not found` 等核心错误。
- 若仅满足 runner 成功，但原始日志仍含上述错误，应记为“runner 通过，但未达到零错误真通过”，不能作为进入下一 step 的充分依据。
- 对 `phase3_base_guard`、`phase4_good`、`phase6_good`、`crash_only` 这类固定回归集，文档中的 `PASS` 还必须显式写出 `几/几`，并与当前功能基线一致：
  - `phase3_base_guard = 10/10`
  - `phase4_good = 12/12`
  - `phase6_good = 25/25`
  - `crash_only = 6/6`
- 若只满足百分比达标、但 `几/几` 少于上述基线，必须记为“部分达标”或“runner 通过但未回到基线”，不能记为完整 `PASS`。

---

## 分步方案

### Step 1：JBD2 on-disk 数据结构与 journal 设备初始化

**状态：** 已完成
**目标：** 打通 journal 设备层，能读写合法 JBD2 block（journal superblock、descriptor、commit、revoke）
**对应 analysis：** G1
**工作量估计：** 2–3 天

#### 1.1 方案

1. 在 `asterinas/kernel/libs/ext4_rs/src/ext4_defs/` 下新增 `jbd2.rs`，定义：
   - `JournalHeader`（magic `0xc03b3998`、blocktype、sequence）
   - `JournalSuperblock`（v2：`s_blocksize`、`s_maxlen`、`s_first`、`s_sequence`、`s_start`、`s_feature_*`、`s_uuid`）
   - `BlockTag` / `BlockTag3`（journal descriptor block 中的 tag）
   - `CommitBlock`（含 commit time、checksum）
   - `RevokeBlock`（数组形式的 block number）
   - 常量：`JBD2_DESCRIPTOR_BLOCK`、`JBD2_COMMIT_BLOCK`、`JBD2_SUPERBLOCK_V2`、`JBD2_REVOKE_BLOCK`、各 feature flag。
2. 新增 `asterinas/kernel/libs/ext4_rs/src/ext4_impls/jbd2/` 模块：
   - `device.rs`：根据 superblock 的 `journal_inode_number` 读 journal inode，用 extent 映射展开为物理 block 列表，封装"journal 内逻辑块 ↔ 物理块"的映射；
   - `superblock.rs`：journal superblock 的读 / 写 / 校验 / 更新 `s_start`、`s_sequence` 的接口；
   - `space.rs`：ring buffer 空间管理（head、tail、free blocks、wrap 判定）。
3. Mount 时检查 superblock `features_compat & HAS_JOURNAL`：
   - 若置位，加载 journal，打印 journal inode、size、`s_sequence`、`s_start` 等信息；
   - 若未置位，降级走无 journal 路径（保留现有行为，便于旧镜像调试）。

#### 1.2 验收标准

- mkfs.ext4 生成的镜像在 Asterinas 下 mount 时能读出 journal superblock，日志字段与 `dumpe2fs -h` 结果一致；
- 向 journal 写一个手工构造的 descriptor + commit block 对，`e2fsck -fy` 能识别为合法 journal（可能为空事务）；
- 现有 crash_only / phase3 / phase4 / phase6 测试全部不回归。

---

### Step 2：事务管理与 metadata block-level 日志写入

**状态：** 已完成
**目标：** 建立 handle / transaction 抽象，所有 metadata 写入改为走 journal
**对应 analysis：** G2
**工作量估计：** 4–5 天

#### 2.1 方案

1. 在 `ext4_impls/jbd2/` 下新增：
   - `handle.rs`：`JournalHandle { transaction_id, reserved_blocks, modified_blocks }`，提供 `start()` / `get_write_access(block)` / `dirty_metadata(block)` / `stop()`；
   - `transaction.rs`：`Transaction { tid, state, buffers: BTreeMap<block_nr, Buffer>, handle_count }`，状态机 `Running → Locked → Flush → Commit → Checkpoint`；
   - `journal.rs`：全局 `Journal { running: Option<Arc<Transaction>>, committing: Option<Arc<Transaction>>, checkpoint_list: VecDeque<Arc<Transaction>> }`。
2. 在 ext4_rs 侧引入"metadata 写入拦截层"：
   - 新增 trait `MetadataWriter`，代替直接调用 `block_device.write_offset` 的 metadata 路径；
   - 在 fs.rs 层把 `BlockDevice` 包装为 `JournaledBlockDevice`，内部根据当前 handle 决定是走 journal 还是直写（无事务 = 直写，兼容 journal 关闭）；
   - 改写 [analysis §2.2 列出的所有 metadata 写点]：inode.rs:470、block_group.rs:187、balloc.rs:640、block.rs:99、super_block.rs:220、file.rs:836/876。
3. 在 fs.rs 所有高层操作（create / mkdir / unlink / rmdir / rename / write / truncate / ...）用 `JournalHandle::start(reserved_credits)` 包裹；保留 `run_journaled()` 函数签名，内部改为基于 handle 实现。
4. **ordered mode 处理**：
   - 区分 data block 与 metadata block；
   - data block 依旧直写 `block_device.write_offset`；
   - 在 `handle.stop()` 前，通过 `block_device.sync()` 确保 data 落盘后再让 transaction 进入 commit。

#### 2.2 验收标准

- 所有 metadata 写入路径都通过 handle 进行，可以通过日志统计到每个操作的修改 block 数；
- 关闭 journal（通过 kernel cmdline 开关）时代码仍能运行，等价于直写；
- crash_only 的 3 个场景在 journal 开启模式下仍 PASS；
- 功能回归：`phase3_base_guard = 10/10`、`phase4_good = 12/12`、`phase6_good = 25/25`、`crash_only = 6/6`，且日志无核心错误。

---


### Step 2.5：`generic/013` 超时修复（并发 commit 触发机制）

**状态：** 已完成
**目标：** 让 `generic/013`（fsstress 20 进程 × 1000 ops）在 600s 内完成，恢复固定回归集到基线
**对应 analysis：** G2、G3
**工作量估计：** 2–3 天

#### 2.5.1 背景与已完成工作

Step 2 引入 JBD2 journal 后，`generic/013` 的失败模式经历了两个阶段：

**阶段一（已解决）：映射一致性错误**
- `logical block not mapped` / `ext4 write_at failed` / `root reload mismatch` 等 extent tree 错误
- 根因：O_DIRECT 写路径缺少 JBD2 handle、extent leaf sync 后回退、stale inode 读取等
- 已通过多轮修复压到零：overlay 基线读取修复、O_DIRECT handle 补齐、get_pblock_idx stale-tree 重试、ext_remove_leaf/ext_correct_indexes 修复等

**阶段二（已解决）：超时（rc=124）**
- `generic/013` 一度不再有 extent 映射错误，而是在 600s 内无法完成 20,000 ops（~12,000 ops 完成时超时）
- Fix 1（已应用）：移除每次 commit 前的 `data_sync_required` BioType::Flush（~50ms/op），确认有效（block profile 无 flush-bio）
- Fix 2（已证伪）：batch commit（JOURNAL_COMMIT_BATCH_BLOCKS=64）——20 个并发进程使 `handle_count` 几乎永远 > 0，`commit_ready()` 永远为 false，导致 journal commit 完全停止。**已回滚。**

#### 2.5.2 当前根因

Block profile 分析：两轮测试（Fix 1 only、Fix 1+2）均显示 **零 write-bio**，说明 journal commit 从未发生。

根因：
1. `commit_ready()` 要求 `handle_count == 0`。20 个并发进程轮流竞争全局锁，stop_handle 刚把 count 降到 0，下一个进程立即 start_handle 把 count 升回 1，commit_ready() 永远为 false。
2. 全局锁 `EXT4_RS_RUNTIME_LOCK` 把 20 个进程完全串行化。每个操作需要多次 metadata 磁盘读（Block::load → bio read）。串行化 × 读延迟导致吞吐不足。

#### 2.5.3 最终状态（2026-04-24）

已通过 transaction rotation、checkpoint 批量化、regular-file `fsync` 轻量化、`xfs_io` helper 提速以及 allocator 快速检查收口。最终整体回归中 `generic/013` 已在 `phase3_base_guard`、`phase4_good`、`phase6_good` 均 `rc=0`，严格关键词扫描为空。

实现要点：

1. 引入”transaction rotation”：当 modified_block_count 超过阈值（如 64 blocks）时，调用新函数 `rotate_running_transaction()`：
   - 把当前 running transaction 切换到 committing（即使 handle_count > 0，剩余 handle 继续在 committing 上完成）
   - 创建新的空 running transaction，后续新 handle 开在新 transaction 上
   - 等所有 committing transaction 的 handle 退出后（handle_count 降到 0）再触发 commit
2. 保留 Fix 1（无 data_sync flush）
3. `fsync` 路径优先保证“commit ready transaction + device sync”，避免 regular file `fsync` 退化成全文件系统级 checkpoint sweep；全局 `sync()` 仍保留 `flush_pending_jbd2_transactions()` 语义。

#### 2.5.4 验收标准

- `generic/013` rc=0（在 600s 内完成），block profile 出现正常的 write-bio（journal 实际写盘）
- `generic/014` 保持通过
- 固定回归集恢复到基线：
  - `phase3_base_guard = 10/10`
  - `phase4_good = 12/12`
  - `phase6_good = 25/25`
  - `crash_only = 6/6`
- 日志无 ext4/JBD2 核心错误

---

### Step 3：Commit 流程与 checkpoint

**状态：** 已完成
**目标：** 实现标准 JBD2 commit 序列，以及让 journal 可持续运行的 checkpoint
**对应 analysis：** G2、G3
**工作量估计：** 3–4 天

#### 3.1 方案

1. **Commit 序列**（在 `journal.rs::commit_transaction()` 中实现）：
   - 步骤 1：切换 running → committing，新操作进入下一个 transaction；
   - 步骤 2：ordered 模式下，等待所有 data block 落盘（`block_device.sync()`）；
   - 步骤 3：为 committing transaction 的每个 modified metadata block 分配 journal 槽位，生成 descriptor block（填 tag 数组 + UUID），落盘；
   - 步骤 4：按 descriptor 顺序写入 metadata block 到 journal；
   - 步骤 5：写 commit block（含 `s_sequence`、commit time、checksum），落盘；
   - 步骤 6：更新 journal superblock 的 `s_sequence`，transaction 进入 checkpoint 队列。
2. **Commit 触发点**：
   - 每次 `handle.stop()` 后检查是否达到 commit 阈值（修改块数、时间、显式 sync）；
   - 显式 `fsync` / `sync` 强制 commit；
   - Phase 1 不做后台 commit 线程，使用同步 commit 简化模型。
3. **Checkpoint**：
   - 新增 `checkpoint::checkpoint_transaction()`：从 checkpoint 队列头取 transaction，将其 modified metadata block 按原位置写回 `block_device.write_offset`，完成后推进 journal `s_start`（tail），释放 journal 空间；
   - 触发条件：journal 剩余空间低于阈值（例如 < 25%）、显式 sync、unmount。
4. **Unmount 流程**：flush running → commit → checkpoint 全部完成 → 写 journal superblock（`s_start = 0` 表示无未完成事务） → 关 journal。

#### 3.2 验收标准

- 大量小操作循环跑不会因 journal 满而 stall（checkpoint 工作正常）；
- Unmount 后再 mount，`s_start` / `s_sequence` 状态正确，无 spurious replay；
- 修改前后用 `e2fsck -n` 离线检查镜像：无 "Journal has been aborted" / "Journal ... inconsistent" 报错；
- 功能回归：`phase3_base_guard = 10/10`、`phase4_good = 12/12`、`phase6_good = 25/25`、`crash_only = 6/6`，且日志无核心错误。

---

### Step 4：崩溃恢复（Replay）与测试基线

**状态：** 已完成
**目标：** 实现标准 JBD2 三遍扫描 recovery；建立 xfstests jbd_phase1 列表与多场景 crash 测试
**对应 analysis：** G4、G5、G6
**工作量估计：** 4–5 天

#### 4.1 方案

1. **标准 JBD2 recovery**（替换 `replay_mount_crash_journal`）：
   - `recovery::recover()` 实现三遍扫描：
     - PASS_SCAN：从 `s_start` 出发读 descriptor / commit block，找到最后一个合法 commit 的 sequence；
     - PASS_REVOKE：重扫同一范围，收集所有 revoke block 中的 block number，组成 revoke table；
     - PASS_REPLAY：重扫同一范围，对每个 descriptor 覆盖的 metadata block，若不在 revoke table 中则按原位置写回；
   - 完成后写 journal superblock：`s_start = 0`、`s_sequence = 最后 sequence + 1`。
2. **与旧 CrashJournal 的替换**：
   - 删除 `CrashJournalOp`、`run_journaled`、`replay_mount_crash_journal` 及相关常量（`CRASH_JOURNAL_OFFSET` 等）；
   - 保留 kernel cmdline 参数 `ext4fs.replay_hold` 用于 crash 测试注入点，但改为在 commit block 写入前 spin；
   - `ext4fs.crash_journal` 参数废弃，改为默认根据 superblock `HAS_JOURNAL` feature 启用。
3. **xfstests jbd_phase1 列表**（已建立初版，跑通后迭代扩展）：
   - 候选列表：[asterinas/test/initramfs/src/syscall/xfstests/testcases/jbd_phase1.list](asterinas/test/initramfs/src/syscall/xfstests/testcases/jbd_phase1.list)
   - 静态排除：[asterinas/test/initramfs/src/syscall/xfstests/blocked/jbd_phase1_excluded.tsv](asterinas/test/initramfs/src/syscall/xfstests/blocked/jbd_phase1_excluded.tsv)
   - runner 模式：`XFSTESTS_MODE=jbd_phase1`（已在 [run_xfstests_test.sh](asterinas/test/initramfs/src/syscall/xfstests/run_xfstests_test.sh) 注册）
   - 初版范围：`generic/068,076,083,192,530` + `ext4/021,022,030,031,032,033,045`
   - 已排除：所有需要 `dm-log-writes` / `dm-flakey` 的 crash 注入测试（Asterinas 暂无 device-mapper 支持），交由自研 crash 场景覆盖
   - 目标通过率 ≥ 95%
4. **多场景 crash 测试扩展**（在 `run_ext4_crash_test.sh` 中新增，建议覆盖）：
   - `large_write`：单文件 4 MB 顺序写，crash 点在 commit block 前；
   - `fsync_durability`：`write + fsync + crash`，验证 fsync 后数据必然存在；
   - `multi_file_create`：并发 8 个文件 create，crash 后文件集合合法；
   - `dir_tree_churn`：嵌套目录创建 + 删除，crash 后无孤儿 inode；
   - `rename_across_dir`：跨目录 rename，crash 后源/目标状态合法；
   - `truncate_shrink`：大文件 truncate 到更小，crash 后 size 与 extent 一致；
   - `append_concurrent`：多个 writer 追加同一文件（即使 Phase 1 串行化，也要证明 journal 表现正确）；
   - 保留原有 `create_write`、`rename`、`truncate_append`。

#### 4.1.1 当前已开始的最小闭环（2026-04-21）

1. 在 `ext4_rs/ext4_impls/jbd2/` 中新增 recovery 模块，先实现“descriptor/data/commit 扫描 + metadata replay + journal 清空”第一版骨架。
2. mount 入口在 `initialize_jbd2_journal()` 之后新增标准 JBD2 replay 尝试：
   - 若 journal `s_start != 0` 或 ext4 superblock 仍带 `needs_recovery`，则优先执行 JBD2 recovery；
   - recovery 成功后清除 ext4 superblock 的 `needs_recovery`，并重置 runtime 的 journal sequence。
3. 当前版本优先覆盖我们自己已经写出的 transaction 格式：
   - descriptor/data/commit 顺序事务；
   - metadata block replay；
   - revoke 扫描骨架；
   - journal superblock `s_start/head/sequence` 收口。
4. 2026-04-24 已完成 Step 4 收口：
   - 真实 crash 场景验证已扩展到 9 个默认 JBD2 场景；
   - `jbd_phase1` 整组达到 `6 PASS / 0 FAIL / 6 NOTRUN`、`pass_rate=100.00%`；
   - old CrashJournal 路径已移除；
   - dirty journal recovery 已通过 `jbd2_probe recover + e2fsck -fn` 闭环验证。

#### 4.1.2 2026-04-21 当天新增验证结论

1. **crash 注入点已从旧 CrashJournal 迁到新 JBD2 commit 完成后**：
   - `replay_hold` 不再隐式启用旧 `crash_journal`；
   - `finish_jbd2_handle()` 在命中 `replay_hold_op` 时会强制触发当前 ready transaction 的 commit；
   - prepare 日志现已能稳定看到 `replay hold point reached for op=... after JBD2 commit`。
2. **最小 crash 闭环已实测通过一轮**：
   - `create_write` / `rename` / `truncate_append` 在 `CRASH_ROUNDS=1` 下均 PASS；
   - 这组结果说明“commit 后杀机 + mount 后 verify”链路已打通，且 verify 不再依赖旧 CrashJournal replay 日志。
3. **`jbd_phase1` 首轮已开始跑出真实结果**：
   - 资产侧已补齐 `prepare_phase4_part3_initramfs.sh`，会把 `jbd_phase1.list` 和 `jbd_phase1_excluded.tsv` 一并注入 initramfs；
   - 当前有效样本已看到：`generic/068/076/083/530 = PASS`，`generic/192 = FAIL`，`ext4/021 = FAIL (Found no journal)`；
   - 说明 `jbd_phase1` 入口已打通，但 ext4 日志可见性/兼容性仍有残留缺口，需要继续收口。

#### 4.1.3 2026-04-21 晚间定点收口

1. **`ext4/021` 已从环境/兼容性失败修复到定点 PASS**：
   - 这轮修补补齐了 initramfs 中的 `dumpe2fs` / `xfs_io` / `umount` 兼容层，避免测试把“工具链缺口”误判成“文件系统没有 journal”；
   - 定点结果见 `ext4_021_20260421_101142.log`，当前可将 `ext4/021` 从 Step 4 主 blocker 中移出。
2. **`generic/192` 已完成第一轮有效收敛**：
   - trace 结果表明，原始失败同时包含“按设备名 unmount 失败”“clean remount 后文件消失”“atime 不更新”三类问题；
   - 通过在 VFS `Path::unmount()` 前增加 `sync()`，clean remount 后文件丢失问题已修复；
   - 随后再通过 `RealTimeClock + run_inode_metadata_update()` 修掉 atime 更新时间与 JBD2 overlay 视图错位后，最新定点复跑 `generic_192_jbd_rerun_20260421_142144.log` 已显示 `generic/192 rc=0`、`jbd_phase1 1/1 PASS`。
3. **由此调整 Step 4 的近端优先级**：
   - `ext4/021` 已收口；
   - `generic/192` 也已收口；
   - 下一轮优先级转为整组 `jbd_phase1` 复跑，并提炼新的真实 blocker。

#### 4.1.6 2026-04-23 phase4_good 剩余问题收敛

1. `phase4_good` 新一轮回归表明，`generic/052` / `generic/054` 的失败主要来自 xfstests ext4 shutdown fallback 与 `dumpe2fs` logstate probe 不一致，而不是新的 JBD2 replay correctness 回退。
2. 已在 [run_xfstests_test.sh](asterinas/test/initramfs/src/syscall/xfstests/run_xfstests_test.sh) 修正 `dumpe2fs` shim：当 `xfstests_ext4_needs_recovery` one-shot marker 存在时，优先返回一次带 `needs_recovery` 的 header，再清掉 marker。
3. 单例验证已通过：
   - `generic/052`：`phase4_good_20260423_010314.log`
   - `generic/054`：`phase4_good_20260423_010153.log`
4. 目前 `phase4_good` 的近端主 blocker 已收敛到 `generic/047`。
5. 对 `generic/047` 的第一轮判断是：regular file 的 `sync_all` / `sync_data` 不应直接落到全局 `Ext4Fs::sync()`。当前已新增 `fsync_regular_file()` 轻量路径，只做：
   - commit ready JBD2 transactions；
   - 不主动触发全文件系统级 checkpoint sweep。
6. 随后的 `warn` 定点日志进一步表明，xfstests 的 `xfs_io` fallback 在 `fsync/fdatasync/syncfs` 上实际上落到了全局 `sync`。这一层已通过新增 `fsync_file` helper 修正为真正的 file-level fsync。
7. helper 接入后，`generic/047` 暴露出新的剩余问题：如果 regular-file `fsync` 只 commit 不 checkpoint，committed transaction 会在内存中持续堆积，并在约 `150s` 时触发 heap allocation error。
8. 当前已在 `fsync_regular_file()` 中增加基于 checkpoint depth 的周期性批量出清（`REGULAR_FILE_FSYNC_CHECKPOINT_DEPTH=8`）。最新定点日志 `phase4_good_20260423_020500.log` 已确认：
   - `tid=8` 与 `tid=16` 后出现 `batch checkpointed 8 transactions with single sync`；
   - 越过前一轮 OOM 的 `150s` 门槛后仍继续推进到 `tid=21`；
   - 未再出现 `Failed to allocate a large slot / Heap allocation error`。
9. 最后一层根因出在 `xfs_io` shim 的 `pwrite` fallback：旧实现使用 `awk | dd bs=1` 的壳层模拟，`generic/047` 的 `999 * (pwrite + fsync)` 会被用户态辅助程序开销严重拖慢。当前已将 [fsync_file.c](/home/lby/os_com_codex/asterinas/test/initramfs/src/syscall/xfstests/fsync_file.c) 扩展为通用 file I/O helper，并让 `run_xfstests_test.sh` 在 `pwrite` 上优先走原生 helper。
10. 这条修复后，单例长跑 [phase4_good_20260423_032231.log](/home/lby/os_com_codex/asterinas/benchmark/logs/phase4_good_20260423_032231.log) 已确认 `generic/047 rc=0`，且错误关键词扫描为空；说明此前 `phase4_good` 的近端 blocker 已被解除。
11. 随后的整组补跑 [phase4_good_20260423_033846.log](/home/lby/os_com_codex/asterinas/benchmark/logs/phase4_good_20260423_033846.log) 已完整收口为 `phase4_good 12 PASS / 0 FAIL / 6 NOTRUN / 22 STATIC_BLOCKED`，runner 口径 `pass_rate=100.00%`；同时对该日志执行 `Extentindex not found / ext4 write_at failed / logical block not mapped / Heap allocation error / Failed to allocate a large slot / panicked / ERROR:` 严格关键词扫描为空，说明这轮 helper 与 regular-file fsync 调整没有引入新的核心回退。
12. 紧接着的 `phase3_base` 补跑 [phase3_base_guard_20260423_041603.log](/home/lby/os_com_codex/asterinas/benchmark/logs/phase3_base_guard_20260423_041603.log) 也已完整收口为 `10 PASS / 0 FAIL / 6 NOTRUN / 22 STATIC_BLOCKED`，runner 口径 `pass_rate=100.00%`；其中此前长期敏感的 `generic/013`、`generic/047`、`generic/083` 均为 `rc=0`，且整份日志严格关键词扫描同样为空。
13. 因此，`2026-04-23` 这轮修复的下一步已不再是回头处理 `phase3_base` / `phase4_good`，而是继续推进 `phase6_good` 与 `crash_only`，确认这批 ext4/JBD2 与 xfstests helper 变更在更大回归面上同样稳定。

#### 4.1.5 2026-04-22 ext4/045 超时根因与 O(1) dir 操作修复

**根因**：`ext4/045` 创建 65537 个子目录后，用 `ls | xargs rmdir` 逐一删除。每次 rmdir 调用 `dir_find_entry` 线性扫描父目录的所有 block（O(n) per rmdir），65537 次 = O(n²) 总复杂度。之前的修复只解决了 mkdir 侧（`dir_add_entry_unchecked`），rmdir 侧没有处理。

**修复方案**：目录字节偏移缓存 + O(1) 快速删除路径

1. **`DirEntryCache`**：`entries` 从 `BTreeMap<String, u32>` 改为 `BTreeMap<String, (u32, u64)>`，额外存储每个 entry 在父目录流中的绝对字节偏移。
2. **`load_dir_cache_if_needed`**：改用 `ext4_readdir_with_offsets`（新增），在加载缓存时同时记录字节偏移。
3. **`ext4_mkdir_unchecked_at`**：返回值改为 `(ino, dir_byte_offset)`，`dir_add_entry_unchecked` 返回 `u64`，`link_unchecked` / `create_unchecked` 相应更新。
4. **`dir_add_entry_unchecked`**：返回新 entry 在目录流中的绝对字节偏移（`last_iblock * block_size + within_block_offset`）。
5. **`dir_remove_entry_at_offset`**（新增）：接收绝对字节偏移，计算 `iblock = offset / block_size`，直接加载对应 block，在块内扫描前驱 entry（最多 ~32 项，O(1)），完成 entry 删除。
6. **`ext4_rmdir_at_fast`**（新增）：接收 `(parent_ino, child_ino, dir_byte_offset)`，直接调用 `dir_remove_entry_at_offset`，绕过 `dir_find_entry` 全扫描。
7. **`rmdir_at`**：从缓存取 `(ino, offset)`，当 `offset != u64::MAX` 时走快速路径 `ext4_rmdir_at_fast`，否则退回慢路径 `ext4_rmdir_at`。

**复杂度**：缓存加载后，mkdir O(1)、rmdir O(1)，65537 次操作总复杂度从 O(n²) 降到 O(n)（含一次初始线性加载）。

#### 4.1.4 2026-04-21 深夜定点结论

1. **`generic/192` 的 atime 根因已进一步收敛**：
   - 在 `touch_atime()` 中加入定点探针后，确认 read 路径确实会命中 atime 更新；
   - 当时钟源切到 `RealTimeClock` 后，内核已能拿到正确的 Unix 秒值；
   - 但在旧实现里，`ext4_set_inode_times()` 更新后立刻 `stat()` 仍看到旧 atime，说明问题不再是“时间没算出来”，而是“时间戳更新结果没有立刻穿过 JBD2 metadata overlay 被看见”。
2. **针对性修复已落地**：
   - `now_unix_seconds_u32()` 改为读取 `RealTimeClock`，避免 buffered read 的 atime/mtime/ctime 继续依赖 coarse clock；
   - 新增 `run_inode_metadata_update()`，让 `set_inode_times()` / `set_inode_mode()` / `set_inode_uid()` / `set_inode_gid()` / `set_inode_rdev()` 在 journaling 开启且当前没有活跃 handle 时，也通过匿名 JBD2 handle 执行，而不是裸写 inode table；
   - 这样可以保证 inode metadata 更新与 JBD2 overlay 视图保持一致，不再出现“磁盘块已改、overlay 仍返回旧 inode image”的错位。
3. **修复后的探针结果已经变化**：
   - 定点日志 `generic_192_jbd_atime_trace_20260421_1918.log` 里，已从
     - `post-update ... seen_atime=0`
     变为
     - `post-update ... seen_atime=1776769445`
   - 说明“atime 写入后立刻不可见”这一层已经被修正。
4. **最新收口结果**：
   - 最新定点复跑 `generic_192_jbd_rerun_20260421_142144.log` 中，`generic/192` 已 `rc=0`，并且 `jbd_phase1` 单用例结果为 `1/1 PASS`；
   - 这说明 `RealTimeClock + run_inode_metadata_update()` 这组修复不仅让 atime 写后可见，也足以恢复 xfstests 口径；
   - Step 4 当前不再需要继续深挖 `generic/192`，而应回到整组 `jbd_phase1` 的通过率与剩余失败项收口。
5. **整组 `jbd_phase1` 首轮复跑已给出新的主 blocker**：
   - 完整结果见 `jbd_phase1_rerun_20260421_142457.log`，当前为 `4 PASS / 2 FAIL / 6 NOTRUN`，denominator=6，pass_rate=66.67%；
   - 两个真实失败项已收敛为 `generic/083` 和 `ext4/045`，且都表现为 `timeout 600s`，不再是“找不到 journal”或 “atime 错位”这类 correctness 入口；
   - `generic/530`、`ext4/022/030/031/032/033` 当前则是工具/环境能力缺口导致的 `NOTRUN`，下一步需要区分哪些应补齐运行环境，哪些应进入 `excluded.tsv` 并写清理由。
6. **`generic/083` 与 `ext4/045` 已在干净串行链路上稳定复现**：
   - 定点日志 `generic_083_rerun_serial_20260421_145502.log` 与 `ext4_045_rerun_serial_20260421_150610.log` 均再次得到 `rc=124 / timeout 600s`；
   - 两份 `TIMEOUT FULL LOG TAIL` 都没有出现新的 JBD2/extent correctness 错误，`generic/083` 只停在 fsstress `seed = ...` 之后，`ext4/045` 只停在“开始创建 65537 个子目录”的 banner 之后；
   - 因而当前更像是**吞吐/操作实现覆盖不足导致的长尾超时**，而不是 journaling correctness 回归。
7. **2026-04-22 深夜整组复跑后，近端 blocker 已进一步变化**：
   - 日志 `jbd_phase1_20260422_145005.log` 在 runner 口径下已经达到 `6 PASS / 0 FAIL / 6 NOTRUN`、denominator=`6`、`pass_rate=100.00%`；
   - 其中 `generic/083 rc=0`、`ext4/045 rc=0`，说明前一阶段的 timeout / OOM 主问题确实已经收口；
   - 但同一份日志中，`generic/068` 仍出现大量 `Extentindex not found` / `ext4 write_at failed`；
   - 因此 Step 4 当前不应把注意力继续放在 `generic/083` / `ext4/045`，而应把 `generic/068` 的 extent correctness 残留提升为新的近端修复对象。
8. **2026-04-22 深夜进一步修复后，`generic/068` 已收口**：
   - 根因是 `ext_remove_idx()` 在 root 最后一条 index 被删除时，没有把 `depth=1` 的空 index root 折回空 leaf；
   - 这会让后续对同 inode 的写入在 `find_extent()` 中命中 `Extentindex not found`，即便 runner 最后仍可能 `rc=0`；
   - 修复后，单例日志 `jbd_phase1_20260422_152723.log` 中 `generic/068 rc=0`，并且严格关键词扫描为空；
   - 随后的整组日志 `jbd_phase1_20260422_153027.log` 仍保持 `6 PASS / 0 FAIL / 6 NOTRUN`、`pass_rate=100.00%`，同时不再出现 `Extentindex not found` / `ext4 write_at failed`；
   - 因此 Step 4 当前的 `jbd_phase1` 已从“runner 通过、严格口径未过”推进到“runner 与严格口径同时通过”。

#### 4.2 验收标准

- 标准 JBD2 recovery 能正确处理：
  - `e2fsck -fy` 预先制造的 dirty journal（journal 中有 committed transaction）；
  - 我们自己在 commit 前/中/后注入崩溃的镜像；
- 旧 CrashJournal 代码完全移除后，所有 crash 场景（新旧共 ≥ 8 个）PASS；
- xfstests jbd_phase1 列表通过率 ≥ 95%；
- `phase3_base_guard = 10/10`、`phase4_good = 12/12`、`phase6_good = 25/25`、`crash_only = 6/6`，且日志无核心错误。

---

## 执行顺序

```
Step 1 (基础设施) → Step 2 (事务与拦截) → Step 3 (commit + checkpoint) → Step 4 (recovery + 测试基线)
```

每步完成后立刻跑完整回归，确保不引入退路。

当前优先级调整（2026-04-21）：

```text
Step 1 / 2 / 3 基础能力已基本打通
→ Step 2.5 保留为高优先级残留风险跟踪
→ Step 4 提升为当前主线
→ `generic/013` 继续保留在固定回归集中，待 Step 4 与后续 Phase 2 并发优化共同收口
```

---

## 验证流程

```bash
# 功能回归（每个 Step 都要跑）
PHASE4_DOCKER_MODE=phase6_with_guard ENABLE_KVM=1 NETDEV=tap VHOST=on \
  bash tools/ext4/run_phase4_in_docker.sh

# crash 测试（Step 2 起每步都要跑，Step 4 扩展场景后全跑）
PHASE4_DOCKER_MODE=crash_only ENABLE_KVM=1 NETDEV=tap VHOST=on \
  bash tools/ext4/run_phase4_in_docker.sh

# xfstests jbd_phase1
# 在 initramfs 中设置 XFSTESTS_MODE=jbd_phase1 运行
# 列表：asterinas/test/initramfs/src/syscall/xfstests/testcases/jbd_phase1.list
```

性能基线（防止 JBD2 引入性能回退）：

```bash
BENCH_ENABLE_KVM=1 BENCH_ASTER_NETDEV=tap BENCH_ASTER_VHOST=on \
  bash benchmark/bench_fio.sh  # 参考 benchmark.md
```

要求 fio read/write 相对于 Phase 1 开始前的基线不下降超过 5 个百分点（Phase 1 基线 read 95.79%、write 90.48%，守底线 read ≥ 90%、write ≥ 85%；2026-04-24 收口复跑 read 93.49%、write 87.01%，满足守底）。

---

## 风险与注意事项

1. **ordered mode 的 data 顺序**：data block 必须早于对应 metadata 的 commit block 落盘，否则崩溃后 data 可能未落但 metadata 指向旧/空内容，产生数据泄露。Step 2 的 `handle.stop()` 前必须 sync data。
2. **Checkpoint 与 journal 空间不足**：若 checkpoint 跟不上 commit，journal 会满。Phase 1 先做同步 checkpoint（commit 前检查空间，不够就立即 checkpoint），避免后台线程带来的复杂性。
3. **Revoke 机制不可省略**：若某 block 曾作为 metadata 被日志，后来被释放并重新分配为 data，recovery 不能把旧 metadata 覆盖回 data 位置。Step 2 在 block 释放路径必须生成 revoke record，Step 4 的 recovery 必须先扫 revoke。
4. **sequence 回绕**：`s_sequence` 是 u32，本 Phase 不做回绕处理（初始化时 `s_sequence = 1`，40 亿次 commit 才回绕，远超测试规模）。需在代码里加 `assert` 检测，避免未来踩坑。
5. **旧 CrashJournal 测试兼容**：crash 测试脚本的 `replay_hold` 逻辑要同步迁移到 JBD2，否则 Step 4 的 crash 场景无法复现。
6. **journal inode 大小**：mkfs.ext4 默认 journal 大小 128 MB。Asterinas 虚拟磁盘如较小，mkfs 会缩小 journal；需要确保小 journal（如 8 MB）下 Phase 1 功能仍正确，Step 1 完成后用 `mkfs.ext4 -J size=8` 的镜像做一次验证。
7. **与 Asterinas PageCache 的交互**：PageCache 针对 data block 的缓存不受影响；但 metadata block 目前走的是 ext4_rs 内部缓存（`block.rs` 中的结构），要确保 commit 时读的是"最新脏 metadata"，而不是落盘前的旧版本。Step 2 的 `dirty_metadata` 必须持有 metadata block 的当前内容，不能依赖稍后重读。

---

## 附录：OSTD 构建说明

- 当前 `cargo check -p aster-kernel` 在本工作树下若直接使用宿主机默认 target（`x86_64-unknown-linux-gnu`），会在 `ostd` 侧报 `acpi`、`x86_64`、`x86`、`tdx_guest`、`multiboot2`、`unwinding` 等 crate 未解析。
- 根因不是这些依赖在仓库中缺失，而是 `ostd` 的这些依赖声明在 [ostd/Cargo.toml](/home/lby/os_com_codex/asterinas/ostd/Cargo.toml:50) 的 target-specific 区段下，只对 `x86_64-unknown-none` 生效；而 `ostd` 源码里的 x86 路径使用的是 `#[cfg(target_arch = "x86_64")]`，当用宿主机 x86_64 target 检查时会编到这些代码，但对应依赖不会被 Cargo 注入。
- 已验证：`cargo check -p ostd --target x86_64-unknown-none` 可以通过；因此这条问题当前不阻塞 ext4/JBD2 开发，只是检查命令需要显式带 kernel target。
- 后续若要统一收口 workspace 构建体验，可单独开一项 `ostd` 维护任务，二选一处理：
  - 方案 A：统一约定 kernel/ostd 检查命令显式使用 `--target x86_64-unknown-none`；
  - 方案 B：调整 `ostd` 的 Cargo target 依赖写法与源码 `cfg` 条件，使宿主机 `x86_64-unknown-linux-gnu` 下的 `cargo check` 也不会触发这类未解析依赖。
