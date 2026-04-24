# Asterinas ext4 JBD2 功能实现 Phase 1 — 问题分析

更新时间：2026-04-18（Asia/Shanghai）

## 1. 目标与背景

### 1.1 赛题要求映射

优秀档（EXT4 实现完整度量化评分表）JBD2 相关要求：

1. 实现 JBD2 日志完整功能（**日志刷盘**、**事务管理**、**全量崩溃恢复**）；
2. 遵循 xfstests 规范完成核心功能全量验证，用例通过率 ≥ 95%；
3. 完成多场景崩溃恢复测试，数据一致性 100%；
4. 支持多文件并发基本读写，无数据错乱与丢失。

其中 (1)(2)(3) 是 JBD2 Phase 1 的目标，(4) 并发读写延后至 Phase 2。

### 1.2 Phase 1 范围

- **实现方案：block-level JBD2**，日志单位为磁盘块，与 Linux JBD2 on-disk 格式兼容，能被 `e2fsck` 识别；
- **journaling mode：ordered**（metadata 走 journal，data 先写原位置再 commit metadata）；
- **journal 存储：复用 mkfs 创建的 journal inode**（默认 inode 8，已在 superblock `journal_inode_number` 字段中）；
- **并发读写不在 Phase 1 范围**，保留现有 `EXT4_RS_RUNTIME_LOCK` 串行化，待 Phase 2 拆锁。

---

## 2. 当前实现状态

### 2.1 现有 CrashJournal（自研操作级日志，代号 JBR2）

位置：[asterinas/kernel/src/fs/ext4/fs.rs](asterinas/kernel/src/fs/ext4/fs.rs)（CrashJournal 系列函数，约 1280–1700 行）。

核心机制：

- **日志单位：用户操作**。枚举 `CrashJournalOp` 覆盖 Create / Mkdir / Unlink / Rmdir / Rename / Write / Truncate 七种；
- **存储位置：固定扇区 `CRASH_JOURNAL_OFFSET`**（单扇区 512 字节），不使用 journal inode；
- **格式：自研**。头部 24B（magic + version + state + op + len + checksum），剩余为 payload；
- **两步提交协议**：先写 PREPARED，再写 COMMITTED；mount 时扫描，未清除且为 COMMITTED 的 record 被 replay；
- **串行化**：通过 `crash_journal_lock: Mutex<()>` 保证同时只有一条 record 在途；
- **启用方式：内核命令行参数** `ext4fs.crash_journal=1` 或 `ext4fs.replay_hold=1`，默认关闭。

已通过测试：`crash_only`（6/6，含 `create_write`、`rename`、`truncate_append` 三个场景的 prepare/verify 对）。

### 2.2 ext4_rs 侧的 metadata 写入路径

ext4_rs 内核库所有 metadata 写入都直接走 `block_device.write_offset()`，没有任何日志层：

