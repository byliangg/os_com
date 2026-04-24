# Asterinas ext4 fio_read 优化计划 Phase 1

## 目标与最终结果

将 `ext4_seq_read_bw` 从当前 65.89%（3180 / 4826 MB/s）提升到 >= 80%。

**只聚焦 fio_read，不做 fio_write / lmbench 的改动。**
**Phase 1 的默认优化口径改为”吞吐优先”而不是”单次 read 时延优先”。**

### Phase 1 最终验收（2026-04-17）

| 测试项 | Asterinas | Linux | 比值 | 目标 | 结论 |
|--------|-----------|-------|------|------|------|
| ext4_seq_read_bw | 4870 MB/s | 5084 MB/s | **95.79%** | >= 80% | ✅ 达成 |
| ext4_seq_write_bw | 2651 MB/s | 2930 MB/s | **90.48%** | 不回归 | ✅ 达成 |
| phase3_base | 100% | - | - | PASS | ✅ |
| phase4_good | 100% | - | - | PASS | ✅ |
| phase6_good | 100% | - | - | PASS | ✅ |

**Phase 1 已完成，可进入 Phase 2。**

这意味着：
- 首要目标是把 `MB/s` 拉上去，把设备持续喂满
- `avg_wait_us`、`avg_copy_us` 仍然要看，但只作为解释吞吐变化的辅助指标
- 凡是只能改善单次 read 暴露时延、却不能提高 sustained throughput / in-flight depth 的改动，都不再作为默认主线

## 当前结论修正（2026-04-16）

“用户页直接做 DMA 目标”的零拷贝路线目前改为`待定`，不再作为 Phase 1 的默认主线。

原因是这轮原型验证的实际 benchmark 结果明显回归：
- 原稳定基线：Asterinas 约 `3180 MB/s`
- 零拷贝原型：Asterinas 实测约 `812 MiB/s`
- 加连续性阈值后的折中版：Asterinas 实测约 `1594 MiB/s`

当前判断是：fio 的用户 buffer 在物理页上不够连续，zero-copy 读会拆成过多 scatter/gather segment；在当前 virtio-blk 队列条件下，这部分 SG/bio 开销大于省掉的 `memcpy`，所以总吞吐下降。

因此，后续只有在补充连续性/segment 分布打点，并证明可以稳定避免碎片化时，才重新打开这条路线。当前仓库代码已回退到实验前的稳定 direct-read 路径。

本日继续完成了 `Step 0` 的真实路径打点，结论比前一版更明确：
- 单边打点 benchmark 实测：`READ: bw=2923MiB/s (3065MB/s)`，日志见 `asterinas/.tmp_ext4_seq_read_bw_profile.log`
- direct-read 稳态统计：`avg_wait_us=186`，`avg_copy_us=54`
- `plan/alloc/submit` 都接近 0-1μs 量级
- `cache_miss=3 / 458752 reads`，几乎可以忽略
- `avg_mappings_x100=97`、`max_mappings=2`，说明当前稳定路径并不碎，绝大多数请求只有 1 个 mapping

这说明当前 ext4 direct-read 主路径里，最重的不是 `memcpy`，而是 bio 完成等待时间；单纯继续做 ext4 层 bookkeeping 优化，收益空间已经明显小于最初估计。

进一步的 block / virtio 分层打点也已经完成，日志见 `asterinas/.tmp_ext4_seq_read_bw_block_profile_reset.log`。当前额外确认到：
- software queue wait 约 `1μs`
- dispatch 约 `1-2μs`
- driver 侧可见的 `device_wait` 稳态约 `91μs`
- read bio 的 `avg_bytes` 在 benchmark 稳态收敛到约 `339KB`

这意味着当前差距已经不在 ext4 自己的 `plan/alloc/submit` 上，下一步更值得优先查的是：
- 为什么 1MB direct read 到块层后只形成约 `339KB` 的平均 read bio
- 块层 / virtio 是否存在 request split 或队列上限，把本来更大的读请求拆小了

补充验证后，`ext4` 自身的 direct-read 画像已经更清楚，日志见 `asterinas/.tmp_ext4_seq_read_bw_mapping_profile.log`：
- `avg_bytes=1048576`
- `avg_mapped_bytes=1015808`
- `avg_zero_fill_bytes=32768`
- `max_mapped_bytes=1048576`

这说明 ext4 稳态下每次 1MB direct read 实际提交的映射数据大约是 `992KB`，并不是只有 `339KB`。因此，之前对“request split”的怀疑已经明显降级；当前 `block-profile` 的 `339KB` 更像是被 benchmark 运行期混入的小 read bio 稀释后的平均值。

---

## 当前差距量化

| 指标 | 当前 | 目标 (80%) | 需提升 |
|------|------|-----------|--------|
| Asterinas 绝对带宽 | 3180 MB/s | 3861 MB/s | +681 MB/s |
| 每次 1MB 读等效周期 | ~314 μs | ~259 μs | -55 μs（仅作吞吐换算参考） |

最新打点结论：每次 1MB O_DIRECT 读里，`bio_waiter.wait()` 稳态约 `186μs`，`segment.reader().read_fallible(writer)` 约 `54μs`。这些数字仍然重要，但在 Phase 1 里它们的意义主要是帮助判断“还能不能继续提高 sustained throughput”，而不是单独追求更低时延。

---

## 分步优化方案

### Step 0：打点与前提验证（主线重排前置）

**状态：** 已完成
**目标：** 量化 `fio_read` 主路径真实耗时分布，判断主线应落在哪条路径上
**对应 analysis：** B1, B2, B5, B6，以及零拷贝路线的连续性前提
**工作量估计：** 0.5-1 天

#### 0.1 为什么先做这一步

零拷贝路线已经完成过一轮原型验证，但结果明显回归，说明“少一次 memcpy”在当前环境里并不自动等于更高吞吐。

当前最需要先搞清楚的是：
- `memcpy` 现在是否仍然是主导瓶颈
- `BioSegment::alloc`、bio 提交、等待完成各自占多少
- 1MB 用户 buffer 的物理页连续性到底有多差
- direct read 在块层实际被拆成了多少 segment / bio

只有这些数据明确后，才能判断：
- 是继续重启零拷贝路线
- 还是改攻 `Step 2 / Step 3`
- 或者转向别的 read 路径优化点

#### 0.2 打点项

建议在 ext4 direct-read 路径中补最小侵入式打点，至少覆盖：
- `plan_direct_read_cached()` 总耗时，以及 cache hit / miss 次数
- `BioSegment::alloc()` 总耗时
- `read_blocks_async()` / bio submit 总耗时
- `bio_waiter.wait()` 总耗时
- `segment.reader().read_fallible(writer)` 总耗时
- `mappings.len()` 的分布

