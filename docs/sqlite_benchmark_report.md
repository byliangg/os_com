# SQLite 真实应用 benchmark 报告（ext4 on Asterinas vs Linux）

## 1. 测试目的

赛题明确要求"使用 SQLite 或其他应用测试文件系统在真实工作环境下的性能表现"。fio/lmbench 是合成负载，SQLite 是**真实应用**：整个数据库是文件系统上的一个文件，运行时产生 buffered I/O + 频繁 fsync（事务提交）+ 随机小块读写 + truncate/rename 的混合负载——这是 fio 测不到的真实画像。

## 2. 测试环境与口径

| 项 | 内容 |
|----|------|
| 测试日期 | 2026-06-05 ~ 06 |
| 代码 | main / Phase 5 读优化 + 并发套件收口后 |
| 负载 | `sqlite-speedtest1 --size 1000 /ext4/test.db`（SQLite 3.48.0 官方性能程序）|
| 文件系统 | ext4（`/dev/vda`，`mkfs.ext4 -b 4096`），Asterinas 与 Linux 同口径 |
| VM | 8G RAM、SMP=1、KVM、virtio-blk、drop_caches 公平基线 |
| Asterinas 配置 | 两种：`page_cache=1`（PageCache buffered 路径）与 `page_cache=0`（旧 Vec buffered 路径）|
| 入口 | `test/initramfs/src/benchmark/sqlite/run_sqlite_summary.sh` |
| 日志 | `benchmark/logs/sqlite_20260605_232934/`（size 1000）、`sqlite_linux_20260605_235927.log`（Linux 基线）|

> 注：`speedtest1 --size` 不缩放插入行数（100 与 1000 均为固定 50 万行），所以无法靠减小 size 规避崩溃。

## 3. 关键结论（先说）

1. **读（SELECT）有竞争力**：1.0–4.1× 慢，无索引 SELECT 基本与 Linux **持平（1.0×）**，有索引 SELECT ~1.8×。Phase 5 读优化在真实 buffered 读上同样生效。
2. **写（INSERT/CREATE INDEX）灾难性**：**28–244× 慢**。buffered write + 每事务 fsync + 元数据更新路径是真实瓶颈。
3. **标准负载直接把我们 ext4 跑崩**——两种配置两种崩法：
   - `page_cache=0`：**分配器越界 panic**（`ext4.rs:32`，block group index 越界）
   - `page_cache=1`：持续插入 ~600s 后**内核堆耗尽 OOM**（无界内存增长）
4. Asterinas **无法跑完** speedtest1（Linux 全程 TOTAL 仅 51.96s）；仅前 11 项就累计 ~405s 且随后崩溃。

## 4. 逐 sub-test 对比（page_cache=1，完成的 11 项，秒，越低越好）

| # | 操作 | Asterinas (s) | Linux (s) | 慢 N× | 类型 |
|---|------|---:|---:|---:|------|
| 100 | 500000 INSERT 无索引 | 7.935 | 0.284 | **27.9×** | 写 |
| 110 | 500000 有序 INSERT w/PK | 33.166 | 0.411 | **80.7×** | 写 |
| 120 | 500000 无序 INSERT w/PK | 223.786 | 0.918 | **243.8×** | 写 |
| 130 | 25 SELECT BETWEEN 无索引 | 1.452 | 0.358 | 4.1× | 读 |
| 140 | 10 SELECT LIKE 无索引 | 0.695 | 0.683 | **1.0×** | 读 |
| 142 | 10 SELECT ORDER BY 无索引 | 1.166 | 1.146 | **1.0×** | 读 |
| 145 | 10 SELECT ORDER BY+LIMIT | 0.534 | 0.509 | **1.0×** | 读 |
| 150 | CREATE INDEX ×5 | 126.531 | 0.999 | **126.7×** | 写(元数据) |
| 160 | 100000 SELECT BETWEEN 有索引 | 3.140 | 1.777 | 1.8× | 读 |
| 161 | 100000 SELECT BETWEEN PK | 3.096 | 1.762 | 1.8× | 读 |
| 170 | 100000 SELECT text 有索引 | 3.146 | 1.776 | 1.8× | 读 |
| 180 | 500000 INSERT w/3 索引 | **崩溃** | 2.124 | — | 写 |
| …190–520 | DELETE/VACUUM/UPDATE/JOIN/REPLACE 等 ~18 项 | **未执行（已崩）** | 各 0.02–6.3 | — | 混合 |
| **TOTAL** | 全部 | **N/A（崩溃）** | **51.963** | — | — |

读/写分野一目了然：**读 1–4×、写 28–244×**。

## 5. 两个崩溃 bug 详解

### Bug A — 分配器 block group 越界 panic（page_cache=0）

```
ERROR: Uncaught panic: index out of bounds: the len is 16 but the index is 16
at kernel/libs/ext4_rs/src/ext4_defs/ext4.rs:32
```

