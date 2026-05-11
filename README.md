# Asterinas EXT4 + JBD2 赛题版本

本仓库是基于 Asterinas 的 EXT4 文件系统赛题工程版本。当前主线已经完成 **JBD2 Phase 3 功能收口**：在 Phase 2 的完整事务管理、崩溃恢复与多文件并发 correctness baseline 之上，补齐了 `fsync` / `fdatasync` / block flush / Linux 持久化语义对齐、Tier 1 shutdown xfstests、自研 host-crash fsync 场景与 fsync-heavy benchmark 证据链。

当前日期口径：2026-05-11（Asia/Shanghai）；最新 Phase 3 功能验证基线为 2026-05-08，普通 O_DIRECT write 性能 hardening 转入后续阶段。

## 当前状态

### 已完成

- EXT4 基础文件与目录能力：`create/open/close/read/write/truncate/lseek/mkdir/rmdir/unlink/rename/stat` 等核心路径已接入并持续通过阶段回归。
- Extent 连续块管理：支持多块分配、extent tree 插入/删除/折叠修复，并修复 `generic/013`、`generic/068` 等长期暴露的一致性问题。
- fio O_DIRECT 性能 Phase 1：顺序读写曾达到守底目标，JBD2 Phase 1 收口后结果为 read `93.49%`、write `87.01%`（对比 Linux ext4）。Phase 3 语义收口后普通 write 重新暴露为性能 hardening blocker，见下方最新 fio 结果。
- JBD2 Phase 1：已完成完整事务管理、日志写盘、checkpoint、dirty journal recovery、crash 注入与旧 CrashJournal 移除。
- JBD2 Phase 2：多文件并发读写 correctness baseline 已收口；完成锁顺序文档化、`runtime_block_size` 显式化、handle-local context、operation-local allocator guard、allocator/block-group correctness 协议、目录 cache-backed readdir，以及 Step 8 fio write profile。
- JBD2 Phase 3：已完成 raw block fd、virtio-blk 与 ext4 regular-file 的 fsync/flush 持久化语义收口；`jbd_phase3_fsync_flush` 默认 11 PASS / 1 NOTRUN / 0 FAIL，12G scratch 下 `generic/048` 单点 PASS，自研 host-crash fsync matrix 4/4 PASS。

### Phase 2 收口结论

- 赛题功能侧要求已经覆盖：JBD2 完整事务管理、崩溃恢复、多文件并发基本读写 correctness 均有固定测试资产与 2026-05-05 验证日志。
- 当前保留的全局 fence/局部锁策略是 correctness-first 的实现选择；Phase 2 验收以数据正确性、日志恢复和核心回归为主，不把更激进的并发吞吐拆锁作为本轮必过项。
- fio read 达标；fio write 最新正式值为 `87.01%`，低于 90%，已通过 Step 8 profile 判断为后续性能优化项，而不是 Phase 2 功能缺口。
- `EXT4_PHASE2_WORKERS=8 EXT4_PHASE2_ROUNDS=64 EXT4_PHASE2_SEED=100` 属于额外高压探针，曾观察到偶发短读/extent mapping 风险；当前功能验收 baseline 固定为 `workers=4 rounds=8 seed=78`。

## JBD2 Phase 2 收口进展

JBD2 Phase 2 在 Phase 1 完整日志与崩溃恢复能力之上，补齐赛题优秀档剩余功能要求：多文件并发基本读写 correctness、核心 xfstests 回归、并发 workload 固定 baseline，以及功能大全量复跑。Phase 1 的 block-level journal 能力是 Phase 2 的基础，当前已经作为整体收口结果统一验收。

Phase 1 日志基础能力已经完成：

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

Phase 2 correctness 收口能力已经完成：

