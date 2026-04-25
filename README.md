# Asterinas EXT4 + JBD2 赛题版本

本仓库是基于 Asterinas 的 EXT4 文件系统赛题工程版本。当前主线已经从早期 EXT4 适配与 fio 性能优化，推进到 **JBD2 Phase 1 完成状态**：在 Asterinas 上实现 block-level JBD2 事务管理、日志刷盘、checkpoint、标准 recovery，并用 xfstests、crash matrix、fio 与编译/单测完成闭环验证。

当前日期口径：2026-04-25（Asia/Shanghai）。

## 当前状态

### 已完成

- EXT4 基础文件与目录能力：`create/open/close/read/write/truncate/lseek/mkdir/rmdir/unlink/rename/stat` 等核心路径已接入并持续通过阶段回归。
- Extent 连续块管理：支持多块分配、extent tree 插入/删除/折叠修复，并修复 `generic/013`、`generic/068` 等长期暴露的一致性问题。
- fio O_DIRECT 性能 Phase 1：顺序读写已达到守底目标，最新 JBD2 收口后结果为 read `93.49%`、write `87.01%`（对比 Linux ext4）。
- JBD2 Phase 1：已完成完整事务管理、日志写盘、checkpoint、dirty journal recovery、crash 注入与旧 CrashJournal 移除。

### Phase 2 已启动

- 多文件并发读写是 `feature_jbd2_phase2` 范围；当前已完成 Phase 2 Step 0 并发测试资产、Step 1 锁顺序/低噪声观测点、Step 2 `runtime_block_size` 显式化、Step 3 JBD2 handle-local operation context、Step 4 operation allocated block guard 本地化。
- Phase 2 按“先 correctness，再性能”推进：先盘点并修复全局锁、共享状态、JBD2 handle 归属、allocator guard、inode/目录/cache 并发风险，再恢复并发吞吐。
- 更大范围 xfstests、更多 Linux ext4 兼容语义、并发吞吐优化和 PageCache 深度优化仍可继续推进。

## JBD2 Phase 1 进展

JBD2 Phase 1 的目标是替换旧的自研 sector-based CrashJournal，改为使用与 Linux ext4/JBD2 兼容的 block-level journal 格式，并满足“日志刷盘、事务管理、全量崩溃恢复、多场景 crash 一致性”的优秀档核心要求。

本阶段已经完成：

- 新增 JBD2 on-disk 数据结构：journal header、superblock v2、descriptor tag、commit block、revoke block header 与相关 feature/type 常量。
- 复用 mkfs.ext4 创建的 journal inode（默认 inode 8），实现 journal inode 物理块映射、journal superblock 读取/校验、ring buffer head/tail 管理。
- 新增 `JournalHandle`、`JournalTransaction`、`JournalRuntime`，按事务记录 metadata block 镜像、handle 生命周期、提交计划与 checkpoint 队列。
- 新增 `MetadataWriter` 拦截层，superblock、inode、block group、bitmap、dir、extent 等 metadata 写入路径统一进入 JBD2 handle。
- ordered mode 语义：文件 data block 仍写 home location，metadata 进入 journal；commit 前保证需要的数据写入顺序。
- 实现 JBD2 commit 序列：descriptor block、metadata data block、commit block、journal superblock head/sequence 更新。
- 实现 checkpoint：committed transaction 的 metadata block 批量回写 home block，随后推进 journal tail / `s_start`。
- 实现标准 recovery 入口：mount 时根据 journal `s_start` 或 ext4 `needs_recovery` 扫描 descriptor/data/commit transaction，replay metadata，清空 journal，并清除 `needs_recovery`。
- 新增 host 工具 `jbd2_probe`，支持 `show-super`、`write-probe-tx`、`recover`，用于离线构造 dirty journal 与验证 recovery。
- 移除旧 CrashJournal 路径：旧 sector record、mount replay、sector 0 read/write、`crash_journal` 参数与 sync 清理分支均已删除。

## 最新验证结果

### 功能回归

| 测试项 | 最新结果 |
| --- | --- |
| `phase3_base_guard` | `10 PASS / 0 FAIL / 6 NOTRUN / 24 STATIC_BLOCKED` |
| `phase4_good` | `12 PASS / 0 FAIL / 6 NOTRUN / 22 STATIC_BLOCKED` |
| `phase6_good` | `25 PASS / 0 FAIL / 0 NOTRUN / 26 STATIC_BLOCKED` |
| `jbd_phase1` | `6 PASS / 0 FAIL / 6 NOTRUN` |
| JBD2 crash matrix | `9/9 PASS` |
| commit 前/中 crash uncommitted 语义 | PASS |
| dirty journal recovery | PASS（`jbd2_probe write-probe-tx -> recover -> e2fsck -fn`） |

最新证据日志：

