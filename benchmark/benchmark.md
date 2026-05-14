# Asterinas EXT4 Benchmark 最新结果快照

更新时间：2026-05-14（Asia/Shanghai）

Phase 3 收口说明（2026-05-11）：`fsync` / `fdatasync` / block flush / shutdown ioctl / host-crash fsync 语义线已结束；普通 O_DIRECT write 仍低于红线，作为后续性能 hardening blocker 单独推进，不再阻塞 Phase 3 功能退场。

Phase 4 PageCache 说明（2026-05-14）：PageCache correctness 守底已恢复，性能最小闭环采用 A-E 口径：`lmbench_only`、官方 fio `direct=0` buffered cold/warm read、官方 fio `direct=0` buffered write、原 O_DIRECT fio cache-off 守底。O_DIRECT 结果单独作为 non-PageCache guard，不与 buffered PageCache 收益混算。

## 1. 本文用途

- 本文件只保留当前最新一轮已确认的 benchmark 结果。
- 旧历史结果、旧阶段记录、旧对比结论已全部移除。
- 环境准备、代理、Docker、KVM 与复现注意事项请看 `environment.md`。

## 2. 当前结果总览

### 2.1 ext2 顺序写

- job：`fio/ext2_seq_write_bw`
- Asterinas：`2973 MB/s`
- Linux：`2488 MB/s`
- ratio：`119.49%`
- 结果文件：`asterinas/result_fio-ext2_seq_write_bw.json`

### 2.2 ext2 顺序读

- job：`fio/ext2_seq_read_bw`
- Asterinas：`3577 MB/s`
- Linux：`5027 MB/s`
- ratio：`71.16%`
- 结果文件：`asterinas/result_fio-ext2_seq_read_bw.json`

### 2.3 ext4 顺序写

- job：`fio/ext4_seq_write_bw`
- Asterinas：`1189 MB/s`
- Linux：`3035 MB/s`
- ratio：`39.18%`
- 结果文件：`asterinas/result_fio-ext4_seq_write_bw.json`
- 说明：Phase 3 Step 6 普通 O_DIRECT 复跑结果，低于 75% hardening 红线；同代码首轮观察 `1625/3192=50.91%`，仍低于红线。

### 2.4 ext4 顺序读

- job：`fio/ext4_seq_read_bw`
- Asterinas：`5179 MB/s`
- Linux：`4076 MB/s`
- ratio：`127.06%`
- 结果文件：`asterinas/result_fio-ext4_seq_read_bw.json`
- 说明：Phase 3 Step 6 普通 O_DIRECT 复跑结果，通过 90% 目标。

### 2.5 ext4 JBD2 / Phase 2 功能基线

- `phase3_base_guard`：10 PASS / 0 FAIL / 6 NOTRUN / 24 STATIC_BLOCKED，日志 `asterinas/benchmark/logs/phase3_base_guard_20260505_144845.log`
- `phase4_good`：12 PASS / 0 FAIL / 6 NOTRUN / 22 STATIC_BLOCKED，日志 `asterinas/benchmark/logs/phase4_good_20260505_144845.log`
- `phase6_good`：25/25 PASS，日志 `asterinas/benchmark/logs/phase6_good_20260505_151230.log`
- `jbd_phase1`：6 PASS / 0 FAIL / 6 NOTRUN，日志 `asterinas/benchmark/logs/jbd_phase1_20260505_152645.log`
- JBD2 crash matrix：18/18 PASS，summary `asterinas/benchmark/logs/crash/phase4_part3_crash_summary_20260505_144845.tsv`
- lmbench regression：8/8 PASS，summary `asterinas/benchmark/logs/lmbench/phase4_part3_lmbench_summary_20260505_144845.tsv`
- Phase 2 concurrency final baseline：7/7 PASS，`EXT4_PHASE2_WORKERS=4 EXT4_PHASE2_ROUNDS=8 EXT4_PHASE2_SEED=78`
- 最新 baseline 日志：`asterinas/benchmark/logs/jbd_phase2_concurrency_20260505_153745.log`
- 说明：`EXT4_PHASE2_WORKERS=8 EXT4_PHASE2_ROUNDS=64 EXT4_PHASE2_SEED=100` 属于额外高压探针，曾观察到偶发短读/extent mapping 风险，不作为当前功能验收基线。

