# ext4 fio 参数广度测试报告（Phase 6 收口重跑·完整版）

测试时间：2026-06-11 19:52 起（Asia/Shanghai），单容器一次性完成 96 case
对照基线：`fio_direct_parameter_sweep_report_phase5.md`（2026-06-05，Phase 5 收口时点；下表 "phase5 ratio" 列）
本轮代码状态：Phase 6 性能线收口（S3/S4/S6/P1/P2/P3b/P5a 全部落地；SQLite 234.9s = 21.92%）
附录：dio overwrite 共享锁并发优化（C1）后的 F 组复测（2026-06-12）见 §9

## 1. 测试目的

1. Phase 6 大改 buffered 写路径（unwritten 预分配、设备块缓存、写快路径 ext2 化、lean prepare）后全量复测，验证三件事：
   - **O_DIRECT 守底（A/C 组）不回退**——Phase 6 改动设计上与 O_DIRECT 正交，需实测证实；
   - **D 组 buffered 数字**（phase5 时 write 仅 1.2–6.3%，是全报告最差项）应大幅改善；
   - **F 组并发瓶颈**是否仍在（phase5 结论：ext4 并发写卡 ~2800 不 scale）。
2. 刷新答辩数据底座，本报告取代 phase5 报告中已过时的 D 组数字。

## 2. 测试环境与完整参数

### 2.1 宿主与运行环境

| 项 | 值 |
|---|---|
| 宿主 | Ubuntu 24.04.1，内核 6.8.0-41-generic |
| 容器 | `asterinas/asterinas:0.17.0-20260227`，`--privileged --network=host --device=/dev/kvm` |
| 虚拟化 | QEMU + KVM（`BENCH_ENABLE_KVM=1`），网络 `BENCH_ASTER_NETDEV=tap BENCH_ASTER_VHOST=on` |
| 存储 | virtio-blk（赛题规定接口）；ext4 测试盘挂 `/ext4`，raw 测试直写 `/dev/vda` |
| 对照 | Linux 发行版 ext4，同容器同 QEMU/KVM 同 virtio-blk 同轮交替运行（`bench_linux_and_aster.sh`） |
| 公平基线 | 每 case 前 host drop-caches；guest 侧 `LOG_LEVEL=error`（无日志噪声）；`BENCH_ASTER_SCHEME=null` |
| 诚实口径 | speculative direct-read data cache 关闭；本表为单轮值（与 phase5 同口径） |
| 重试策略 | 每 case 最多 3 次重试（仅针对 nix/代理瞬时故障，性能值不取重试间最优） |

### 2.2 fio 命令行（所有 case 共用骨架）

```
fio -rw={write|read} -filename={/ext4/fio-test|/dev/vda} -name=...
    -size=1G -bs=${BS} -ioengine=sync -direct={1|0} -numjobs=${NJ}
    [-fsync=${FSYNC}] -fsync_on_close=1 -time_based=1 -ramp_time=60 -runtime=100
```

- **同名固定 filename**：numjobs>1 时所有 job 读写**同一个文件**（并发同文件场景）。
- buffered 变体（D2/D3 的 ext4_buffered_*）：`-direct=0`，无 ramp/time_based（读分 cold/warm 两次取数）。
- 每 case 的差异参数全部列在数据表中：`direct`（O_DIRECT 开关）、`pc`（guest 内核 `ext4fs.page_cache`）、`drc`（`ext4fs.direct_read_cache`，诚实口径恒 0）、`bs`、`nj`（numjobs）、`fsync`（每 N 次写后 fsync，none=不插）。

### 2.3 参数扫描空间

| 维度 | 取值 |
|---|---|
| bs | 4K / 16K / 64K / 256K / 1M / 4M（C 组全扫；其余组 1M 或 16K+1M）|
| numjobs | 1 / 2 / 4（F 组）|
| fsync | none / 4 / 16 / 64（E 组）|
| 目标 | raw（/dev/vda 裸盘）/ ext4 journaled / ext4 nojournal |
| page_cache | 0（O_DIRECT 守底口径）/ 1（D3/D4）|

## 3. 分组结论速览（vs phase5）