- 新增 `jbd_phase2_concurrency` 自研并发测试资产，覆盖多文件写后校验、读写交错、create/unlink churn、rename churn、write/truncate/fsync、allocator churn 与 unlink-open 等核心并发路径。
- 移除或显式化关键共享状态：`runtime_block_size` 改为显式上下文，JBD2 metadata/data-sync 归属改为 handle-local context，operation allocated block guard 改为 operation-local / handle-local。
- 补齐 inode、目录、allocator 与 block-group correctness 协议，在保守锁/fence 下保证不同文件并发 workload 不出现数据错乱、重复物理块、旧 mapping 或 metadata 归属错误。
- 完成目录 cache-backed readdir，修复 `ext4/045` 1200s 预算边界问题，并保持 `generic/011`、Phase 2 并发 baseline 与 crash matrix 不回退。
- 完成 Step 8 fio write profile：当前 fio write 稳态为 1 mapping / 1 bio / 1 segment，request queue merge 为 0；fio 1MiB user buffer 为 256 pages / 256 physical runs，因此 naive page-SG zero-copy 不作为当前收口实现。
- 最新功能大全量复跑通过：crash 18/18、phase3 10 PASS + 6 NOTRUN、phase4 12 PASS + 6 NOTRUN、phase6 25/25、jbd_phase1 6 PASS + 6 NOTRUN、lmbench 8/8、Phase 2 concurrency 7/7。

## JBD2 Phase 3 收口结论

Phase 3 已按“先语义、后性能”的边界结束。阶段目标不是继续追普通顺序写吞吐，而是把此前偏轻量的 `fsync` / `fdatasync` / flush 语义补到可解释、可回归的持久化边界。

- raw `/dev/vda` `fsync` / `fdatasync` 已接入底层 `BlockDevice::sync()`，virtio-blk `FLUSH` feature 判断与请求分支已修正。
- ext4 regular-file `fsync` / `fdatasync` 走 inode -> TID 追踪、目标事务 force commit、ordered data drain 与最终 device flush；`fdatasync` 当前采用保守等价实现。
- JBD2 commit block 前增加 PREFLUSH 等价 barrier，VFS inode sync 末尾保留 block-device flush。
- `EXT4_IOC_SHUTDOWN` 三种 flag 已接入，Tier 1 shutdown xfstests 不再依赖 sync-marker shim 伪造 recovery 状态。
- dm-flakey / dm-log-writes / dm-error 等环境依赖 case 保持 blocked 透明化，并用 4 个自研 host-crash 场景覆盖核心 fsync、fdatasync、rename+dir fsync 与并发 fsync 风险。
- 普通 O_DIRECT write 已确认是后续性能 hardening blocker，不作为 Phase 3 功能退场条件继续纠缠。

## 最新验证结果

### 功能回归

| 测试项 | 最新结果 |
| --- | --- |
| `phase3_base_guard` | `10 PASS / 0 FAIL / 6 NOTRUN / 24 STATIC_BLOCKED` |
| `phase4_good` | `12 PASS / 0 FAIL / 6 NOTRUN / 22 STATIC_BLOCKED` |
| `phase6_good` | `25 PASS / 0 FAIL / 0 NOTRUN / 26 STATIC_BLOCKED` |
| `jbd_phase1` | `6 PASS / 0 FAIL / 6 NOTRUN` |
| JBD2 crash matrix | `18/18 PASS` |
| lmbench regression | `8/8 PASS` |
| Phase 2 concurrency baseline | `7/7 PASS`（`workers=4 rounds=8 seed=78`） |
| `jbd_phase3_fsync_flush` | `11 PASS / 0 FAIL / 1 NOTRUN` |
| Phase 3 12G scratch `generic/048` | `PASS` |
| Phase 3 host-crash fsync matrix | `4/4 PASS` |

最新证据日志：

- `benchmark/logs/phase3_base_guard_20260505_144845.log`
- `benchmark/logs/phase4_good_20260505_144845.log`
- `benchmark/logs/phase6_good_20260505_151230.log`
- `benchmark/logs/jbd_phase1_20260505_152645.log`
- `benchmark/logs/crash/phase4_part3_crash_summary_20260505_144845.tsv`
- `benchmark/logs/lmbench/phase4_part3_lmbench_summary_20260505_144845.tsv`
- `benchmark/logs/jbd_phase2_concurrency_20260505_153745.log`
- `benchmark/logs/jbd_phase3_fsync_durability_20260508_023301.log`
- `benchmark/logs/jbd_phase3_fsync_durability_20260508_025646.log`
- `benchmark/logs/crash/phase4_part3_crash_summary_20260507_173023.tsv`