触发点：test 120（50 万无序插入）~29s。根因在 `AllocatorBlockGroupLocks::lock_block_group`：

```rust
pub fn lock_block_group(&self, bgid: u32) -> MutexGuard<'_, ()> {
    self.block_groups[bgid as usize].lock()   // bgid==16，但锁数组只有 0..15
}
```

`bgid == block_group_count`（16==16）。SQLite 的 ~2GB DB 文件涨到最后一个 block group 边界时，allocator 算出的 bgid 等于组数（越界 1）。**xfstests 没测出来**——它的测试文件不够大，跑不到第 16 个 block group；SQLite 的大 DB 触发了这个边界。这是一个真实正确性 bug。

### Bug B — 内核堆耗尽 OOM（page_cache=1）

```
ERROR: Failed to allocate a large slot
ERROR: Heap allocation error, layout = Layout { size: 0x1200 (4608B), align: 1 }
```

触发点：test 180（50 万插入 w/3 索引）~601s。注意申请的只有 **4608 字节**——不是大块分配，而是**内核堆在持续插入 ~600s 后被耗尽**，连 4.6KB 都分不出。指向 buffered I/O 路径**无界内存增长**（写回/回收缺失或泄漏），与下方"写慢"同源。

## 6. 分析

### 6.1 为什么写慢 28–244×、读只慢 1–4×

- **读**：SQLite SELECT = buffered 随机读，命中页缓存后主要是内存操作。Phase 5 的 inode/extent 元数据缓存让我们的读路径 per-op 开销很低，所以无索引扫描基本追平 Linux，有索引点查 ~1.8×。**读路径已不是瓶颈。**
- **写**：SQLite 每个事务 COMMIT 触发 fsync，每次 INSERT 涉及 buffered 写 + 元数据更新 + JBD2 日志。我们的 buffered write 路径本就只有 Linux 的 1–6%（fio D2/D3 已暴露），叠加每事务 fsync 的同步持久化往返，于是放大成 28–244×。`120 无序插入 243×` 最惨——无序插入打散到多个 block group，元数据 I/O 最碎。
- **CREATE INDEX 126×**：建索引 = 大量元数据写 + 排序回写，同属写/元数据路径。

### 6.2 page_cache=0 vs =1

| 配置 | 崩溃点 | 性质 |
|------|------|------|
| page_cache=0（旧 Vec 路径）| test 120 ~29s | 分配器越界 panic（更早、更脆）|
| page_cache=1（PageCache 路径）| test 180 ~601s | 堆 OOM（更稳但仍崩）|

PageCache 路径更健壮（多撑了 ~570s、多跑了 60 个测试项），但仍因无界内存增长 OOM。**两种 buffered 路径都不能跑完真实 SQLite 负载。**

### 6.3 与 fio 合成负载的对照

| 维度 | fio O_DIRECT 守底 | SQLite 真实应用 |
|------|------|------|
| 读 | 小块 81–91%、1M 127% | SELECT 1–4× 慢（25–100%）|
| 写 | O_DIRECT 75–92% | buffered INSERT 28–244× 慢 |
| 稳定性 | 全程稳定 | **崩溃（OOM/panic）** |

fio 用 O_DIRECT 绕过了 buffered 写路径，所以守底数据漂亮；SQLite 走 buffered + fsync，**暴露了 O_DIRECT 口径掩盖的真实短板**。这正是赛题要"真实应用测试"的意义。

## 7. 下一步优化方向（按优先级）

1. **修分配器越界 bug A**（`ext4.rs:32`）：定位 `bgid == block_group_count` 的来源——是 `block_group_count` 少算 1，还是 allocator 在最后一个组边界算错 bgid。这是正确性 bug，优先级最高（崩溃 + 潜在数据损坏）。中等难度，需读 allocator 的 bgid 计算。
2. **修内存泄漏/无界增长 bug B**（page_cache=1 路径）：buffered write 持续运行堆耗尽，需查 PageCache writeback / dirty page 回收 / ext4 buffer 释放。较难，是 Phase 4 PageCache hardening 的延伸。
3. **buffered write 吞吐优化**：写慢 28–244× 的根本是 buffered write + 每事务 fsync 路径。修完崩溃后，这是把 SQLite 真实性能拉起来的主线（对应此前 D2/D3 = 1–6% 的遗留项）。
4. 修完上述后重测，争取 speedtest1 **完整跑完**并给出 TOTAL ratio——这才是可答辩的"真实应用性能"数据。

## 8. 结论

SQLite 真实应用测试**成功落地并产出强信号**：读路径已追平/接近 Linux（Phase 5 成果延续到真实负载），但 **buffered write + fsync 路径是真实瓶颈（慢 28–244×）且在标准负载下崩溃（2 个真实 bug）**。这既补齐了赛题"真实应用测试"维度，也明确了下一阶段的优化主线：**先修两个崩溃 bug，再攻 buffered write 吞吐**。