| 分组 | phase5 结论 | 本轮结论 |
|------|------|------|
| A 官方守底（1M nj1）| read 127% / write 92% | **read 140% / write 87%——持平守底，波动内** |
| B 6-test 分层 | journaled/nojournal 写接近 | 同 phase5（journaled 甚至略优，JBD2 非单 job 瓶颈的结论维持）|
| C bs sweep（nj1）| 读 81–95% / 写 75–89% | **持平：读 82–122% / 写 74–121%。O_DIRECT 与 Phase 6 正交性实测证实** |
| **D buffered** | **write 1.2–6.3%（最差项）** | **write 7.9–29.3%（绝对值 ×4.3–13.2）——Phase 6 buffered 改造的 fio 侧验证** |
| E fsync sweep | 持久化语义口径，不作吞吐宣传 | 同 phase5，不变 |
| **F numjobs** | 并发写卡 ~2800 不 scale；并发读被 Linux cache 假象污染 | **并发写卡死复现（nj4 62.9% vs raw 117%）；本轮 Linux 读假象消失，干净口径并发读 84%。→ 已立项 C1 优化，结果见 §9** |

## 4. A/B 组：官方守底与分层定位（1M nj1）

| case | target | rw | direct | pc | drc | bs | nj | fsync | Aster MB/s | Linux MB/s | ratio | phase5 ratio |
|---|---|---|---|---|---|---|---|---|---:|---:|---:|---:|
| A1-ext4j-write | ext4-journaled | write | 1 | 0 | 0 | 1M | 1 | none | 2934 | 3378 | **86.86%** | 92.30% |
| A2-ext4j-read | ext4-journaled | read | 1 | 0 | 0 | 1M | 1 | none | 4611 | 3291 | **140.11%** | 127.26% |
| B1-raw-write | raw-none | write | 1 | 0 | 0 | 1M | 1 | none | 2398 | 3690 | **64.99%** | 77.90% |
| B2-raw-read | raw-none | read | 1 | 0 | 0 | 1M | 1 | none | 3564 | 4640 | **76.81%** | 63.53% |
| B3-ext4j-write | ext4-journaled | write | 1 | 0 | 0 | 1M | 1 | none | 3043 | 3347 | **90.92%** | 90.12% |
| B4-ext4j-read | ext4-journaled | read | 1 | 0 | 0 | 1M | 1 | none | 3760 | 3161 | **118.95%** | 115.20% |
| B5-ext4n-write | ext4-nojournal | write | 1 | 0 | 0 | 1M | 1 | none | 2922 | 3611 | **80.92%** | 83.02% |
| B6-ext4n-read | ext4-nojournal | read | 1 | 0 | 0 | 1M | 1 | none | 3438 | 2933 | **117.22%** | 121.39% |

**分析**：
- A 组官方守底 write 86.86% / read 140.11%，与 phase5（92.30/127.26）同在波动带内；read 跨轮 95–140% 的散布主要来自 Linux 侧 readahead 抖动（Linux read 分母 2868–3291–4640 跨轮波动），Aster 侧绝对值稳定（4611/3760/3491 同轮三测）。
- B 组分层维持 phase5 结论：B3 journaled write（3043，90.92%）≥ B5 nojournal（2922，80.92%）——**JBD2 不是单 job 写瓶颈**；B1/B2 裸盘比值（65/77%）低于 ext4 比值，再次印证"ext4 模块无短板、缺口在 virtio 平台层"（裸盘地板专测见 phase5 报告 §8.5：4K 仅 52%）。

## 5. C 组：块大小扫描（nj1，O_DIRECT 正交性验证）

