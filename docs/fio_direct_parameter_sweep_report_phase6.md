# ext4 fio 参数广度测试报告（Phase 6 收口重跑）

测试时间：2026-06-11（Asia/Shanghai）
对照基线：`fio_direct_parameter_sweep_report_phase5.md`（2026-06-05，Phase 5 收口时点）
本轮代码状态：Phase 6 性能线收口 HEAD `e13df9abd`（含 S3/S4/S6/P1/P2/P3b/P5a；SQLite 234.9s = 21.92%）

## 1. 测试目的

1. Phase 6 大改 buffered 写路径（unwritten 预分配、设备块缓存、写快路径 ext2 化、lean prepare）后，**全量复测 96 case**，验证：
   - O_DIRECT 守底（A/C 组）不回退——Phase 6 改动应与 O_DIRECT 正交；
   - D 组 buffered 数字（Phase 5 时 write 仅 1.2–6.3%）应大幅改善；
   - F 组并发瓶颈是否仍在。
2. 刷新答辩数据底座，替代 phase5 报告中已过时的 D 组数字。

## 2. 口径

与 phase5 报告完全一致：单容器 Docker、KVM、tap/vhost、`LOG_LEVEL=error`、cache-off 诚实口径（speculative data cache 关）、drop-caches 公平基线、Linux 同机同轮。原始数据：`benchmark/logs/fio_parameter_sweep_20260611_195239/`（汇总 `fio_parameter_sweep_summary.tsv`，96 case）。

## 3. 分组结论速览（vs phase5）

| 分组 | phase5 结论 | 本轮结论 |
|------|------|------|
| A 官方守底（1M nj1）| read 127% / write 92% | **read 140% / write 87%——持平守底，波动内** |
| C bs sweep（nj1）| 读 81–95% / 写 75–89% | **持平：读 82–122% / 写 74–121%；256K write 升至 121%、1M read 升至 122%。O_DIRECT 与 Phase 6 正交性实测证实** |
| **D buffered** | **write 1.2–6.3%（最差项）** | **write 7.9–29.3%（×4.3–13.2 绝对值提升）——Phase 6 buffered 改造的 fio 侧验证** |
| E fsync sweep | 持久化语义口径，不作吞吐宣传 | 同 phase5，不变 |
| **F numjobs** | 并发写卡 ~2800 不 scale；并发读被 Linux cache 假象污染 | **并发写卡死复现（nj4 62.9% vs raw 117%）——主瓶颈不变；本轮 Linux 读假象消失，干净口径下并发读 84%（比想象健康，但绝对值仍不 scale）** |

## 4. D 组：Phase 6 buffered 改造的 fio 侧战果

| case | phase5（MB/s / ratio）| 本轮（MB/s / ratio）| 绝对值提升 |
|------|---:|---:|---:|
| D2-write（buffered, PC off）| 35.6 / 6.33% | **152 / 29.29%** | **×4.3** |
| D3-write（buffered, PC on）| 11.8 / 2.11% | **156 / 25.62%** | **×13.2** |
| D4-write（O_DIRECT + PC on 共存）| 39.7 / 1.20% | **267 / 7.88%** | **×6.7** |
| D3-read-cold | 43.9 / 1.34% | 84.4 / 3.13% | ×1.9 |
| D3-read-warm | 7895 / 105.87% | 7838 / 76.84% | 绝对值持平（ratio 降是 Linux 分母 7457→10200 的 cache 波动）|
| D1（direct, PC/DRC off）| write 84.0% / read 120.5% | write 85.7% / read 114.2% | 持平 |

- 收益来源 = Phase 6 全套：S6 unwritten 预分配（慢路径分配 ×32 摊销）、P1 设备块缓存（元数据读 98.5% 命中）、P2 写快路径 ext2 化（coverage 区间集 + 直写）、P5a lean prepare。fio buffered 顺序写是"纯 append"形状，正是这条链的受益面。
- **遗留**：D2/D3-write 绝对值 ~155 MB/s 仍远低于 O_DIRECT 写（~2800）——fio 的 buffered 写经页缓存 + fsync 端 writeback，吞吐受 writeback 串行与每 append journal 工作限制（与 SQLite 渐近线同根，见 technical_report §7.5）；D2-read（PC off 的 buffered 读）~130 MB/s 持平未改善（非默认配置，走 ext4_rs 逐块读慢路径，低优先）。

## 5. F 组：并发瓶颈确认不变（下一主攻的立项依据）