如果继续评估零拷贝路线，还要额外补：
- 每次 1MB 请求对应的用户页连续 run 数
- 平均每个 run 的字节数
- 每次 bio 实际 segment 数

#### 0.3 实际产出

本轮已经拿到可直接指导下一步决策的 profiling 结论：
- benchmark：单边 `ext4_seq_read_bw` 实测 `3065 MB/s`
- 主路径耗时占比：`wait ~186μs`，`copy ~54μs`，`plan ~0-1μs`
- `BioSegment::alloc` / `read_blocks_async` submit 在当前粒度下都接近 0
- `cache_miss=3 / 458752 reads`，说明 `Step 2` 对 fio_read 主路径帮助极小
- `avg_mappings≈0.97`、`max_mappings=2`，说明稳定 direct-read 路径本身并没有明显 mapping 碎片问题
- block/virtio 分层打点：`queue_wait ~1μs`、`dispatch ~1-2μs`、`device_wait ~91μs`
- block 侧 `avg_bytes` 稳态约 `339KB`，说明 benchmark 主读流在块层并不是完整 1MB request 形态
- ext4 direct-read 映射画像：`avg_mapped_bytes ~992KB`、`avg_zero_fill_bytes ~32KB`

#### 0.4 验收标准

已完成，并得到以下明确答案：
- 当前 `memcpy` 不是第一大头，`wait` 才是
- `BioSegment::alloc` 和 bio submit 不是当前最值得优先优化的点
- “用户页碎片度否决零拷贝路线” 只适用于 zero-copy 原型；对当前稳定 direct-read 路径不是主要问题
- 下一步不该直接进入 `Step 2 / Step 3`，而应把优化重心转到 block / virtio / bio wait 路径
- “request split 导致只有 339KB bio” 目前没有得到支持，优先级下降
- 下一步优先隔离出 fio 主流的大 read bio，再继续看 block device wait 的真实占比

### Step 0.5：隔离 fio 主流 read bio，重算 block wait

**状态：** 已完成
**目标：** 把 fio 主流的大 read bio 从 benchmark 杂音里分离出来，重算 block / virtio 的真实 `device_wait`
**对应 analysis：** Step 0 新增 block/virtio profiling 结论
**工作量估计：** 0.5-1 天

#### 0.5.1 当前怀疑点

- `block-profile` 当前混入了大量 4KB 级别的小 read bio，导致平均字节数失真
- ext4 主流 direct-read 本身接近 `1MB`，所以 request split 不是当前最强嫌疑
- 当前 ext4 看到的 `avg_wait≈218μs` 与 block 侧 `device_wait≈95μs` 之间仍有显著差距，说明还需要更干净的主流 read 样本

#### 0.5.2 下一步打点

- 对 block profile 增加“大 read bio”过滤统计，只看 `>=512KB` 的 read bio
- 用这组过滤后的样本重算 `queue_wait / dispatch / device_wait`
- 如果过滤后 `large_avg_bytes` 接近 `1MB`，就可以基本排除 request split，把重点收敛到真正的大 read I/O 完成等待

#### 0.5.3 验收标准

- 拿到 fio 主流大 read bio 的独立 block timing
- 明确 `device_wait` 在主流 1MB 读样本里到底是多少
- 用这组数据决定下一步是继续查 virtio/host I/O 完成链路，还是回到 ext4 层

#### 0.5.4 最新结果

`large read bio` 过滤统计已经跑到，日志见 `asterinas/.tmp_ext4_seq_read_bw_large_profile.log`。当前主流样本稳定值大致是：
- `large_avg_bytes ≈ 1047287`
- `large_avg_segments_x100 = 100`
- `large_avg_queue_wait_us = 0`
- `large_avg_dispatch_us = 1`
- `large_avg_device_wait_us = 221-224`

和 ext4 同轮的 direct-read 打点对比：
- `avg_mapped_bytes ≈ 1015808`
- `avg_wait_us = 218-221`

这说明两件事：
- fio 主流的大 read bio 基本就是单个 `~1MB` 请求，`request split` 可以继续降级
- ext4 里的 `bio_waiter.wait()` 几乎完全等于“大 read bio 的真实完成等待”，软件队列和提交路径已经几乎不占时间

因此，下一步应该转向两个方向择一：
- 继续往 virtio completion / host I/O 这条链路深挖
- 或者回头评估 copy 路径，因为当前 `~61μs` copy 已经足以决定是否能冲到 `80%`

继续把 completion 进一步拆分后，日志见 `asterinas/.tmp_ext4_seq_read_bw_irq_profile.log`。当前稳定值已经收敛到：
- `large_avg_device_wait_us = 243-245`
- `large_avg_irq_delivery_us = 243-244`
- `large_avg_irq_reap_us = 0`
- `large_avg_resp_sync_us = 0`
- ext4 同轮 `avg_wait_us = 240-242`
- ext4 同轮 `avg_copy_us = 65-66`

这说明 guest 内部从“进入 IRQ 回调”到“reap used ring / sync 响应头”的软件开销几乎可以忽略，`device_wait` 的主体已经基本等于“virtqueue notify 之后，到 completion IRQ 真正送达前”的等待。

同时，copy 路径本身也没有看到明显的低级实现问题：
- `VmReader::read_fallible()` 最终调用 `memcpy_fallible()`，见 `ostd/src/mm/io.rs`
- x86 的 `__memcpy_fallible` 使用 `rep movsb`，见 `ostd/src/arch/x86/mm/memcpy_fallible.S`
- OSDK 在 `x86_64` 构建时默认打开 `-C target-feature=+ermsb`
- 按当前 `avg_mapped_bytes ≈ 1015808`、`avg_copy_us = 65-66` 估算，copy 吞吐约为 `14.3-14.6 GiB/s`

所以，下一步的默认主线应进一步收敛为：
- 不再继续花时间优化 guest 内的 block/virtio completion bookkeeping
- 不把 “微调 memcpy 实现” 视为主收益点
- 优先确认 `virtio completion / interrupt delivery / host I/O` 这条链路还能不能动

补充的最新单边验证（`asterinas/.tmp_ext4_seq_read_bw_1g_verify.log`）也支持这一点：
- fio 入口已经统一到 `size=1G bs=1M`
- 单边 benchmark 实测：`READ: bw=3479MiB/s (3648MB/s)`
- 同轮 ext4：`avg_wait_us ≈ 73`、`avg_copy_us ≈ 55`
- 同轮大 bio：`large_avg_queue_wait_us ≈ 70`、`large_avg_device_wait_us ≈ 169`