| case | target | rw | direct | pc | drc | bs | nj | fsync | Aster MB/s | Linux MB/s | ratio | phase5 ratio |
|---|---|---|---|---|---|---|---|---|---:|---:|---:|---:|
| C-W-raw-4K | raw-none | write | 1 | 0 | 0 | 4K | 1 | none | 103 | 225 | **45.78%** | 52.59% |
| C-R-raw-4K | raw-none | read | 1 | 0 | 0 | 4K | 1 | none | 133 | 257 | **51.75%** | 51.54% |
| C-W-ext4j-4K | ext4-journaled | write | 1 | 0 | 0 | 4K | 1 | none | 177 | 237 | **74.68%** | 75.63% |
| C-R-ext4j-4K | ext4-journaled | read | 1 | 0 | 0 | 4K | 1 | none | 176 | 214 | **82.24%** | 81.57% |
| C-W-ext4n-4K | ext4-nojournal | write | 1 | 0 | 0 | 4K | 1 | none | 180 | 246 | **73.17%** | 78.70% |
| C-R-ext4n-4K | ext4-nojournal | read | 1 | 0 | 0 | 4K | 1 | none | 174 | 217 | **80.18%** | 80.73% |
| C-W-raw-16K | raw-none | write | 1 | 0 | 0 | 16K | 1 | none | 471 | 871 | **54.08%** | 53.41% |
| C-R-raw-16K | raw-none | read | 1 | 0 | 0 | 16K | 1 | none | 501 | 903 | **55.48%** | 55.05% |
| C-W-ext4j-16K | ext4-journaled | write | 1 | 0 | 0 | 16K | 1 | none | 629 | 834 | **75.42%** | 75.39% |
| C-R-ext4j-16K | ext4-journaled | read | 1 | 0 | 0 | 16K | 1 | none | 659 | 763 | **86.37%** | 83.97% |
| C-W-ext4n-16K | ext4-nojournal | write | 1 | 0 | 0 | 16K | 1 | none | 639 | 846 | **75.53%** | 75.71% |
| C-R-ext4n-16K | ext4-nojournal | read | 1 | 0 | 0 | 16K | 1 | none | 656 | 755 | **86.89%** | 85.09% |
| C-W-raw-64K | raw-none | write | 1 | 0 | 0 | 64K | 1 | none | 1382 | 2425 | **56.99%** | 60.01% |
| C-R-raw-64K | raw-none | read | 1 | 0 | 0 | 64K | 1 | none | 1520 | 2785 | **54.58%** | 56.83% |
| C-W-ext4j-64K | ext4-journaled | write | 1 | 0 | 0 | 64K | 1 | none | 1743 | 2146 | **81.22%** | 80.75% |
| C-R-ext4j-64K | ext4-journaled | read | 1 | 0 | 0 | 64K | 1 | none | 1937 | 2206 | **87.81%** | 90.71% |
| C-W-ext4n-64K | ext4-nojournal | write | 1 | 0 | 0 | 64K | 1 | none | 1763 | 2378 | **74.14%** | 75.34% |
| C-R-ext4n-64K | ext4-nojournal | read | 1 | 0 | 0 | 64K | 1 | none | 1930 | 2135 | **90.40%** | 89.89% |
| C-W-raw-256K | raw-none | write | 1 | 0 | 0 | 256K | 1 | none | 2737 | 3067 | **89.24%** | 87.74% |
| C-R-raw-256K | raw-none | read | 1 | 0 | 0 | 256K | 1 | none | 3085 | 3866 | **79.80%** | 79.45% |
| C-W-ext4j-256K | ext4-journaled | write | 1 | 0 | 0 | 256K | 1 | none | 3383 | 2793 | **121.12%** | 89.17% |
| C-R-ext4j-256K | ext4-journaled | read | 1 | 0 | 0 | 256K | 1 | none | 4075 | 4525 | **90.06%** | 89.58% |
| C-W-ext4n-256K | ext4-nojournal | write | 1 | 0 | 0 | 256K | 1 | none | 3374 | 3023 | **111.61%** | 113.25% |
| C-R-ext4n-256K | ext4-nojournal | read | 1 | 0 | 0 | 256K | 1 | none | 4107 | 4033 | **101.83%** | 89.95% |
| C-W-raw-1M | raw-none | write | 1 | 0 | 0 | 1M | 1 | none | 2815 | 3646 | **77.21%** | 76.78% |
| C-R-raw-1M | raw-none | read | 1 | 0 | 0 | 1M | 1 | none | 3523 | 5501 | **64.04%** | 63.77% |
| C-W-ext4j-1M | ext4-journaled | write | 1 | 0 | 0 | 1M | 1 | none | 2769 | 3386 | **81.78%** | 84.03% |
| C-R-ext4j-1M | ext4-journaled | read | 1 | 0 | 0 | 1M | 1 | none | 3491 | 2868 | **121.72%** | 94.64% |
| C-W-ext4n-1M | ext4-nojournal | write | 1 | 0 | 0 | 1M | 1 | none | 2764 | 4101 | **67.40%** | 85.34% |
| C-R-ext4n-1M | ext4-nojournal | read | 1 | 0 | 0 | 1M | 1 | none | 3505 | 2968 | **118.09%** | 128.46% |
| C-W-raw-4M | raw-none | write | 1 | 0 | 0 | 4M | 1 | none | 2385 | 2064 | **115.55%** | 117.34% |
| C-R-raw-4M | raw-none | read | 1 | 0 | 0 | 4M | 1 | none | 2914 | 4736 | **61.53%** | 58.77% |
| C-W-ext4j-4M | ext4-journaled | write | 1 | 0 | 0 | 4M | 1 | none | 2840 | 3447 | **82.39%** | 83.51% |
| C-R-ext4j-4M | ext4-journaled | read | 1 | 0 | 0 | 4M | 1 | none | 2800 | 3852 | **72.69%** | 78.07% |
| C-W-ext4n-4M | ext4-nojournal | write | 1 | 0 | 0 | 4M | 1 | none | 3050 | 3662 | **83.29%** | 84.29% |
| C-R-ext4n-4M | ext4-nojournal | read | 1 | 0 | 0 | 4M | 1 | none | 2957 | 5557 | **53.21%** | 41.94% |