| metadata 类型 | 写入位置 |
|---------------|----------|
| superblock | [super_block.rs:220](asterinas/kernel/libs/ext4_rs/src/ext4_defs/super_block.rs#L220) |
| inode | [inode.rs:470](asterinas/kernel/libs/ext4_rs/src/ext4_defs/inode.rs#L470) |
| block group descriptor | [block_group.rs:187](asterinas/kernel/libs/ext4_rs/src/ext4_defs/block_group.rs#L187) |
| block bitmap | [balloc.rs:640](asterinas/kernel/libs/ext4_rs/src/ext4_impls/balloc.rs#L640) |
| directory block / extent block | [file.rs:836](asterinas/kernel/libs/ext4_rs/src/ext4_impls/file.rs#L836), [block.rs:99](asterinas/kernel/libs/ext4_rs/src/ext4_defs/block.rs#L99) |

即：**ext4_rs 对 journal 无感知**，上层 CrashJournal 只是在 fs.rs 包了一层高层 op 记录。

### 2.3 标准 JBD2 缺口

| 模块 | 标准 JBD2 | 当前实现 |
|------|-----------|----------|
| 日志存储 | Journal inode（ring buffer，MB ~ GB 级） | 单扇区固定偏移 |
| On-disk 格式 | Linux JBD2（journal superblock v2、descriptor/commit/revoke block、tag v3） | 自研 24B 头 + payload |
| 事务单位 | 一组 metadata block 的修改 | 单条高层操作 |
| 事务并发 | 多 handle 共享 running transaction，后台 commit thread | 全串行 |
| data 与 metadata 关系 | ordered / writeback / journal 三种模式 | 无区分，Write 操作直接把 data 存日志 payload |
| 崩溃恢复 | scan → revoke → replay 三遍扫描，sequence number 驱动 | 单 record replay |
| checkpoint | 定期把已 commit 的 metadata 写回原位置，推进 journal tail | 无概念 |
| revoke 机制 | 防止已删除 metadata 被 replay 覆盖新数据 | 无 |
| 工具兼容 | `e2fsck` 可识别并恢复 | 无法识别 |

---

## 3. 瓶颈 / 缺口分析

### G1 [高]：无 block-level journal 基础设施

标准 JBD2 的核心数据结构在当前代码库中完全缺失：

- Journal superblock（`journal_superblock_t`，v2 格式）
- Journal header（`journal_header_t`，含 magic `0xc03b3998`、blocktype、sequence）
- Descriptor block / tag（描述被日志的 metadata block 的原位置）
- Commit block（事务提交标记，含 commit time、checksum）
- Revoke block（撤销记录，防止旧 metadata 被 replay）

ext4_rs [ext4_defs/](asterinas/kernel/libs/ext4_rs/src/ext4_defs/) 目录下没有任何 journal 相关 struct 定义。

superblock 中的 `journal_inode_number` 和 `journal_blocks[17]` 已读入但**从未使用**。

**影响：** 无法写入合法 JBD2 日志，`e2fsck` 不认，xfstests journaling 相关用例全部无法通过。

### G2 [高]：无事务管理抽象

当前 metadata 写入是"直接落盘"模式，没有"事务"概念：

- 没有 `handle_t`（操作句柄，描述一次原子操作内会修改多少块）；
- 没有 `running transaction` / `committing transaction` 状态机；
- 没有 commit thread / commit trigger；
- 所有 metadata 写入分散在 ext4_rs 各个 impl 文件中，无统一拦截点。

**影响：** 无法实现"一组 metadata 修改要么全部可见要么全部回滚"的原子性；无法做 Phase 2 并发（handle 是并发的基础单元）。

### G3 [高]：无 checkpoint 与 journal 空间管理

JBD2 journal 是一个 ring buffer：

- 新 transaction 从 head 开始写；
- checkpoint 把已 commit 但未 checkpoint 的 metadata 从 journal 写回原位置，推进 tail；
- 满了要 stall 等 checkpoint 释放空间。

当前 CrashJournal 固定 1 个扇区循环使用，根本不涉及空间管理。实现 block-level JBD2 后必须同步实现 checkpoint，否则：

- journal 写满即文件系统停摆；
- recovery 要 replay 的数据量无界。

### G4 [中]：崩溃恢复流程不匹配标准

当前 `replay_mount_crash_journal()` 是"读一条 record → 解码 op → 调用高层 API 重放"的模式。

标准 JBD2 recovery 是三遍扫描：

1. **PASS_SCAN**：找到 journal 中最后一个合法 transaction（由 commit block 的 sequence 决定）；
2. **PASS_REVOKE**：收集所有 revoke block 中的 block number；
3. **PASS_REPLAY**：从 journal 读出 metadata block，按 tag 指向的原位置写回，跳过已 revoke 的块。

**影响：** 不重写 recovery 逻辑则无法处理真实的 JBD2 日志；且"操作级重放"天然无法处理大写入和复杂操作的部分失败。

### G5 [中]：测试基线不足以覆盖优秀档

- **xfstests：** 当前 `phase6_good` 是自定义精简集，不能证明符合官方 xfstests ≥95% 的要求。需要从官方 xfstests 中抽取与 JBD2 相关、优秀档理论上能通过的用例，组成 `jbd_phase1` 列表作为本阶段基线；
- **crash 场景：** 当前仅 3 个场景，不足以证明"全量崩溃恢复"。Phase 1 需要扩展至覆盖 metadata 更新、data 写入、大文件、目录树变更等多种 crash 点。

### G6 [低]：与旧 CrashJournal 的共存 / 迁移策略未定

Phase 1 上线 JBD2 后，旧 CrashJournal 是否保留？当前开关是 kernel cmdline 的 `crash_journal=1`，JBD2 是否也沿用此开关、是否互斥、是否默认启用，都需要在实现前约定。

---

## 4. 测试现状

| 测试项 | 当前结果 | 优秀档要求 | 差距 |
|--------|----------|-----------|------|
| crash_only | PASS (6/6，3 场景 × prepare/verify) | 多场景全覆盖 | 需扩至 ≥ 8 个场景 |
| phase3_base | runner 口径 PASS (100%)，但需按原始日志复核 | — | 不能只看 `rc=0` |
| phase4_good | runner 口径 PASS (100%)，但需按原始日志复核 | — | 不能只看通过率 |
| phase6_good（自定义） | runner 口径 PASS / 部分达标，需结合失败用例与内核日志复核 | — | 不能只看通过率 |
| xfstests（官方 jbd_phase1 子集） | 未建立 | ≥ 95% | 需抽取列表 + 建立运行环境 |
| 并发读写 | 未测试 | 无数据错乱（Phase 2 目标） | Phase 1 不覆盖 |
| `e2fsck -n` 对 journal 的识别 | 不通过（自研格式） | 通过（标准 JBD2） | 需实现标准格式 |

补充说明（2026-04-18）：

- 对历史 `phase3_base_guard` 原始日志复核后发现，`generic/013` 在更早几轮里就已经出现“runner `rc=0`，但内核日志含 ext4 错误”的情况，因此它不是本轮才首次引入的问题。
- 这也意味着此前文档中的 `phase3_base/phase4_good/phase6_good PASS` 需要理解为“runner 口径通过”，不能直接等价为“零错误真通过”。

---

## 5. 优先级汇总

| 优先级 | 方向 | 对应 Gap | 预期收益 | 实现难度 |
|--------|------|----------|----------|----------|
| P0 | JBD2 on-disk 数据结构与 journal 设备初始化 | G1 | 打通基础，所有后续工作的前置 | 中 |
| P0 | 事务与 block-level metadata 日志写入 | G2 | 替代 CrashJournal，覆盖所有 metadata 路径 | 高 |
| P0 | Commit 流程与 checkpoint | G2、G3 | 让 journal 可持续运行 | 高 |
| P0 | 标准 JBD2 recovery（scan + revoke + replay） | G4 | 全量崩溃恢复的核心 | 中 |
| P1 | xfstests jbd_phase1 列表 + 多场景 crash tests | G5 | 建立优秀档验收基线 | 中（含人工抽取） |
| P2 | 旧 CrashJournal 的替换与 kernel cmdline 开关整理 | G6 | 降低维护成本，避免双轨 | 低 |

---

## 6. 代码位置速查

| 功能 | 路径 |
|------|------|
| 现有 CrashJournal 实现 | [asterinas/kernel/src/fs/ext4/fs.rs](asterinas/kernel/src/fs/ext4/fs.rs)（`CrashJournalOp`、`run_journaled`、`replay_mount_crash_journal` 等） |
| ext4_rs on-disk 结构定义 | [asterinas/kernel/libs/ext4_rs/src/ext4_defs/](asterinas/kernel/libs/ext4_rs/src/ext4_defs/) |
| ext4_rs 操作实现 | [asterinas/kernel/libs/ext4_rs/src/ext4_impls/](asterinas/kernel/libs/ext4_rs/src/ext4_impls/) |
| superblock 中 journal 字段 | [super_block.rs:51-63](asterinas/kernel/libs/ext4_rs/src/ext4_defs/super_block.rs#L51-L63) |
| metadata 写盘散点 | inode.rs:470、block_group.rs:187、balloc.rs:640、block.rs:99、super_block.rs:220、file.rs:836/876 |
| 全局串行锁 | [fs.rs:60](asterinas/kernel/src/fs/ext4/fs.rs#L60)（`EXT4_RS_RUNTIME_LOCK`） |
| crash 测试脚本 | [asterinas/test/initramfs/src/syscall/ext4_crash/run_ext4_crash_test.sh](asterinas/test/initramfs/src/syscall/ext4_crash/run_ext4_crash_test.sh) |
| Linux JBD2 参考 | `fs/jbd2/` in Linux 5.15（`journal.c`、`commit.c`、`recovery.c`、`revoke.c`、`transaction.c`） |
