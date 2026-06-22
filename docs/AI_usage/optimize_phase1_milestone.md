# ext4 fio_read 优化 Phase 1 Milestone 记录

## Phase 1 总结（2026-04-17 完成）

| 项目 | 基线 | 最终 | 变化 |
|------|------|------|------|
| ext4_seq_read_bw（Asterinas/Linux） | 65.89%（3180/4826 MB/s） | **95.79%**（4870/5084 MB/s） | +29.9pp ✅ |
| ext4_seq_write_bw（Asterinas/Linux） | 114.57%（3074/2683 MB/s） | **90.48%**（2651/2930 MB/s） | 仍超 80% ✅ |
| 功能回归（phase3/phase4/phase6） | 全部 PASS | **全部 PASS** | ✅ |

**主要贡献路径：** Step 0.7（scoped fast-submit）→ Step 1（single-slot speculative readahead + prepare-before-wait）→ Step 4 部分（planning window 128/512MiB）

**未执行：** Step 1.5、Step 2、Step 3（目标已达成，ROI 不足）；Research Track Z（实验回退）

**代码改动范围：**
- `kernel/src/fs/ext4/fs.rs`：speculative readahead 状态机、planning window 扩大、打点（打点代码已清理）
- `kernel/comps/block/src/bio.rs`：bio fast-submit hint 字段
- `kernel/comps/block/src/impl_block_device.rs`：fast-submit 路径
- `kernel/comps/virtio/src/device/block/device.rs`：scoped direct-submit 逻辑
- `kernel/comps/virtio/src/id_alloc.rs`：支持 fast-submit 的 id 分配

---

## 基线数据 (Phase 0 完成后, 2026-04-16)

### fio 顺序读

| 测试项 | Asterinas | Linux | 比值 | 目标 |
|--------|-----------|-------|------|------|
| ext4_seq_read_bw | 3180 MB/s | 4826 MB/s | 65.89% | >= 80% |

fio 参数：`size=1G bs=1M ioengine=sync direct=1 numjobs=1 fsync_on_close=1 time_based=1 ramp_time=60 runtime=100`

### 功能测试基线

| 测试项 | 结果 |
|--------|------|
| phase3_base_guard | PASS (10/10) |
| phase4_good | PASS (12/12) |
| phase6_good | PASS (25/25) |
| crash_only | PASS (6/6) |

---

## Step 0：direct-read 打点与前提验证

**状态：** 已完成
**目标：** 拿到 `ext4_seq_read_bw` 主路径真实耗时分布，决定 Step 1 是否值得重启

### 改动概要

在 ext4 的稳定 direct-read 路径上补了聚合打点，覆盖了：
- `plan_direct_read_cached`
- `BioSegment::alloc`
- `read_blocks_async` submit
- `bio_waiter.wait`
- `segment.reader().read_fallible(writer)`
- mapping 数、cache hit/miss

同时把打点输出改成串口 `println!`，确保 benchmark 日志能稳定看到 profile 摘要。

### 涉及文件

- `asterinas/kernel/src/fs/ext4/fs.rs`

### 性能结果

本轮只跑了单边带打点 benchmark，用于路径分析，不替换正式双边基线：

| 项目 | 结果 |
|------|------|
| benchmark | `READ: bw=2923MiB/s (3065MB/s)` |
| steady-state wait | `avg_wait_us=186` |
| steady-state copy | `avg_copy_us=54` |
| steady-state plan | `avg_plan_us=0-1` |
| mappings | `avg_mappings_x100=97`，`max_mappings=2` |
| cache miss | `3 / 458752 reads` |

日志位置：
- `asterinas/.tmp_ext4_seq_read_bw_profile.log`

### 结论

- 当前稳定 direct-read 路径里，第一大头是 `bio_waiter.wait()`，不是 `memcpy`
- `Step 2` 的 cache miss 优化对 fio_read 主路径帮助极小
- `Step 3` 的 Mutex / Vec 优化也不是当前主要矛盾
- Phase 1 下一步应把主线转向 block / virtio / bio wait 路径，而不是直接重启 Step 1

### 功能回归

- `cargo check -p aster-kernel --lib --target x86_64-unknown-none ...`：PASS
- 带打点 benchmark 可以正常跑完

---

## Step 0.5：block / virtio 分层打点

**状态：** 已完成
**目标：** 隔离 fio 主流大 read bio，重算 block / virtio 的真实 `device_wait`，并确认 guest completion 路径是否还有优化空间

