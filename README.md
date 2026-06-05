# Asterinas EXT4 + JBD2 赛题版本

本仓库是基于 Asterinas 的 EXT4 文件系统赛题工程版本。功能主线（JBD2 完整事务管理 / 全量崩溃恢复 / 多文件并发 correctness / Phase 3 fsync/flush 持久化语义 / Phase 4 PageCache buffered I/O / mmap）均已收口，守底回归全绿。当前主线已完成 **性能优化 Phase 5 读侧收口**：以延迟归因 profiling 驱动，落地 extent 映射缓存、全文件覆盖、atime 节流与 **inode 元数据缓存** 四个 ext4 域内优化，在诚实 cache-off + drop 公平口径下把读写从优化前的 16–63% 拉到 **read 86/84/87/95/123%、write 76/76/84/121/88%**（bs=4K/16K/64K/256K/1M，`direct=1 nj=1` 中位数）。ext4 域内 per-op 固定开销已榨干，剩余瓶颈在 Asterinas virtio 设备往返（平台层、跨 FS 通用，ext2 同口径同样卡在 82–85%）。

当前日期口径：2026-06-05（Asia/Shanghai）；最新 Phase 5 证据链见 `docs/feature_perf_phase5_plan.md` / `docs/feature_perf_phase5_milestone.md` / `benchmark/benchmark.md` §6.6。

> 口径声明：历史 `read 127.06% / write 39.18%` 是 speculative direct-read **data cache 开**的不诚实数，**不能用于答辩**。Phase 5 起一律 cache-off + extent_map/inode 元数据缓存 + drop 公平基线，中位数取数。

## 当前状态

### 已完成

- EXT4 基础文件与目录能力：`create/open/close/read/write/truncate/lseek/mkdir/rmdir/unlink/rename/stat` 等核心路径已接入并持续通过阶段回归。
- Extent 连续块管理：支持多块分配、extent tree 插入/删除/折叠修复，并修复 `generic/013`、`generic/068` 等长期暴露的一致性问题。
- fio O_DIRECT 性能 Phase 1：顺序读写曾达到守底目标，JBD2 Phase 1 收口后结果为 read `93.49%`、write `87.01%`（对比 Linux ext4）。Phase 3 语义收口后普通 write 重新暴露为性能 hardening blocker，见下方最新 fio 结果。
- JBD2 Phase 1：已完成完整事务管理、日志写盘、checkpoint、dirty journal recovery、crash 注入与旧 CrashJournal 移除。
- JBD2 Phase 2：多文件并发读写 correctness baseline 已收口；完成锁顺序文档化、`runtime_block_size` 显式化、handle-local context、operation-local allocator guard、allocator/block-group correctness 协议、目录 cache-backed readdir，以及 Step 8 fio write profile。
- JBD2 Phase 3：已完成 raw block fd、virtio-blk 与 ext4 regular-file 的 fsync/flush 持久化语义收口；`jbd_phase3_fsync_flush` 默认 11 PASS / 1 NOTRUN / 0 FAIL，12G scratch 下 `generic/048` 单点 PASS，自研 host-crash fsync matrix 4/4 PASS。
- PageCache Phase 4 correctness：ext4 buffered read/write 与 mmap 已接入共享 `PageCache` / `Vmo`；`pagecache_phase4` upstream xfstests 最新 full list 为 `9 PASS / 0 FAIL / 4 NOTRUN`，有效样本 pass rate `100.00%`。

- 性能优化 Phase 5 读侧：以四层延迟归因 profiling（FS / virtio / 锁 / JBD2，门控 `ext4fs.phase2_profile=1`）定位 per-op 固定开销，落地 extent 映射缓存、随机读全文件覆盖、relatime atime 节流、in-memory inode 元数据缓存，小块读 ×2.6–5.2、write 4K 20%→76%；完整守底回归全绿。

### 进行中

- 性能 Phase 5 剩余开放项（virtio / 平台层，需与学长对齐答辩口径）：并发读 `nj>1` 锁退化、bio_copy、symlink / 512B-align 小缺口；ext2 4K O_DIRECT write 在同平台挂死（参考实现侧问题，非本实现）。

## EXT4 性能优化 Phase 5 收口结论

Phase 5 是延迟归因驱动的性能优化主线。诚实口径下（cache-off、`ext4fs.extent_map_cache` + inode 元数据缓存开启、host drop-caches 公平基线、`direct=1 nj=1`、中位数），四个 ext4 域内优化把读写带宽拉到下表水平：