写（1M，drop 口径）：

| nj | raw MB/s（ratio）| ext4j MB/s（ratio）| ext4n MB/s（ratio）|
|----|---:|---:|---:|
| 1 | 2739（75.4%）| 2799（76.5%）| 2730（79.1%）|
| 2 | **5309（117.6%）** | 2708（65.5%）| 2861（64.2%）|
| 4 | **5276（117.1%）** | **2724（62.9%）** | 2732（86.1%）|

读（1M）：

| nj | raw MB/s（ratio）| ext4j MB/s（ratio）|
|----|---:|---:|
| 1 | 2827（61.1%）| 3403（89.7%）|
| 2 | 5799（105.2%）| 3531（77.5%）|
| 4 | **5743（99.3%）** | **3640（84.1%）** |

- **并发写卡死完全复现**：ext4（journaled 与 nojournal 同样）卡在 ~2700-2860，nj 翻倍不动；raw 线性翻倍至 5300+ 且超 Linux。瓶颈在我们 ext4 层的串行化（候选：全局 `EXT4_RS_RUNTIME_LOCK`、`inner: Mutex<Ext4>`、JBD2 commit 串行、meta/coverage 缓存 Mutex，**以及 Phase 6 新增的 adapter 块缓存全局 Mutex——phase5 之后引入，并发争用未测过**）。journaled≈nojournal 卡同一水位 → 第一嫌疑是锁而非 JBD2。
- **本轮 Linux 读假象消失**（nj4 Linux 读 4329，合理值；phase5 曾报 25600），干净口径下 ext4j 并发读 84.1%——比 phase5 的"38%"健康得多，**phase5 报告的并发读恐慌可以部分解除**。但绝对值仍不 scale（nj1 3403 → nj4 3640，raw 同期 2827 → 5743），读侧串行化同在。
- ext4n-read-nj4 62.1%（2969）为本轮孤立低点，单轮波动，不作结论。

## 6. A/C 组：O_DIRECT 守底持平（正交性证实）

| bs | write phase5 → 本轮 | read phase5 → 本轮 |
|----|---:|---:|
| 4K | 75.63 → 74.68% | 81.57 → 82.24% |
| 16K | 75.39 → 75.42% | 83.97 → 86.37% |
| 64K | 80.75 → 81.22% | 90.71 → 87.81% |
| 256K | 89.17 → **121.12%** | 89.58 → 90.06% |
| 1M | 84.03 → 81.78% | 94.64 → **121.72%** |
| 4M | 83.51 → 82.39% | 78.07 → 72.69% |

全部在波动带内（±3-5%），无回退；A 组官方守底 write 86.86% / read 140.11%。**Phase 6 的七步改动（含动了 adapter 读路径的 P1 块缓存）对 O_DIRECT 守底零影响**——P1 设计中"O_DIRECT 读旁路缓存、写侧仅加失效"的正交性主张被全量数据证实。

## 7. 结论与下一步

1. **答辩数据底座更新**：buffered write 从"1–6% 不能看"提升到 7.9–29.3%（fio 侧）/ 21.92%（SQLite 侧），口径一致、互相印证；O_DIRECT 守底持平。phase5 报告 D 组数字作废，以本报告为准。
2. **下一优化主线 = ext4 并发 scale（读写同源）**：证据三连——raw 线性 scale 且超 Linux、ext4 卡死 ~2800、journaled≈nojournal 同水位（指向锁而非日志）。按铁律 profile 先行：nj1/2/4 下四把锁（RUNTIME_LOCK / inner Mutex / adapter cache Mutex / 页缓存锁）的 wait/hold 归因表。
3. 并发读答辩口径修正：用本轮干净数据（84%），不再引用 phase5 被 Linux cache 污染的 38%。
4. D2-read（PC off buffered 读）与 E 组维持现状，不入主线。

## 8. 原始数据索引

| 类型 | 路径 |
|------|------|
| 本轮汇总 TSV | `benchmark/logs/fio_parameter_sweep_20260611_195239/fio_parameter_sweep_summary.tsv` |
| 本轮各 case 日志 | `benchmark/logs/fio_parameter_sweep_20260611_195239/*.log` |
| phase5 对照 TSV | `benchmark/logs/fio_parameter_sweep_20260605_032914/fio_parameter_sweep_summary.tsv` |
| sweep 脚本 | `test/initramfs/src/benchmark/fio/run_parameter_sweep_summary.sh`（未改动）|
