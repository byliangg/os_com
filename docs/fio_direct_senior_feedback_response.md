# fio direct 参数测试：对学长反馈的逐项回应

## 0. 总体态度

学长的反馈是对的：当前报告不能只说 `bs=1M`、`numjobs=4` 下能达标，也不能把 B 组里 ext4/raw 的派生比例简单解释成“ext4 没有代价”。更合理的说法应该是：

- B 组的 1M 大块 direct I/O 只能说明“大块顺序吞吐下 ext4 额外成本被摊薄”，不能说明文件系统没有成本。
- C 组小块 `4K~64K` 数据更能暴露 ext4 direct I/O 的真实开销，是答辩时必须展示的数据。
- Linux raw 与 Linux ext4 的差距是正常现象，成熟文件系统在挂载后经过 VFS、extent、inode、journal、flush 等路径，本来就应有代价。
- Asterinas ext4 在某些大块场景下接近 raw，可能既有实现较轻、请求更连续的原因，也可能有 flush / barrier / 测试口径差异，需要谨慎解释。

## 1. B 组里为什么挂载 ext4 后比没挂载 raw 还快一些？

B 组数据：

| 指标 | Asterinas MB/s |
|------|---------------:|
| raw write | 1748.0 |
| ext4 journaled write | 1839.0 |
| ext4 nojournal write | 1852.0 |
| raw read | 2710.0 |
| ext4 journaled read | 2797.0 |
| ext4 nojournal read | 2786.0 |

派生比例确实出现了 ext4/raw 大于 100%：

| 指标 | 比例 |
|------|------:|
| ext4 journaled write / raw write | 105.21% |
| ext4 nojournal write / raw write | 105.95% |
| ext4 journaled read / raw read | 103.21% |
| ext4 nojournal read / raw read | 102.80% |

这个现象不能解释为“ext4 没有代价”或“ext4 比裸设备更快”。更稳妥的解释是：

1. **B 组是单轮 1M 大块顺序 direct I/O，文件系统开销被摊薄。**  
   单次 I/O 1MB 时，extent lookup、inode lock、journal handle 等固定成本占比很小。

2. **raw 路径和 ext4 文件路径不是完全同一条代码路径。**  
   raw `/dev/vda` 走块设备文件路径，ext4 regular file 走文件系统映射后再提交 bio。Asterinas 当前 raw 路径本身可能不是最高效路径，因此 ext4 略高于 raw 不代表 ext4 “负开销”。

3. **这个现象在其他组并不稳定。**  
   C/E/F 组的 1M 单 job 写里，ext4 多数没有稳定超过 raw：

   | 组别 | raw write | ext4 journaled write | ext4/raw |
   |------|----------:|---------------------:|---------:|
   | B 组 1M | 1748.0 | 1839.0 | 105.21% |
   | C 组 1M | 1904.0 | 1773.0 | 93.12% |
   | E 组 1M none | 2119.0 | 1819.0 | 85.84% |
   | F 组 1M nj1 | 2228.0 | 1800.0 | 80.79% |

结论：B 组 ext4 略快于 raw 更像是大块顺序测试下的路径差异和单轮波动，不能作为“ext4 无代价”的证据。报告里应该把它写成“B 组不能单独说明 ext4 开销，必须结合 C/F 组解释”。

## 2. Linux 里挂载 ext4 后读写性能都会降低，为什么？

B 组 Linux 数据：

| case | Linux MB/s |
|------|-----------:|
| raw write | 3566.0 |
| raw read | 4780.0 |
| ext4 journaled write | 3342.0 |
| ext4 journaled read | 2784.0 |
| ext4 nojournal write | 3291.0 |
| ext4 nojournal read | 3748.0 |

Linux 挂载 ext4 后低于 raw 是正常的。原因包括：

1. **文件系统层有固定开销。**  
   raw block 只需要按块设备偏移读写；ext4 需要 VFS、inode、extent mapping、权限/状态维护、mtime/ctime、文件大小、对齐检查等。

2. **O_DIRECT 仍然不是“没有文件系统”。**  
   `direct=1` 绕过 PageCache，但不会绕过文件系统。ext4 仍要把文件 offset 翻译成磁盘 block，并处理稀疏文件、extent、分配状态、inode 状态等。

3. **写路径还会受 journal / ordered mode / flush 影响。**  
   即使 nojournal，文件系统也有 metadata、allocation 和 inode 更新；journaled 模式还需要事务、提交顺序和持久化约束。

4. **Linux ext4 是完整工业级实现，语义更重。**  
   它处理大量边界：barrier、writeback、quota、discard、extent status tree、delalloc、block plugging、错误恢复等。性能下降是完整语义的代价。

所以 Linux 挂载后降低不是异常，反而是我们解释 Asterinas 数据时应该参考的“正常文件系统成本基线”。

## 3. 看起来我们的 ext4 一点代价都没有，怎么解释？

不能这样解释。当前数据说明的是：

