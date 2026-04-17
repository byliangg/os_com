# Asterinas EXT4 Benchmark 最新结果快照

更新时间：2026-04-17（Asia/Shanghai）

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
- Asterinas：`2651 MB/s`
- Linux：`2930 MB/s`
- ratio：`90.48%`
- 结果文件：`asterinas/result_fio-ext4_seq_write_bw.json`

### 2.4 ext4 顺序读

- job：`fio/ext4_seq_read_bw`
- Asterinas：`4870 MB/s`
- Linux：`5084 MB/s`
- ratio：`95.79%`
- 结果文件：`asterinas/result_fio-ext4_seq_read_bw.json`

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

## 5. 当前观察与说明

- ext4 已经按本轮要求对齐到 ext2 参数，不再使用此前的 `size=128M` 口径。
- 这几轮 Linux 对照侧都出现了 `kvm_intel: VMX not supported by CPU 0`。
- 因此当前结果适合用于“本地最新观测值”和方案推进参考。
- 如果后续要写正式 milestone 或对外结论，建议同时记录该环境现象，避免把 Linux 对照侧异常忽略掉。

## 6. 对应仓库内文档

- 根目录：`benchmark.md`
- 仓库内同步副本：`asterinas/benchmark/benchmark.md`