- `benchmark/logs/phase3_base_guard_20260424_070912.log`
- `benchmark/logs/phase4_good_20260424_070912.log`
- `benchmark/logs/phase6_good_20260424_070912.log`
- `benchmark/logs/jbd_phase1_20260424_064149.log`
- `benchmark/logs/crash/phase4_part3_crash_summary_20260424_063654.tsv`
- `benchmark/logs/crash/phase4_part3_crash_summary_20260424_063948.tsv`
- `benchmark/logs/crash/phase4_part3_crash_summary_20260424_064038.tsv`

严格关键词扫描为空，扫描范围包括 `ERROR`、`panic`、`BUG`、`logical block not mapped`、`mapped block out of range`、`Extentindex not found`、`ext4 write_at failed`、`Heap allocation error`、`Failed to allocate a large slot`。

### fio EXT4 对照

fio 参数口径：`size=1G bs=1M ioengine=sync direct=1 numjobs=1 fsync_on_close=1 time_based=1 ramp_time=60 runtime=100`。

| 测试项 | Asterinas | Linux | 比值 | Phase 1 守底 |
| --- | ---: | ---: | ---: | --- |
| `ext4_seq_read_bw` | `4453 MB/s` | `4763 MB/s` | `93.49%` | PASS（>= 90%） |
| `ext4_seq_write_bw` | `2417 MB/s` | `2778 MB/s` | `87.01%` | PASS（>= 85%） |

历史 optimize Phase 1 基线为 read `95.79%`、write `90.48%`。JBD2 Phase 1 收口后，read/write 相对基线均未超过 5 个百分点回退。

## 复现命令

所有命令默认在仓库根目录执行：

```bash
cd /home/lby/os_com_codex/asterinas
```

### 编译与单测

```bash
CARGO_TARGET_DIR=/tmp/os_com_codex_ext4_rs_target \
cargo test -p ext4_rs --lib

VDSO_LIBRARY_DIR="$(pwd)/benchmark/assets/linux_vdso" \
CARGO_TARGET_DIR=/tmp/os_com_codex_kernel_target \
cargo check -p aster-kernel --target x86_64-unknown-none
```

### xfstests 总体验收

```bash
PHASE4_DOCKER_MODE=phase6_with_guard \
ENABLE_KVM=1 \
NETDEV=tap \
VHOST=on \
KLOG_LEVEL=error \
bash tools/ext4/run_phase4_in_docker.sh
```

该模式会串行覆盖 `phase4_good`、`phase3_base_guard` 与 `phase6_good`，适合作为 JBD2 Phase 1 的总体验收入口。

### JBD2 专项 xfstests

```bash
PHASE4_DOCKER_MODE=jbd_phase1 \
ENABLE_KVM=1 \
RELEASE_LTO=0 \
XFSTESTS_CASE_TIMEOUT_SEC=1200 \
XFSTESTS_RUN_TIMEOUT_SEC=3600 \
bash tools/ext4/run_phase4_in_docker.sh
```

`jbd_phase1` 列表位于：

- `test/initramfs/src/syscall/xfstests/testcases/jbd_phase1.list`
- `test/initramfs/src/syscall/xfstests/blocked/jbd_phase1_excluded.tsv`

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

该模式运行自研多文件并发 baseline，并执行严格关键词扫描；case 列表可通过 `EXT4_PHASE2_CASES=multi_file_write_verify,rename_churn` 缩小。

Step 2 收口证据：`cargo check -p ext4_rs` PASS、`cargo test -p ext4_rs --lib` 21/21 PASS、`runtime_block_size` 残留扫描为空；`phase3_base_guard`、`phase4_good`、`phase6_good`、`jbd_phase1`、Phase 2 并发 baseline 与 9 场 crash matrix 均不回退。

Step 3 收口证据：`cargo test -p ext4_rs --lib` 24/24 PASS；Phase 2 并发 baseline 5/5 PASS（`workers=4 rounds=8 seed=1`）；`phase3_base_guard`、`phase4_good`、`phase6_good`、`jbd_phase1` 与 9 场 crash matrix 均不回退。

Step 4 收口证据：全局 `OP_ALLOCATED_BLOCKS` 已移除，allocator guard 改为 handle/operation-local；`cargo check -p ext4_rs` PASS、`cargo test -p ext4_rs --lib` 25/25 PASS；Phase 2 并发 smoke/baseline 5/5 PASS（`jbd_phase2_concurrency_20260425_034616.log`、`jbd_phase2_concurrency_20260425_034834.log`）；`phase3_base_guard`、`phase4_good`、`phase6_good`、`jbd_phase1` 与 9 场 crash matrix 均不回退。

### Crash 测试

默认 9 场 JBD2 crash matrix：

```bash
PHASE4_DOCKER_MODE=crash_only \
ENABLE_KVM=1 \
NETDEV=tap \
VHOST=on \
CRASH_ROUNDS=1 \
CRASH_HOLD_STAGE=after_commit \
bash tools/ext4/run_phase4_in_docker.sh
```

commit 前/中未提交语义验证示例：