- 在 `bs=1M` 大块顺序场景，Asterinas ext4 的额外开销有时被摊薄；
- 在 `4K~64K` 小块场景，Asterinas ext4 开销非常明显；
- journaled 和 nojournal 差距小，只说明 JBD2 不是当前唯一瓶颈，不说明 ext4 没有成本。

C 组同系统内 ext4/raw 比例能直接证明 ext4 有明显成本：

| bs | journaled write/raw | journaled read/raw | nojournal write/raw | nojournal read/raw |
|----|--------------------:|-------------------:|--------------------:|-------------------:|
| 4K | 36.67% | 17.84% | 37.24% | 17.92% |
| 16K | 38.70% | 18.72% | 38.70% | 18.72% |
| 64K | 47.26% | 23.60% | 47.11% | 23.53% |
| 256K | 67.19% | 39.20% | 68.52% | 38.63% |
| 1M | 93.12% | 109.31% | 98.63% | 102.36% |

这张表是答辩时更应该展示的：它说明小块 I/O 下文件系统成本很明显，1M 大块只是把成本摊薄了。

## 4. flush 指令问题怎么解释？

学长说的点很关键：设备 flush / barrier 会暂停或约束设备上的 I/O 顺序，成本很高。如果一个系统没有真正执行 flush，速度快是合理的，但那不是同等语义下的快。

这里要分两层说：

1. **普通 direct 吞吐测试不是每次 I/O 都 flush。**  
   B/C/F 组主要是 `fsync=none`，只有 `fsync_on_close=1`。因此它测的是普通顺序 direct throughput，不是每次写都持久化的 durable write。

2. **fsync-heavy 结果必须单独解释。**  
   E 组就是为了观察 `fsync=4/16/64` 的成本。里面 Asterinas 有时超过 Linux，不能直接宣传为普通吞吐达标，因为 Linux 的同步持久化成本可能更高。

当前报告应该这样表述：

- 普通 direct 吞吐和 fsync/flush 语义测试必须分开。
- 如果某组结果 Asterinas 明显高于 Linux，尤其是 fsync-heavy 场景，要优先检查 flush 是否等价、barrier 是否真正到设备、host-crash fsync matrix 是否通过。
- 本轮 G 组 host-crash fsync matrix 是 4/4 PASS，说明当前持久化语义有守底，但性能解释仍不能把 fsync-heavy 和普通吞吐混在一起。

## 5. C 组 `4K~64K` 小块 I/O 为什么重要？

小块 I/O 是文件系统性能研究和评测里非常常见的重点，不应被一句“1M 是稳定代表点”带过去。

原因：

1. **很多真实 workload 是小 I/O。**  
   数据库、日志、编译、包管理、metadata-heavy 场景、随机读写、目录操作，都不是纯 1M 顺序流。

2. **小 I/O 更能暴露文件系统固定成本。**  
   4K/16K 下，每次 I/O 都要经过 offset translation、extent lookup、lock、bio prepare、journal/accounting 等路径，固定成本无法被 1MB 数据量摊薄。

3. **我们的数据确实显示小块是短板。**  
   C 组 `4K~64K` 下，ext4 journaled write 只有 raw 的 `36.67%~47.26%`，read 只有 raw 的 `17.84%~23.60%`。这比 1M 更能定位 ext4 direct I/O 的瓶颈。

结论：后续报告应该把 C 组小块结果提升为核心分析之一，而不是只放在附录。

## 6. 现在测 `bs=1M` 是否看不出瓶颈？

只测 `bs=1M` 确实看不出很多瓶颈。

`bs=1M` 的价值是：

- 代表大块顺序吞吐；
- 方便和官方 fio 顺序读写口径对齐；
- 能观察 block/virtio 的带宽上限；
- 能展示成熟顺序路径是否跑通。

但 `bs=1M` 的不足是：

- 会把 extent lookup、inode lock、journal handle、bio setup 等固定成本摊薄；
- 不容易暴露小 I/O 下的 per-op 开销；
- 不能解释为什么 `4K~64K` 下 ext4 只有 raw 的一小部分；
- 对评委追问“文件系统真实开销在哪里”回答不够有力。

所以正确做法是：`bs=1M` 作为顺序吞吐主线保留，同时必须展示 `4K/16K/64K/256K` 的细粒度趋势。

## 7. “细粒度 I/O 不重要”这类说法是否应该避免？

应该避免。更准确的说法是：

- 大块顺序 I/O 是比赛 fio 吞吐目标的重要口径；
- 小块 I/O 是文件系统评测、研究和真实 workload 的重要口径；
- 两者回答的问题不同，不能互相替代。

答辩时如果只展示 1M，很容易被问：

- 4K direct read/write 怎么样？
- metadata 和 extent lookup 成本在哪里？
- 和 raw block 的差距是否随 bs 缩小而扩大？
- 多线程提升是否只对大块顺序有效？

因此细粒度数据不仅要展示，还要主动解释。

