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
  - `BENCH_ASTER_SCHEME=null`：仅用于 raw/ext4 分层诊断时关闭 Asterinas IOMMU；官方/历史可比口径未显式设置时仍沿用 benchmark runner 默认 `SCHEME=iommu`
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

如需按“关闭 IOMMU 后先看 fs/disk”的诊断建议复跑官方 ext4 双项，可额外加：

```bash
BENCH_ASTER_SCHEME=null \
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

其中“fs 和裸盘比”指同一个系统内比较 `ext4` 文件 I/O 与 `/dev/vda` raw 块设备 I/O，例如 `Asterinas ext4_journaled_write / Asterinas raw_write`。这个口径先不问 Asterinas 是否接近 Linux，而是先判断 ext4/JBD2 是否明显拖慢了本机块层上限；如果 raw 本身已经慢，优先看 block/virtio/IOMMU，如果 raw 快但 ext4 慢，才优先看 ext4/JBD2。

### 5.2 6 个测试项

| # | job | filename | rw | 说明 |
|---|-----|----------|----|------|
| 1 | `fio/raw_seq_read_bw` | `/dev/vda` | read | 裸块设备，理论读上限 |
| 2 | `fio/raw_seq_write_bw` | `/dev/vda` | write | 裸块设备，理论写上限 |
| 3 | `fio/ext4_seq_read_bw` | `/ext4/fio-test` | read | 有日志 ext4，与官方评审口径相同 |
| 4 | `fio/ext4_seq_write_bw` | `/ext4/fio-test` | write | 有日志 ext4，与官方评审口径相同 |
| 5 | `fio/ext4_nojournal_seq_read_bw` | `/ext4/fio-test` | read | 无日志 ext4（`^has_journal`） |
| 6 | `fio/ext4_nojournal_seq_write_bw` | `/ext4/fio-test` | write | 无日志 ext4（`^has_journal`） |

6 个测试共用相同 fio 参数。综合诊断默认使用 `direct=1`，并额外关闭 Asterinas IOMMU、PageCache 与 ext4 O_DIRECT direct-read cache，用于观察不依赖 IOMMU/DMA remapping、PageCache、自研 direct-read cache / speculative direct read 的直通读写成本：

```bash
-size=1G -bs=1M
-ioengine=sync -direct=1 -numjobs=1 -fsync_on_close=1
-time_based=1 -ramp_time=60 -runtime=100
```

Asterinas 侧默认附加参数：

```bash
BENCH_ASTER_SCHEME=null
EXT4_PAGE_CACHE=0
EXT4_DIRECT_READ_CACHE=0
```

- `BENCH_ASTER_SCHEME=null` 会移除 benchmark runner 默认的 `SCHEME=iommu`，即 Asterinas 侧不启用 IOMMU；如需复跑历史 IOMMU-on 对照，可显式设置 `BENCH_ASTER_SCHEME=iommu`。
- `EXT4_PAGE_CACHE=0` 会传入内核命令行 `ext4fs.page_cache=0`，即关闭 Asterinas PageCache；配合 fio `direct=1`，避免 buffered/mmap PageCache 命中混入 fs-vs-raw 诊断。
- `EXT4_DIRECT_READ_CACHE=0` 会传入内核命令行 `ext4fs.direct_read_cache=0`。
- 关闭范围：ext4 O_DIRECT read mapping cache 与 speculative direct read。
- 保留范围：write overwrite mapping reuse 仍按默认路径启用，避免把写路径映射复用也一并关掉后混入额外变量。
- 如需复跑历史 cache-on 对照，可显式设置 `EXT4_DIRECT_READ_CACHE=1`。
- `BENCH_ASTER_VHOST=on` 只影响 tap 网络后端 `vhost=on`，不表示磁盘 fio 使用 vhost 块设备；六项 fio 磁盘路径仍是 virtio-blk。

### 5.3 入口与运行方法

```bash
cd /home/lby/os_com_codex
BENCH_ASTER_SCHEME=null EXT4_PAGE_CACHE=0 EXT4_DIRECT_READ_CACHE=0 \
    ./asterinas/test/initramfs/src/benchmark/fio/run_6test_summary.sh
```

- 脚本：`asterinas/test/initramfs/src/benchmark/fio/run_6test_summary.sh`
- 行为：顺序执行 6 个 job，每个都跑 Asterinas 和 Linux 两侧，最后打印汇总
- 输出：先打印每项的 Asterinas/Linux ratio，再打印 Asterinas 内部 `ext4/raw`、`nojournal/raw`、`journaled/nojournal` 分层 ratio
- 默认口径：脚本默认 `BENCH_ASTER_SCHEME=null`、`EXT4_PAGE_CACHE=0` 与 `EXT4_DIRECT_READ_CACHE=0`，上面的命令显式写出参数只是为了避免误读
- 脚本会跳过 `result_*.json` 写回，只从日志汇总结果，避免 no-IOMMU 诊断数据覆盖官方 fio 快照
- 日志：默认执行完自动清理；如需保留日志排查问题，加 `KEEP_LOGS=1`：

```bash
BENCH_ASTER_SCHEME=null EXT4_PAGE_CACHE=0 EXT4_DIRECT_READ_CACHE=0 KEEP_LOGS=1 \
    ./asterinas/test/initramfs/src/benchmark/fio/run_6test_summary.sh