### 改动概要

继续把打点往 block / virtio 路径下推，覆盖了：
- `Bio::submit`
- request queue dequeue
- virtio request submit
- IRQ completion
- DMA sync 完成
- bio complete

另外在 ext4 首次进入 direct-read profiling 时自动 reset block 统计，尽量把启动和挂载阶段的小 I/O 从观测窗口里排出去。

### 涉及文件

- `asterinas/kernel/comps/block/src/bio.rs`
- `asterinas/kernel/comps/block/src/request_queue.rs`
- `asterinas/kernel/comps/virtio/src/device/block/device.rs`
- `asterinas/kernel/src/fs/ext4/fs.rs`

### 性能结果

这一步仍然是分析性跑测，不替换正式双边基线。当前从 `asterinas/.tmp_ext4_seq_read_bw_block_profile_reset.log` 观察到：

| 项目 | 结果 |
|------|------|
| queue wait | `~1μs` |
| dispatch | `~1-2μs` |
| device wait | `~91μs` |
| dma sync | `~0μs` |
| complete path | `~0μs` |
| block avg_bytes | `~339KB`（稳态） |

补充的大 read bio 过滤结果（`asterinas/.tmp_ext4_seq_read_bw_large_profile.log`）：

| 项目 | 结果 |
|------|------|
| large avg_bytes | `~1047287` |
| large queue wait | `~0μs` |
| large dispatch | `~1μs` |
| large device wait | `~221-224μs` |

同轮 ext4 映射画像（`asterinas/.tmp_ext4_seq_read_bw_mapping_profile.log`）：

| 项目 | 结果 |
|------|------|
| avg_mapped_bytes | `~1015808` |
| avg_zero_fill_bytes | `~32768` |
| ext4 avg_wait | `~218-221μs` |

继续把 completion wait 拆细后的结果（`asterinas/.tmp_ext4_seq_read_bw_irq_profile.log`）：

| 项目 | 结果 |
|------|------|
| large device wait | `~243-245μs` |
| large irq delivery | `~243-244μs` |
| large irq reap | `~0μs` |
| large resp sync | `~0μs` |
| ext4 avg_wait | `~240-242μs` |
| ext4 avg_copy | `~65-66μs` |

### 结论

- 软件队列与 virtio 提交路径不是当前主瓶颈
- `device_wait` 才是 block 侧最重的一段
- 继续补 ext4 mapping 画像后发现：`avg_mapped_bytes ~992KB`、`avg_zero_fill_bytes ~32KB`
- 因此，“块层只有 `339KB` 平均 read bio” 更像是被小 read 杂音稀释后的结果，`request split` 暂时降级
- 过滤到 fio 主流大 read bio 后，`large_avg_device_wait ≈ 221-224μs`，已经几乎和 ext4 的 `avg_wait ≈ 218-221μs` 重合
- 继续拆分后发现：`large_avg_irq_delivery` 几乎和 `large_avg_device_wait` 重合，而 `irq_reap / resp_sync` 约等于 `0μs`
- 这说明 guest 内的 completion bookkeeping 几乎已经没有收益空间，当前更值得转向 `virtio/host I/O 完成等待`
- copy 仍占 `~65-66μs`，但其实现已经是 `rep movsb + ermsb` 方向，优先级低于 completion wait 链路

### 功能回归

- `cargo check -p aster-block -p aster-virtio -p aster-kernel --lib --target x86_64-unknown-none ...`：PASS

---

## Step 1.5：completion wait 验证性实验

**状态：** ⏭ 跳过（Phase 1 目标已达成，无需执行）
**目标：** 用 `poll-before-sleep` 验证 `bio_waiter.wait()` 里的隐藏调度损耗是否值得作为辅助手段保留

### 改动概要

本阶段尚未开始写代码。当前已明确：
- `poll-before-sleep` 方向是合理的
- 但它更像是验证性实验，不是当前默认主线
- 即使有效，预期收益也更可能是回收一部分 `WaitQueue sleep + wakeup + reschedule` 的隐藏成本，而不是单独把 `fio_read` 拉到 `>= 80%`

### 涉及文件

（待填写）

### 性能结果

（待填写）

### 功能回归

（待填写）

---

## Step 0.7：Speculative Fast Submit / Queue-Wait Reduction

**状态：** 已完成
**目标：** 把 speculative direct-read 从“更早 enqueue”推进到“更早 device-submit”，优先压低 `large_avg_queue_wait_us`