这说明 `submit-before-copy` 方向本身没有错，但新的主矛盾已经不再是 ext4 里的同步 `wait` 或 `copy` 本身，而是 speculative request 从 `enqueue` 到真正 `device_submit` 前暴露出来的 software queue wait。

---

### Step 1.5：completion wait 链路优化原型

**状态：** 未执行（Phase 1 目标已达成，不再需要）
**目标：** 作为吞吐主线受阻时的补充实验，验证 completion wait 改动是否还能带来额外带宽收益
**对应 analysis：** Step 0.5 最新结论、`irq delivery` 拆分结果
**工作量估计：** 1-2 天

#### 1.5.1 当前判断

- ext4 同轮 `avg_wait_us ≈ 240-242`
- block 主流样本 `large_avg_device_wait_us ≈ 243-245`
- 其中 `large_avg_irq_delivery_us ≈ 243-244`
- `irq_reap / resp_sync` 约等于 `0μs`

这说明当前主瓶颈不是 ext4 自身，也不是 guest 内 virtio completion 回调逻辑，而是“提交大 read bio 之后，到 completion IRQ 被 guest 观察到之前”的等待。

同时需要明确一点：`poll-before-sleep` 不是当前默认主线，只是验证性实验。按现有数据，它更可能回收一部分隐藏的调度损耗，而不是单独把 `fio_read` 拉到 `>= 80%`。

#### 1.5.2 第一版原型方向

先做一个保守的 `poll-before-sleep` 原型，只针对 fio 主流的大块同步读场景：
- 优先放在 `BioWaiter::wait()` 或其附近
- 在真正进入睡眠等待前，短暂主动轮询完成状态
- 只对 `BioType::Read` 生效
- 只对大 bio 生效，例如 `>= 512KB`
- 保持 fallback：超过轮询预算后仍然走原有睡眠等待路径

这样做的目的不是长期保留 busy-poll，而是先验证：
- 当前 `irq delivery` 这段等待里，到底有多少是可以被“更快观察到完成状态”吃掉的
- 如果 benchmark 能正向变化，后面再决定是否做更稳妥的 completion-path 优化

#### 1.5.3 实现边界

- 不改 ext4 文件映射逻辑
- 不重启 zero-copy 路线
- 不优先微调 memcpy 实现
- 不在这一阶段做 host/QEMU 外部环境改造

当前只把它当作 guest 内可控、最小侵入的补充实验，不再作为默认主线。

#### 1.5.4 验收标准

- 单边 `ext4_seq_read_bw` 能稳定跑通
- benchmark 吞吐相对当前基线出现正向变化
- 若吞吐变化不明显，仅有 `avg_wait_us` 下降，不视为主线成功
- 若无收益或收益被 CPU 忙等抵消，则明确判定该原型无效，并转向 host/virtio completion 链路分析

#### 1.5.5 成功后的下一步

- 如果 `poll-before-sleep` 有效：把它作为辅助手段保留，但不改变 `Step 1` 的吞吐主线地位
- 如果无效：继续保持它为可选项，不再继续在 guest completion bookkeeping 上消耗精力

---

### Step 0.7：Speculative Fast Submit / Queue-Wait Reduction

**状态：** 已完成
**目标：** 把 speculative direct-read 从“更早 enqueue”推进到“更早 device-submit”，优先压低 `large_avg_queue_wait_us`
**对应 analysis：** Step 0.5/1.5 的 block 路径结论，以及 `submit-before-copy` 原型暴露出的 queue-wait 新瓶颈
**工作量估计：** 1-2 天

#### 0.7.1 为什么要插入这一步

当前 `Step 1` 的第一版原型已经验证：
- 在 ext4 层做 `submit-before-copy`，可以明显降低 direct-read 的同步等待窗口
- 但这部分时间没有直接转成吞吐，反而在 block profile 里表现为更高的 `queue_wait`

最新单边验证（`size=1G bs=1M`）稳定值已经接近：
- benchmark：`3479MiB/s (3648MB/s)`
- ext4：`avg_wait_us ≈ 73`、`avg_copy_us ≈ 55`
- large read bio：`large_avg_queue_wait_us ≈ 70`、`large_avg_device_wait_us ≈ 169`

因此，当前最直接的问题不是“还能不能继续隐藏 copy”，而是：
- speculative bio 虽然更早提交到了 software queue
- 但没有足够早地真正进入 virtqueue / device in-flight
- `submit-before-copy` 先变成了 `enqueue-before-copy`

#### 0.7.2 当前目标

把主线叙事从“只降低关键路径时延”补成“为吞吐服务的更早提交”：
- 优先减少 `submit -> request_queue.dequeue` 这一段
- 让 speculative request 更快走到 `add_dma_buf()/notify()`
- 目标指标从 `avg_wait_us` 扩展为 `large_avg_queue_wait_us`

换句话说，这一步不是去优化 ext4 的 mapping，也不是继续微调 `memcpy`，而是尽量把 speculative read 更早变成真正的 device outstanding request。

#### 0.7.3 实际实现与迭代结论

本步最终采用了“两轮实验、一次收窄”的方式：
- 第一轮先验证 block/virtio 侧的最小 fast-submit 能否真正吃掉 queue handoff
- 结果证明 `queue_wait` 的确能被压掉，但如果对所有大 read 一视同仁地直提，会明显扰动正常 foreground read 路径
- 第二轮把 fast-submit 收窄为“只服务 speculative direct-read prefetch”，并通过 bio hint 从 ext4 一直传到 virtio-block

最终保留的实现边界是：
- 只针对 `O_DIRECT read`
- 只针对已经满足 `Step 1` gating 的 speculative request
- 只针对大块请求，例如 `>= 512KB`
- 任一条件不满足，立即回到现有稳定 queue 路径

#### 0.7.4 实现边界

- 不重启 zero-copy DMA 路线
- 不先扩展到 `2+` 个 speculative outstanding slot
- 不优先改 host/QEMU 外部环境
- 不在这一阶段回头做 inode cache、Vec、Mutex 微优化

当前只做一件事：验证把 speculative request 更早送进 virtqueue，是否能把已经暴露出来的 `queue_wait` 吃掉一部分。

#### 0.7.5 验收标准

- 单边 `ext4_seq_read_bw` 稳定跑通
- `large_avg_queue_wait_us` 相比当前 `~70us` 有可见下降
- benchmark 吞吐相对当前 `~3648 MB/s` 再出现正向变化
- 若 `queue_wait` 下降但吞吐无收益，再决定是否把主线转向“多 outstanding slot / 更高 queue depth”