```bash
PHASE4_DOCKER_MODE=crash_only \
ENABLE_KVM=1 \
NETDEV=tap \
VHOST=on \
CRASH_ROUNDS=1 \
CRASH_SCENARIOS=create_write:write \
CRASH_HOLD_STAGE=before_commit \
CRASH_EXPECT=uncommitted \
bash tools/ext4/run_phase4_in_docker.sh
```

可用 `CRASH_HOLD_STAGE=before_commit_block` 覆盖 commit block 写入前的注入点。

### fio 守底复跑

```bash
BENCH_ENABLE_KVM=1 \
BENCH_ASTER_NETDEV=tap \
BENCH_ASTER_VHOST=on \
bash test/initramfs/src/benchmark/fio/run_ext4_summary.sh
```

该脚本顺序执行 `fio/ext4_seq_write_bw` 与 `fio/ext4_seq_read_bw`，默认只打印 Asterinas、Linux 与 ratio 摘要。需要保留过程日志时可加：

```bash
KEEP_LOGS=1 \
BENCH_ENABLE_KVM=1 \
BENCH_ASTER_NETDEV=tap \
BENCH_ASTER_VHOST=on \
bash test/initramfs/src/benchmark/fio/run_ext4_summary.sh
```

## 重要目录

| 路径 | 作用 |
| --- | --- |
| `kernel/src/fs/ext4/` | Asterinas ext4 VFS 集成层，包含 mount、inode、JBD2 runtime 桥接、direct I/O 等 |
| `kernel/libs/ext4_rs/` | EXT4 核心库，包含 extent、block allocator、dir、inode、JBD2 on-disk/recovery/transaction 逻辑 |
| `kernel/libs/ext4_rs/src/ext4_impls/jbd2/` | JBD2 device、superblock、space、handle、transaction、journal、recovery 实现 |
| `kernel/libs/ext4_rs/src/bin/jbd2_probe.rs` | host 侧 JBD2 验证工具 |
| `test/initramfs/src/syscall/ext4_crash/` | crash matrix 测试脚本 |
| `test/initramfs/src/syscall/xfstests/` | xfstests runner、case list、blocked list 与兼容 helper |
| `test/initramfs/src/benchmark/fio/` | fio benchmark job 与 ext4 摘要脚本 |
| `tools/ext4/` | Docker / initramfs / phase runner 入口 |
| `benchmark/logs/` | 最新验证日志与 crash summary |
| `benchmark/benchmark.md` | 当前 benchmark 快照 |

## 文档索引

本仓库的 `docs/` 目录保留了本赛题阶段文档：

| 文档 | 作用 |
| --- | --- |
| `docs/feature_jbd2_phase1_analysis.md` | JBD2 Phase 1 问题分析 |
| `docs/feature_jbd2_phase1_plan.md` | JBD2 Phase 1 实现计划 |
| `docs/feature_jbd2_phase1_milestone.md` | JBD2 Phase 1 进度与验证记录 |
| `docs/feature_jbd2_phase2_analysis.md` | JBD2 Phase 2 并发正确性问题分析 |
| `docs/feature_jbd2_phase2_plan.md` | JBD2 Phase 2 实现计划（先 correctness，再性能） |
| `docs/feature_jbd2_phase2_lock_order.md` | JBD2 Phase 2 锁顺序、同步原语与回退约定 |
| `docs/feature_jbd2_phase2_milestone.md` | JBD2 Phase 2 进度跟踪模板 |
| `docs/analysis_phase1.md` | fio 性能 Phase 1 诊断报告 |
| `docs/optimize_plan_phase1.md` | fio 性能 Phase 1 计划 |
| `docs/optimize_phase1_milestone.md` | fio 性能 Phase 1 里程碑 |
| `docs/benchmark.md` | 根目录 benchmark 快照，与 `benchmark/benchmark.md` 同步 |
| `docs/environment.md` | Docker、KVM、代理、benchmark 环境说明 |
| `docs/赛题要求.md` | 比赛评审标准 |

## 当前边界

- Phase 1 为同步 commit/checkpoint 模型，没有引入后台 JBD2 commit 线程。
- Revoke 机制已有结构与扫描骨架，当前 crash/recovery 验证覆盖的是本实现实际写出的 descriptor/data/commit 事务格式。
- `rename_across_dir` crash 场景函数已保留，但 marker 触发不稳定，未纳入默认 crash matrix；默认矩阵已有 9 个场景并全部通过。
- `STATIC_BLOCKED` 用例主要来自当前阶段未覆盖的 Linux ext4 语义或环境能力，例如 hardlink/symlink、AIO、xattr/chacl、renameat2、部分 fallocate/fiemap/collapse-range、device-mapper crash tests。
- 多文件并发读写与更高并发吞吐优化属于下一阶段。

## 来源说明

本工程基于 Asterinas 社区项目进行 EXT4/JBD2 赛题方向开发，保留原工程结构与许可证信息。