### 改动概要

本阶段最终做成了一个“收窄到 speculative request”的 fast-submit：
- 在 block bio 上增加 per-bio fast-submit hint
- ext4 只给 speculative direct-read prefetch 打这个 hint
- virtio-block 只在大块 read、software queue 为空、且 hint 命中时尝试直接提交到 virtqueue
- 同时保留完整 fallback，任何条件不满足都回到现有 queue 路径

中间还做过一轮更宽的 fast-submit 实验：
- 第一版把所有大 read 都尝试直提，`queue_wait` 的确下降了
- 但吞吐明显回退，说明正常 foreground read 路径不该被一起扰动
- 因此最终版本明确收敛成“只优化 speculative request 的 queue handoff”

### 涉及文件

- `asterinas/kernel/comps/block/src/bio.rs`
- `asterinas/kernel/comps/block/src/impl_block_device.rs`
- `asterinas/kernel/comps/virtio/src/device/block/device.rs`
- `asterinas/kernel/comps/virtio/src/id_alloc.rs`
- `asterinas/kernel/src/fs/ext4/fs.rs`

### 性能结果

单边 `size=1G bs=1M` 结果如下：

| 项目 | 结果 |
|------|------|
| 基线（仅 submit-before-copy） | `READ: bw=3479MiB/s (3648MB/s)` |
| 过宽 fast-submit（失败实验） | `READ: bw=2499MiB/s (2620MB/s)` |
| 收窄后 fast-submit（最终保留） | `READ: bw=3518MiB/s (3689MB/s)` |
| 最终 ext4 avg_wait | `~64μs` |
| 最终 ext4 avg_copy | `~64μs` |
| 最终 large queue wait | `0μs` |
| 最终 large device wait | `~229μs` |

这说明：
- `queue_wait` 确实是被这一步正面打掉的瓶颈
- 但 fast-submit 必须限制在 speculative request，不能顺手覆盖所有大 read
- 最终版本相对 0.7 前的单边基线多出了 `+39MB/s` 左右的吞吐

### 功能回归

- `cargo check -p aster-block -p aster-virtio -p aster-kernel --lib --target x86_64-unknown-none` 通过
- 单边 `fio/ext4_seq_read_bw`（`size=1G`）可稳定跑通
- 功能回归（2026-04-17）：phase3_base 100%、phase4_good 100%、phase6_good 100%，全部 PASS

---

## Step 1：Speculative Readahead + Double Buffering（主线）

**状态：** ✅ 已完成（2026-04-17，双边对照 95.79%，目标达成）
**目标：** 通过 `submit-before-copy` 时序，把当前次 copy 与下一次 I/O 等待重叠起来，作为当前最有希望冲击 `>= 80%` 的主线

### 调研结论

当前的关键判断已经收敛到：
- `device_wait / irq_delivery` 是主瓶颈
- copy 仍有 `~65-66μs`
- 真正的结构性问题不是单个慢函数，而是 `I/O wait + copy` 完全串行
- 但在第一版原型之后，还新增确认了一点：`submit-before-copy` 还需要配套更早的 device submit，否则收益会被新的 `queue_wait` 吞掉

因此，新主线不再是零拷贝 DMA，而是：
- 只针对 `O_DIRECT read`
- 只针对同 inode、连续顺序的大块读
- 使用极保守的 speculative readahead
- 并明确采用 `submit-before-copy`

### 改动概要

当前主线已经进入第一轮实现，保留了 `Step 0.7` 的 scoped fast-submit 前提，并完成了 single-slot speculative pipeline 的第一次稳定化：
- `O_DIRECT read`
- 同 inode 连续顺序读
- `next_offset == current_offset + current_direct_len`
- 大块请求，例如 `>= 512KB`
- 同时最多只保留 `1` 个 in-flight speculative request

这轮最终保留的时序是：
- 先在当前 I/O 仍 inflight 时提前做 `plan_next`
- 当前 bio 完成后，立刻 `submit_next`
- 再做当前次 `copy`
- 下次 `read_direct_at` 进来时，优先命中已经 in-flight 的 pending bio

中间还试过一轮“两槽 speculative pipeline”：
- `pending slot = 2` 的版本可以维持 `large_avg_queue_wait_us = 0`
- 但会把 `large_avg_device_wait_us` 推高到 `~370us`
- 这说明当前阶段盲目加 depth 只是在设备侧重新堆 wait，因此没有保留

### 涉及文件

- `asterinas/kernel/src/fs/ext4/fs.rs`