#### 0.7.6 验收结果

两轮单边 `size=1G bs=1M` 验证结果如下：
- 基线（仅 `submit-before-copy`）：`3479MiB/s (3648MB/s)`
- 过宽 fast-submit（所有大 read）：`2499MiB/s (2620MB/s)`，虽然 `large_avg_queue_wait_us` 接近 `0`，但吞吐明显回退
- 收窄后 fast-submit（仅 speculative prefetch）：`3518MiB/s (3689MB/s)`，`large_avg_queue_wait_us = 0`，且吞吐重新超过基线

这说明：
- `queue_wait` 确实是被 `0.7` 正面命中的瓶颈
- 但 fast-submit 必须保持“只服务 speculative request”的边界，不能扰动正常 foreground 提交流程
- `Step 0.7` 现在已经足够作为 `Step 1` 的固定前置条件保留
#### 0.7.7 与 Step 1 的关系

- `Step 1` 已经覆盖了 “submit-before-copy / speculative enqueue”
- `Step 0.7` 补的是 “让 speculative request 更早 device-submit”
- 当前 `0.7` 已完成，下一步回到 `Step 1` 的更完整版本，继续评估是否扩展到更强的吞吐导向流水线

---

### Step 1：Throughput-First Speculative Read Pipeline（主线）

**状态：** ✅ 已完成（single-slot speculative readahead + scoped fast-submit，最终双边 95.79%）
**目标：** 在 `Step 0.7` 已把 speculative `queue_wait` 压低的前提下，优先提升 sustained throughput / device occupancy，并以此冲击 `ext4_seq_read_bw >= 80%`
**对应 analysis：** B7, B8，以及 Step 0.5/1.5/0.7 对 wait 路径与 queue-wait 的最新结论
**工作量估计：** 2-4 天

#### 1.0 为什么它是主线

当前主流 1MB `O_DIRECT read` 的关键时间分布已经收敛到：
- `device_wait / irq_delivery ≈ 220μs` 量级
- `copy ≈ 60-66μs`
- ext4 自身的 `plan / alloc / submit` 几乎可忽略

如果仍然保持“一次只服务一个 foreground read”的串行节奏：
```
wait_1 -> copy_1 -> wait_2 -> copy_2 -> ...
```
那么无论单次 `wait` 和 `copy` 再怎么优化，设备都很难持续处在更高的有效 in-flight 状态，吞吐上限也会提前到来。

真正对带宽有意义的，是把时序改成：
```
wait_1 -> submit_2 -> copy_1 -> wait_2(剩余部分) -> submit_3 -> copy_2 -> ...
```
这样做的意义不是单纯“少暴露几十微秒 copy”，而是：
- 让下一次大 read 更早进入 device in-flight
- 提高 sustained pipeline occupancy
- 在不显著增加软件队列等待的前提下，把设备更稳定地喂满

所以 `Step 1` 的主叙事从现在开始改成：
- 第一目标是吞吐
- `submit-before-copy`、copy overlap、wait 缩短都只是为吞吐服务的手段

但在最新原型里，还需要补上一个新的现实约束：
- `submit-before-copy` 不自动等于 “更早 device-submit”
- 如果 speculative bio 只是更早进入 software queue，而没有更早进入 virtqueue
- 那么收益会被新的 `queue_wait` 吃掉

因此，`Step 1` 的后续推进默认建立在 `Step 0.7` 先把 queue handoff 压下去的前提上。

#### 1.1 核心设计：先喂设备，再消费数据

这里最关键的不是“有没有 speculative readahead”，而是**设备 admission 时机**。

必须采用：
- 当前 bio 一完成，先 `plan_next + submit_next`
- 然后再做当前次 `copy`
- 下次 `read_direct_at` 进来时，优先命中已经 in-flight 的 pending bio

不能采用：
- 先做当前次 `copy`
- 再提交下一次 bio

后者只能吃到很小一段窗口，前者才能真正把 copy 隐藏到下一次 I/O 里。

#### 1.2 吞吐优先的第一版边界

第一版只覆盖 fio 当前主流场景：
- 只做 `O_DIRECT read`
- 只做同 inode 的连续顺序读
- 判定条件写死为 `next_offset == current_offset + current_direct_len`
- 只做大块请求，例如 `>= 512KB`
- 同时最多只允许 `1` 个 in-flight speculative request
- 任意条件不满足，立即 fallback 到当前稳定串行路径

这样做不是因为“一个 slot 最优”，而是因为：
- `0.7` 已经证明先把 single-slot speculative request 更早送到 virtqueue 是值得的
- 但在没有确认 foreground path 不会被扰动前，不应该直接扩到更高 queue depth
- 第一版应先验证：在 `queue_wait` 保持低位时，single-slot pipeline 还能把吞吐再往上推多少

#### 1.3 结构草图

在 `DirectReadCache` 或相邻状态中增加一个极小的 pending speculative state：
- `ino`
- `offset`
- `len`
- `mappings`
- `bio_segments`
- `bio_waiter`
- `stale / matched` 状态位

当前次 read 的流程：
1. 在当前 bio 仍 inflight 时，先尝试 `plan_direct_read_cached(next_offset)`
2. 正常完成当前 bio 的 `wait`
3. 若预先规划成功，则立刻提交 speculative bio
4. 再执行当前次 `copy`
5. 返回用户态

下一次 read 的流程：
1. 若命中 pending speculative state，直接接管该 in-flight bio
2. 等待其剩余完成时间
3. copy 到用户 buffer
4. 继续按同样方式提交下一次 speculative bio

#### 1.3.1 当前已验证结果

截至 2026-04-17，single-slot 的第一轮稳定化已经做完：
- 保留 `1` 个 pending speculative request
- 不再扩到 `2` 个 pending slot
- 把 `plan_next` 从 `wait` 之后前移到 `wait` 之前
- 在 `wait` 返回后只做“尽快 submit + copy 当前数据”
- 并把 direct-read planning window 从 `64/256MiB` 放大到 `128/512MiB`

这轮单边 `size=1G bs=1M` 结果：
- `Step 0.7` 基线：`3518MiB/s (3689MB/s)`
- 当前 `Step 1`：`3533MiB/s (3705MB/s)`
- 当前保留版本：`4076MiB/s (4274MB/s)`
- ext4 `avg_plan_us ≈ 39`
- ext4 `avg_wait_us ≈ 50`
- ext4 `avg_copy_us ≈ 57`
- `large_avg_queue_wait_us = 0`
- `large_avg_device_wait_us ≈ 196-197`