### 2.6 JBD2 Phase 3 fsync/flush 预研基线

- Phase 3 目标是收口 `fsync` / `fdatasync` / block flush / Linux 持久化语义，不把 fsync-heavy 结果混入普通顺序吞吐指标。
- 预研记录见 `feature_jbd2_phase3_pretest.md`。
- `bs=16K + fsync=4` 预研显示 Asterinas sync latency 远低于 Linux：raw `302 ns` vs Linux `1913.51 us`，ext4 journaled `50.13 us` vs Linux `3337.87 us`。
- 初步判断：raw block fd `fsync` 可能没有触达底层 flush；ext4 regular-file `fsync` 当前不是 Linux 等价持久化屏障；该组结果在语义收口前不能作为性能宣传。

2026-05-08 Step 4c 后复跑 `bs=16K + fsync=4`，已修正 summary 脚本单位解析（直接解析 fio `WRITE: bw=...` 并归一化到十进制 `MB/s`）：

| 测试项 | Asterinas | Linux | ratio | 说明 |
|--------|----------:|------:|------:|------|
| raw_write_16k_fsync4 | 33.240 MB/s | 47.186 MB/s | 70.44% | raw block fd `fsync` 已触发真实 flush |
| ext4_journaled_write_16k_fsync4 | 5.415 MB/s | 22.649 MB/s | 23.91% | commit block 前 PREFLUSH + VFS final flush 后的真实成本 |
| ext4_nojournal_write_16k_fsync4 | 11.010 MB/s | 37.958 MB/s | 29.01% | nojournal 仍需 device flush |

注意：旧 JSON 中 `ext4_journaled` 的 Asterinas `5288` 来自 fio 的 `5288KiB/s`，不能读成 `5288 MB/s`。本项是 fsync 持久化语义压力测试，不作为普通顺序写吞吐宣传。

### 2.7 JBD2 Phase 3 普通 O_DIRECT fio 复跑

2026-05-08 Step 6 复跑官方普通 ext4 fio 口径：

| 测试项 | Asterinas | Linux | ratio | 说明 |
|--------|----------:|------:|------:|------|
| ext4_seq_read_bw | 5179 MB/s | 4076 MB/s | 127.06% | read 通过 |
| ext4_seq_write_bw | 1189 MB/s | 3035 MB/s | 39.18% | write 低于 75% hardening 红线 |

命令：

```bash
KEEP_LOGS=1 bash ./asterinas/test/initramfs/src/benchmark/fio/run_ext4_summary.sh
```

保存日志：write 最终复跑 `/tmp/ext4-fio-summary.final/ext4_seq_write_bw.log`，read 复跑 `/tmp/ext4-fio-summary.iqspHK/ext4_seq_read_bw.log`。普通 write 首轮同代码观察 `1625/3192=50.91%`，仍低于红线；最终 JSON 当前记录 `1189/3035=39.18%`。该回归与 fsync-heavy 口径分开记录，作为后续性能 hardening blocker。

### 2.8 PageCache Phase 4 benchmark A-E

本组用于观察 PageCache buffered I/O 收益和守底回归。buffered fio 使用官方 `/benchmark/bin/fio`，核心参数为 `direct=0`；Asterinas 侧用 `EXT4_PAGE_CACHE=0/1` 对比 `ext4fs.page_cache` 开关。