```

- 仅跑 Asterinas 侧（跳过 Linux，节省一半时间）：

```bash
BENCH_ASTER_SCHEME=null EXT4_PAGE_CACHE=0 EXT4_DIRECT_READ_CACHE=0 BENCH_RUN_ONLY=asterinas \
    ./asterinas/test/initramfs/src/benchmark/fio/run_6test_summary.sh
```

### 5.4 最新结果（2026-05-14，6-test 综合诊断，no-IOMMU，PageCache/direct-read cache off）

命令：

```bash
BENCH_ASTER_SCHEME=null EXT4_PAGE_CACHE=0 EXT4_DIRECT_READ_CACHE=0 \
KEEP_LOGS=1 LOG_LEVEL=error \
./asterinas/test/initramfs/src/benchmark/fio/run_6test_summary.sh
```

| 测试 | Asterinas | Linux | Aster/Linux |
|------|----------:|------:|:-----------:|
| raw read | 2535.0 MB/s | 4887.0 MB/s | 51.87% |
| raw write | 2480.0 MB/s | 3631.0 MB/s | 68.30% |
| ext4 journaled read | 2978.0 MB/s | 3939.0 MB/s | 75.60% |
| ext4 journaled write | 1954.0 MB/s | 3128.0 MB/s | 62.47% |
| ext4 nojournal read | 2968.0 MB/s | 2995.0 MB/s | 99.10% |
| ext4 nojournal write | 1981.0 MB/s | 3518.0 MB/s | 56.31% |

Asterinas 内部 fs-vs-raw 分层：

| 分层比值 | ratio |
|----------|------:|
| ext4 journaled read / raw read | 117.48% |
| ext4 nojournal read / raw read | 117.08% |
| ext4 journaled write / raw write | 78.79% |
| ext4 nojournal write / raw write | 79.88% |
| journaled write / nojournal write | 98.64% |

关键结论：

- 本轮为 no-IOMMU + PageCache/direct-read-cache off 的 direct-I/O 诊断口径，已排除 Asterinas IOMMU、PageCache、direct-read mapping cache 与 speculative direct read 变量。
- Asterinas raw write 提升到 2480.0 MB/s；ext4 journaled/nojournal write 分别为 raw write 的 78.79% / 79.88%，说明当前 ext4 direct-write 共同路径仍有约 20% 损耗。
- journaled write / nojournal write 为 98.64%，本轮下 JBD2 额外写开销很小；write hardening 应优先看 ext4 data I/O 共同路径、direct-write bio/metadata prepare，而不是单独归因到 JBD2。
- ext4 journaled/nojournal read 均约为 raw read 的 117%，说明在本轮 cache-off 口径下，read 侧不是当前主要 blocker；该现象可能来自同轮 raw 读基线偏低或 ext4 顺序映射组织更有利，后续应继续用同轮分层结果判断。

## 6. fio 参数 sweep 工具（A–G 全量参数画像）

用于在单个 Docker 容器内，一次性跑出 ext4 O_DIRECT / buffered 的全量参数画像（不同 `bs`/`numjobs`/`fsync`/`direct`/`page_cache`），并对每个 case 与 Linux ext4 同轮对照，输出一张可信的 ratio 汇总 TSV。是 Phase 5 性能优化的基线证据来源。

- **脚本**：`asterinas/test/initramfs/src/benchmark/fio/run_parameter_sweep_summary.sh`
- **前置**：宿主机有 `docker` 且 `/dev/kvm` 可用（脚本会自检，缺失直接报错退出）

### 6.1 运行方法

```bash
cd /home/lby/os_com_codex
./asterinas/test/initramfs/src/benchmark/fio/run_parameter_sweep_summary.sh
```

跑完后：

```
fio parameter sweep finished.
Summary TSV: .../benchmark/logs/fio_parameter_sweep_<TS>/fio_parameter_sweep_summary.tsv
```

- **输出目录**：`asterinas/benchmark/logs/fio_parameter_sweep_<TS>/`（每个 case 一个 `<case>.log` + 一张 `fio_parameter_sweep_summary.tsv`）
- **汇总 TSV 列**：`group / case / target / journal / rw / direct / page_cache / direct_read_cache / bs / numjobs / fsync / asterinas_mb_s / linux_mb_s / ratio_pct / log / note`

### 6.2 测试分组（A–G）

| 组 | 内容 | 主要用途 |
|----|------|----------|
| A | 官方 O_DIRECT 守底（`bs=1M, nj=1, cache-off`）write/read | 复现守底 ratio |
| B | 6-test 分层（raw / ext4 journaled / ext4 nojournal）| 区分 block 层 vs ext4 vs JBD2 |
| C | bs sweep（`4K/16K/64K/256K/1M/4M`）| 找块大小拐点、小块 per-request 开销 |
| D | direct/cache 对照（direct×page_cache 四象限）| 分离 O_DIRECT 与 buffered/PageCache |
| E | fsync sweep（`none/4/16/64`，`bs=16K/1M`）| 持久化语义成本（不作普通吞吐宣传）|
| F | numjobs sweep（`1/2/4`）| 判断并发提交能力 / 队列深度 |
| G | correctness 回归（crash / phase3-6 / jbd / concurrency / pagecache / host-crash fsync）| 确认性能没有建立在 correctness 回退上 |

### 6.3 可调环境变量

| 变量 | 默认 | 说明 |
|------|------|------|
| `IMAGE` | `asterinas/asterinas:0.17.0-20260227` | Docker 镜像 |
| `LOG_DIR` | `benchmark/logs/fio_parameter_sweep_<TS>` | 输出目录 |
| `SUMMARY_TSV` | `<LOG_DIR>/fio_parameter_sweep_summary.tsv` | 汇总文件 |
| `RUN_G_CORRECTNESS` | `1` | 设 `0` 跳过 G 组 correctness，只跑 A–F 性能（省时间）|
| `LOG_LEVEL` | `error` | guest 内核日志级别。**勿设 verbose/trace**，否则单个 case 日志可达数百 MB |
| `BENCH_ASTER_SCHEME` | `null` | `null` = no-IOMMU 诊断口径 |
| `http_proxy` / `https_proxy` / `all_proxy` | `127.0.0.1:7890` | 容器内首次装 `cargo-osdk` 用 |

只跑性能、不跑 correctness：

```bash
RUN_G_CORRECTNESS=0 ./asterinas/test/initramfs/src/benchmark/fio/run_parameter_sweep_summary.sh
```

### 6.4 注意

- 全量（含 G）一轮约 2–3 小时；纯 A–F 明显更快。
- 固定口径：A–F 全程 `direct` 用 O_DIRECT、`EXT4_DIRECT_READ_CACHE=0`；page_cache 仅 D 组开启对照。
- `LOG_LEVEL` 默认 `error` 即可；曾出现过 656MB 的 verbose 日志，既超 GitHub 100MB 上限又无分析价值，不要提交。
- 最近一轮完整结果与三瓶颈分析见根目录 `fio_direct_parameter_sweep_report.md`（2026-05-18/19）。

## 6.5 宿主机 page cache 与 Linux 基线口径（drop_caches，Phase 5 起默认）

**现象**：同一配置下 Linux O_DIRECT 顺序读基线跨会话剧烈波动（1M read 2768 ↔ 4942 MB/s）。

**根因**：QEMU `-drive` 默认 `cache=writeback`，宿主机会缓存 backing image（`build/ext2.img`）；guest 侧 `direct=1`（O_DIRECT）**不绕过宿主机的 page cache**。fio 读测试先写出 1G 文件再读，读的是热在宿主机 RAM 里的数据，吞吐取决于"这 1G 有多少留在宿主机 cache"——随宿主机内存压力波动。**与 KVM 无关**（`kvm_intel: VMX not supported` 是 guest 嵌套虚拟化噪音，host 仍正常加速）。

**口径（Phase 5 起默认开）**：`bench_linux_and_aster.sh` 在每次 QEMU 启动前 `sync; echo 3 > /proc/sys/vm/drop_caches`（privileged 容器共享宿主内核），由 `BENCH_DROP_CACHES`（默认 `1`）控制，`=0` 可恢复旧 warm-cache 行为。仅影响 perf 路径（correctness 走 `cargo osdk run`，不经此脚本）。

**实测效果**（同套 A/B，1M read Linux 基线）：

| | Linux 1M read |
|---|---|
| 2 周前 sweep | 2768 |
| 不带 drop | 4942–5111（warm cache 虚高）|
| **带 drop（默认）** | **2818–2813**（复现 sweep、c0/c1 仅差 0.2%）|

**重要**：drop 后比较才公平——之前"Asterinas 1M read 49–61%"是被 Linux 的热 cache 坑了；公平测量下 **Asterinas 1M read = 104–126%**（追平/反超 Linux）。小块读（per-IO bound）受 host cache 影响小、波动主要是 per-IO 噪声，建议每档跑 3 次取中位数。更彻底的备选：QEMU `-drive cache=none`（宿主机也 O_DIRECT、永不缓存）。

## 6.6 Phase 5 读写优化结果与测量脚本

四个 ext4 优化（extent 映射缓存 / 全文件覆盖 / atime 按秒节流 / **inode 元数据缓存**）把 O_DIRECT 读写从 16–63% 拉到 **75–123%**。详见 `feature_perf_phase5_milestone.md`。

### 当前结果（`direct=1, nj=1`，cache-off + `extent_map_cache=1` + inode 缓存默认 on + `BENCH_DROP_CACHES=1`，中位数）

| bs | read ratio | write ratio |
|----|-----------:|------------:|
| 4K | 86.38% | 75.54% |
| 16K | 84.42% | 75.78% |
| 64K | 86.89% | 84.09% |
| 256K | 94.81% | 121.07% |
| 1M | 122.94% | 88.28% |

ext4 域内 per-op 固定开销（extent 查找 / atime stat / inode stat）已榨干；读写现都顶在 **virtio 设备往返**这个平台地板（跨 FS 通用，ext2 同此极限）。

### Phase 5 测量脚本（均默认 `BENCH_DROP_CACHES=1`）

| 脚本 | 用途 |
|------|------|
| `fio/run_phase5_guard_median.sh` | 单 job 守底 / bs 扫描，多轮取**中位数**，两边对照。env：`READ_BS_LIST` `WRITE_BS_LIST`（` ` 空格=跳过该方向）、`READ_JOB`/`WRITE_JOB`（可指向 `fio/ext2_seq_*` 做 ext2 对照）、`REPEATS`、`CASE_TIMEOUT_SEC`（每-case 超时防 hang）|
| `fio/run_phase5_ratio_ab.sh` | extent_map_cache `0 vs 1` 同轮 A/B（两边）；`RATIO_BS_LIST`/`RATIO_READ_JOB` 可调 |
| `fio/run_phase5_profile_probe.sh` | Asterinas-only 四层 profile（`phase2_profile=1` `LOG_LEVEL=warn`），收割 `[ext4-direct-write]`/`[ext4-profile] direct-read`/`[block-profile]`；`CASES` 选 `ext4j-{read,write,randread}-{4K..1M}` |
| `tools/ext4/run_phase5_regression.sh` | 守底回归：`FULL_SUITE=1` 跑完整套（crash+concurrency+fsync+全 xfstests+host-crash），`FULL_GUARD=1` 跑 phase4_good/phase3_base/jbd_phase1，均 drc=0 激活 extent+inode 缓存 |

## 7. 当前观察与说明

- ext4 已经按本轮要求对齐到 ext2 参数，不再使用此前的 `size=128M` 口径。
- 2026-05-14 起，6-test 综合诊断默认关闭 Asterinas IOMMU：`BENCH_ASTER_SCHEME=null`，并关闭 Asterinas PageCache：`EXT4_PAGE_CACHE=0` / `ext4fs.page_cache=0`、ext4 direct-read cache：`EXT4_DIRECT_READ_CACHE=0` / `ext4fs.direct_read_cache=0`；2026-05-09 的 IOMMU-on 结果与 2026-05-06 的 cache-on 结果仅作为历史对照，不再作为默认综合测试口径。
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

## 口径说明：virtio 后端 / device flush / 裸盘对照（2026-06-09）

- **virtio 后端是文件镜像**：`-drive if=none,format=raw,file=test/initramfs/build/ext2.img`，host 端是文件 `ext2.img`，**非物理裸盘**。"裸盘"测试 = fio 直接写 virtio-blk 设备 `/dev/vda`（绕过 FS），符合赛题"统一 virtio-blk 接口"。Aster（`qemu_args.sh normal`）与 Linux（`bench_linux_and_aster.sh`）用同一文件、同一份 virtio-blk-pci 配置（逐字节相同）。
- **device flush 公平**：`cache=writeback` 默认 → QEMU advertise `VIRTIO_BLK_F_FLUSH` → Aster `support_flush=true`，日志实证 `virtio-blk: flush() support_flush=true → sending ReqType::Flush to device`，**真发 flush 非 no-op**，与 Linux 公平。
- **裸盘 vs ext4 对照必须同 run**：raw 与 ext4 的 fio job 仅 `-filename` 不同，其余完全相同。**不得混用 ext4 单次值 + raw 中位数**（曾犯此错）。同一次 sweep 同 run 数据显示 Asterinas 侧 raw < ext4（4K：132<180）——Linux 侧 raw>ext4（理论），Asterinas 倒挂是因**裸块设备写路径未享 Phase 5 ext4 direct-write 优化、per-request 开销更高**。故"裸盘地板"偏悲观，ext4 绝对吞吐更代表真实设备能力。详见 `docs/fio_direct_parameter_sweep_report_phase5.md` §8.5.1。
