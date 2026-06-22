# ext4 JBD2 Phase 3 预研测试记录

首次更新时间：2026-05-06（Asia/Shanghai）

## 1. 目的

这份文档记录进入 `feature_jbd2_phase3` 前的两轮补充测试与初步分析，目标是回答两个问题：

1. 当前 fio write 低于 90% 的主要瓶颈是否在 JBD2。
2. `fsync` / flush / 持久化语义是否已经与 Linux 等价，还是仍有功能风险需要单独收口。

本文只记录新增预研测试，不替代当前正式 benchmark 基线。正式基线仍以 `benchmark.md` 当前快照为准。

## 2. 测试一：6-test 综合复跑

### 2.1 口径

沿用现有 6-test 诊断脚本：

```bash
cd /home/lby/os_com_codex
KEEP_LOGS=1 ./asterinas/test/initramfs/src/benchmark/fio/run_6test_summary.sh
```

统一 fio 参数：

```bash
-size=1G -bs=1M
-ioengine=sync -direct=1 -numjobs=1 -fsync_on_close=1
-time_based=1 -ramp_time=60 -runtime=100
```

日志目录：

- `/tmp/6test-fio-summary.3gtxga`

### 2.2 结果

| case | Asterinas | Linux | ratio |
|------|----------:|------:|------:|
| raw_read | 2334 MB/s | 4552 MB/s | 51.27% |
| raw_write | 1379 MB/s | 3362 MB/s | 41.02% |
| ext4_journaled_read | 5331 MB/s | 2025 MB/s | 263.26% |
| ext4_journaled_write | 1337 MB/s | 2069 MB/s | 64.62% |
| ext4_nojournal_read | 5243 MB/s | 2367 MB/s | 221.50% |
| ext4_nojournal_write | 1499 MB/s | 2457 MB/s | 61.01% |

### 2.3 初步结论

1. write 侧仍然不是 JBD2 主导瓶颈。
2. Asterinas `raw_write / ext4_journaled_write / ext4_nojournal_write` 三者都落在 `1.3-1.5 GB/s` 量级。
3. nojournal 相比 journaled 只高约 `12%`，说明日志层不是决定性瓶颈。
4. 真正的主瓶颈仍更像在 block / virtio / sync write 路径。
5. read 侧 Linux 对照明显异常，本轮 6-test 更适合做方向判断，不适合替代正式 baseline。

## 3. 测试二：`bs=16K` + `fsync=4` 写入测试

### 3.1 背景

按学长建议，补跑更小单次写入量和更高同步频率的写测试：

- `bs=16K`
- 每写 4 次执行一次 `fsync`

为避免污染现有正式 benchmark 配置，本轮新增了临时 write-only 预研脚本：

```bash
cd /home/lby/os_com_codex
KEEP_LOGS=1 bash ./asterinas/test/initramfs/src/benchmark/fio/run_write_16k_fsync4_summary.sh
```

日志目录：

- `/tmp/write-16k-fsync4.MgU98s`

### 3.2 结果

| case | Asterinas | Linux | ratio |
|------|----------:|------:|------:|
| raw_write_16k_fsync4 | 405 MB/s | 26 MB/s | 1545.80% |
| ext4_journaled_write_16k_fsync4 | 140 MB/s | 16 MB/s | 858.90% |
| ext4_nojournal_write_16k_fsync4 | 145 MB/s | 27 MB/s | 531.14% |

### 3.3 `sync` 延迟摘要

| case | Asterinas `sync avg` | Linux `sync avg` |
|------|---------------------:|-----------------:|
| raw_write_16k_fsync4 | 302 ns | 1913.51 us |
| ext4_journaled_write_16k_fsync4 | 50.13 us | 3337.87 us |
| ext4_nojournal_write_16k_fsync4 | 34.85 us | 1848.48 us |

### 3.4 初步结论

1. 这轮测试的主导成本已经不是顺序写吞吐，而是 `fsync` 成本。
2. Linux 侧掉到 `16-28 MB/s` 与毫秒级 `sync` 延迟是自洽的。
3. Asterinas 侧 `sync` 延迟只有纳秒到几十微秒，不像做了与 Linux 同等级别的持久化同步。
4. 这一轮更像是在暴露 `fsync` / flush 语义差异，而不是证明 Asterinas 写性能显著优于 Linux。

## 4. 代码侧观察