| 测试项 | Asterinas | Linux | ratio / 说明 | 日志 |
|--------|----------:|------:|--------------|------|
| A. `lmbench_only` | `8/8 PASS` | N/A | VFS/lmbench regression clean | `benchmark/logs/lmbench/phase4_part3_lmbench_summary_20260514_051539.tsv` |
| B/C. buffered fio read, `page_cache=0` | cold 121.0 MB/s, warm 122.0 MB/s | cold 3948.0 MB/s, warm 7457.0 MB/s | warm 为 Linux 1.64% | `benchmark/logs/pagecache_buffered_fio/pagecache_buffered_fio_summary_20260514_130056.tsv` |
| B/C. buffered fio read, `page_cache=1` | cold 19.9 MB/s, warm 4022.0 MB/s | cold 3948.0 MB/s, warm 7457.0 MB/s | warm 为 Linux 53.94%，为 `page_cache=0` warm 的 3296.72% | 同上 |
| D. buffered fio write, `page_cache=0` | 38.4 MB/s | 633.0 MB/s | 6.07% | 同上 |
| D. buffered fio write, `page_cache=1` | 10.8 MB/s | 633.0 MB/s | 1.71%，后续 hardening 点 | 同上 |
| E. O_DIRECT ext4 read cache-off | 2570 MB/s | 2643 MB/s | 97.24% | `benchmark/logs/fio_ext4_cacheoff_20260514_1345/ext4_seq_read_bw.log` |
| E. O_DIRECT ext4 write cache-off | 1706 MB/s | 3158 MB/s | 54.02%，仍为 hardening blocker | `benchmark/logs/fio_ext4_cacheoff_20260514_1345/ext4_seq_write_bw.log` |

结论：PageCache-on 的 warm read 已能体现缓存命中收益；cold read 和 buffered write 暴露出当前 PageCache backend / dirty writeback 的性能成本，列入 Phase 4 Step 7 hardening。

### 2.9 PageCache Phase 4 correctness 测试记录

本组记录 Phase 4 新增 `pagecache_phase4` upstream xfstests 验收集的最近几次结果。该集合专门覆盖 buffered/direct coherency、mmap、truncate、PageCache invalidation 与 writeback 相关风险；默认通过 `PHASE4_DOCKER_MODE=pagecache_phase4` 运行，并显式开启 `ext4fs.page_cache=1`。

| 时间 | 测试项 | 结果 | 日志 / 说明 |
|------|--------|------|-------------|
| 2026-05-12 | `pagecache_phase4` full list | `7 PASS / 2 FAIL / 4 NOTRUN` | 早期 full-list 基线，剩余 blocker 为 `generic/263`、`generic/418` |
| 2026-05-12 | `pagecache_phase4` full list | `8 PASS / 1 FAIL / 4 NOTRUN` | `benchmark/logs/pagecache_phase4_20260512_160858.log`，仅剩 `generic/418` |
| 2026-05-13 | clean `generic/263` | `PASS` | `benchmark/logs/pagecache_phase4_20260513_091148.log` |
| 2026-05-13 | clean `generic/247,generic/418` | `2 PASS / 0 FAIL` | `benchmark/logs/pagecache_phase4_20260513_091558.log` |
| 2026-05-13 | `pagecache_phase4` full list | `9 PASS / 0 FAIL / 4 NOTRUN` | `benchmark/logs/pagecache_phase4_20260513_091938.log`，有效样本 pass rate `100.00%` |

当前结论：PageCache Phase 4 correctness 验收集已恢复到 `FAIL=0`；4 个 `NOTRUN` 来自 helper/debugfs/512-byte aligned O_DIRECT 能力缺口，未作为静态排除规避。性能侧则以 2.8 的 A-E benchmark 为当前闭环。

## 3. 当前 fio 参数口径

当前 ext2 与 ext4 采用同一套 fio 参数，只有 `filename` 和 `rw/name` 随测试项变化。

统一参数：

```bash
-size=1G -bs=1M \
-ioengine=sync -direct=1 -numjobs=1 -fsync_on_close=1 \
-time_based=1 -ramp_time=60 -runtime=100
```

四个 job 的完整口径如下。