| bs | read | write |
| --- | ---: | ---: |
| 4K | `86.38%` | `75.54%` |
| 16K | `84.42%` | `75.78%` |
| 64K | `86.89%` | `84.09%` |
| 256K | `94.81%` | `121.07%` |
| 1M | `122.94%` | `88.28%` |

- 四个优化：(1) O_DIRECT 读 extent 映射 plan 缓存（带 generation guard，TOCTOU 安全）；(2) 随机读全文件覆盖窗口；(3) relatime atime 决策缓存，消除每次读的 atime stat；(4) **in-memory inode 元数据缓存**（最大收益 —— `get_inode_ref` 此前每次 `stat` 都从设备重读 inode block，小块读热路径上每读多次 ~25µs 的 inode-block 重读）。
- 优化前小块读 16–24%、write 4K=20% / 1M=63%；inode 缓存把小块读拉到 84–87%，write 也因 `write_at` 的 `type_()` stat 命中缓存而受益。
- 单点失效协议：所有写路径经 `run_journaled_ext4` 单一收敛点 `fetch_add` generation 并清空缓存，保证缓存与磁盘一致。
- ext4 域内 per-op 固定开销已榨干；剩余 read/write gap 是 Asterinas virtio 设备写往返比读慢（平台层）。用 Asterinas 成熟的 ext2 同口径对照确认：ext2 在同平台同样卡在 82–85%，证明瓶颈不是 ext4 模块特有，而是 virtio 平台底噪。
- 完整守底回归全绿：crash matrix `18/18`、Phase 2 concurrency `7/7`、xfstests 各集有效样本 `100%`，读优化未改变功能正确性。证据见 `docs/feature_perf_phase5_milestone.md` 与 `benchmark/logs/phase5_*`。

## PageCache Phase 4 收口结论

Phase 4 已把 ext4 regular-file buffered I/O / mmap 接入 Asterinas `PageCache` / `Vmo`，并将旧的自研 direct-read cache 从默认 correctness / benchmark 口径中隔离。当前阶段按“先 correctness、再 hardening”的边界收口：PageCache 负责 buffered read/write、mmap 与 dirty writeback；O_DIRECT 继续绕过 PageCache，但必须和 PageCache 建立 flush / discard 一致性协议。

- `pagecache_phase4` upstream xfstests 验收集最新 full list 为 `9 PASS / 0 FAIL / 4 NOTRUN`，有效样本 pass rate `100.00%`；4 个 `NOTRUN` 来自 helper 未构建、debugfs 不可用或 512-byte aligned O_DIRECT 能力缺口，没有通过静态排除规避。
- buffered/direct/mmap coherency 已覆盖 `generic/091`、`generic/130`、`generic/133`、`generic/247`、`generic/263`、`generic/412`、`generic/418`、`generic/469`、`generic/749` 等关键场景；checkpoint metadata 覆盖复用后 data block 的 stale data 风险已通过 revoke 协议修复。
- Phase 2/3 守底回归保持通过：Phase 2 concurrency `7/7`，JBD2 crash matrix `18/18`，`jbd_phase3_fsync_flush` `11 PASS / 0 FAIL / 1 NOTRUN`，Phase 3 host-crash fsync matrix `4/4`，lmbench regression `8/8`。
- PageCache benchmark A-E 已建立：`lmbench_only` 通过；buffered fio warm read 在 `page_cache=1` 下达到 `4022.0 MB/s`，相比 `page_cache=0` 的 `122.0 MB/s` 已体现缓存命中收益；cold read 与 buffered write 仍是 Phase 4 Step 7 hardening 点。
- O_DIRECT fio cache-off 守底单独统计，不与 PageCache 收益混算：read `2570/2643 MB/s = 97.24%` 通过，write `1706/3158 MB/s = 54.02%` 仍作为后续 direct-write 性能 hardening blocker。

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
| `pagecache_phase4` upstream xfstests | `9 PASS / 0 FAIL / 4 NOTRUN` |
| PageCache Phase 4 benchmark A-E | 已完成首轮结果记录，见下方和 `benchmark/benchmark.md` |

最新证据日志：