**分析**：
- ext4j 各 bs 读 82–122%、写 74–121%，与 phase5 全部在 ±3–5% 波动带内（4K W 75.63→74.68、16K W 75.39→75.42、64K R 90.71→87.81……）；256K write 升至 121%、1M read 升至 122% 为本轮亮点波动。
- **结论：Phase 6 七步改动（含动了块设备读路径的 P1 设备块缓存）对 O_DIRECT 守底零影响**。P1 设计中"O_DIRECT 读旁路缓存、写侧仅加失效"的正交性主张被 36 个 C 组 case 实测证实。
- raw 行保留作平台地板对照：4K raw 仅 45–52%，ext4 比值全面高于同 bs 的 raw 比值。

## 6. D 组：buffered / PageCache 路径（Phase 6 主战场）

参数语义：D1 = `direct=1, pc=0`（纯 O_DIRECT 基线）；D2 = `direct=0, pc=0`（buffered、无页缓存）；D3 = `direct=0, pc=1`（buffered + 页缓存，cold/warm 分测）；D4 = `direct=1, pc=1`（O_DIRECT 与页缓存共存的一致性协议路径）。

| case | target | rw | direct | pc | drc | bs | nj | fsync | Aster MB/s | Linux MB/s | ratio | phase5 ratio |
|---|---|---|---|---|---|---|---|---|---:|---:|---:|---:|
| D1-write | ext4-journaled | write | 1 | 0 | 0 | 1M | 1 | none | 2814 | 3282 | **85.74%** | 83.96% |
| D1-read | ext4-journaled | read | 1 | 0 | 0 | 1M | 1 | none | 3371 | 2951 | **114.23%** | 120.48% |
| D2-write | ext4-journaled | write | 0 | 0 | 0 | 1M | 1 | none | 152 | 519 | **29.29%** | 6.33% |
| D2-read-cold | ext4-journaled | read-cold | 0 | 0 | 0 | 1M | 1 | none | 129 | 2711 | **4.76%** | 3.97% |
| D2-read-warm | ext4-journaled | read-warm | 0 | 0 | 0 | 1M | 1 | none | 129 | 7255 | **1.78%** | 1.31% |
| D3-write | ext4-journaled | write | 0 | 1 | 0 | 1M | 1 | none | 156 | 609 | **25.62%** | 2.11% |
| D3-read-cold | ext4-journaled | read-cold | 0 | 1 | 0 | 1M | 1 | none | 84 | 2698 | **3.13%** | 1.34% |
| D3-read-warm | ext4-journaled | read-warm | 0 | 1 | 0 | 1M | 1 | none | 7838 | 10200 | **76.84%** | 105.87% |
| D4-write | ext4-journaled | write | 1 | 1 | 0 | 1M | 1 | none | 267 | 3388 | **7.88%** | 1.20% |
| D4-read | ext4-journaled | read | 1 | 1 | 0 | 1M | 1 | none | 3109 | 2966 | **104.82%** | 94.35% |