严格关键词扫描为空，扫描范围包括 `ERROR`、`panic`、`BUG`、`logical block not mapped`、`mapped block out of range`、`Extentindex not found`、`ext4 write_at failed`、`Heap allocation error`、`Failed to allocate a large slot`。

### fio EXT4 对照

fio 参数口径：`size=1G bs=1M ioengine=sync direct=1 numjobs=1 fsync_on_close=1 time_based=1 ramp_time=60 runtime=100`。

| 测试项 | Asterinas | Linux | 比值 | Phase 1 守底 |
| --- | ---: | ---: | ---: | --- |
| `ext4_seq_read_bw` | `5179 MB/s` | `4076 MB/s` | `127.06%` | PASS（>= 90%） |
| `ext4_seq_write_bw` | `1189 MB/s` | `3035 MB/s` | `39.18%` | 后续性能 hardening |

历史 optimize Phase 1 基线为 read `95.79%`、write `90.48%`。JBD2 Phase 1 收口后 read/write 为 `93.49%` / `87.01%`；Phase 3 fsync/flush 语义收口后，read 仍通过，write 暴露为 block/virtio/direct-write 路径性能 hardening 问题。

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

### xfstests / 功能大全量验收

```bash
PHASE4_DOCKER_MODE=part3_full \
ENABLE_KVM=1 \
BENCH_ENABLE_KVM=1 \
BENCH_ASTER_NETDEV=tap \
BENCH_ASTER_VHOST=on \
XFSTESTS_CASE_TIMEOUT_SEC=1200 \
PERF_ROUNDS=1 \
PERF_CASE_TIMEOUT_SEC=600 \
bash tools/ext4/run_phase4_in_docker.sh
```

该模式覆盖 crash matrix、`phase4_good`、`phase3_base_guard` 与 lmbench regression。Phase 6 与 JBD 专项按下面两个入口单独复跑。

```bash
PHASE4_DOCKER_MODE=phase6_only \
ENABLE_KVM=1 \
BENCH_ENABLE_KVM=1 \
BENCH_ASTER_NETDEV=tap \
BENCH_ASTER_VHOST=on \
XFSTESTS_CASE_TIMEOUT_SEC=1200 \
bash tools/ext4/run_phase4_in_docker.sh
```

### JBD2 专项 xfstests

```bash
PHASE4_DOCKER_MODE=jbd_phase1 \
ENABLE_KVM=1 \
BENCH_ENABLE_KVM=1 \
BENCH_ASTER_NETDEV=tap \
BENCH_ASTER_VHOST=on \
XFSTESTS_CASE_TIMEOUT_SEC=1200 \
XFSTESTS_RUN_TIMEOUT_SEC=5400 \
bash tools/ext4/run_phase4_in_docker.sh
```

`jbd_phase1` 列表位于：

- `test/initramfs/src/syscall/xfstests/testcases/jbd_phase1.list`
- `test/initramfs/src/syscall/xfstests/blocked/jbd_phase1_excluded.tsv`

### JBD2 Phase 3 fsync/flush 回归

```bash
PHASE4_DOCKER_MODE=jbd_phase3_fsync_flush \
ENABLE_KVM=1 \
BENCH_ENABLE_KVM=1 \
BENCH_ASTER_NETDEV=tap \
BENCH_ASTER_VHOST=on \
XFSTESTS_CASE_TIMEOUT_SEC=1200 \
bash tools/ext4/run_phase4_in_docker.sh
```

默认 2G scratch 口径结果为 11 PASS / 1 NOTRUN / 0 FAIL；`generic/048` 需要 12G scratch 单独复跑：

```bash
PHASE4_DOCKER_MODE=jbd_phase3_fsync_flush \
ENABLE_KVM=1 \
BENCH_ENABLE_KVM=1 \
BENCH_ASTER_NETDEV=tap \
BENCH_ASTER_VHOST=on \
XFSTESTS_SCRATCH_IMG_SIZE=12G \
XFSTESTS_TEST_IMG_SIZE=12G \
XFSTESTS_CASES=generic/048 \
bash tools/ext4/run_phase4_in_docker.sh
```