- `benchmark/logs/phase3_base_guard_20260505_144845.log`
- `benchmark/logs/phase4_good_20260505_144845.log`
- `benchmark/logs/phase6_good_20260505_151230.log`
- `benchmark/logs/jbd_phase1_20260505_152645.log`
- `benchmark/logs/crash/phase4_part3_crash_summary_20260514_043248.tsv`
- `benchmark/logs/lmbench/phase4_part3_lmbench_summary_20260514_051539.tsv`
- `benchmark/logs/jbd_phase2_concurrency_20260514_034441.log`
- `benchmark/logs/jbd_phase3_fsync_durability_20260514_034641.log`
- `benchmark/logs/crash/phase4_part3_crash_summary_20260514_043536.tsv`
- `benchmark/logs/pagecache_phase4_20260513_091938.log`
- `benchmark/logs/pagecache_buffered_fio/pagecache_buffered_fio_summary_20260514_130056.tsv`
- `benchmark/logs/fio_ext4_cacheoff_20260514_1345/ext4_seq_read_bw.log`
- `benchmark/logs/fio_ext4_cacheoff_20260514_1345/ext4_seq_write_bw.log`

严格关键词扫描为空，扫描范围包括 `ERROR`、`panic`、`BUG`、`logical block not mapped`、`mapped block out of range`、`Extentindex not found`、`ext4 write_at failed`、`Heap allocation error`、`Failed to allocate a large slot`。

### fio EXT4 对照（Phase 5 诚实口径）

fio 参数口径：`direct=1 ioengine=sync numjobs=1`，bs 全扫，host drop-caches 公平基线，`ext4fs.extent_map_cache` + inode 元数据缓存开启，中位数取数。复跑入口 `test/initramfs/src/benchmark/fio/run_phase5_guard_median.sh`，详见 `benchmark/benchmark.md` §6.6。

| bs | read 比值 | write 比值 |
| --- | ---: | ---: |
| 4K | `86.38%` | `75.54%` |
| 16K | `84.42%` | `75.78%` |
| 64K | `86.89%` | `84.09%` |
| 256K | `94.81%` | `121.07%` |
| 1M | `122.94%` | `88.28%` |

**口径演进**：历史 optimize Phase 1 基线 read `95.79%` / write `90.48%`，JBD2 Phase 1 后 `93.49%` / `87.01%`，但这些与 Phase 3 一度宣传的 `read 127.06% / write 39.18%` 都受 speculative direct-read **data cache** 影响，属不诚实口径，**不能用于答辩**。Phase 5 起统一改为 cache-off + 元数据缓存 + drop 公平基线的诚实口径，并补齐小块全扫，即上表。剩余 read/write gap 在 Asterinas virtio 设备写往返（平台层，跨 FS 通用）。

### PageCache Phase 4 benchmark

Phase 4 新增 A-E 口径：A 为 `lmbench_only`，B/C 为官方 fio `direct=0` buffered cold/warm read，D 为官方 fio `direct=0` buffered write，E 为原 O_DIRECT fio cache-off 守底。Asterinas 侧通过 `EXT4_PAGE_CACHE=0/1` 对比 `ext4fs.page_cache` 开关。

| 测试项 | Asterinas | Linux | 说明 |
| --- | ---: | ---: | --- |
| A. `lmbench_only` | `8/8 PASS` | N/A | `benchmark/logs/lmbench/phase4_part3_lmbench_summary_20260514_051539.tsv` |
| B/C. buffered read, `page_cache=0` | cold `121.0 MB/s`, warm `122.0 MB/s` | cold `3948.0 MB/s`, warm `7457.0 MB/s` | 无 PageCache 收益 |
| B/C. buffered read, `page_cache=1` | cold `19.9 MB/s`, warm `4022.0 MB/s` | cold `3948.0 MB/s`, warm `7457.0 MB/s` | warm read 为 Linux `53.94%`，为 cache-off warm 的 `3296.72%` |
| D. buffered write, `page_cache=0` | `38.4 MB/s` | `633.0 MB/s` | `6.07%` |
| D. buffered write, `page_cache=1` | `10.8 MB/s` | `633.0 MB/s` | `1.71%`，后续 hardening 点 |
| E. O_DIRECT read cache-off | `2570 MB/s` | `2643 MB/s` | `97.24%` |
| E. O_DIRECT write cache-off | `1706 MB/s` | `3158 MB/s` | `54.02%`，仍为 hardening blocker |

结论：PageCache-on 的 warm read 已能体现缓存命中收益；cold read 与 buffered write 暴露出当前 PageCache backend / dirty writeback 的性能成本，转入 Phase 4 Step 7 hardening。

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

### PageCache Phase 4 xfstests / benchmark

PageCache correctness 验收集：