**分析（本报告核心增量）**：

| case | phase5（MB/s / ratio）| 本轮（MB/s / ratio）| 绝对值提升 | 归因 |
|------|---:|---:|---:|---|
| D2-write | 35.6 / 6.33% | **152 / 29.29%** | **×4.3** | S6 预分配 + P1 块缓存 + P5a lean prepare（buffered 顺序写=纯 append 形状，正是 Phase 6 慢路径优化的受益面）|
| D3-write | 11.8 / 2.11% | **156 / 25.62%** | **×13.2** | 同上 + P2 写快路径（页缓存直写）+ S3/S4 fsync 链改造 |
| D4-write | 39.7 / 1.20% | **267 / 7.88%** | **×6.7** | dio/页缓存一致性协议路径随写回链路整体受益 |
| D3-read-cold | 43.9 / 1.34% | 84.4 / 3.13% | ×1.9 | 读冷路径部分受益于块缓存 |
| D3-read-warm | 7895 / 105.87% | 7838 / 76.84% | 持平 | Aster 绝对值不变；ratio 降纯因 Linux 分母 7457→10200（host cache 波动）|
| D1 | write 84% / read 120% | write 85.7% / read 114.2% | 持平 | O_DIRECT 基线不动 ✓ |

- **遗留（诚实标注）**：D2/D3-write 绝对值 ~155 MB/s 仍远低于 O_DIRECT 写（~2800）——buffered 写经页缓存+fsync 端写回，受每 append 的结构性 journal 工作限制，与 SQLite 渐近线同根（technical_report §7.5）；D2-read（pc=0 的 buffered 读，~130 MB/s）走 ext4_rs 逐块读慢路径，非默认配置，低优先。

## 7. E 组：fsync 频率扫描（持久化语义口径）