这一步的直接原因也已经比较清楚：
- `cache_miss` 从 `3072` 降到 `2048`
- `avg_plan_us` 从 `~45us` 降到 `~39us`
- 说明当前 single-slot steady-state 里，“少量但昂贵的 re-plan miss” 仍然是值得打的点

同时，一轮 `pending slot = 2` 的实验已经显示：
- `queue_wait` 虽然仍然很低
- 但 `large_avg_device_wait_us` 会升到 `~370us`
- 说明当前阶段盲目扩 depth 只是把时间重新堆到设备侧

另外，一轮 single-slot 的 `submit-before-wait` 实验也已经验证：
- 尽管 ext4 `avg_wait_us` 会进一步降到 `~36-37us`
- 但单边 benchmark 会回退到 `3293MiB/s (3452MB/s)`
- 同期 `large_avg_device_wait_us` 会抬到 `~259us`

这说明在当前环境下，“更早 submit”并不自动等于“更高吞吐”：
- 它确实能缩短 foreground wait
- 但也会更早增加设备侧竞争
- 最终不如当前保留的 `prepare-before-wait + submit-after-wait` 稳定

因此，当前 Step 1 的默认推进方向已经收敛为：
- 继续打磨 single-slot steady-state
- 优先隐藏 `plan_next`、压缩 foreground wait
- 暂不把 `pending slot = 2` 当作默认下一步
- 也不把 `submit-before-wait` 当作默认下一步
- 在时序不变的前提下，优先继续找“降低 miss 频率 / 降低 miss 成本”的 steady-state 机会

#### 1.4 吞吐主线的第二阶段条件

如果 single-slot pipeline 在 `queue_wait` 仍然接近 `0` 的情况下，吞吐依旧明显低于 `80%` 门线，则下一阶段优先考虑：
- 把 pending speculative slot 从 `1` 扩到 `2`
- 或者把 speculative submit 的触发点再提前半拍
- 或者允许“当前 copy 期间同时维持更稳定的下一次 in-flight”

也就是说，后续是否继续扩 depth，不看单次 `avg_wait_us` 是否还能下降，而看：
- `MB/s` 是否已经平台化
- `large_avg_queue_wait_us` 是否仍然维持低位
- 扩 depth 是否会重新把时间堆回 software queue 前面

#### 1.5 状态失配与回退

- 文件末尾：`plan_direct_read_cached` 返回 `direct_len = 0`，则不提交 speculative bio
- offset 不匹配：不复用 pending speculative state，立即 fallback 到串行路径
- 并发写 / truncate：`invalidate_direct_read_cache` 时同时把 pending state 标成 stale
- stale speculative bio：不尝试取消 I/O；等它自然完成后，丢弃其结果并释放资源
- close / umount：清理 speculative state，确保 in-flight 资源最终被回收

#### 1.6 资源与实现边界

- 只使用现有 `BioSegmentPool`
- 同时持有“当前 + speculative”两个 1MB buffer，池容量 16MB 足够
- 不重启 zero-copy 路线
- 不优先微调 memcpy 实现
- 不先碰 host/QEMU 外部环境

#### 1.7 验收标准

- 单边 `ext4_seq_read_bw` 稳定跑通
- benchmark 吞吐继续上升，并优先评估是否逼近或达到 `>= 80%`
- `large_avg_queue_wait_us` 不得明显回升，最好继续维持在低位
- 若 single-slot 版本吞吐继续增长，则优先沿吞吐主线推进
- 若 single-slot 版本吞吐平台化，再决定是否进入“更高 outstanding depth”或叠加 `Step 1.5`

---

### Research Track Z：零拷贝 DMA（待定）

**状态：** 待定，暂不继续落地
**原目标：** 消除 B1 + B2，每次读节省 ~60μs
**原预期提升：** 65.89% → ~80%+
**对应 analysis：** B1, B2
**工作量估计：** 2-3 天

#### Z.0 本轮实验结论

这条路线在理论上成立，但当前实现前提和 benchmark 场景不匹配，暂时不适合作为主优化路线。

实际观察到的问题：
- 用户页物理连续性不足，1MB buffer 往往会拆成很多小 segment
- virtio-blk 的 segment 数限制和 SG 提交开销明显放大
- 去掉一次 1MB `memcpy` 的收益，小于额外的 SG/bio 成本

本轮实验结果：
- 零拷贝原型：Asterinas only 实测 `812 MiB/s`
- 加平均连续段阈值后的折中版：Asterinas only 实测 `1594 MiB/s`
- 两者都明显低于当前稳定基线 `3180 MB/s`

当前处理：
- 实验代码已回退，不保留在仓库主线
- 该路线降级为待定
- 只有在补充碎片度/segment 数量打点后，才决定是否重启

#### Z.1 技术方案

当前路径：
```
BioSegment::alloc(256 blocks) → DMA 到独立 buffer → memcpy 到 VmWriter(用户页)
```

优化后路径：
```
获取 VmWriter 底层物理页 → BioSegment::new_from_segment(用户页) → DMA 直接到用户页
```

#### Z.2 实现细节

**核心问题：** `VmWriter<'_, Fallible>` 只暴露 `cursor: *mut u8`（用户虚拟地址）。需要从用户 VA 获取物理帧，创建 BioSegment 做零拷贝 DMA。

**API 链已全部验证可行，不需要新增 ostd API：**

```
VmWriter::cursor() → user_va
  ↓
current_userspace!().vmar().vm_space() → VmSpace   [context.rs:57-66, 85-87]
  ↓
disable_preempt() → DisabledPreemptGuard           [preempt/guard.rs:9-18, impl AsAtomicModeGuard]
  ↓
vm_space.cursor(&guard, &va_range) → Cursor         [vm_space.rs:95-101]
  ↓
cursor.query() → (Range<Vaddr>, VmQueriedItem)      [vm_space.rs:272-275]
  ↓
VmQueriedItem::MappedRam { frame: FrameRef, .. }    [vm_space.rs:573-582]
  ↓
frame.clone() → Frame<dyn AnyUFrameMeta> = UFrame   [mod.rs:237-247, 增引用计数]
  ↓
物理连续页合并 → USegment                           [segment.rs:221-229, from_raw]
  ↓
BioSegment::new_from_segment(useg, FromDevice)      [bio.rs:467-478]
  ↓
Bio::new(Read, start_sid, Vec<BioSegment>, ...)     [bio.rs:40-55, 原生多段支持]
```

**设计决策：零拷贝快路径 + 旧路径 fallback**

零拷贝需要满足两个前置条件：
1. 用户 buffer 物理页全部连续（1MB 分配通常满足）
2. mappings 完全覆盖请求范围、无 hole（预写文件通常满足）