### 性能结果

单边 `size=1G bs=1M` 结果如下：

| 项目 | 结果 |
|------|------|
| 0.7 基线 | `READ: bw=3518MiB/s (3689MB/s)` |
| Step 1 两槽实验（失败） | 中途观测 `large_avg_device_wait_us ≈ 370us`，未保留 |
| Step 1 single-slot + prepare-before-wait | `READ: bw=3533MiB/s (3705MB/s)` |
| Step 1 single-slot + submit-before-wait（失败） | `READ: bw=3293MiB/s (3452MB/s)`，未保留 |
| Step 1 single-slot + larger plan window（当前保留） | `READ: bw=4076MiB/s (4274MB/s)` |
| Step 1 正式双边对照（`size=1G bs=1M`，2026-04-17 最终） | Asterinas `4644MiB/s (4870MB/s)`；Linux `4849MiB/s (5084MB/s)`；Asterinas/Linux `= 95.79%` |
| 当前 ext4 avg_plan | `~39us` |
| 当前 ext4 avg_wait | `~50us` |
| 当前 ext4 avg_copy | `~57us` |
| 当前 large queue wait | `0us` |
| 当前 large device wait | `~196-197us` |

这轮结论是：
- `plan_next` 提前到 wait 之前是正收益，说明 Step 1 不只是“submit-before-copy”，也包括“prepare-before-wait”
- single-slot 版本仍然是当前最稳的吞吐前进方向
- `pending slot = 2` 暂时不是该继续追的方向
- 再把 `submit_next` 前移到当前 `wait` 之前会让 `avg_wait_us` 看起来更低，但总吞吐反而回退，因此这条线不保留
- 在保留当前 single-slot 时序的前提下，把 direct-read planning window 从 `64/256MiB` 放大到 `128/512MiB` 后，`cache_miss` 从 `3072` 降到 `2048`，单边吞吐直接抬到 `4076MiB/s (4274MB/s)`
- 正式双边 `size=1G bs=1M` 对照下，Asterinas `4186MiB/s (4390MB/s)`，Linux `4378MiB/s (4591MB/s)`，真实比例约为 `95.6%`
- 最终双边（2026-04-17）：Asterinas `4644MiB/s (4870MB/s)`，Linux `4849MiB/s (5084MB/s)`，比例 `95.79%`

### 功能回归

- `cargo check -p aster-kernel --lib --target x86_64-unknown-none` 通过
- 双边 `fio/ext4_seq_read_bw` 和 `fio/ext4_seq_write_bw`（`size=1G`）可稳定跑通
- 功能回归（2026-04-17）：phase3_base 100%、phase4_good 100%、phase6_good 100%，全部 PASS

---

## Research Track Z：零拷贝 DMA（用户页直接做 DMA 目标）

**状态：** 待定，已做原型验证但结果回归，实验代码已回退
**目标：** 仅作为备选研究方向保留，不再作为当前主线

### 调研结论

API 链已全部验证可行，不需要新增 ostd API：
- `VmWriter::cursor()` → 用户 VA
- `current_userspace!().vmar().vm_space()` → VmSpace
- `disable_preempt()` → DisabledPreemptGuard (满足 AsAtomicModeGuard)
- `vm_space.cursor(&guard, &va_range).query()` → FrameRef → UFrame → USegment
- `BioSegment::new_from_segment(useg, FromDevice)` → 零拷贝 bio
- virtio-blk 原生支持多段 scatter/gather（最多 62 segments/bio）
- DMA 完成后自动 sync_from_device()
- Frame::clone() 增引用计数保护 DMA 期间页安全

### 改动概要

本轮做过一版 ext4 direct read zero-copy 原型验证，思路是把用户页直接转换成 `BioSegment`，绕过中间 DMA buffer 与 `memcpy`。

实验后结论是：
- 该原型在当前环境下会把 1MB direct read 拆成过多 scatter/gather segment
- virtio-blk 队列侧的 segment/bio 开销大于省掉的 copy 开销
- benchmark 明显回归，因此没有保留这版实现

处理结果：
- 已将这条路线降级为待定
- 相关实验代码已从仓库回退
- 当前代码恢复到实验前的稳定 direct-read 路径

### 涉及文件

本轮实验临时涉及过：
- `asterinas/kernel/src/fs/ext4/fs.rs`
- `asterinas/ostd/src/mm/frame/segment.rs`

