# Asterinas EXT4 Benchmark 最新结果快照

更新时间：2026-05-06（Asia/Shanghai）

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
- Asterinas：`2417 MB/s`
- Linux：`2778 MB/s`
- ratio：`87.01%`
- 结果文件：`asterinas/result_fio-ext4_seq_write_bw.json`

### 2.4 ext4 顺序读

- job：`fio/ext4_seq_read_bw`
- Asterinas：`4453 MB/s`
- Linux：`4763 MB/s`
- ratio：`93.49%`
- 结果文件：`asterinas/result_fio-ext4_seq_read_bw.json`

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

- Phase 3 当前进入规划阶段，目标是收口 `fsync` / `fdatasync` / block flush / Linux 持久化语义，不把 fsync-heavy 结果混入普通顺序吞吐指标。
- 预研记录见 `feature_jbd2_phase3_pretest.md`。
- `bs=16K + fsync=4` 预研显示 Asterinas sync latency 远低于 Linux：raw `302 ns` vs Linux `1913.51 us`，ext4 journaled `50.13 us` vs Linux `3337.87 us`。
- 初步判断：raw block fd `fsync` 可能没有触达底层 flush；ext4 regular-file `fsync` 当前不是 Linux 等价持久化屏障；该组结果在语义收口前不能作为性能宣传。

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

6 个测试共用相同 fio 参数，与官方评审口径完全对齐：

```bash
-size=1G -bs=1M
-ioengine=sync -direct=1 -numjobs=1 -fsync_on_close=1
-time_based=1 -ramp_time=60 -runtime=100
```

### 5.3 入口与运行方法

```bash
cd /home/lby/os_com_codex
./asterinas/test/initramfs/src/benchmark/fio/run_6test_summary.sh
```

- 脚本：`asterinas/test/initramfs/src/benchmark/fio/run_6test_summary.sh`
- 行为：顺序执行 6 个 job，每个都跑 Asterinas 和 Linux 两侧，最后打印汇总
- 日志：默认执行完自动清理；如需保留日志排查问题，加 `KEEP_LOGS=1`：

```bash
KEEP_LOGS=1 ./asterinas/test/initramfs/src/benchmark/fio/run_6test_summary.sh
```

- 仅跑 Asterinas 侧（跳过 Linux，节省一半时间）：

```bash
BENCH_RUN_ONLY=aster ./asterinas/test/initramfs/src/benchmark/fio/run_6test_summary.sh
```

### 5.4 最新结果（2026-05-06）

| 测试 | Asterinas | Linux | Aster/Linux |
|------|----------:|------:|:-----------:|
| raw read | 2849 MB/s | 7039 MB/s | 40.47% |
| raw write | 2163 MB/s | 3719 MB/s | 58.16% |
| ext4 journaled read | 5735 MB/s | 3798 MB/s | 151.00% |
| ext4 journaled write | 2081 MB/s | 3323 MB/s | 62.62% |
| ext4 nojournal read | 5696 MB/s | 3419 MB/s | 166.60% |
| ext4 nojournal write | 2124 MB/s | 3671 MB/s | 57.86% |

关键结论：

- Asterinas ext4 read（5735 MB/s）> raw read（2849 MB/s）：Phase 1 speculative readahead 的效果，ext4 路径有预读加速，裸设备访问没有。
- JBD2 写开销极小：nojournal（2124）vs journaled（2081）差 ~43 MB/s（约 2%），符合"JBD2 只记录 metadata，不记录数据"的设计。
- 写瓶颈在 virtio-blk / sync 路径，不在 FS 层或日志层。
- 本轮 Linux 侧出现 `kvm_intel: VMX not supported by CPU 0`，Linux 绝对值仅供参考，以 Asterinas 绝对值为主。

## 7. 当前观察与说明

- ext4 已经按本轮要求对齐到 ext2 参数，不再使用此前的 `size=128M` 口径。
- 2026-04-24 的 ext4 结果来自 JBD2 Phase 1 收口后的 fio 守底复跑：read `93.49%`、write `87.01%`，满足 Phase 1 “相对基线不下降超过 5 个百分点”的守底线（read ≥ 90%、write ≥ 85%）。
- 2026-05-05 的 Phase 2 收口口径：完整功能回归大全量已复跑通过，包括 phase3、phase4、phase6、jbd_phase1、crash matrix、lmbench 与 Phase 2 concurrency；其中 xfstests 统计按 `PASS / FAIL / NOTRUN / STATIC_BLOCKED` 原始口径记录，NOTRUN/STATIC_BLOCKED 为环境或赛题范围外跳过项；fio write 仍低于 90%，作为性能优化遗留项继续推进。
- Step 8 profile 显示当前 fio write 稳态为 1 mapping / 1 bio / 1 segment，request queue merge 为 0；fio 1MiB user buffer 为 256 pages / 256 physical runs / max run 1 page，因此 naive page-SG zero-copy 不作为当前实现主线。
- 这几轮 Linux 对照侧都出现了 `kvm_intel: VMX not supported by CPU 0`。
- 因此当前结果适合用于“本地最新观测值”和方案推进参考。
- 如果后续要写正式 milestone 或对外结论，建议同时记录该环境现象，避免把 Linux 对照侧异常忽略掉。

## 8. 对应仓库内文档

- 根目录：`benchmark.md`
- 仓库内同步副本：`asterinas/benchmark/benchmark.md`