### 3.1 ext2_seq_write_bw

```bash
/benchmark/bin/fio -rw=write -filename=/ext2/fio-test -name=seqwrite \
-size=1G -bs=1M \
-ioengine=sync -direct=1 -numjobs=1 -fsync_on_close=1 \
-time_based=1 -ramp_time=60 -runtime=100
```

- 脚本：`asterinas/test/initramfs/src/benchmark/fio/ext2_seq_write_bw/run.sh`

### 3.2 ext2_seq_read_bw

```bash
/benchmark/bin/fio -rw=read -filename=/ext2/fio-test -name=seqread \
-size=1G -bs=1M \
-ioengine=sync -direct=1 -numjobs=1 -fsync_on_close=1 \
-time_based=1 -ramp_time=60 -runtime=100
```

- 脚本：`asterinas/test/initramfs/src/benchmark/fio/ext2_seq_read_bw/run.sh`

### 3.3 ext4_seq_write_bw

```bash
/benchmark/bin/fio -rw=write -filename=/ext4/fio-test -name=seqwrite \
-size=1G -bs=1M \
-ioengine=sync -direct=1 -numjobs=1 -fsync_on_close=1 \
-time_based=1 -ramp_time=60 -runtime=100
```

- 脚本：`asterinas/test/initramfs/src/benchmark/fio/ext4_seq_write_bw/run.sh`

### 3.4 ext4_seq_read_bw

```bash
/benchmark/bin/fio -rw=read -filename=/ext4/fio-test -name=seqread \
-size=1G -bs=1M \
-ioengine=sync -direct=1 -numjobs=1 -fsync_on_close=1 \
-time_based=1 -ramp_time=60 -runtime=100
```

- 脚本：`asterinas/test/initramfs/src/benchmark/fio/ext4_seq_read_bw/run.sh`

### 3.5 ext4 write/read 摘要复跑脚本

```bash
cd /home/lby/os_com_codex
./asterinas/test/initramfs/src/benchmark/fio/run_ext4_summary.sh
```

- 脚本：`asterinas/test/initramfs/src/benchmark/fio/run_ext4_summary.sh`
- 行为：顺序执行 `fio/ext4_seq_write_bw` 与 `fio/ext4_seq_read_bw`
- 终端输出：默认不输出 benchmark 过程日志，只打印每项的 `Asterinas`、`Linux`、`ratio`
- 排障方式：如需保留临时日志，可使用 `KEEP_LOGS=1 ./asterinas/test/initramfs/src/benchmark/fio/run_ext4_summary.sh`

## 4. 本轮执行方式

- 工作树：`/home/lby/os_com_codex`
- 主仓库：`/home/lby/os_com_codex/asterinas`
- 执行环境：Docker `asterinas/asterinas:0.17.0-20260227`
- benchmark 关键环境变量：
  - `BENCH_ENABLE_KVM=1`
  - `BENCH_ASTER_NETDEV=tap`
  - `BENCH_ASTER_VHOST=on`
- 代理：Clash `127.0.0.1:7890`

实际使用的 benchmark 入口是：

```bash
bash test/initramfs/src/benchmark/bench_linux_and_aster.sh <job> x86_64
```

本轮涉及的 job：

- `fio/ext2_seq_write_bw`
- `fio/ext2_seq_read_bw`
- `fio/ext4_seq_write_bw`
- `fio/ext4_seq_read_bw`
- `fio/ext4_buffered_seq_read_bw`
- `fio/ext4_buffered_seq_write_bw`

PageCache Phase 4 新增 benchmark 入口：

```bash
EXT4_DIRECT_READ_CACHE=0 \
BENCH_FIO_SIZE=1G \
LOG_LEVEL=error \
bash test/initramfs/src/benchmark/fio/run_pagecache_buffered_summary.sh
```

O_DIRECT cache-off 守底入口：