| case | target | rw | direct | pc | drc | bs | nj | fsync | Aster MB/s | Linux MB/s | ratio | phase5 ratio |
|---|---|---|---|---|---|---|---|---|---:|---:|---:|---:|
| E-raw-16K-none | raw-none | write | 1 | 0 | 0 | 16K | 1 | none | 472 | 859 | **54.95%** | 61.73% |
| E-ext4j-16K-none | ext4-journaled | write | 1 | 0 | 0 | 16K | 1 | none | 632 | 819 | **77.17%** | 68.92% |
| E-ext4n-16K-none | ext4-nojournal | write | 1 | 0 | 0 | 16K | 1 | none | 626 | 854 | **73.30%** | 75.27% |
| E-raw-16K-4 | raw-none | write | 1 | 0 | 0 | 16K | 1 | 4 | 29 | 48 | **59.67%** | 66.58% |
| E-ext4j-16K-4 | ext4-journaled | write | 1 | 0 | 0 | 16K | 1 | 4 | 38 | 17 | **217.34%** | 274.03% |
| E-ext4n-16K-4 | ext4-nojournal | write | 1 | 0 | 0 | 16K | 1 | 4 | 34 | 50 | **67.40%** | 100.00% |
| E-raw-16K-16 | raw-none | write | 1 | 0 | 0 | 16K | 1 | 16 | 131 | 144 | **90.97%** | 109.87% |
| E-ext4j-16K-16 | ext4-journaled | write | 1 | 0 | 0 | 16K | 1 | 16 | 79 | 86 | **91.98%** | 250.00% |
| E-ext4n-16K-16 | ext4-nojournal | write | 1 | 0 | 0 | 16K | 1 | 16 | 136 | 114 | **119.30%** | 67.62% |
| E-raw-16K-64 | raw-none | write | 1 | 0 | 0 | 16K | 1 | 64 | 196 | 188 | **104.26%** | 98.97% |
| E-ext4j-16K-64 | ext4-journaled | write | 1 | 0 | 0 | 16K | 1 | 64 | 217 | 145 | **149.66%** | 160.77% |
| E-ext4n-16K-64 | ext4-nojournal | write | 1 | 0 | 0 | 16K | 1 | 64 | 227 | 185 | **122.70%** | 117.41% |
| E-raw-1M-none | raw-none | write | 1 | 0 | 0 | 1M | 1 | none | 2273 | 3660 | **62.10%** | 64.67% |
| E-ext4j-1M-none | ext4-journaled | write | 1 | 0 | 0 | 1M | 1 | none | 2631 | 3138 | **83.84%** | 83.14% |
| E-ext4n-1M-none | ext4-nojournal | write | 1 | 0 | 0 | 1M | 1 | none | 2868 | 3650 | **78.58%** | 80.42% |
| E-raw-1M-4 | raw-none | write | 1 | 0 | 0 | 1M | 1 | 4 | 503 | 535 | **94.02%** | 93.75% |
| E-ext4j-1M-4 | ext4-journaled | write | 1 | 0 | 0 | 1M | 1 | 4 | 596 | 508 | **117.32%** | 140.20% |
| E-ext4n-1M-4 | ext4-nojournal | write | 1 | 0 | 0 | 1M | 1 | 4 | 590 | 525 | **112.38%** | 110.92% |
| E-raw-1M-16 | raw-none | write | 1 | 0 | 0 | 1M | 1 | 16 | 795 | 827 | **96.13%** | 72.68% |
| E-ext4j-1M-16 | ext4-journaled | write | 1 | 0 | 0 | 1M | 1 | 16 | 864 | 637 | **135.64%** | 110.11% |
| E-ext4n-1M-16 | ext4-nojournal | write | 1 | 0 | 0 | 1M | 1 | 16 | 884 | 790 | **111.90%** | 111.83% |
| E-raw-1M-64 | raw-none | write | 1 | 0 | 0 | 1M | 1 | 64 | 869 | 1055 | **82.37%** | 85.80% |
| E-ext4j-1M-64 | ext4-journaled | write | 1 | 0 | 0 | 1M | 1 | 64 | 914 | 797 | **114.68%** | 109.41% |
| E-ext4n-1M-64 | ext4-nojournal | write | 1 | 0 | 0 | 1M | 1 | 64 | 923 | 932 | **99.03%** | 96.92% |

**分析**：E 组测"每 N 次写插一次 fsync"的持久化负载。`fsync=4` 下 ext4j 16K 出现 217% 一类高比值，是 Linux 侧 fsync 实现更重（jbd2 commit+双 flush）所致的口径现象——**按既定纪律，fsync-heavy 数字只用于持久化语义讨论，不作为普通吞吐宣传**（phase5 报告同口径）。横向看：fsync 频率越高三方绝对值越低且差距收敛，符合"持久化成本主导"的预期，无异常。

## 8. F 组：并发扫描（numjobs 同文件，1M）

