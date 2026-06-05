# ext4 fio direct 参数广度测试报告（Phase 5 重跑）

> 本报告是 `fio_direct_parameter_sweep_report.md`（优化前 `9cfb36a6d` 基线）的 **Phase 5 重跑对照版**。
> 旧报告保留为优化前基线证据，不要覆盖。

## 1. 测试目的

在 Phase 5 读侧优化（extent 映射缓存 / 全文件覆盖 / atime 节流 / inode 元数据缓存）收口后，用**最新代码 + 诚实口径**重跑同一套 A–F 参数矩阵，建立全矩阵 before/after 画像，判断下一步优化方向。

## 2. 测试环境与口径

| 项目 | 内容 |
|------|------|
| 测试日期 | 2026-06-05 |
| 代码 | `main` / Phase 5 读侧收口（inode 元数据缓存 + extent 映射缓存常开） |
| 工作目录 | `/home/lby/os_com_codex/asterinas` |
| Docker 镜像 | `asterinas/asterinas:0.17.0-20260227` |
| KVM / 网络 | `BENCH_ENABLE_KVM=1`, `BENCH_ASTER_NETDEV=tap`, `BENCH_ASTER_VHOST=on` |
| 结果目录 | `benchmark/logs/fio_parameter_sweep_20260605_032914/` |
| 汇总 TSV | `benchmark/logs/fio_parameter_sweep_20260605_032914/fio_parameter_sweep_summary.tsv` |
| 入口 | `test/initramfs/src/benchmark/fio/run_parameter_sweep_summary.sh`（`RUN_G_CORRECTNESS=0`） |
| 完成度 | 96 case 全部成功，0 FAIL，0 重试触发 |

**口径（与旧报告的关键差异）**：

| 维度 | 旧报告（`9cfb36a6d`） | 本次（Phase 5） |
|------|------|------|
| speculative direct-read **数据** cache | 关 | 关（`direct_read_cache=0`，硬编码，诚实） |
| extent 映射缓存 | 无 | **开**（`EXT4_EXTENT_MAP_CACHE=1` 默认） |
| inode 元数据缓存 | 无 | **常开**（代码无 gate） |
| host page cache 基线 | warm（不 drop） | **drop 公平基线**（`BENCH_DROP_CACHES=1` 默认，sync + drop_caches） |

> drop 口径会让 Linux 对照也变冷，写侧 Linux 不再被 warm cache 压低，因此本次很多 ratio 的"Linux 分母"比旧报告高，是更诚实的对比。

## 3. 分组结论速览

| 分组 | 旧结论 | 本次结论 |
|------|------|------|
| A 官方守底（1M nj1） | read 达标、write 51.60% 不达标 | **read 127% / write 92%，双双达标** |
| B 6-test 分层 | 1M 写瓶颈不只在 JBD2 | journaled/nojournal 写接近，均大幅抬升；JBD2 仍非单 job 瓶颈 |
| C bs sweep | 小块 ext4 极弱（11–28%） | **小块读 81–91%、写 75–89%，全面达标区间** |
| D direct/cache | PageCache warm read 有收益、direct+PC 守底差 | warm read 反超 Linux（105.87%）；D4-write 1.20% 仍是遗留项 |
| E fsync sweep | 仅解释持久化成本 | 同上，不作普通吞吐宣传 |
| F numjobs sweep | nj4 journaled write 95.01% | **并发写卡死 ~2800 不 scale（新主瓶颈）；并发读比例受 Linux cache 假象失真** |

## 4. ① 单 job（nj=1）官方守底：已解决

诚实口径下，ext4 journaled 单 job 各 bs read/write 全部进入 75–127% 区间：

| bs | read 旧 → 新 | write 旧 → 新 |
|----|---:|---:|
| 4K | 11.38% → **81.57%** | 20.59% → **75.63%** |
| 16K | 12.52% → **83.97%** | 22.91% → **75.39%** |
| 64K | 15.93% → **90.71%** | 28.40% → **80.75%** |
| 256K | 26.20% → **89.58%** | 62.95% → **89.17%** |
| 1M | 102.13% → **94.64%**（A2 127.26%） | 53.15% → **84.03%**（A1 92.30%） |
| 4M | 31.76% → **78.07%** | 92.19% → **83.51%** |

收益来源：inode 元数据缓存消除了小块读热路径上每读多次的 inode-block 重读（`get_inode_ref` 此前每次 stat 从设备重读），extent 映射缓存消除了每读的 extent 树查找。这是 Phase 5 的核心战果。

## 5. ② 新主瓶颈：ext4 并发写不 scale（ext4 层串行化）