```bash
PHASE4_DOCKER_MODE=pagecache_phase4 \
ENABLE_KVM=1 \
BENCH_ENABLE_KVM=1 \
BENCH_ASTER_NETDEV=tap \
BENCH_ASTER_VHOST=on \
XFSTESTS_CASE_TIMEOUT_SEC=1200 \
bash tools/ext4/run_phase4_in_docker.sh
```

PageCache A-E 中的 A：

```bash
KLOG_LEVEL=error \
PHASE4_DOCKER_MODE=lmbench_only \
ENABLE_KVM=1 \
BENCH_ENABLE_KVM=1 \
BENCH_ASTER_NETDEV=tap \
BENCH_ASTER_VHOST=on \
PERF_ROUNDS=1 \
PERF_CASE_TIMEOUT_SEC=600 \
bash tools/ext4/run_phase4_in_docker.sh
```

PageCache A-E 中的 B/C/D：

```bash
EXT4_DIRECT_READ_CACHE=0 \
BENCH_FIO_SIZE=1G \
LOG_LEVEL=error \
BENCH_ENABLE_KVM=1 \
BENCH_ASTER_NETDEV=tap \
BENCH_ASTER_VHOST=on \
bash test/initramfs/src/benchmark/fio/run_pagecache_buffered_summary.sh
```

PageCache A-E 中的 E：

```bash
EXT4_DIRECT_READ_CACHE=0 \
EXT4_PAGE_CACHE=0 \
KEEP_LOGS=1 \
LOG_LEVEL=error \
BENCH_ENABLE_KVM=1 \
BENCH_ASTER_NETDEV=tap \
BENCH_ASTER_VHOST=on \
bash test/initramfs/src/benchmark/fio/run_ext4_summary.sh
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

### 并发专项 xfstests 套件

标准 xfstests 并发/压力 case 的独立套件（`testcases/concurrency.list`），与自研 `phase2_concurrency.c` 互补——前者用 fsstress 多进程随机操作 + shutdown 恢复 + 并发 dio 覆盖标准并发场景，后者做确定性多文件数据完整性校验。

```bash
PHASE4_DOCKER_MODE=concurrency \
ENABLE_KVM=1 \
BENCH_ENABLE_KVM=1 \
BENCH_ASTER_NETDEV=tap \
BENCH_ASTER_VHOST=on \
XFSTESTS_CASE_TIMEOUT_SEC=1200 \
bash tools/ext4/run_phase4_in_docker.sh
```

套件内容（`concurrency.list`，page_cache=0，10 case）：fsstress 并发压力 `generic/013/068/076/083/476/269`、并发压力+崩溃恢复 `generic/051/054/055/388`。并发 direct-I/O-vs-mmap / direct-vs-buffered 一致性（`generic/247/263`）依赖 PageCache（`page_cache=1`），归属 `pagecache_phase4` 模式覆盖，不在本套件。刻意排除项（attrs/quota/dm-error/PageCache 依赖）见 `blocked/concurrency_excluded.tsv`。

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
EXT4_DIRECT_READ_CACHE=0 \
EXT4_PAGE_CACHE=0 \
BENCH_ENABLE_KVM=1 \
BENCH_ASTER_NETDEV=tap \
BENCH_ASTER_VHOST=on \
bash test/initramfs/src/benchmark/fio/run_ext4_summary.sh
```

该脚本顺序执行 `fio/ext4_seq_write_bw` 与 `fio/ext4_seq_read_bw`，默认只打印 Asterinas、Linux 与 ratio 摘要。需要保留过程日志时可加：

```bash
KEEP_LOGS=1 \
EXT4_DIRECT_READ_CACHE=0 \
EXT4_PAGE_CACHE=0 \
BENCH_ENABLE_KVM=1 \
BENCH_ASTER_NETDEV=tap \
BENCH_ASTER_VHOST=on \
bash test/initramfs/src/benchmark/fio/run_ext4_summary.sh
```

## 重要目录

