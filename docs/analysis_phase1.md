# Asterinas ext4 fio_read 性能瓶颈分析 Phase 1

本文档聚焦 **fio sequential read (O_DIRECT)** 的性能差距，目标是将当前 65.89% 提升到 >= 80%。

## 当前结论修正（2026-04-16）

“零拷贝 DMA，把用户页直接作为 DMA 目标”这条路线目前改为`待定`。

原因不是方向完全错误，而是当前 benchmark 场景下实际收益与理论预期相反：
- 原稳定基线：Asterinas 约 `3180 MB/s`
- 零拷贝原型：Asterinas only 实测约 `812 MiB/s`
- 加连续性阈值后的折中版：Asterinas only 实测约 `1594 MiB/s`

这说明当前主要矛盾不是“少一次 memcpy 就一定更快”，而是：
- 用户 buffer 物理页不够连续
- direct read 被拆成过多 scatter/gather segment
- virtio-blk 队列侧的 segment/bio 开销大于省掉的 copy 开销

因此，本文后续关于 zero-copy 的分析保留为“研究方向”，不再视为已经验证有效的主方案。当前仓库代码也已回退到实验前的稳定 direct-read 路径。

## 最新 profiling 结论（2026-04-16）

基于带打点的单边 `ext4_seq_read_bw` 运行，日志见 `asterinas/.tmp_ext4_seq_read_bw_profile.log`，当前稳定 direct-read 路径的关键观测值是：
- benchmark 实测：`READ: bw=2923MiB/s (3065MB/s)`
- 稳态 profile：`avg_wait_us=186`
- 稳态 profile：`avg_copy_us=54`
- 稳态 profile：`avg_plan_us=0-1`
- 稳态 profile：`avg_mappings_x100=97`，`max_mappings=2`
- `cache_miss=3 / 458752 reads`

这意味着：
- 当前稳定路径里，第一大头是 `bio_waiter.wait()`，不是 `memcpy`
- ext4 层的 `plan_direct_read_cached`、`BioSegment::alloc`、submit 开销都已经很小
- `Step 2` 和 `Step 3` 即使做完，也很难解释当前与 Linux 的主要差距
- Phase 1 下一步更应该转向 block / virtio / bio wait 路径，而不是继续假设“少一次 memcpy 就够了”

## 新增 block / virtio profiling 结论（2026-04-16）

在继续把打点往下一层推进后，日志 `asterinas/.tmp_ext4_seq_read_bw_block_profile_reset.log` 显示：
- `queue_wait` 稳态约 `1μs`
- `dispatch` 稳态约 `1-2μs`
- `device_wait` 稳态约 `91μs`
- `dma_sync` / `complete` 基本可忽略
- `avg_bytes` 在 benchmark 稳态收敛到约 `339KB`

这里有两个重要结论：

1. software queue 与提交路径本身很轻  
`request_queue` 出队和 virtio 提交都不是主要耗时源，ext4 之前怀疑的“锁、分配、submit bookkeeping”现在基本可以继续降级优先级。

2. 当时最值得继续验证的是 request split  
ext4 层每次 direct read 目标是 1MB，但 block 层 read bio 的平均字节数只收敛到约 `339KB`。这在当时提示我们需要继续验证 benchmark 主读流是否在更下层被拆小。

后续的 ext4 mapping profiling 已经表明，这个怀疑没有得到直接支持；但它仍然帮助我们把调查范围从 ext4 bookkeeping 转到了“块层主流样本是否被杂音稀释”这个方向。

它已经足够说明一个方向：
- 当前差距不在 ext4 的 `plan_direct_read_cached()`
- 也不主要在 `BioSegment::alloc()` 或 bio submit
- 更可能在块设备等待时间，以及统计窗口被小 read 混入后带来的误判

## 对 request split 的修正判断（2026-04-16）

继续把 ext4 自身的 mapping 画像打出来之后，日志 `asterinas/.tmp_ext4_seq_read_bw_mapping_profile.log` 显示：
- `avg_bytes=1048576`
- `avg_mapped_bytes=1015808`
- `avg_zero_fill_bytes=32768`
- `max_mapped_bytes=1048576`

这说明 ext4 稳态下每次 direct-read 实际提交给块层的映射数据量约为 `992KB`，而不是只有 `339KB`。因此，前面基于 block 平均字节数提出的 “1MB 被拆成 ~339KB request” 这个怀疑，目前没有得到 ext4 侧数据支持。

更合理的解释是：
- `block-profile` 当前统计窗口里混入了大量 4KB 级别的小 read bio
- 这些小 read 很可能来自 benchmark 运行期的文件布局、元数据访问或其他辅助路径
- 它们把 block 侧的 `avg_bytes` 明显拉低了

所以，当前更准确的下一步不是直接追 `request split`，而是先把 fio 主流的大 read bio 单独过滤出来，再重新看 `device_wait`

## large read bio 过滤结果（2026-04-16）