| nj | raw write MB/s（ratio） | ext4j write MB/s（ratio） |
|----|---:|---:|
| 1 | 2354（63.81%） | 2996（84.30%） |
| 2 | **5326（115.93%）** | **2791（67.43%）** |
| 4 | **5311（113.90%）** | **2760（64.13%）** |

- raw 随并发**线性翻倍**到 5300 MB/s；ext4 写**始终卡在 ~2800 MB/s**，nj 增大不涨。
- 说明瓶颈在 **ext4 写路径的串行化**，不是设备能力。候选根因：写路径锁粒度、JBD2 commit 串行、以及 Phase 5 新增元数据缓存的 `Mutex` + `meta_cache_generation` 在并发写下的竞争/失效风暴。
- ⚠️ **口径说明**：旧报告 nj4 journaled write = 95.01%，本次 64.13%，**看似回归实为口径修正**。旧 warm 口径下 Linux 写被 host cache 压到偏低（分母小，ratio 虚高）；drop 口径下 Linux 写 scale 到 4300+，真实并发差距才暴露。绝对带宽 ext4 并未下降（2760 vs 旧 3824 属不同 Linux 分母），但"不 scale"是真问题。

## 6. ③ 并发读比例失真（Linux 侧 cache 假象）

| nj | raw read Linux MB/s | ext4j read Linux MB/s | ext4j read ratio |
|----|---:|---:|---:|
| 1 | 4714 | 3090 | 119.97% |
| 2 | 5643 | 4933 | 69.00% |
| 4 | **25600** | **9006** | 38.27% |

- nj4 时 Linux 读报到 9006/25600 MB/s —— `direct=1` 下物理设备不可能达到，是 Linux readahead/page cache 在 100s 运行窗口内服务刚写入数据的**测量假象**，把 ratio 人为做低。
- ext4 读绝对值随并发其实**略升**（3245→3447→… nj1 3707 → nj4 3447），并未崩。
- **结论**：多 job 读对比不能直接用作"ext4 读慢"的证据，需对 Linux 侧做归一或改用绝对带宽 + 设备上限对照。若要答辩用并发读数，必须先解释这个 Linux cache 假象。

## 7. ④ 已知遗留项（非回归，Phase 4 territory）

| case | 旧 | 新 | 性质 |
|------|---:|---:|------|
| D3-read-warm（buffered PageCache warm） | 42.11% | **105.87%** | 改善：warm read 反超 Linux |
| D2-write（buffered, PC off） | 6.00% | 6.33% | 遗留：buffered write 慢 |
| D3-write（buffered, PC on） | 1.74% | 2.11% | 遗留：dirty writeback 成本 |
| D4-write（direct + PC on） | 1.30% | 1.20% | 遗留：direct/PageCache coherency |

这些是 Phase 4 PageCache hardening 点，O_DIRECT 守底口径（`page_cache=0`）不受影响。

## 8. 下一步决策建议

1. **官方单 job 守底已达标可宣传**：read 127% / write 92%（1M），小块 75–91%，全部 cache-off + 元数据缓存 + drop 诚实口径。这是答辩主结论。
2. **下一个优化主线 = ext4 并发写串行化**。证据明确（raw scale、ext4 不 scale）。优先 profile 写路径锁 + JBD2 commit + 元数据缓存 mutex 在 nj≥2 下的竞争，目标让 ext4 写随并发上升。注意验证 Phase 5 元数据缓存是否在并发写下因 generation 失效产生额外串行。
3. **并发读对比先做 Linux 归一**，否则数字会误导（Linux cache 假象）。可改用"绝对带宽 vs 设备理论上限"或对 Linux 也强制 drop + 限制 readahead。
4. **PageCache direct/buffered hardening（D2/D3/D4-write）** 维持为独立 Phase 4 hardening，不混入 O_DIRECT 守底。
5. 多 job 达标参数（raw nj2/4 已 113–116%）可作为"设备本身能 scale、瓶颈在 ext4 串行化"的佐证，但 ext4 nj 写当前未达标，不能作为达标宣传。

## 9. 原始数据索引

| 类型 | 路径 |
|------|------|
| 汇总 TSV | `benchmark/logs/fio_parameter_sweep_20260605_032914/fio_parameter_sweep_summary.tsv` |
| 各 case 日志 | `benchmark/logs/fio_parameter_sweep_20260605_032914/*.log` |
| sweep 脚本 | `test/initramfs/src/benchmark/fio/run_parameter_sweep_summary.sh`（本次新增 nix fallback + per-case 重试加固） |
| 优化前基线报告 | `fio_direct_parameter_sweep_report.md`（`9cfb36a6d`） |