## 8. 多线程已经完成，是否说明 ext4 整体框架差不多了？

F 组确实给了正面信号：

| case | Asterinas MB/s | Linux MB/s | ratio |
|------|---------------:|-----------:|------:|
| raw write, numjobs=1 | 2228.0 | 3828.0 | 58.20% |
| raw write, numjobs=2 | 4611.0 | 4422.0 | 104.27% |
| raw write, numjobs=4 | 4628.0 | 4319.0 | 107.15% |
| ext4 journaled write, numjobs=1 | 1800.0 | 3345.0 | 53.81% |
| ext4 journaled write, numjobs=4 | 3824.0 | 4025.0 | 95.01% |

这说明：

- direct write 的底层吞吐能力是有的；
- 单 job 慢主要不是“完全写不动”，而是队列深度、请求 overlap、同步提交路径不够；
- ext4 journaled write 在 `numjobs=4` 达到 95.01%，说明整体 direct-write 框架已经接近可用。

但也要保留两个风险：

- ext4 read 多 job 反而下降，说明读路径并发扩展性还要查；
- G 组 `phase6_good generic/011` 有 1 项 output mismatch，需要复测，不能完全忽略。

所以可以说“框架已接近成熟，下一步重点从能不能跑转向如何解释和优化不同参数下的表现”，但不能说已经没有问题。

## 9. 4M journaled write 92.19% 为什么不建议作为核心宣传点？

C 组里 `C-W-ext4j-4M`：

| case | Asterinas MB/s | Linux MB/s | ratio |
|------|---------------:|-----------:|------:|
| C-W-ext4j-4M | 1760.0 | 1909.0 | 92.19% |
| C-W-ext4n-4M | 1836.0 | 3361.0 | 54.63% |
| C-W-raw-4M | 1875.0 | 3607.0 | 51.98% |

这里 journaled 4M 的 ratio 高，主要是因为该轮 Linux journaled write 只有 1909.0 MB/s，明显低于同组 Linux raw 和 nojournal。Asterinas 自身 1760.0 MB/s 并没有比 1M 或 nojournal 更突出。

因此这个点不适合作为核心宣传点。更稳妥的写法是：

- 4M journaled write 出现过 92.19%，但该点受 Linux 分母偏低影响；
- 它可以作为“现象记录”，不能作为“稳定达标证据”；
- 稳定达标证据应优先用 F 组 `bs=1M,numjobs=4`，因为 raw 和 ext4 都能解释出队列深度收益。

## 10. 建议修改原报告的表达

建议把原报告里的几个表述调整为：

1. B 组分析增加一句：  
   “B 组 ext4/raw 超过 100% 不代表 ext4 无代价；该现象在 C/E/F 组不稳定，可能来自 raw 路径偏低、大块 I/O 摊薄开销和单轮波动。”

2. C 组分析加强：  
   “`4K~64K` 是暴露 ext4 direct I/O 开销的关键区间，journaled/nojournal 均只有 raw 的约 18%~47%，说明小 I/O 是后续优化和答辩解释重点。”

3. 4M 结论改弱：  
   “4M journaled write ratio 高主要受 Linux 对照偏低影响，仅作现象记录，不作为核心达标宣传点。”

4. 汇总结论增加：  
   “大块顺序 I/O 和小块细粒度 I/O 是不同维度；1M 用于官方顺序吞吐，4K~64K 用于解释文件系统开销。”

## 11. 我的见解

我的判断是：学长这次提醒的核心不是否定我们的测试，而是提醒我们把“性能好”讲得更像文件系统工程，而不是像单点跑分。

现在最有说服力的叙事应该是：

1. **我们已经做出了一个能跑复杂 correctness 的 ext4。**  
   crash、JBD、PageCache、phase3/phase4 多数回归都守住了，三个月做到这一步确实不容易。

2. **大块顺序 direct I/O 已经有成熟迹象。**  
   `numjobs=4` 下 journaled write 到 95.01%，single-job read 超 100%，说明主路径不是完全失败。

3. **真正需要解释和优化的是小 I/O 与语义成本。**  
   `4K~64K` 下 ext4/raw 差距很大；fsync/flush 场景必须和普通吞吐分开；这些才是评委更可能追问的点。

4. **后续优化应从“刷高 1M 分数”转向“解释并改善细粒度开销”。**  
   建议下一步做小块 direct I/O profile：extent lookup、inode correctness lock、JBD handle 创建、bio submit 次数、block allocation、virtio queue overlap。最好补一个 `bs=4K/16K/64K` + `numjobs=1/2/4` 的小矩阵，专门回答“小 I/O 能不能靠并发补回来”。

一句话总结：  
当前 ext4 的大块顺序框架已经比较像样了，但答辩真正要讲清楚的是：为什么 Linux ext4 有代价、为什么我们 1M 下代价被摊薄、为什么小块下代价又显著出现，以及后续如何针对这些代价优化。