进一步把 block profile 过滤到 `>=512KB` 的 read bio 后，日志 `asterinas/.tmp_ext4_seq_read_bw_large_profile.log` 显示：
- `large_avg_bytes ≈ 1047287`
- `large_avg_segments_x100 = 100`
- `large_avg_queue_wait_us = 0`
- `large_avg_dispatch_us = 1`
- `large_avg_device_wait_us = 221-224`

同时，同轮 ext4 侧打点是：
- `avg_mapped_bytes ≈ 1015808`
- `avg_wait_us = 218-221`

这组数据很重要，因为它把“主流样本”和“杂音”彻底分开了：
- 主流 fio read 确实是单个接近 1MB 的大 bio，不是碎小 request
- `request_queue` 和 virtio submit 的软件开销几乎可以忽略
- ext4 里的 `bio_waiter.wait()` 已经几乎完全等于“大块 read bio 的真实完成等待”

换句话说，guest 内部目前已经没有明显的大块软件瓶颈；差距主要落在：
- 大 read bio 的完成等待时间本身
- 以及 `~61μs` 左右的用户态 copy 成本

## 进度基准

| 指标 | Asterinas | Linux | 比值 |
|------|-----------|-------|------|
| ext4_seq_read_bw | 3180 MB/s | 4826 MB/s | 65.89% |

fio 参数：`size=1G bs=1M ioengine=sync direct=1 numjobs=1 fsync_on_close=1 time_based=1 ramp_time=60 runtime=100`

---

## 1. 热路径逐帧分析

每次 1MB O_DIRECT 顺序读的完整调用链：

```
[用户态] fio: read(fd, buf, 1MB)  (O_DIRECT)
  │
[syscall] sys_read → InodeIo::read_at()
  │
[inode.rs:148-153] 检测 O_DIRECT → fs.read_direct_at(ino, offset, writer)
  │
[fs.rs:1669] plan_direct_read_cached(ino, offset, 1MB)
  │       ├─ lock inode_direct_read_cache (Mutex)          ← B6
  │       ├─ [cache HIT] slice_mappings_for_range()        ← B5 (Vec 分配)
  │       │   └─ 遍历 cached mappings，创建新 Vec<SimpleBlockRange>
  │       ├─ [cache MISS] run_ext4(ext4_plan_direct_read)  ← B3, B4
  │       │   ├─ prepare_ext4_io()
  │       │   ├─ lock EXT4_RS_RUNTIME_LOCK (static Mutex)  ← B4
  │       │   ├─ lock self.inner (Mutex<Ext4>)             ← B4
  │       │   ├─ sync_runtime_block_size()
  │       │   ├─ get_inode_ref(ino)                        ← B3 (磁盘 I/O!)
  │       │   │   └─ Block::load() → read_offset() → Vec<u8> 分配 + 同步块读
  │       │   ├─ collect_block_ranges()                    ← extent tree 遍历
  │       │   └─ finish_ext4_io()
  │       └─ unlock + 更新 cache
  │
[fs.rs:1676-1683] 提交 bio（对每个 mapping）
  │       ├─ BioSegment::alloc(N blocks, FromDevice)       ← B2 (DMA 分配)
  │       │   └─ 从 pool 切 N×4KB；pool 不够则 DmaStream::alloc_uninit()
  │       ├─ bio_segment.clone() (Arc clone)
  │       └─ read_blocks_async(Bid, segment) → Bio::new + submit
  │
[fs.rs:1685] bio_waiter.wait()                             ← 真实磁盘 I/O 时间
  │
[fs.rs:1693-1706] 数据拷贝（对每个 segment）
  │       └─ segment.reader().read_fallible(writer)        ← B1 (1MB memcpy!)
  │
[fs.rs:1712] touch_atime_after_direct_read()
        ├─ lock inode_direct_read_cache (Mutex)            ← B6
        ├─ 检查 last_atime_sec == now → 通常早退
        └─ lock inode_atime_cache (Mutex)                  ← B6
```

---

## 2. 瓶颈量化分析

### 时间预算

| 项目 | Asterinas | Linux |
|------|-----------|-------|
| 每秒读取次数 | ~3180 次/s | ~4826 次/s |
| 每次读耗时 | ~314 μs | ~207 μs |
| **差距** | **~107 μs / 次** | - |

以下逐一量化每个瓶颈对 107μs 差距的贡献。

---

### B1 [关键 ~50μs/次]：DMA buffer → 用户 buffer 的 1MB memcpy