Phase 3 自研 host-crash fsync matrix：

```bash
PHASE4_DOCKER_MODE=jbd_phase3_host_crash \
ENABLE_KVM=1 \
BENCH_ENABLE_KVM=1 \
BENCH_ASTER_NETDEV=tap \
BENCH_ASTER_VHOST=on \
KLOG_LEVEL=warn \
bash tools/ext4/run_phase4_in_docker.sh
```

### Phase 2 并发 baseline

```bash
PHASE4_DOCKER_MODE=jbd_phase2_concurrency \
ENABLE_KVM=1 \
BENCH_ENABLE_KVM=1 \
BENCH_ASTER_NETDEV=tap \
BENCH_ASTER_VHOST=on \
EXT4_PHASE2_SEED=78 \
EXT4_PHASE2_WORKERS=4 \
EXT4_PHASE2_ROUNDS=8 \
bash tools/ext4/run_phase4_in_docker.sh
```

该模式运行自研多文件并发 baseline，并执行严格关键词扫描；case 列表可通过 `EXT4_PHASE2_CASES=multi_file_write_verify,rename_churn` 缩小。

Phase 2 最终收口证据：完整功能大全量已复跑通过，包含 crash 18/18、phase3 10 PASS + 6 NOTRUN、phase4 12 PASS + 6 NOTRUN、phase6 25/25、jbd_phase1 6 PASS + 6 NOTRUN、lmbench 8/8、Phase 2 concurrency 7/7；严格关键词扫描为空。

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
| `docs/feature_jbd2_phase3_pretest.md` | JBD2 Phase 3 预研测试，记录 fsync/flush 语义风险与 fsync-heavy fio 现象 |
| `docs/feature_jbd2_phase3_plan.md` | JBD2 Phase 3 实现计划与完成口径，覆盖环境固化、raw/virtio/ext4 fsync/flush 语义收口 |
| `docs/feature_jbd2_phase3_milestone.md` | JBD2 Phase 3 进度、验证证据与后续性能 hardening 记录 |
| `docs/analysis_phase1.md` | fio 性能 Phase 1 诊断报告 |
| `docs/optimize_plan_phase1.md` | fio 性能 Phase 1 计划 |
| `docs/optimize_phase1_milestone.md` | fio 性能 Phase 1 里程碑 |
| `docs/benchmark.md` | 根目录 benchmark 快照，与 `benchmark/benchmark.md` 同步 |
| `docs/environment.md` | Docker、KVM、代理、benchmark 环境说明 |
| `docs/赛题要求.md` | 比赛评审标准 |

## 当前边界

- Phase 1 为同步 commit/checkpoint 模型，没有引入后台 JBD2 commit 线程。
- Revoke 机制已有结构与扫描骨架，当前 crash/recovery 验证覆盖的是本实现实际写出的 descriptor/data/commit 事务格式。
- `rename_across_dir` crash 场景函数已保留，但 marker 触发不稳定，未纳入默认 crash matrix；默认矩阵每轮 9 个场景，最新收口复跑两轮共 18/18 PASS。
- `STATIC_BLOCKED` 用例主要来自当前阶段未覆盖的 Linux ext4 语义或环境能力，例如 hardlink/symlink、AIO、xattr/chacl、renameat2、部分 fallocate/fiemap/collapse-range、device-mapper crash tests。
- 多文件并发基本读写 correctness 与 Phase 3 fsync/flush 持久化语义已完成；更激进拆锁、更高并发吞吐、PageCache 深度优化与 fio write >= 90% 属于后续性能 hardening。
- Phase 3 已确认：`bs=16K fsync=4` 修复后触发真实 flush，旧的纳秒级/微秒级高吞吐结果不能作为性能宣传；fsync-heavy 与普通顺序吞吐分开统计。

## 来源说明

本工程基于 Asterinas 社区项目进行 EXT4/JBD2 赛题方向开发，保留原工程结构与许可证信息。
