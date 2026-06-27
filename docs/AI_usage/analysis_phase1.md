# Asterinas ext4 fio_read 性能瓶颈分析 Phase 1

## 进度基准

| 指标 | Asterinas | Linux | 比值 |
|------|-----------|-------|------|
| ext4_seq_read_bw | 3180 MB/s | 4826 MB/s | 65.89% |

fio 参数：`size=1G bs=1M ioengine=sync direct=1 numjobs=1 fsync_on_close=1 time_based=1 ramp_time=60 runtime=100`

---

## 1. 分析

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