### 4.1 raw block file 的 `fsync` 路径风险

`sys_fsync` 走的是：

- `kernel/src/syscall/fsync.rs`
- `path.sync_all()`

而 devtmpfs / systree 默认 inode 的：

- `kernel/src/fs/utils/systree_inode.rs`
- `kernel/src/fs/utils/inode.rs`

都提供了默认 `sync_all() -> Ok(())` / `sync_data() -> Ok(())`。

当前 `BlockFile` / `OpenBlockFile` 位于：

- `kernel/src/device/registry/block.rs`

实现了 `read_at` / `write_at` / `ioctl`，但没有看到专门覆盖的 `sync_all` / `sync_data` 行为。

这与 raw case 观测到的 `sync avg = 302 ns` 是一致的：`/dev/vda` 上的 `fsync=4` 很可能没有真正走到底层 flush。

### 4.2 ext4 regular-file 的 `fsync` 路径不是 Linux 等价语义

当前 ext4 regular-file `fsync` 关键逻辑位于：

- `kernel/src/fs/ext4/fs.rs`

`fsync_regular_file()` 的注释明确说明：

1. 不执行 `block_device.sync()`
2. 依赖当前 virtio-blk 栈上的同步 DMA
3. 依赖写序而非每次 `fsync` 都做全设备 flush

实现上主要是：

- `commit_pending_jbd2_transactions()`
- checkpoint depth 足够大时尝试 batch checkpoint

这也与测试结果一致：

1. journaled/nojournal 的 `sync avg` 只有 `35-50 us`
2. 明显低于 Linux 的 `1.8-3.3 ms`

因此，即便 ext4 crash matrix 目前通过，也不能直接把这条路径解释为“已与 Linux `fsync` 等价”。

### 4.3 virtio block flush feature 仍需复核

当前 virtio block feature 判断中还有一个需要单独复核的点：

- `kernel/comps/virtio/src/device/block/mod.rs`

其中 `support_flush` 的判断写法为：

```rust
transport.read_device_features() & BlockFeatures::FLUSH.bits() == 1
```

而 `FLUSH` 位不是 bit 0。这个判断本身就可疑。

同时：

- `kernel/comps/virtio/src/device/block/device.rs`

里的 `flush()` 会根据 `support_flush` 分支决定是“直接 complete”还是“发 `ReqType::Flush` 请求”。

这不一定是当前现象的唯一根因，但值得在 Phase 3 一并审计。

## 5. 综合判断

### 5.1 关于性能

1. 当前 fio write 低于 90% 的主瓶颈，仍更像在 block / virtio / sync write 路径。
2. 单靠优化 JBD2，不太可能把 with-journal write 从当前水平拉到 90%。

### 5.2 关于功能

1. 不能据此说“现有 ext4 / JBD2 整体功能已经坏掉”。
2. 现有 `phase3 / phase4 / phase6 / jbd_phase1 / crash / concurrency baseline` 仍说明大部分 correctness 基线是通的。
3. 但也不能再轻易说“功能已完全达标，只剩优化和文档”。
4. 至少在 `fsync` / flush / 持久化语义这条线上，当前实现仍有明显风险，需要单独收口。

## 6. 建议作为 Phase 3 的起点

建议 `feature_jbd2_phase3` 首先聚焦持久化语义核查，而不是先做 write 吞吐优化：

1. 明确 `/dev/vda` 的 `fsync` 是否真正下发到底层 `block_device.sync()`
2. 明确 ext4 regular-file `fsync` 当前语义边界，与 Linux 的差异是否可接受
3. 复核 virtio block flush feature 判断与 flush 请求路径
4. 在语义核清前，不把 `bs=16K + fsync=4` 的结果用于性能宣传或优秀档结论

## 7. 附：本轮新增临时测试资产

本轮预研新增了以下临时 benchmark 资产，便于 Phase 3 继续复现：

- `asterinas/test/initramfs/src/benchmark/fio/raw_seq_write_bw_16k_fsync4/`
- `asterinas/test/initramfs/src/benchmark/fio/ext4_seq_write_bw_16k_fsync4/`
- `asterinas/test/initramfs/src/benchmark/fio/ext4_nojournal_seq_write_bw_16k_fsync4/`
- `asterinas/test/initramfs/src/benchmark/fio/run_write_16k_fsync4_summary.sh`

这些资产仅用于预研，不代表当前正式 benchmark 口径。