当前状态：
- 上述 zero-copy 实验性改动已回退
- 仓库未保留这条路线的实现代码

### 性能结果

实验结果只记录为路线评估依据，不作为正式里程碑收益：

| 版本 | Asterinas 结果 | 说明 |
|------|----------------|------|
| 稳定基线 | 3180 MB/s | Phase 0 完成后现状 |
| 零拷贝原型 | 812 MiB/s | 全量尝试用户页 zero-copy，明显回归 |
| 阈值折中版 | 1594 MiB/s | 只在平均连续段较大时尝试，仍明显低于基线 |

结论：该路线当前不能提升 `ext4_seq_read_bw`，反而会拉低吞吐，故改为待定。

### 功能回归

- `cargo osdk check`：PASS
- zero-copy 实验代码已回退，仓库恢复到稳定 direct-read 实现
- 本轮未保留任何会导致 fio_read 回归的实验代码

---

## Step 2：消除 cache miss 时的 inode 磁盘读

**状态：** ⏭ 跳过（Phase 1 目标已达成，收益 ~0.2%，留给后续阶段）
**目标：** cache miss 路径减少 ~100-200μs

### 改动概要

（待填写）

### 涉及文件

（待填写）

### 性能结果

（待填写）

### 功能回归

（待填写）

---

## Step 3：减少 per-read Mutex + Vec 开销

**状态：** ⏭ 跳过（Phase 1 目标已达成，收益 ~1-2%，留给后续阶段）
**目标：** 稳态每次读减少 ~5-7μs

### 改动概要

（待填写）

### 涉及文件

（待填写）

### 性能结果

（待填写）

### 功能回归

（待填写）

---

## Step 4（可选）：扩大 mapping cache 窗口

**状态：** ⚡ 部分完成（window 128/512MiB 已作为 Step 1 子改动落地；进一步扩到 1GiB 未执行）
**目标：** 减少 cache miss 频率

### 改动概要

（待填写）

### 涉及文件

（待填写）

### 性能结果

（待填写）

### 功能回归

（待填写）

---

## 变更日志

| 日期 | Step | 操作 | 结果 |
|------|------|------|------|
| 2026-04-16 | - | 建立 Phase 1 基线、完成瓶颈分析、制定优化方案 | 见 analysis_phase1.md / optimize_plan_phase1.md |
| 2026-04-16 | Step 0 | 完成 ext4 direct-read 聚合打点并跑通带日志 benchmark | 确认 wait 才是主瓶颈，Step 1 不再是默认主线 |
| 2026-04-16 | Step 0.5 | 完成 block / virtio 分层打点，并补充 ext4 mapping 画像 | 确认 queue/dispatch 很轻，request split 优先级下降，下一步隔离大 read bio 重算 device wait |
| 2026-04-16 | Step 0.5 | 继续拆分 completion wait 到 IRQ delivery / reap / 响应头同步 | 确认 `device_wait` 几乎全是 IRQ delivery，guest completion bookkeeping 收益空间极小 |
| 2026-04-16 | Step 1 | 完成 zero-copy DMA 原型验证、记录回归原因、回退实验代码 | 路线降级为 Research Track Z，未进入主线 |
| 2026-04-17 | Step 1.5 / Step 1 | 按最新 profiling 重排 Phase 1 主线结构 | `Step 1.5` 保留为验证性实验，主线改成 speculative readahead + double buffering |
| 2026-04-17 | Step 1 | 完成 single-slot 首轮稳定化，并验证 prepare-before-wait 为正收益 | 单边提升到 `3533MiB/s (3705MB/s)`，两槽扩 depth 暂不保留 |
| 2026-04-17 | Step 1 | 验证 submit-before-wait 并回退实验代码 | `avg_wait_us` 下降，但单边回退到 `3293MiB/s (3452MB/s)`，说明更早 submit 不等于更高吞吐 |
| 2026-04-17 | Step 1 | 放大 direct-read planning window 并保留该版本 | `cache_miss` 显著下降，单边提升到 `4076MiB/s (4274MB/s)` |
| 2026-04-17 | Step 1 | 跑正式双边 `ext4_seq_read_bw`（统一 `1G` 口径） | Asterinas `4186MiB/s (4390MB/s)`，Linux `4378MiB/s (4591MB/s)`，真实比例 `95.6%` |
| 2026-04-17 | 最终验收 | 双边 fio read + write + phase6_with_guard 功能回归 | read 95.79%、write 90.48%；phase3/phase4/phase6 全 100% PASS |
