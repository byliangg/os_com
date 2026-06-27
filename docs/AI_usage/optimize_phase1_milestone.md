# ext4 fio_read 测试结果与优化方向

## 1. 测试结果

### 1.1 最终性能结果

| 测试项 | 基线 | 最终 | 变化 |
| --- | ---: | ---: | ---: |
| ext4 顺序读带宽 | Asterinas 3180 MB/s / Linux 4826 MB/s，65.89% | Asterinas 4870 MB/s / Linux 5084 MB/s，95.79% | +29.9 pp |
| ext4 顺序写带宽 | Asterinas 3074 MB/s / Linux 2683 MB/s，114.57% | Asterinas 2651 MB/s / Linux 2930 MB/s，90.48% | 仍高于 80% 目标 |

fio 参数口径：

```text
size=1G
bs=1M
ioengine=sync
direct=1
numjobs=1
fsync_on_close=1
time_based=1
ramp_time=60
runtime=100
```

最终结果中，Asterinas ext4 顺序读达到 Linux 的 95.79%，已经超过 80% 目标；顺序写为 Linux 的 90.48%，也超过 80% 目标。

### 1.2 关键 profiling 结果

| 观察项 | 结果 |
| --- | --- |
| ext4 direct-read 主耗时 | `bio_waiter.wait()` / device completion wait |
| guest queue / dispatch 开销 | 很低，通常约 `0-2us` |
| large read bio device wait | 约 `196-245us` |
| IRQ delivery | 几乎覆盖 device wait 的主要部分 |
| copy 开销 | 约 `57-66us` |
| mapping / plan 开销 | 最终约 `39us` |
| cache miss | 放大 planning window 后明显下降 |

瓶颈不是 memcpy，也不是 ext4 mapping cache miss，而是 `I/O wait + copy` 串行导致设备等待无法被隐藏。



## 2. 结论

1. ext4 fio 顺序读优化目标已经达成：Asterinas 从 Linux 的 65.89% 提升到 95.79%。
2. 当前顺序读的核心问题不是单个函数过慢，而是原始路径中 `I/O wait` 和 `copy` 串行执行，无法重叠。
3. speculative readahead + single-slot pipeline 是有效主线，它通过提前规划和提前提交下一次读，把当前 copy 与下一次 I/O 等待部分重叠。
4. queue wait 可以通过 scoped fast-submit 降低，但 fast-submit 必须限制在 speculative request；如果覆盖所有大 read，吞吐会回退。
5. zero-copy DMA 当前不是有效方向。它减少了 copy，但引入了更高的 scatter/gather 和 virtio 队列成本，整体性能下降。
6. 写带宽最终为 Linux 的 90.48%，虽然相比基线比例下降，但仍明显高于 80% 目标，没有成为本阶段阻塞项。

## 3. 优化方向

### 3.1 短期可保留和完善

1. 保留 single-slot speculative readahead，不继续盲目增加 pipeline depth。
2. 保留 scoped fast-submit，只作用于 speculative direct-read request。
3. 继续保持较大的 direct-read planning window，减少顺序读 cache miss。
4. 保留 fio read/write 和 phase3/phase4/phase6 回归测试，防止读路径优化影响写路径或文件系统正确性。

### 3.2 后续可继续探索

1. 继续分析 virtio / host I/O completion wait，当前最大剩余空间在 device wait 和 IRQ delivery 链路。
2. 如果要重新评估 zero-copy，需要先降低 scatter/gather segment 数量，否则很难抵消 virtio 队列成本。
3. Step 2 的 cache miss 优化和 Step 3 的 per-read Mutex / Vec 优化收益预计较小，可以作为后续小优化处理。
4. 后续优化应优先保证 `O_DIRECT read` 的顺序场景稳定，不应为了局部 latency 指标牺牲整体吞吐。