当前置条件不满足时，fallback 到现有的 alloc+copy 路径。这保证了正确性，同时覆盖了 fio benchmark 的场景。

**hole 处理：** 零拷贝路径仅在无 hole 时使用（`mappings_fully_cover_range()` 已有检查函数，[fs.rs:872](asterinas/kernel/src/fs/ext4/fs.rs#L872)）。有 hole 的文件自动走 fallback。

**多 mapping 切分：** `Segment::slice(&range)` 可从连续 USegment 中按偏移切出子段（自动增引用计数，[segment.rs:148-175](asterinas/ostd/src/mm/frame/segment.rs#L148-L175)），每个 mapping 用 `slice` 获取对应的子 USegment，再用 `read_blocks_async` 提交。无需直接构造 `Bio::new`，避免 `general_complete_fn` 不可导出的问题。

**实现方案：**

```rust
pub(super) fn read_direct_at(
    &self, ino: u32, offset: usize, writer: &mut VmWriter, status_flags: StatusFlags,
) -> Result<usize> {
    let (direct_len, mappings) = self.plan_direct_read_cached(ino, offset, writer.avail())?;
    if direct_len == 0 { return Ok(0); }

    // 尝试零拷贝快路径
    if let Some(n) = self.try_read_direct_zero_copy(offset, direct_len, &mappings, writer)? {
        self.touch_atime_after_direct_read(ino, status_flags)?;
        return Ok(n);
    }

    // ---- Fallback: 现有 alloc+copy 路径（原封不动保留）----
    let mut bio_waiter = BioWaiter::new();
    let mut bio_segments = Vec::with_capacity(mappings.len());
    for mapping in &mappings {
        let bio_segment = BioSegment::alloc(mapping.len as usize, BioDirection::FromDevice);
        bio_segments.push(bio_segment.clone());
        let waiter = self.block_device.read_blocks_async(Bid::new(mapping.pblock), bio_segment)?;
        bio_waiter.concat(waiter);
    }
    if Some(BioStatus::Complete) != bio_waiter.wait() {
        return_errno!(Errno::EIO);
    }
    // ... 原有 gap/copy 逻辑 ...
    self.touch_atime_after_direct_read(ino, status_flags)?;
    Ok(direct_len)
}

fn try_read_direct_zero_copy(
    &self,
    offset: usize,
    direct_len: usize,
    mappings: &[SimpleBlockRange],
    writer: &mut VmWriter,
) -> Result<Option<usize>> {
    // 前置条件 1: 无 hole
    if !Self::mappings_fully_cover_range(offset, direct_len, mappings)? {
        return Ok(None);
    }

    // 前置条件 2: 用户 VA 页对齐
    let user_va = writer.cursor() as usize;
    if user_va % PAGE_SIZE != 0 {
        return Ok(None);
    }

    // 前置条件 3: 查页表，检查用户页全部物理连续
    let user_space = current_userspace!();
    let vm_space = user_space.vmar().vm_space();
    let guard = disable_preempt();
    let mut cursor = vm_space.cursor(&guard, &(user_va..user_va + direct_len))?;

    let total_pages = direct_len / PAGE_SIZE;
    let mut first_paddr: Option<Paddr> = None;
    let mut expected_paddr: Paddr = 0;

    for i in 0..total_pages {
        let (_va_range, item) = cursor.query()?;
        match item {
            Some(VmQueriedItem::MappedRam { frame, .. }) => {
                let paddr = frame.paddr();
                if i == 0 {
                    first_paddr = Some(paddr);
                    expected_paddr = paddr + PAGE_SIZE;
                } else if paddr != expected_paddr {
                    return Ok(None); // 不连续，fallback
                } else {
                    expected_paddr = paddr + PAGE_SIZE;
                }
            }
            _ => return Ok(None),
        }
    }
    drop(cursor);
    drop(guard);

    let first_paddr = first_paddr.unwrap();

    // 构造 USegment：clone 每帧增引用计数，ManuallyDrop 保留，from_raw 构造
    let guard2 = disable_preempt();
    let mut cursor2 = vm_space.cursor(&guard2, &(user_va..user_va + direct_len))?;
    for _ in 0..total_pages {
        let (_, item) = cursor2.query()?;
        if let Some(VmQueriedItem::MappedRam { frame, .. }) = item {
            let owned = (*frame).clone();              // 增引用计数
            core::mem::forget(owned);                  // 保留引用计数
        }
    }
    drop(cursor2);
    drop(guard2);

    // SAFETY: 每一帧的引用计数已通过 clone+forget 增加，
    //         Segment drop 时会 from_raw 每帧并 drop，正好配对。
    let user_segment = unsafe { USegment::from_raw(first_paddr..first_paddr + direct_len) };

    // 提交 bio：用 Segment::slice 按 mapping 切分
    let mut bio_waiter = BioWaiter::new();
    let mut seg_offset = 0usize;
    for mapping in mappings {
        let mapping_bytes = mapping.len as usize * EXT4_BLOCK_SIZE;
        let sub_seg = user_segment.slice(&(seg_offset..seg_offset + mapping_bytes));
        let bio_segment = BioSegment::new_from_segment(sub_seg, BioDirection::FromDevice);
        let waiter = self.block_device.read_blocks_async(Bid::new(mapping.pblock), bio_segment)?;
        bio_waiter.concat(waiter);
        seg_offset += mapping_bytes;
    }

    if Some(BioStatus::Complete) != bio_waiter.wait() {
        return_errno!(Errno::EIO);
    }

    // 数据已在用户页中，前进 writer cursor
    writer.skip(direct_len);
    Ok(Some(direct_len))
}
```

**为什么用 `read_blocks_async` 而不直接用 `Bio::new`：** `read_blocks_async` 内部包装了 `general_complete_fn`（错误日志回调），该函数为 crate 私有，从 ext4 模块无法导入。零拷贝路径中每个 mapping 物理页连续（一个 BioSegment），直接复用 `read_blocks_async` 即可。

**为什么做两次页表遍历：** 第一次只读，确认全部连续后再做第二次增引用计数。避免在不连续 fallback 时泄漏引用计数（否则需要复杂的回滚逻辑）。两次遍历的开销（~1μs for 256 pages）远小于消除的 50μs memcpy。

#### Z.3 调研结论（已验证）

| 问题 | 结论 | 依据 |
|------|------|------|
| VmWriter → 物理页转换 | **可行**，API 链完整 | `VmSpace::cursor()` → `query()` → `VmQueriedItem::MappedRam { frame }` |
| USegment 构造 | **可行**，单帧 `Segment::from(frame)`，连续帧 `clone+forget` 后 `Segment::from_raw(range)` | [segment.rs:117-124, 221-229](asterinas/ostd/src/mm/frame/segment.rs) |
| USegment 按 mapping 切分 | **可行**，`Segment::slice(&range)` 返回子段并自动增引用计数 | [segment.rs:148-175](asterinas/ostd/src/mm/frame/segment.rs#L148-L175) |
| AsAtomicModeGuard 要求 | **可满足**，`disable_preempt()` 返回 `DisabledPreemptGuard` 即实现 | [guard.rs:18](asterinas/ostd/src/task/preempt/guard.rs#L18) |
| DMA 期间页安全性 | **安全**，Frame clone 增引用计数防释放，Asterinas 无 swap | Frame::clone at [mod.rs:237-247](asterinas/ostd/src/mm/frame/mod.rs#L237-L247) |
| DMA 完成后同步 | **自动**，virtio-blk 驱动读完成时自动 `sync_from_device()` | [device.rs:308-318](asterinas/kernel/comps/virtio/src/device/block/device.rs#L308-L318) |
| hole 处理 | **用 fallback 兜底**，`mappings_fully_cover_range()` 已存在 | [fs.rs:872](asterinas/kernel/src/fs/ext4/fs.rs#L872) |
| `general_complete_fn` 可见性 | **不可导出**（crate 私有），但用 `read_blocks_async` 绕过 | [impl_block_device.rs:213](asterinas/kernel/comps/block/src/impl_block_device.rs#L213) |
| VmWriter::skip() 存在 | **存在**，前进 cursor 不写入 | [io.rs:981](asterinas/ostd/src/mm/io.rs#L981) |
| 对齐要求 | **已满足**，fio O_DIRECT 用 `posix_memalign` 页对齐，inode.rs 已有对齐检查 | [inode.rs:149](asterinas/kernel/src/fs/ext4/inode.rs#L149) |

#### Z.4 验证

```bash
# Docker 内
BENCH_ENABLE_KVM=1 BENCH_ASTER_NETDEV=tap BENCH_ASTER_VHOST=on \
  bash tools/ext4/run_fio_perf_compare.sh ext4_seq_read_bw
```

验收标准：`ext4_seq_read_bw >= 80%`

---

### Step 2：消除 cache miss 时的 inode 磁盘读

**状态：** 未执行（Phase 1 目标已达成，此项收益 ~0.2%，留给后续阶段按需评估）
**目标：** 消除 B3，cache miss 时节省 ~100-200μs
**预期提升：** +0.2%（频率低但每次影响大）
**对应 analysis：** B3
**工作量估计：** 0.5 天

#### 2.1 当前问题

`plan_direct_read_cached` 在 cache miss 时调用 `ext4_plan_direct_read(ino, offset, window)`，而 `plan_direct_read` 里的 `get_inode_ref(ino)` 会从磁盘读 inode 块，**仅仅为了获取 `file_size`**。

kernel 侧的 `inode_meta_cache` 已经缓存了 `SimpleInodeMeta.size`，但 ext4_rs 不知道。

#### 2.2 方案

1. 在 `plan_direct_read_cached` 的 cache miss 分支中，先从 `inode_meta_cache` 获取 `file_size`
2. 为 ext4_rs 新增 `plan_direct_read_with_size(ino, offset, len, known_file_size)` 入口
3. 该入口跳过 `get_inode_ref()` 的 size 查询，直接用传入的 size 裁剪长度
4. 仍然需要 `get_inode_ref()` 来获取 extent tree root（inode.block 字段），但可以缓存 inode

```rust
// ext4_rs 新增接口
pub fn plan_direct_read_with_size(
    &self,
    inode: u32,
    offset: usize,
    len: usize,
    known_file_size: usize,
) -> Result<(usize, Vec<SimpleBlockRange>)> {
    let read_end = offset.saturating_add(len).min(known_file_size);
    let read_len = read_end.saturating_sub(offset);
    let direct_len = read_len / block_size * block_size;
    if direct_len == 0 { return Ok((0, Vec::new())); }
    
    // 仍需 get_inode_ref 获取 extent root，但可以用 inode cache 避免读盘
    let inode_ref = self.get_inode_ref(inode);
    let lblock_start = (offset / block_size) as u32;
    let lblock_count = (direct_len / block_size) as u32;
    let mappings = self.collect_block_ranges(&inode_ref, lblock_start, lblock_count)?;
    Ok((direct_len, mappings))
}
```

**注意：** 这一步的真正收益要等到 ext4_rs 内部有 inode cache 后才能完全体现。当前只消除 "为拿 size 读盘" 的部分。

#### 2.3 验证

同新的主线 Step 1。

---

### Step 3：减少 per-read 的 Mutex 和 Vec 开销

**状态：** 未执行（Phase 1 目标已达成，此项收益 ~1-2%，留给后续阶段按需评估）
**目标：** 降低 B5 + B6，每次读节省 ~5-7μs
**预期提升：** +1-2%
**对应 analysis：** B5, B6
**工作量估计：** 0.5 天

#### 3.1 合并 atime 检查到 DirectReadCache

当前 `touch_atime_after_direct_read` 要额外锁 `inode_direct_read_cache` 和 `inode_atime_cache` 两把 Mutex。

**方案：** 将 `last_atime_sec` 从 `DirectReadCache` 中提升为 `plan_direct_read_cached` 的返回值的一部分，在 `plan_direct_read_cached` 已锁 `inode_direct_read_cache` 的同时检查并更新 atime，避免后续再锁。

```rust
fn plan_direct_read_cached(...) -> Result<(usize, Vec<SimpleBlockRange>, bool /* atime_ok */)> {
    let cache = self.inode_direct_read_cache.lock();
    if let Some(entry) = cache.get(&ino) {
        // ... cache hit 逻辑 ...
        let atime_ok = entry.last_atime_sec == now;
        if !atime_ok { entry.last_atime_sec = now; }
        return Ok((direct_len, mappings, atime_ok));
    }
    // ... cache miss 逻辑 ...
}
```

这样 `touch_atime_after_direct_read` 只需要在 `atime_ok == false` 时才做后续处理，且不再需要重新锁 `inode_direct_read_cache`。

#### 3.2 避免 `slice_mappings_for_range` 的 Vec 分配

当前每次 cache hit 都 `slice_mappings_for_range()` 创建新 Vec。对于顺序读，绝大多数情况下整个 cache 的 mappings 可以直接使用，或者只需要 1 个元素。

**方案：** 返回 `(&[SimpleBlockRange], usize /* skip_offset */, usize /* take_len */)` 的引用切片而非新 Vec，让调用者直接迭代 cached mappings 的子区间。

但这需要在 Mutex 锁内返回引用，存在生命周期问题。实际方案：

- 使用 `SmallVec<[SimpleBlockRange; 1]>` 代替 `Vec`，避免堆分配
- 或者让 cache hit 路径直接在锁内计算 bio 需要的 (pblock, nblocks) 列表，用固定大小数组

#### 3.3 验证

同新的主线 Step 1。

---

### Step 4（可选）：扩大 mapping cache 窗口减少 miss 频率

**状态：** 部分执行（window 已从 64/256MiB 扩大到 128/512MiB，作为 Step 1 的一部分完成；进一步扩到 1GiB 未执行）
**目标：** 减少 cache miss 次数，降低 B3 + B4 的总影响
**预期提升：** <1%
**对应 analysis：** B3, B4
**工作量估计：** 0.5 天

#### 4.1 方案

当前自适应窗口：64MB 起步，最大 256MB。对于 1GB 文件，稳态下每遍 ~4 次 miss。

将最大窗口扩大到 1GB（`DIRECT_READ_PLAN_MAX_WINDOW_BYTES = 1GB`），让一次 cache fill 覆盖整个文件。

```rust
const DIRECT_READ_PLAN_MAX_WINDOW_BYTES: usize = 1024 * 1024 * 1024; // 1GB
```

**注意：** 这会增加单次 cache miss 时 `collect_block_ranges` 的耗时（要扫描 1GB 的 extent tree），但对于顺序 I/O 的大文件，extent tree 通常很浅（可能只有 1-2 个 extent），所以扫描代价很低。

**风险：** 对于非常碎片化的文件，1GB 的映射向量可能占用较多内存。可以加一个上限（如 mappings.len() > 1024 就不缓存）。

---

## 执行顺序与依赖关系

```
Step 0（profiling）                 ← ✅ 已完成，确认 wait 才是主瓶颈
       ↓
Step 0.5（隔离主流大 bio）         ← ✅ 已完成，确认 request split 降级
       ↓
Step 0.7（scoped fast submit）     ← ✅ 已完成，作为吞吐主线前置条件
       ↓
Step 1（throughput-first pipeline）← ✅ 已完成，最终双边 95.79%，目标达成
       ↓
Step 1.5（poll-before-sleep）      ← ⏭ 跳过（目标已达成，无需执行）
Step 2（消除 inode 读盘）           ← ⏭ 跳过（收益 ~0.2%，留给后续阶段）
Step 3（减少 Mutex + Vec）          ← ⏭ 跳过（收益 ~1-2%，留给后续阶段）
Step 4（扩大 cache 窗口）           ← ⚡ 部分完成（128/512MiB，进一步扩大留给后续）
Research Track Z（零拷贝 DMA）     ← ❌ 实验后回退，保持待定
```

**Phase 1 执行结论：**
- Step 0 → 0.5 → 0.7 → Step 1 路径有效，目标提前达成
- Step 1.5/2/3 未执行，原因是目标已满足，ROI 不足以值得在 Phase 1 内继续
- Step 4 的 window 扩大已作为 Step 1 的子改动完成，1GiB 上限未触碰

---

## 验证流程

每完成一个 Step 后：

```bash
# 1. 编译检查
cargo osdk check

# 2. fio 双边对照（正式口径）
docker run -it --privileged --network=host --device=/dev/kvm -v /dev:/dev \
  -v $(pwd)/asterinas:/root/asterinas \
  asterinas/asterinas:0.17.0-20260227

cd /root/asterinas
BENCH_ENABLE_KVM=1 BENCH_ASTER_NETDEV=tap BENCH_ASTER_VHOST=on \
  bash tools/ext4/run_fio_perf_compare.sh ext4_seq_read_bw

# 3. fio_write 不回归
BENCH_ENABLE_KVM=1 BENCH_ASTER_NETDEV=tap BENCH_ASTER_VHOST=on \
  bash tools/ext4/run_fio_perf_compare.sh ext4_seq_write_bw

# 4. 功能回归四件套
#    phase3_base_guard / phase4_good / phase6_good / crash_only 必须全部通过
```

### 阶段验收标准

| 阶段 | fio_read | fio_write | 功能回归 |
|------|----------|-----------|----------|
| Step 1 完成 | **目标冲击 >= 80%，或显著逼近该门线** | 不回归（>= 100%） | 全部 PASS |
| Step 1.5 完成 | 只有在带宽继续正向变化时才算有效 | 不回归（>= 100%） | 全部 PASS |
| Step 2 完成 | +0.2% | 不回归 | 全部 PASS |
| Step 3 完成 | +1-2% | 不回归 | 全部 PASS |
| Step 4 完成 | +<1% | 不回归 | 全部 PASS |

---

## 风险与注意事项

1. **speculative bio 不能指望取消**：state 失配时，应标记 pending request 为 stale，等其自然完成后丢弃结果，而不是假设块 I/O 可以安全取消。

2. **submit-before-copy 是吞吐手段，不是目标本身**：如果实现成 “先 copy 后 submit”，流水线收益会被大幅削弱，设备 admission 也会变慢，最终很难继续抬高带宽。

3. **顺序流判定必须保守**：只在同 inode、连续 offset、大块读、单个 pending request 的条件下打开 speculative 路径，其他情况立即 fallback。

4. **fio_write 不回归**：所有改动都限定在 `read_direct_at` 路径，不应影响 write。但仍需复测确认。

5. **cache 窗口扩大的内存占用**：1GB 的 `Vec<SimpleBlockRange>` 在大文件单 extent 场景下只有 1 个元素（~24 字节），可以忽略。但碎片文件可能有数千元素，需要加上限保护。

6. **mapping cache 与 write 的一致性**：`write_direct_at` 在 append/grow 时会 `invalidate_direct_read_cache`，新的 speculative state 也必须在同一时机失效。

7. **Research Track Z 的安全性前提已确认，但当前不进入主线**：零拷贝 DMA 的安全性与 API 可行性已经验证，后续只作为备选研究方向保留。

8. **吞吐优先不等于盲目加 depth**：如果后续把 outstanding slot 从 `1` 扩到 `2+`，必须同时盯住 `large_avg_queue_wait_us`。一旦 queue handoff 重新抬头，带宽可能不升反降。

9. **Linux 对照侧 KVM 问题**：当前 Linux 侧持续出现 `kvm_intel: VMX not supported by CPU 0`。这意味着如果宿主机 KVM 恢复正常，Linux 的 baseline 可能变化，需要在环境恢复后重新评估。