| 路径 | 作用 |
| --- | --- |
| `kernel/src/fs/ext4/` | Asterinas ext4 VFS 集成层，包含 mount、inode、JBD2 runtime 桥接、direct I/O 等 |
| `kernel/src/fs/ext2/` | ext2 PageCache / VFS 集成参考实现 |
| `kernel/src/fs/utils/page_cache.rs` | Asterinas PageCache / Vmo pager 基础设施 |
| `kernel/libs/ext4_rs/` | EXT4 核心库，包含 extent、block allocator、dir、inode、JBD2 on-disk/recovery/transaction 逻辑 |
| `kernel/libs/ext4_rs/src/ext4_impls/jbd2/` | JBD2 device、superblock、space、handle、transaction、journal、recovery 实现 |
| `kernel/libs/ext4_rs/src/bin/jbd2_probe.rs` | host 侧 JBD2 验证工具 |
| `test/initramfs/src/syscall/ext4_crash/` | crash matrix 测试脚本 |
| `test/initramfs/src/syscall/xfstests/` | xfstests runner、case list、blocked list 与兼容 helper |
| `test/initramfs/src/benchmark/fio/` | fio benchmark job、ext4 摘要脚本与 PageCache buffered fio runner |
| `tools/ext4/` | Docker / initramfs / phase runner 入口 |
| `benchmark/logs/` | 最新验证日志与 crash summary |
| `benchmark/benchmark.md` | 当前 benchmark 快照，含 Phase 4 PageCache A-E benchmark |

## 文档索引

本仓库的 `docs/` 目录与 benchmark 文档保留了本赛题阶段记录：

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
| `docs/feature_pagecache_phase4_plan.md` | PageCache Phase 4 实现计划，覆盖 ext4 buffered I/O / mmap / writeback、自研 cache 退役边界与 A-E benchmark 口径 |
| `docs/feature_pagecache_phase4_milestone.md` | PageCache Phase 4 进度、代码审计、`pagecache_phase4` 回归与 PageCache benchmark A-E 记录 |
| `docs/analysis_phase1.md` | fio 性能 Phase 1 诊断报告 |
| `docs/optimize_plan_phase1.md` | fio 性能 Phase 1 计划 |
| `docs/optimize_phase1_milestone.md` | fio 性能 Phase 1 里程碑 |
| `docs/feature_perf_phase5_plan.md` | 性能优化 Phase 5 计划：延迟归因驱动，extent/inode 缓存、小块、读并发优化 |
| `docs/feature_perf_phase5_milestone.md` | 性能优化 Phase 5 进度、read/write 完整占比表、回归与 benchmark 记录 |
| `docs/fio_direct_parameter_sweep_report.md` | Phase 5 基线证据：fio direct 全量参数 sweep（A–G 组），三瓶颈分解 |
| `docs/fio_direct_senior_feedback_response.md` | Phase 5 基线证据：学长性能优化指导与三方对齐结论 |
| `docs/benchmark.md` | docs 目录 benchmark 快照，与 `benchmark/benchmark.md` 和根目录 `benchmark.md` 同步 |
| `benchmark/benchmark.md` | 仓库内 benchmark 快照，含 Phase 4 PageCache A-E benchmark 与最近 PageCache correctness 测试结果 |
| `docs/environment.md` | Docker、KVM、代理、benchmark 环境说明 |
| `docs/赛题要求.md` | 比赛评审标准 |

## 当前边界

- Phase 1 为同步 commit/checkpoint 模型，没有引入后台 JBD2 commit 线程。
- Revoke 机制已有结构与扫描骨架，当前 crash/recovery 验证覆盖的是本实现实际写出的 descriptor/data/commit 事务格式。
- `rename_across_dir` crash 场景函数已保留，但 marker 触发不稳定，未纳入默认 crash matrix；默认矩阵每轮 9 个场景，最新收口复跑两轮共 18/18 PASS。
- `STATIC_BLOCKED` 用例主要来自当前阶段未覆盖的 Linux ext4 语义或环境能力，例如 hardlink/symlink、AIO、xattr/chacl、renameat2、部分 fallocate/fiemap/collapse-range、device-mapper crash tests。
- 多文件并发基本读写 correctness、Phase 3 fsync/flush 持久化语义、Phase 4 PageCache correctness 与 Phase 5 读侧性能优化均已收口；ext4 域内 per-op 开销已榨干。剩余开放项属 virtio / 平台层：PageCache cold read / buffered write、并发读 `nj>1` 锁退化、write 与 read 的 virtio 写往返 gap、更激进拆锁与更高并发吞吐。
- Phase 4 中 PageCache 只服务 buffered I/O / mmap / writeback；O_DIRECT 继续绕过 PageCache，并通过 cache-off 守底单独统计。
- Phase 3 已确认：`bs=16K fsync=4` 修复后触发真实 flush，旧的纳秒级/微秒级高吞吐结果不能作为性能宣传；fsync-heavy 与普通顺序吞吐分开统计。

## 来源说明

本工程基于 Asterinas 社区项目进行 EXT4/JBD2 赛题方向开发，保留原工程结构与许可证信息。