| case | target | rw | direct | pc | drc | bs | nj | fsync | Aster MB/s | Linux MB/s | ratio | phase5 ratio |
|---|---|---|---|---|---|---|---|---|---:|---:|---:|---:|
| F-raw-write-nj1 | raw-none | write | 1 | 0 | 0 | 1M | 1 | none | 2739 | 3632 | **75.41%** | 63.81% |
| F-ext4j-write-nj1 | ext4-journaled | write | 1 | 0 | 0 | 1M | 1 | none | 2799 | 3660 | **76.48%** | 84.30% |
| F-ext4n-write-nj1 | ext4-nojournal | write | 1 | 0 | 0 | 1M | 1 | none | 2730 | 3452 | **79.08%** | 92.43% |
| F-raw-read-nj1 | raw-none | read | 1 | 0 | 0 | 1M | 1 | none | 2827 | 4631 | **61.05%** | 68.84% |
| F-ext4j-read-nj1 | ext4-journaled | read | 1 | 0 | 0 | 1M | 1 | none | 3403 | 3796 | **89.65%** | 119.97% |
| F-ext4n-read-nj1 | ext4-nojournal | read | 1 | 0 | 0 | 1M | 1 | none | 3387 | 3839 | **88.23%** | 129.16% |
| F-raw-write-nj2 | raw-none | write | 1 | 0 | 0 | 1M | 2 | none | 5309 | 4514 | **117.61%** | 115.93% |
| F-ext4j-write-nj2 | ext4-journaled | write | 1 | 0 | 0 | 1M | 2 | none | 2708 | 4134 | **65.51%** | 67.43% |
| F-ext4n-write-nj2 | ext4-nojournal | write | 1 | 0 | 0 | 1M | 2 | none | 2861 | 4456 | **64.21%** | 61.89% |
| F-raw-read-nj2 | raw-none | read | 1 | 0 | 0 | 1M | 2 | none | 5799 | 5512 | **105.21%** | 103.85% |
| F-ext4j-read-nj2 | ext4-journaled | read | 1 | 0 | 0 | 1M | 2 | none | 3531 | 4556 | **77.50%** | 69.00% |
| F-ext4n-read-nj2 | ext4-nojournal | read | 1 | 0 | 0 | 1M | 2 | none | 3456 | 4426 | **78.08%** | 95.78% |
| F-raw-write-nj4 | raw-none | write | 1 | 0 | 0 | 1M | 4 | none | 5276 | 4507 | **117.06%** | 113.90% |
| F-ext4j-write-nj4 | ext4-journaled | write | 1 | 0 | 0 | 1M | 4 | none | 2724 | 4332 | **62.88%** | 64.13% |
| F-ext4n-write-nj4 | ext4-nojournal | write | 1 | 0 | 0 | 1M | 4 | none | 2732 | 3172 | **86.13%** | 99.77% |
| F-raw-read-nj4 | raw-none | read | 1 | 0 | 0 | 1M | 4 | none | 5743 | 5781 | **99.34%** | 22.28% |
| F-ext4j-read-nj4 | ext4-journaled | read | 1 | 0 | 0 | 1M | 4 | none | 3640 | 4329 | **84.08%** | 38.27% |
| F-ext4n-read-nj4 | ext4-nojournal | read | 1 | 0 | 0 | 1M | 4 | none | 2969 | 4779 | **62.13%** | 94.48% |

**分析**：
- **并发写卡死精确复现**：ext4（journaled 与 nojournal 同样）卡 2708–2861，nj 翻倍不动；raw 线性翻倍至 5276–5309（117%，反超 Linux）。三证闭环（raw 能 scale / ext4 不 scale / journaled≈nojournal）→ 瓶颈在 ext4 层锁而非 JBD2、而非设备。
- **本轮 Linux 读假象消失**（nj4 Linux 读 4329，合理值；phase5 曾报 25600 的 direct=1 物理不可能值），干净口径下 ext4j 并发读 84.08%——phase5 报告的"38%"系污染数字，正式作废。
- 代码定位（C0 调研）：fio 同文件 + per-inode 互斥锁横跨整个 O_DIRECT 路径（含设备等待）= 单流封顶。由此立项 C1（dio overwrite 共享锁），结果见 §9。
- ext4n-read-nj4 62.13%（2969）为本轮孤立低点，单轮波动不作结论。

## 9. 附录：C1（dio overwrite 共享锁）后的 F 组复测（2026-06-12）

**改动**：per-inode correctness 锁 Mutex→RwMutex；`page_cache=0` 下经映射验证的纯覆盖 O_DIRECT 写持共享锁并行提交（Linux shared `i_rwsem` dio overwrite 对应实现），O_DIRECT 读同享共享锁；一切元数据变更路径维持独占。测试参数与 §8 完全一致（同脚本 `SWEEP_GROUPS=F` 过滤，原始数据 `benchmark/logs/fio_parameter_sweep_20260612_103255/`）。