```bash
EXT4_DIRECT_READ_CACHE=0 \
EXT4_PAGE_CACHE=0 \
KEEP_LOGS=1 \
LOG_LEVEL=error \
bash test/initramfs/src/benchmark/fio/run_ext4_summary.sh
```

## 5. 整体综合测试（按需运行）

> **注意**：本节测试耗时约 30 分钟，仅在需要诊断各层开销时运行，日常评审和功能验收不需要跑。

### 5.1 测试目的

通过对比 raw 裸设备、ext4 有日志、ext4 无日志三种配置的顺序读写带宽，完成以下开销分解：

- **raw vs ext4 nojournal**：文件系统层（extent 查找、inode 更新）本身的开销
- **ext4 nojournal vs ext4 journaled**：JBD2 日志层的写开销
- **Asterinas raw vs Linux raw**：virtio-blk / 块设备驱动层的差距

### 5.2 6 个测试项

| # | job | filename | rw | 说明 |
|---|-----|----------|----|------|
| 1 | `fio/raw_seq_read_bw` | `/dev/vda` | read | 裸块设备，理论读上限 |
| 2 | `fio/raw_seq_write_bw` | `/dev/vda` | write | 裸块设备，理论写上限 |
| 3 | `fio/ext4_seq_read_bw` | `/ext4/fio-test` | read | 有日志 ext4，与官方评审口径相同 |
| 4 | `fio/ext4_seq_write_bw` | `/ext4/fio-test` | write | 有日志 ext4，与官方评审口径相同 |
| 5 | `fio/ext4_nojournal_seq_read_bw` | `/ext4/fio-test` | read | 无日志 ext4（`^has_journal`） |
| 6 | `fio/ext4_nojournal_seq_write_bw` | `/ext4/fio-test` | write | 无日志 ext4（`^has_journal`） |

6 个测试共用相同 fio 参数。综合诊断默认额外关闭 Asterinas ext4 O_DIRECT direct-read cache，用于观察不依赖自研 direct-read cache / speculative direct read 的直通读写成本：

```bash
-size=1G -bs=1M
-ioengine=sync -direct=1 -numjobs=1 -fsync_on_close=1
-time_based=1 -ramp_time=60 -runtime=100
```

Asterinas 侧默认附加参数：

```bash
EXT4_DIRECT_READ_CACHE=0
```

- 该变量会传入内核命令行 `ext4fs.direct_read_cache=0`。
- 关闭范围：ext4 O_DIRECT read mapping cache 与 speculative direct read。
- 保留范围：write overwrite mapping reuse 仍按默认路径启用，避免把写路径映射复用也一并关掉后混入额外变量。
- 如需复跑历史 cache-on 对照，可显式设置 `EXT4_DIRECT_READ_CACHE=1`。

### 5.3 入口与运行方法

```bash
cd /home/lby/os_com_codex
EXT4_DIRECT_READ_CACHE=0 ./asterinas/test/initramfs/src/benchmark/fio/run_6test_summary.sh
```

- 脚本：`asterinas/test/initramfs/src/benchmark/fio/run_6test_summary.sh`
- 行为：顺序执行 6 个 job，每个都跑 Asterinas 和 Linux 两侧，最后打印汇总
- 默认口径：脚本默认 `EXT4_DIRECT_READ_CACHE=0`，上面的命令显式写出该参数只是为了避免误读
- 日志：默认执行完自动清理；如需保留日志排查问题，加 `KEEP_LOGS=1`：

```bash
EXT4_DIRECT_READ_CACHE=0 KEEP_LOGS=1 ./asterinas/test/initramfs/src/benchmark/fio/run_6test_summary.sh
```

- 仅跑 Asterinas 侧（跳过 Linux，节省一半时间）：

```bash
EXT4_DIRECT_READ_CACHE=0 BENCH_RUN_ONLY=aster ./asterinas/test/initramfs/src/benchmark/fio/run_6test_summary.sh
```

### 5.4 最新结果（2026-05-09，6-test 综合诊断，direct-read cache off）