**位置：** [fs.rs:1700-1704](asterinas/kernel/src/fs/ext4/fs.rs#L1700-L1704)

```rust
segment
    .reader()
    .map_err(Self::vm_io_error)?
    .read_fallible(writer)
    .map_err(|(e, _)| Error::from(e))?;
```

**问题：** 每次 1MB 读完成后，数据先在 DMA buffer 中，然后 `read_fallible(writer)` 把 1MB 完整拷贝到用户空间 buffer。

**量化：** 按 ~20 GB/s 内存带宽计算：
- 1MB memcpy ≈ 50μs
- 占总差距的 ~47%

**对比 Linux：** Linux O_DIRECT 使用 `get_user_pages()` 将用户页直接 pin 为 DMA 目标，**零拷贝**。用户 buffer 就是 DMA buffer，没有中间 memcpy。

**根因：** Asterinas 的 `read_direct_at` 先 `BioSegment::alloc()` 分配独立 DMA buffer，bio 完成后再把数据 copy 到 `VmWriter`（用户 buffer）。缺少将 VmWriter 底层物理页直接映射为 DMA target 的基础设施。

---

### B2 [中等 ~10-15μs/次]：per-read DMA buffer 分配与释放

**位置：** [fs.rs:1677](asterinas/kernel/src/fs/ext4/fs.rs#L1677)

```rust
let bio_segment = BioSegment::alloc(mapping.len as usize, BioDirection::FromDevice);
```

**问题：** 每次 1MB 读都从 BioSegmentPool 分配 256 个 block（1MB），读完后释放回 pool。

**量化：**
- Pool 容量：4096 blocks（16MB），见 [bio.rs:697](asterinas/kernel/comps/block/src/bio.rs#L697)
- 分配路径：SpinLock + bit scan + mark，约 5-8μs
- 释放路径：SpinLock + bit clear，约 3-5μs
- Arc clone (bio_segment.clone)：1-2μs
- 合计 ~10-15μs

**对比 Linux：** Linux O_DIRECT 直接 pin 用户页做 DMA，不需要分配/释放中间 DMA buffer。

---

### B3 [中等 ~100-200μs/次 × 低频]：cache miss 时 `get_inode_ref()` 读盘

**位置：** [ext4_rs/src/ext4_impls/inode.rs:143-154](asterinas/kernel/libs/ext4_rs/src/ext4_impls/inode.rs#L143-L154)

```rust
pub fn get_inode_ref(&self, inode_num: u32) -> Ext4InodeRef {
    let offset = self.inode_disk_pos(inode_num);
    let mut ext4block = Block::load(&self.block_device, offset);  // 同步读盘!
    let inode: &mut Ext4Inode = ext4block.read_as_mut();
    Ext4InodeRef { inode_num, inode: *inode }
}
```

**问题：** `plan_direct_read()` 每次被调用都 `get_inode_ref()` 读 inode 块。虽然 kernel 侧已有 `inode_meta_cache` 缓存了 `SimpleInodeMeta.size`，但 ext4_rs 内部不知道，仍然每次从磁盘加载整个 inode。

**发生频率：**
- Mapping cache 窗口 64MB-256MB（自适应），1GB 文件 ~4 次/遍
- 3180 MB/s 下每秒约 3.18 次全文件遍历 = ~13 次 cache miss/秒
- 每秒实际影响：~13 × 150μs = ~2ms / 秒 ≈ 总时间的 0.2%

虽然频率低，但每次 miss 的绝对开销大：
- `Block::load()` 调用 `read_offset()` → allocate `Vec<u8>` + 同步块设备读
- 加上 `run_ext4` 的两把 Mutex

**对比 Linux：** Linux 的 inode 在内存中有 `struct inode` 缓存（VFS inode cache），direct read 从不为了拿 file_size 去读盘。

---

### B4 [中等 ~5-10μs/次 × 低频]：cache miss 时 `run_ext4` 双 Mutex

**位置：** [fs.rs:944-957](asterinas/kernel/src/fs/ext4/fs.rs#L944-L957)

```rust
pub(super) fn run_ext4<T>(&self, f: impl FnOnce(&Ext4) -> ...) -> Result<T> {
    self.prepare_ext4_io();
    let _runtime_guard = EXT4_RS_RUNTIME_LOCK.lock();  // 全局静态 Mutex
    let result = {
        let inner = self.lock_inner();                  // Mutex<Ext4>
        inner.sync_runtime_block_size();
        f(&inner).map_err(map_ext4_error)?
    };
    self.finish_ext4_io()?;
    Ok(result)
}
```

**问题：** 即使只是做只读的映射查询 (`plan_direct_read`)，也要拿两把排它锁：
- `EXT4_RS_RUNTIME_LOCK`：全局静态 Mutex，保护 ext4_rs 的全局 `runtime_block_size` 变量
- `self.inner`：`Mutex<Ext4>`，独占整个 ext4 实例

**影响：**
- 单线程 fio 下不存在竞争，但 Mutex 获取/释放仍有原子操作开销
- `sync_runtime_block_size()` 每次写全局变量
- `prepare_ext4_io()` / `finish_ext4_io()`：clear/check I/O 失败标记

---

### B5 [低 ~2-3μs/次]：per-read Vec 分配

**位置：** [fs.rs:760-797](asterinas/kernel/src/fs/ext4/fs.rs#L760-L797)（`slice_mappings_for_range`），[fs.rs:1675](asterinas/kernel/src/fs/ext4/fs.rs#L1675)

**问题：**
- `slice_mappings_for_range()` 每次创建新 `Vec<SimpleBlockRange>` 并 push 元素
- `Vec::with_capacity(mappings.len())` 为 bio_segments 分配
- `Bio::new()` 内部 `vec![bio_segment]`

对于连续 1MB 读（常见情况），这些 Vec 都只有 1 个元素，但堆分配开销仍然存在。

---

### B6 [低 ~3-5μs/次]：per-read 3 次 Mutex lock/unlock

**位置：**
- [fs.rs:824](asterinas/kernel/src/fs/ext4/fs.rs#L824)：`inode_direct_read_cache.lock()` in `plan_direct_read_cached`
- [fs.rs:930](asterinas/kernel/src/fs/ext4/fs.rs#L930)：`inode_direct_read_cache.lock()` in `touch_atime_after_direct_read`
- [fs.rs:665](asterinas/kernel/src/fs/ext4/fs.rs#L665)：`inode_atime_cache.lock()` in `touch_atime`

**问题：** 稳态下每次 1MB 读取至少经过 3 次 Mutex lock/unlock。虽然在单线程 fio 场景下无竞争，但每次 Mutex 操作仍涉及原子 compare-and-swap + 可能的缓存行 miss。

---

### B7 [推测 ~35μs/次]：WaitQueue sleep + wakeup + reschedule 隐藏成本

**状态：** 推测，待独立验证

`bio_waiter.wait()` 当前使用 WaitQueue 休眠等待 IRQ 唤醒。IRQ handler 触发 `complete()` 后，阻塞线程仍需要重新被调度回来，才能继续执行 copy 与返回用户态。

这部分成本在当前打点中没有被独立测出，但从总时间反推，存在一个数量级约 `~35μs` 的“隐藏调度成本”是合理的：
- 当前每次 1MB 读总耗时约 `315μs`
- 真实 device wait 更接近 `~220μs` 量级
- copy 约 `~60μs`
- 差值约 `315 - 220 - 60 = 35μs`

这里必须强调：这个数字目前只是工作假设，不是已证明事实。后续若要验证，可在：
- `WaitQueue::wake_all()` 或完成回调触发点
- 线程真正从 `bio_waiter.wait()` 返回的时刻

之间补一组时间戳。

### B8 [结构性 ~60μs/次]：I/O 与 copy 完全串行

这不是一个“局部慢函数”，而是当前 `read_direct_at` 路径的结构性问题：

```
submit_1 -> wait_1 -> copy_1 -> submit_2 -> wait_2 -> copy_2 -> ...
```

在这条时序里：
- 当前次 `copy` 的 `~60μs`
- 与下一次 I/O 的 `device_wait`

是完全串行的，因此 copy 的整个窗口都暴露在关键路径上。

而对于当前 benchmark 场景（单线程、顺序、1MB O_DIRECT read），如果能做到：

```
wait_1 -> submit_2 -> copy_1 -> wait_2(剩余部分) -> submit_3 -> copy_2 -> ...
```

那么 copy 的大部分时间就有机会被下一次 I/O 的 in-flight 窗口覆盖掉。

这也是为什么当前比起“继续微调 memcpy”或“继续抠 completion bookkeeping”，更值得优先考虑 speculative readahead / double buffering 这类结构性改动。

---

## 3. 瓶颈归因汇总

| 编号 | 瓶颈 | 估计每次开销 | 频率 | 每秒总开销 | 占 107μs 差距 |
|------|------|-------------|------|-----------|---------------|
| B1 | 1MB memcpy (DMA→user) | ~50μs | 每次读 | ~159ms/s | **~47%** |
| B2 | DMA buffer alloc+free | ~12μs | 每次读 | ~38ms/s | **~11%** |
| B3 | get_inode_ref 读盘 | ~150μs | ~13次/s | ~2ms/s | ~2% |
| B4 | run_ext4 双锁 | ~8μs | ~13次/s | ~0.1ms/s | <1% |
| B5 | per-read Vec 分配 | ~3μs | 每次读 | ~10ms/s | ~3% |
| B6 | 3x Mutex per read | ~4μs | 每次读 | ~13ms/s | ~4% |
| B7 | WaitQueue 调度损耗（推测） | ~35μs | 每次读 | ~111ms/s | 推测项 |
| B8 | I/O-copy 串行暴露的 copy 窗口 | ~60μs 可隐藏空间 | 每次读 | 结构性 | 主线候选 |
| - | 其他（bio 创建、Arc、WaitQueue 等） | ~30μs | 每次读 | ~95ms/s | ~28% |

**修正结论：** 这张表混合了三类信息：
- 已量到的热点：`wait`、`copy`
- 已明显降级的点：B3-B6
- 仍待验证的结构性推断：B7、B8

最新实测已经表明，当前稳定路径的主瓶颈是 `bio_waiter.wait()`，不是 B1。B1 仍然有成本，但真正关键的是它与下一次 I/O 完全串行。

---

## 4. 可行的优化方向

### 方向一：Speculative Readahead + Double Buffering（主线）

通过非常保守的预测式预读，把“当前次 copy”与“下一次 I/O 等待”重叠起来。

**当前状态：** 最有希望达到 `>= 80%` 的主线，但尚未实做验证。

**关键设计：submit-before-copy**

必须采用：
```
wait_1 -> plan_next -> submit_2 -> copy_1 -> return -> next read 命中 pending bio
```

而不能采用：
```
wait_1 -> copy_1 -> submit_2 -> return
```

因为两者的收益量级差距，几乎完全由“下一次 bio 的提交时机”决定。

**保守 gating：**
- 只做 `O_DIRECT read`
- 只做同 inode 连续顺序读
- `next_offset == current_offset + current_direct_len`
- 只做大块请求，例如 `>= 512KB`
- 同时最多只允许 `1` 个 in-flight speculative request
- 任意失配立即 fallback 到当前稳定串行路径

**资源与正确性边界：**
- 不尝试取消已提交 bio；失配时把 pending request 标成 stale，等其自然完成后丢弃结果
- 使用现有 `BioSegmentPool` 做双缓冲
- stale 状态必须和 `invalidate_direct_read_cache` 一起失效

**预期收益：** 当前最有希望量化冲击 `>= 80%`
**风险：** 中 — 需要小心维护 pending state 与失效边界，但可以用很保守的 gating 控制风险

### 方向二：零拷贝 DMA（待定）

将用户 buffer 的物理页直接映射为 DMA 目标，完全跳过中间 DMA buffer。

**当前状态：** 理论可行，但实验结果回归，暂不继续主推。

**本轮验证结论：**
- full zero-copy 原型会因为用户页碎片化生成过多 SG segment，吞吐下降到约 `812 MiB/s`
- 加连续性阈值后虽有恢复，但仍只有约 `1594 MiB/s`
- 相比稳定基线 `3180 MB/s`，都属于明显回归

**结论：** 这条路线当前只能保留为备选研究方向；现阶段不能作为达到 80% 的主方案

**技术路线（已验证可行）：**

1. 从 `VmWriter::cursor()` 获取用户虚拟地址（[io.rs:955](asterinas/ostd/src/mm/io.rs#L955)）
2. 通过 `current_userspace!().vmar().vm_space()` 获取 VmSpace（[context.rs:57-66](asterinas/kernel/src/context.rs#L57-L66)）
3. `disable_preempt()` 获取 `DisabledPreemptGuard`（实现了 `AsAtomicModeGuard`）
4. `vm_space.cursor(&guard, &va_range)` 创建页表只读 Cursor（[vm_space.rs:95-101](asterinas/ostd/src/mm/vm_space.rs#L95-L101)）
5. 遍历 `cursor.query()` → `VmQueriedItem::MappedRam { frame: FrameRef<dyn AnyUFrameMeta>, ... }`（[vm_space.rs:272-275, 573-582](asterinas/ostd/src/mm/vm_space.rs#L272-L282)）
6. `frame.clone()` 获取 `UFrame`（增引用计数，[mod.rs:237-247](asterinas/ostd/src/mm/frame/mod.rs#L237-L247)）
7. 物理连续页合并：clone 每帧并 `ManuallyDrop::new(frame)` 保留引用计数，然后 `unsafe { Segment::from_raw(start..end) }` 构造 `USegment`
8. `BioSegment::new_from_segment(usegment, FromDevice)` 创建零拷贝 bio segment（[bio.rs:467-478](asterinas/kernel/comps/block/src/bio.rs#L467-L478)）
9. 提交 bio，DMA 直接写入用户页；`VmWriter::skip()` 前进 cursor（[io.rs:981](asterinas/ostd/src/mm/io.rs#L981)）

**scatter/gather 支持：** `Bio::new()` 原生支持 `Vec<BioSegment>` 多段描述。virtio-blk QUEUE_SIZE=64，最多 62 segments/bio（[device.rs:213, 99-100](asterinas/kernel/comps/virtio/src/device/block/device.rs#L99-L100)）。用户页若不连续，按物理连续 run 拆分成多个 BioSegment 即可。

**DMA 同步：** virtio-blk 读完成后自动对每个 segment 调用 `sync_from_device()`（[device.rs:308-318](asterinas/kernel/comps/virtio/src/device/block/device.rs#L308-L318)），用户页同样适用。

**安全性：** Asterinas 无 swap，`BioSegment::new_from_segment` 持有 USegment 所有权（即物理页引用计数），DMA 期间页不会被释放。

**收益：** 消除 ~62μs/次 (memcpy + alloc)，理论提升到 ~80%+
**风险：** 中 — 不需要新增 ostd API，所有基础设施已存在，只需要在 ext4 层组合使用
**参考：** `BioSegment::new_from_segment()` 已有先例（ext2 的 `read_block_async` 用 CachePage 做零拷贝）

### 方向三：预分配可复用 DMA 缓冲区（消除 B2，降低 B1）

在 `Ext4Fs` 或 `DirectReadCache` 中维护一个持久的 1MB DMA buffer，每次读复用它。

**收益：** 消除 ~12μs/次 (alloc)，B1 的 memcpy 仍在
**风险：** 低 — 不涉及 ostd 层改动

### 方向四：消除 cache miss 时的 inode 读盘（消除 B3）

`plan_direct_read_cached` 在 cache miss 时先从 `inode_meta_cache` 获取 `file_size`，传给 ext4_rs 的 `plan_direct_read_with_size()`，避免 `get_inode_ref()` 纯粹为拿 size 读盘。

**收益：** ~13 × 150μs = ~2ms/s，约 0.2%
**风险：** 低

### 方向五：消除 per-read 的冗余 Mutex 和 Vec 分配（降低 B5 + B6）

- 合并 `inode_direct_read_cache` 和 `inode_atime_cache` 到同一结构，一次锁拿两个字段
- `slice_mappings_for_range` 不创建新 Vec，而是返回 (offset, len) 到已缓存 mappings 的引用

**收益：** ~7μs/次 → ~22ms/s
**风险：** 低

### 方向六：bypass `run_ext4` for read-only operations（降低 B4）

对于 `plan_direct_read`、`stat` 等只读操作，不需要 `EXT4_RS_RUNTIME_LOCK`，也不需要独占 `Mutex<Ext4>`。新增 `run_ext4_readonly` 走 `RwLock::read`。

**收益：** 在 cache miss 路径上减少锁开销
**风险：** 中 — 需要确认 ext4_rs 的只读接口确实不修改共享状态

---

## 4.1 新增结论：completion wait 基本就是 IRQ delivery

继续把 block completion 的时间拆细后，日志 `asterinas/.tmp_ext4_seq_read_bw_irq_profile.log` 给出了更明确的结果：
- `large_avg_device_wait_us ≈ 243-245`
- `large_avg_irq_delivery_us ≈ 243-244`
- `large_avg_irq_reap_us = 0`
- `large_avg_resp_sync_us = 0`
- ext4 同轮 `avg_wait_us ≈ 240-242`
- ext4 同轮 `avg_copy_us ≈ 65-66`

这说明：

1. `device_wait` 的主体不是 guest 回调里的 bookkeeping  
从进入 `handle_irq()` 开始，到 `pop_used()/remove()` 完成，再到响应头同步结束，这几段累计都压到了 `0μs` 量级。guest 侧 block/virtio 完成路径的“后半段”已经轻到几乎不可见。

2. 当前最重的是 completion IRQ 真正送达前的等待  
`large_avg_irq_delivery_us` 几乎和 `large_avg_device_wait_us` 重合，说明当前 1MB 主流 read bio 的大头已经变成了“virtqueue notify 之后，到 completion IRQ 被 guest 观察到之前”的等待。

3. ext4 的 `bio_waiter.wait()` 已经非常接近这条链路的真实成本  
因为 ext4 同轮 `avg_wait_us` 与 block 侧 `large_avg_device_wait_us` 基本重合，所以继续在 ext4 层做 wait 周围的逻辑优化，理论收益已经很有限。

## 4.2 新增结论：copy 路径不是明显的低级实现

当前 copy 路径虽然仍然占 `~65-66μs`，但实现本身并不显得低效：
- `VmReader::read_fallible()` 最终走 `memcpy_fallible()`，见 `ostd/src/mm/io.rs:396`
- x86 `__memcpy_fallible` 使用 `rep movsb`，见 `ostd/src/arch/x86/mm/memcpy_fallible.S:15-19`
- OSDK 在 `x86_64` 构建时默认打开 `-C target-feature=+ermsb`，见 `osdk/src/commands/build/mod.rs:224-228`

按当前 ext4 主流样本估算：
- `avg_mapped_bytes ≈ 1015808`
- `avg_copy_us ≈ 65-66`
- 对应 copy 吞吐约 `14.3-14.6 GiB/s`

因此，当前“继续微调 memcpy 实现”并不是最可信的主收益点。除非后续能找到更结构性的 copy 避免方案，否则更值得优先投入的方向仍然是：
- `virtio completion / interrupt delivery`
- 或更靠近 host 侧的 I/O 完成等待链路

## 4.3 新增结论：submit-before-copy 已生效，但暴露了新的 queue wait

在当前 `submit-before-copy` 原型与 `size=1G bs=1M` 单边验证下，日志 `asterinas/.tmp_ext4_seq_read_bw_1g_verify.log` 已显示：
- benchmark：`READ: bw=3479MiB/s (3648MB/s)`
- ext4：`avg_wait_us ≈ 73`、`avg_copy_us ≈ 55`
- large read bio：`large_avg_queue_wait_us ≈ 70`
- large read bio：`large_avg_device_wait_us ≈ 169`

这组数据的重要性在于：

1. `submit-before-copy` 的方向没有错  
   ext4 侧同步等待窗口已经明显下降，说明“先提交下一次 speculative bio，再做当前次 copy”的基本时序确实在起作用。

2. 但 ext4 层减少的等待，没有等比例转成吞吐  
   新暴露出来的 `large_avg_queue_wait_us ≈ 70` 已经足够说明：speculative bio 目前更像是“更早 enqueue 到 software queue”，而不一定是“更早进入 virtqueue / device in-flight”。

3. 当前新主矛盾已经从 `wait/copy` 部分转移到 `queue_wait`  
   一旦 `avg_wait_us` 已被压低到 `~73μs`、`avg_copy_us` 也只剩 `~55μs`，那再继续只围绕 ext4 自身时延打转，边际收益就会明显下降。此时更应该投入的是 speculative request 的更早 device submit。

因此，Phase 1 当前最值得新增的一步不是再做一个“更会藏 copy 的 ext4 状态机”，而是补上一条更偏吞吐导向的路径：
- 减少 `submit -> request_queue.dequeue` 这段 handoff
- 让 speculative request 更快走到 `add_dma_buf()/notify()`
- 先压低 `large_avg_queue_wait_us`，再决定是否扩到更高的 outstanding depth

## 4.4 新增结论：Step 0.7 已验证，关键在于“只加速 speculative request”

`Step 0.7` 完成后，结论已经进一步收敛：

1. 过宽的 fast-submit 会伤到吞吐  
   第一版实验把所有大 read 都尝试直接送进 virtqueue。结果虽然把 `large_avg_queue_wait_us` 几乎打到 `0`，但单边 benchmark 反而掉到 `2499MiB/s (2620MB/s)`。这说明 foreground read 原本的稳定提交路径不应该被一起扰动。

2. 真正有效的是“只服务 speculative prefetch”  
   当 fast-submit 只对 ext4 speculative direct-read 生效时，单边 benchmark 回到 `3518MiB/s (3689MB/s)`，高于 `0.7` 前的 `3479MiB/s (3648MB/s)` 基线，同时：
   - ext4：`avg_wait_us ≈ 64`、`avg_copy_us ≈ 64`
   - large read bio：`large_avg_queue_wait_us = 0`
   - large read bio：`large_avg_device_wait_us ≈ 229`

3. 因果关系已经比较清晰  
   `Step 0.7` 的核心收益不是继续降低 ext4 自身开销，而是把 speculative request 从“更早 enqueue”真正推进到“更早 device-submit”。但这个收益只能建立在严格边界之上，也就是：
   - 只针对 speculative request
   - 只针对大块 direct read
   - 任一条件不满足立即 fallback

因此，接下来的 Phase 1 主线已经可以更新为：
- `Step 0.7` 作为固定前置能力保留
- 后续继续回到 `Step 1`，围绕 speculative pipeline 本身做更强的吞吐优化
- 不再考虑那种“把所有大 read 一起 fast-submit”的宽策略

## 4.5 新增结论：Step 1 的第一轮正收益来自“prepare-before-wait”，不是扩 depth

`Step 1` 开始推进后，最新一轮单边 `size=1G bs=1M` 验证又把方向收窄了一步：

1. single-slot pipeline 继续有效  
   在保留 `Step 0.7` scoped fast-submit 的前提下，把 `plan_next` 从 `wait` 之后前移到 `wait` 之前，最终得到：
   - benchmark：`3533MiB/s (3705MB/s)`
   - ext4：`avg_plan_us ≈ 45`、`avg_wait_us ≈ 59`、`avg_copy_us ≈ 71`
   - large read bio：`large_avg_queue_wait_us = 0`
   - large read bio：`large_avg_device_wait_us ≈ 239-241`

   这说明 Step 1 的收益不只来自 “submit-before-copy”，也来自把 `plan_next` 本身藏进当前 I/O 的 inflight 时间。

2. 盲目扩到 two-slot 会把时间重新堆进 device wait  
   我们也做了一轮 `pending slot = 2` 的实验。结果显示：
   - `large_avg_queue_wait_us` 仍然接近 `0`
   - 但 `large_avg_device_wait_us` 会升到 `~370us`
   - 吞吐没有继续上升，反而不值得保留

3. Step 1 的当前主线已经收敛  
   现阶段最值得继续做的不是“更多 outstanding”，而是：
   - 继续打磨 single-slot steady-state
   - 尽可能把 `plan_next`、submit handoff 和 foreground wait 藏进已有窗口
   - 只有在单槽版本明显平台化后，才重新评估 `pending slot = 2`

4. `submit-before-wait` 也已经被验证不是当前最优  
   我们又做了一轮 single-slot 的“先 submit speculative，再等待当前 bio”实验。结果虽然把 ext4 `avg_wait_us` 继续压到了 `~36-37us`，但单边 benchmark 反而掉到 `3293MiB/s (3452MB/s)`，同时：
   - `large_avg_queue_wait_us` 仍然接近 `0`
   - `large_avg_device_wait_us` 抬到 `~259us`

   这说明在当前单核、单 job 环境下，更早把 speculative 请求送进设备，并不会自动换来更高吞吐；它更像是在把一部分 foreground wait 转移成更早的 device-side 竞争。

5. 当前最有效的 steady-state 优化，是减少昂贵的 plan miss  
   在保留 single-slot `prepare-before-wait + submit-after-wait` 的前提下，把 direct-read planning window 从 `64/256MiB` 放大到 `128/512MiB` 后，单边 benchmark 进一步来到 `4076MiB/s (4274MB/s)`。同期 profile 显示：
   - ext4：`avg_plan_us ≈ 39`、`avg_wait_us ≈ 50`、`avg_copy_us ≈ 57`
   - large read bio：`large_avg_queue_wait_us = 0`
   - large read bio：`large_avg_device_wait_us ≈ 196-197`
   - cache：`cache_miss` 从 `3072` 降到 `2048`

   这说明当前 Step 1 的上限并不只由设备 admission 决定。即使不再改变 submit 时序，只要能继续减少少量但高成本的 re-plan miss，吞吐仍然可以明显上涨。

---

## 5. Phase 1 最终验收结论（2026-04-17）

### 5.1 最终结果

| 测试项 | 基线 | 最终 Asterinas | 最终 Linux | 最终比值 | 目标 |
|--------|------|----------------|------------|---------|------|
| ext4_seq_read_bw | 65.89% | 4870 MB/s | 5084 MB/s | **95.79%** | >= 80% ✅ |
| ext4_seq_write_bw | 114.57% | 2651 MB/s | 2930 MB/s | **90.48%** | 不回归 ✅ |

功能回归（phase3_base / phase4_good / phase6_good）：全部 100% PASS ✅

### 5.2 有效的优化路径（按贡献排序）

| 步骤 | 实际收益 | 关键改动 |
|------|----------|----------|
| Step 0.7：scoped fast-submit | queue_wait 从 ~70μs → 0μs | bio fast-submit hint，只对 speculative request 生效 |
| Step 1：speculative readahead + prepare-before-wait | bw 从 3648 → 4274 MB/s | single-slot pipeline + plan 提前 |
| Step 4（部分）：扩大 planning window | cache_miss 从 3072 → 2048 | 128/512MiB 窗口，减少昂贵的 re-plan |

### 5.3 未执行的路径（原因）

| 路径 | 未执行原因 |
|------|-----------|
| Step 1.5：poll-before-sleep | 目标已达成，无需执行 |
| Step 2：消除 inode 读盘 | 收益 ~0.2%，目标已满足后 ROI 不足 |
| Step 3：减少 Mutex + Vec | 收益 ~1-2%，目标已满足后 ROI 不足 |
| Step 4 完整版：1GiB 窗口 | 128/512MiB 已足够，未继续扩大 |
| Research Track Z：零拷贝 DMA | 实验结果回退（812 MiB/s），已回退代码 |

### 5.4 推荐优先级（Phase 1 最终）

| 优先级 | 方向 | 实际结果 |
|--------|------|----------|
| **P0** | speculative readahead + double buffering | ✅ 已完成，是主要贡献 |
| **P1** | speculative fast submit | ✅ 已完成，作为前置条件保留 |
| **P2** | poll-before-sleep | ⏭ 跳过，目标已达成 |
| **P3** | 零拷贝 DMA | ❌ 实验回退，保持待定 |
| **P4/5/6** | 复用 buffer / 消除 inode 读 / 减少 Mutex | ⏭ 未执行，可移交 Phase 2 评估 |

---

## 6. 代码位置速查

| 功能 | 路径 |
|------|------|
| O_DIRECT 读分流 | [inode.rs:148-153](asterinas/kernel/src/fs/ext4/inode.rs#L148-L153) |
| read_direct_at 主体 | [fs.rs:1662-1714](asterinas/kernel/src/fs/ext4/fs.rs#L1662-L1714) |
| plan_direct_read_cached | [fs.rs:802-870](asterinas/kernel/src/fs/ext4/fs.rs#L802-L870) |
| slice_mappings_for_range | [fs.rs:760-800](asterinas/kernel/src/fs/ext4/fs.rs#L760-L800) |
| run_ext4 (双锁) | [fs.rs:944-957](asterinas/kernel/src/fs/ext4/fs.rs#L944-L957) |
| EXT4_RS_RUNTIME_LOCK | [fs.rs:60](asterinas/kernel/src/fs/ext4/fs.rs#L60) |
| get_inode_ref (读盘) | [ext4_rs inode.rs:143-154](asterinas/kernel/libs/ext4_rs/src/ext4_impls/inode.rs#L143-L154) |
| plan_direct_read (ext4_rs) | [file.rs:449-476](asterinas/kernel/libs/ext4_rs/src/ext4_impls/file.rs#L449-L476) |
| collect_block_ranges | [file.rs:81-145](asterinas/kernel/libs/ext4_rs/src/ext4_impls/file.rs#L81-L145) |
| BioSegment::alloc | [bio.rs:421-465](asterinas/kernel/comps/block/src/bio.rs#L421-L465) |
| BioSegment::new_from_segment | [bio.rs:467-478](asterinas/kernel/comps/block/src/bio.rs#L467-L478) |
| BioSegmentPool (4096 blocks) | [bio.rs:557-697](asterinas/kernel/comps/block/src/bio.rs#L557-L697) |
| DmaStream::map (USegment→DMA) | [dma_stream.rs:158](asterinas/ostd/src/mm/dma/dma_stream.rs#L158) |
| touch_atime_after_direct_read | [fs.rs:923-942](asterinas/kernel/src/fs/ext4/fs.rs#L923-L942) |
| ext2 read_direct_at (参考) | [ext2/inode.rs:1006-1027](asterinas/kernel/src/fs/ext2/inode.rs#L1006-L1027) |
| ext2 read_blocks (参考) | [ext2/inode.rs:1866-1883](asterinas/kernel/src/fs/ext2/inode.rs#L1866-L1883) |