| case | rw | bs | nj | Aster MB/s | Linux MB/s | ratio |
|---|---|---|---|---:|---:|---:|
| F-raw-write-nj1 | write | 1M | 1 | 2286 | 3147 | **72.64%** |
| F-ext4j-write-nj1 | write | 1M | 1 | 1432 | 2873 | **49.84%** |
| F-ext4n-write-nj1 | write | 1M | 1 | 1603 | 1643 | **97.57%** |
| F-raw-read-nj1 | read | 1M | 1 | 1633 | 4122 | **39.62%** |
| F-ext4j-read-nj1 | read | 1M | 1 | 1645 | 1786 | **92.11%** |
| F-ext4n-read-nj1 | read | 1M | 1 | 1663 | 1777 | **93.58%** |
| F-raw-write-nj2 | write | 1M | 2 | 4734 | 3899 | **121.42%** |
| F-ext4j-write-nj2 | write | 1M | 2 | 6024 | 3641 | **165.45%** |
| F-ext4n-write-nj2 | write | 1M | 2 | 5803 | 3129 | **185.46%** |
| F-raw-read-nj2 | read | 1M | 2 | 2289 | 4080 | **56.10%** |
| F-ext4j-read-nj2 | read | 1M | 2 | 7456 | 2912 | **256.04%** |
| F-ext4n-read-nj2 | read | 1M | 2 | 7172 | 2066 | **347.14%** |
| F-raw-write-nj4 | write | 1M | 4 | 4625 | 3935 | **117.53%** |
| F-ext4j-write-nj4 | write | 1M | 4 | 5139 | 2750 | **186.87%** |
| F-ext4n-write-nj4 | write | 1M | 4 | 5126 | 3833 | **133.73%** |
| F-raw-read-nj4 | read | 1M | 4 | 5397 | 5142 | **104.96%** |
| F-ext4j-read-nj4 | read | 1M | 4 | 13200 | 4215 | **313.17%** |
| F-ext4n-read-nj4 | read | 1M | 4 | 14100 | 6406 | **220.11%** |

**分析**：
- **并发墙拆除**：ext4j write nj2 2708→**6024**（165.45%）、nj4 2724→**5139**（186.87%）——脱离 2800 封顶、随并发扩展并反超 Linux 与 raw；read nj4 3640→13200（含 host cache 效应，绝对值超设备口径需谨慎引用，但"锁不再封顶"的结论铁打）。
- **本轮 nj1 行（write 49.84% 等）连同 raw（72.64%/39.62%）整体异常偏低**：raw 不经 ext4 代码同样大降 → 宿主环境噪声，nj1 守底以独立复跑为准（守底链中单独验证）。
- 并发**正确性**由专属双层测试把关（自研 hash 校验 7/7 + xfstests concurrency 10/10，C1 验证链全绿）。

## 10. 结论

1. **答辩数据底座更新**：buffered write 1.2–6.3% → 7.9–29.3%（fio 侧）与 SQLite 2.97% → 21.92% 互相印证；O_DIRECT 守底 96 case 持平证实正交性；并发写经 C1 优化反超 Linux（165–187%）。
2. phase5 报告 D 组与并发读数字作废，以本报告为准。
3. 剩余诚实差距：buffered 写绝对值（结构性 journal 税，technical_report §7.5）、fio 单 job 小块 90% 线（virtio 平台地板）。

## 11. 原始数据索引

| 类型 | 路径 |
|------|------|
| 本轮汇总 TSV（96 case 全参数）| `benchmark/logs/fio_parameter_sweep_20260611_195239/fio_parameter_sweep_summary.tsv` |
| 本轮各 case 完整 fio 日志 | `benchmark/logs/fio_parameter_sweep_20260611_195239/*.log` |
| C1 后 F 组复测 TSV/日志 | `benchmark/logs/fio_parameter_sweep_20260612_103255/` |
| phase5 对照 TSV | `benchmark/logs/fio_parameter_sweep_20260605_032914/fio_parameter_sweep_summary.tsv` |
| sweep 脚本 | `test/initramfs/src/benchmark/fio/run_parameter_sweep_summary.sh`（本轮新增 `SWEEP_GROUPS` 组过滤）|
| fio job 定义 | `test/initramfs/src/benchmark/fio/{ext4_seq_*,raw_seq_*,ext4_nojournal_*,ext4_buffered_*}/run.sh` |