| 测试 | Asterinas | Linux | Aster/Linux |
|------|----------:|------:|:-----------:|
| raw read | 1827.0 MB/s | 4791.0 MB/s | 38.13% |
| raw write | 1708.0 MB/s | 4176.0 MB/s | 40.90% |
| ext4 journaled read | 2463.0 MB/s | 2328.0 MB/s | 105.80% |
| ext4 journaled write | 1713.0 MB/s | 3302.0 MB/s | 51.88% |
| ext4 nojournal read | 2582.0 MB/s | 3366.0 MB/s | 76.71% |
| ext4 nojournal write | 2068.0 MB/s | 4554.0 MB/s | 45.41% |

关键结论：

- 本轮为 cache-off 直通诊断口径，不再把自研 direct-read cache / speculative direct read 的收益计入综合测试。
- 关闭 direct-read cache 后，Asterinas ext4 read 从历史 cache-on 的 5GB/s 级别回落到约 2.5GB/s，说明此前 ext4 read 显著高于 raw read 的主要来源是 direct-read cache / speculative direct read。
- 同轮对比下，journaled write（1713.0 MB/s）低于 nojournal write（2068.0 MB/s）约 355 MB/s，JBD2 写路径仍有可见开销。
- raw read 在本轮为 1827.0 MB/s，低于历史 run，后续分析应优先使用同轮 raw/ext4/Linux 对比，避免跨轮环境波动误判。
- 本轮 Linux 侧仍出现 `kvm_intel: VMX not supported by CPU 0`，Linux 绝对值仅供参考，以同轮相对趋势和 Asterinas 绝对值为主。

## 7. 当前观察与说明

- ext4 已经按本轮要求对齐到 ext2 参数，不再使用此前的 `size=128M` 口径。
- 2026-05-09 起，6-test 综合诊断默认关闭 Asterinas ext4 direct-read cache：`EXT4_DIRECT_READ_CACHE=0` / `ext4fs.direct_read_cache=0`；2026-05-06 的 cache-on 结果仅作为历史对照，不再作为默认综合测试口径。
- 2026-04-24 的 ext4 结果来自 JBD2 Phase 1 收口后的 fio 守底复跑：read `93.49%`、write `87.01%`，满足 Phase 1 “相对基线不下降超过 5 个百分点”的守底线（read ≥ 90%、write ≥ 85%）。
- 2026-05-08 Phase 3 Step 6 普通 fio 复跑：read `127.06%` 通过；write `39.18%` 低于 `75%` hardening 红线。当前不能继续沿用 `87.01%` 作为 Phase 3 最新写性能结论；该问题转入后续性能 hardening，不阻塞 Phase 3 fsync/flush 功能收口。
- 2026-05-05 的 Phase 2 收口口径：完整功能回归大全量已复跑通过，包括 phase3、phase4、phase6、jbd_phase1、crash matrix、lmbench 与 Phase 2 concurrency；其中 xfstests 统计按 `PASS / FAIL / NOTRUN / STATIC_BLOCKED` 原始口径记录，NOTRUN/STATIC_BLOCKED 为环境或赛题范围外跳过项；fio write 仍低于 90%，作为性能优化遗留项继续推进。
- Step 8 profile 显示当前 fio write 稳态为 1 mapping / 1 bio / 1 segment，request queue merge 为 0；fio 1MiB user buffer 为 256 pages / 256 physical runs / max run 1 page，因此 naive page-SG zero-copy 不作为当前实现主线。
- 这几轮 Linux 对照侧都出现了 `kvm_intel: VMX not supported by CPU 0`。
- 因此当前结果适合用于“本地最新观测值”和方案推进参考。
- 如果后续要写正式 milestone 或对外结论，建议同时记录该环境现象，避免把 Linux 对照侧异常忽略掉。

## 8. 对应仓库内文档

- 根目录：`benchmark.md`
- 仓库内同步副本：`asterinas/benchmark/benchmark.md`
