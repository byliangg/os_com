# ext4 fio direct 参数广度测试报告

## 1. 测试目的

本轮测试用于在不牺牲当前 ext4 correctness、JBD2、崩溃恢复与 PageCache Phase 4 语义的前提下，扩大 fio 参数覆盖面，建立可信的性能画像，并判断后续优化方向。

核心问题：

1. O_DIRECT 写性能瓶颈主要来自 raw block / virtio-blk 层，还是 ext4 direct-write 路径？
2. journaled 与 nojournal 写性能差距是否显著，JBD2 是否仍是主要瓶颈？
3. 不同 `bs`、`numjobs`、`fsync`、`direct` 参数下，Asterinas ext4 与 Linux ext4 的差距是否稳定？
4. PageCache buffered I/O 收益与 O_DIRECT cache-off 守底性能是否能清晰分开解释？
5. 是否存在合理、可复现、可用于答辩说明的参数组合，使性能达到赛题优秀档要求，同时不规避核心测试口径？

## 2. 测试环境

| 项目 | 内容 |
|------|------|
| 测试日期 | 2026-05-18 22:24 至 2026-05-19 01:03 |
| 分支 / 提交 | `jbd-phase-4-pagecache` / `9cfb36a6d` |
| 工作目录 | `/home/lby/os_com_codex` |
| Asterinas 目录 | `/home/lby/os_com_codex/asterinas` |
| Docker 镜像 | `asterinas/asterinas:0.17.0-20260227` |
| Docker 策略 | 单次 `docker run --pull=never`，未重复拉取镜像 |
| KVM / 网络 | `BENCH_ENABLE_KVM=1`, `BENCH_ASTER_NETDEV=tap`, `BENCH_ASTER_VHOST=on` |
| fio 工具 | `/benchmark/bin/fio` |
| 结果目录 | `/home/lby/os_com_codex/asterinas/benchmark/logs/fio_parameter_sweep_20260518_222437` |
| 汇总 TSV | `asterinas/benchmark/logs/fio_parameter_sweep_20260518_222437/fio_parameter_sweep_summary.tsv` |
| 备注 | 容器内首次准备 `cargo-osdk` 时产生少量下载；fio 矩阵本身在同一容器内连续运行。 |

## 3. 固定 fio 参数

除单项特别说明外，默认使用：

```bash
/benchmark/bin/fio \
  -size=1G \
  -ioengine=sync \
  -time_based=1 \
  -ramp_time=60 \
  -runtime=100 \
  -fsync_on_close=1
```

变量参数：`direct={0,1}`、`bs={4K,16K,64K,256K,1M,4M}`、`numjobs={1,2,4}`、`fsync={none,4,16,64}`、`EXT4_PAGE_CACHE={0,1}`、`EXT4_DIRECT_READ_CACHE=0`。

## 4. 分组状态

| 分组 | 目标 | 结果状态 | 主要结论 |
|------|------|----------|----------|
| A. 官方 O_DIRECT 守底 | 复现 read/write 口径 | 完成 | read 达标，write 单 job 未达标 |
| B. 6-test 分层诊断 | raw / journaled / nojournal 同轮对比 | 完成 | 1M 单 job 写瓶颈不只在 JBD2 |
| C. bs sweep | 找块大小拐点 | 完成 | 1M 适合 ext4 read；单 job write 仍弱 |
| D. direct/cache 对照 | 区分 O_DIRECT 与 buffered/PageCache | 完成 | PageCache warm read 有收益，但 direct+PageCache 守底很差 |
| E. fsync sweep | 解释持久化语义成本 | 完成 | fsync-heavy 比例不能当普通吞吐宣传 |
| F. numjobs sweep | 判断并发提交能力 | 完成 | `numjobs=4` 下 journaled write 达 95.01% |
| G. correctness 回归 | 确认功能守底 | 完成 | 阈值通过；`phase6_good generic/011` 1 项失败需复查 |

## 5. A 组：官方 O_DIRECT 守底

参数：

| 参数项 | 取值 |
|--------|------|
| 测试对象 | ext4 journaled regular file |
| `rw` | `write`, `read` |
| `direct` | `1` |
| `bs` | `1M` |
| `numjobs` | `1` |
| `fsync` | `none`，仅保留 `fsync_on_close=1` |
| PageCache | `EXT4_PAGE_CACHE=0` |
| direct-read cache | `EXT4_DIRECT_READ_CACHE=0` |
| 对照方式 | Asterinas ext4 vs Linux ext4 |

Cache 使用情况：

| cache 类型 | 是否使用 | 说明 |
|------------|----------|------|
| fio / 文件 PageCache | 否 | `direct=1`，O_DIRECT 绕过文件 PageCache |
| Asterinas ext4 PageCache | 否 | `EXT4_PAGE_CACHE=0` |
| Asterinas ext4 direct-read cache | 否 | `EXT4_DIRECT_READ_CACHE=0` |
| Linux ext4 PageCache | 否 | Linux 对照同样使用 `direct=1` |
| raw/block cache | 不适用 | 本组只测 ext4 regular file |

| case | target | journal | rw | direct | page_cache | bs | jobs | fsync | Asterinas MB/s | Linux MB/s | ratio |
|------|--------|---------|----|-------:|-----------:|----|-----:|-------|---------------:|-----------:|------:|
| A1-ext4j-write | ext4 | journaled | write | 1 | 0 | 1M | 1 | none | 1707.0 | 3308.0 | 51.60% |
| A2-ext4j-read | ext4 | journaled | read | 1 | 0 | 1M | 1 | none | 2821.0 | 2768.0 | 101.91% |

分析：官方口径下 O_DIRECT read 已经超过 90%，write 只有 51.60%。因此如果只看单 job、1M、direct=1，下一步仍要优化 direct write；但 F 组显示并发参数会显著改变结论。

## 6. B 组：6-test 分层诊断

参数：

| 参数项 | 取值 |
|--------|------|
| 测试对象 | raw `/dev/vda`, ext4 journaled, ext4 nojournal |
| `rw` | `write`, `read` |
| `direct` | `1` |
| `bs` | `1M` |
| `numjobs` | `1` |
| `fsync` | `none`，仅保留 `fsync_on_close=1` |
| PageCache | `EXT4_PAGE_CACHE=0` |
| direct-read cache | `EXT4_DIRECT_READ_CACHE=0` |
| 对照方式 | 每个 target 同轮分别跑 Asterinas 与 Linux |
| 诊断目的 | 区分 raw/block 层、ext4 direct I/O 路径、JBD2 额外成本 |

Cache 使用情况：

| cache 类型 | 是否使用 | 说明 |
|------------|----------|------|
| fio / 文件 PageCache | 否 | 全部 case `direct=1` |
| Asterinas ext4 PageCache | 否 | `EXT4_PAGE_CACHE=0` |
| Asterinas ext4 direct-read cache | 否 | `EXT4_DIRECT_READ_CACHE=0` |
| Linux ext4 PageCache | 否 | Linux ext4 对照同样 `direct=1` |
| raw block cache | 不使用文件 cache | raw case 走 `/dev/vda`，不经过 ext4/PageCache |

| case | target | journal | rw | Asterinas MB/s | Linux MB/s | ratio |
|------|--------|---------|----|---------------:|-----------:|------:|
| B1-raw-write | raw | none | write | 1748.0 | 3566.0 | 49.02% |
| B2-raw-read | raw | none | read | 2710.0 | 4780.0 | 56.69% |
| B3-ext4j-write | ext4 | journaled | write | 1839.0 | 3342.0 | 55.03% |
| B4-ext4j-read | ext4 | journaled | read | 2797.0 | 2784.0 | 100.47% |
| B5-ext4n-write | ext4 | nojournal | write | 1852.0 | 3291.0 | 56.27% |
| B6-ext4n-read | ext4 | nojournal | read | 2786.0 | 3748.0 | 74.33% |

派生比例：

| 指标 | 比例 |
|------|------:|
| Asterinas ext4 journaled write / raw write | 105.21% |
| Asterinas ext4 nojournal write / raw write | 105.95% |
| Asterinas journaled write / nojournal write | 99.30% |
| Asterinas ext4 journaled read / raw read | 103.21% |
| Asterinas ext4 nojournal read / raw read | 102.80% |

分析：1M 单 job 下 raw write 本身只有 49.02%，ext4 journaled/nojournal 写差异很小。直接把瓶颈归因到 JBD2 不成立，更像是 direct I/O 同步提交、virtio/block 队列深度或单请求处理路径限制。

## 7. C 组：bs sweep

参数：

| 参数项 | 取值 |
|--------|------|
| 测试对象 | raw `/dev/vda`, ext4 journaled, ext4 nojournal |
| `rw` | `write`, `read` |
| `direct` | `1` |
| `bs` | `4K`, `16K`, `64K`, `256K`, `1M`, `4M` |
| `numjobs` | `1` |
| `fsync` | `none`，仅保留 `fsync_on_close=1` |
| PageCache | `EXT4_PAGE_CACHE=0` |
| direct-read cache | `EXT4_DIRECT_READ_CACHE=0` |
| 对照方式 | 每个 `bs`、target、rw 组合都与 Linux 对比 |
| 诊断目的 | 找 direct I/O 块大小拐点，判断 `1M` 是否合理 |

Cache 使用情况：

| cache 类型 | 是否使用 | 说明 |
|------------|----------|------|
| fio / 文件 PageCache | 否 | 全部 `bs` 组合均为 `direct=1` |
| Asterinas ext4 PageCache | 否 | `EXT4_PAGE_CACHE=0` |
| Asterinas ext4 direct-read cache | 否 | `EXT4_DIRECT_READ_CACHE=0` |
| Linux ext4 PageCache | 否 | Linux 对照同样 `direct=1` |
| raw block cache | 不使用文件 cache | raw case 走块设备直接 I/O 对照 |

| case | target | journal | rw | bs | Asterinas MB/s | Linux MB/s | ratio |
|------|--------|---------|----|----|---------------:|-----------:|------:|
| C-W-raw-4K | raw | none | write | 4K | 123.0 | 234.0 | 52.56% |
| C-R-raw-4K | raw | none | read | 4K | 125.0 | 244.0 | 51.23% |
| C-W-ext4j-4K | ext4 | journaled | write | 4K | 45.1 | 219.0 | 20.59% |
| C-R-ext4j-4K | ext4 | journaled | read | 4K | 22.3 | 196.0 | 11.38% |
| C-W-ext4n-4K | ext4 | nojournal | write | 4K | 45.8 | 226.0 | 20.27% |
| C-R-ext4n-4K | ext4 | nojournal | read | 4K | 22.4 | 201.0 | 11.14% |
| C-W-raw-16K | raw | none | write | 16K | 447.0 | 800.0 | 55.88% |
| C-R-raw-16K | raw | none | read | 16K | 469.0 | 863.0 | 54.35% |
| C-W-ext4j-16K | ext4 | journaled | write | 16K | 173.0 | 755.0 | 22.91% |
| C-R-ext4j-16K | ext4 | journaled | read | 16K | 87.8 | 701.0 | 12.52% |
| C-W-ext4n-16K | ext4 | nojournal | write | 16K | 173.0 | 750.0 | 23.07% |
| C-R-ext4n-16K | ext4 | nojournal | read | 16K | 87.8 | 699.0 | 12.56% |
| C-W-raw-64K | raw | none | write | 64K | 1316.0 | 2138.0 | 61.55% |
| C-R-raw-64K | raw | none | read | 64K | 1428.0 | 2744.0 | 52.04% |
| C-W-ext4j-64K | ext4 | journaled | write | 64K | 622.0 | 2190.0 | 28.40% |
| C-R-ext4j-64K | ext4 | journaled | read | 64K | 337.0 | 2115.0 | 15.93% |
| C-W-ext4n-64K | ext4 | nojournal | write | 64K | 620.0 | 2211.0 | 28.04% |
| C-R-ext4n-64K | ext4 | nojournal | read | 64K | 336.0 | 2090.0 | 16.08% |
| C-W-raw-256K | raw | none | write | 256K | 2630.0 | 3000.0 | 87.67% |
| C-R-raw-256K | raw | none | read | 256K | 2959.0 | 4060.0 | 72.88% |
| C-W-ext4j-256K | ext4 | journaled | write | 256K | 1767.0 | 2807.0 | 62.95% |
| C-R-ext4j-256K | ext4 | journaled | read | 256K | 1160.0 | 4428.0 | 26.20% |
| C-W-ext4n-256K | ext4 | nojournal | write | 256K | 1802.0 | 3020.0 | 59.67% |
| C-R-ext4n-256K | ext4 | nojournal | read | 256K | 1143.0 | 4466.0 | 25.59% |
| C-W-raw-1M | raw | none | write | 1M | 1904.0 | 3554.0 | 53.57% |
| C-R-raw-1M | raw | none | read | 1M | 2589.0 | 4744.0 | 54.57% |
| C-W-ext4j-1M | ext4 | journaled | write | 1M | 1773.0 | 3336.0 | 53.15% |
| C-R-ext4j-1M | ext4 | journaled | read | 1M | 2830.0 | 2771.0 | 102.13% |
| C-W-ext4n-1M | ext4 | nojournal | write | 1M | 1878.0 | 3315.0 | 56.65% |
| C-R-ext4n-1M | ext4 | nojournal | read | 1M | 2650.0 | 2814.0 | 94.17% |
| C-W-raw-4M | raw | none | write | 4M | 1875.0 | 3607.0 | 51.98% |
| C-R-raw-4M | raw | none | read | 4M | 2201.0 | 4854.0 | 45.34% |
| C-W-ext4j-4M | ext4 | journaled | write | 4M | 1760.0 | 1909.0 | 92.19% |
| C-R-ext4j-4M | ext4 | journaled | read | 4M | 2380.0 | 7493.0 | 31.76% |
| C-W-ext4n-4M | ext4 | nojournal | write | 4M | 1836.0 | 3361.0 | 54.63% |
| C-R-ext4n-4M | ext4 | nojournal | read | 4M | 2315.0 | 5974.0 | 38.75% |

分析：小块 direct ext4 路径很弱，4K/16K/64K 均明显低于 Linux；1M 是 ext4 direct read 的稳定达标点。4M journaled write 出现 92.19%，但主要因为该轮 Linux 对照偏低，不建议作为核心宣传点。raw write 在 256K 达到 87.67%，提示块大小存在拐点。

## 8. D 组：direct/cache 对照

参数：

| 子组 | `direct` | `EXT4_PAGE_CACHE` | `EXT4_DIRECT_READ_CACHE` | `rw` | `bs` | `numjobs` | 目标 |
|------|---------:|------------------:|--------------------------:|------|------|----------:|------|
| D1 | 1 | 0 | 0 | `write`, `read` | 1M | 1 | O_DIRECT cache-off 守底 |
| D2 | 0 | 0 | 0 | `write`, `read-cold`, `read-warm` | 1M | 1 | buffered 旧路径 / PageCache-off 对照 |
| D3 | 0 | 1 | 0 | `write`, `read-cold`, `read-warm` | 1M | 1 | buffered PageCache-on |
| D4 | 1 | 1 | 0 | `write`, `read` | 1M | 1 | direct 与 PageCache coherency 守底 |

共同参数：`fsync=none`，仅保留 `fsync_on_close=1`；测试对象为 ext4 journaled regular file；每项均与 Linux ext4 对比。

Cache 使用情况：

| 子组 | fio / 文件 PageCache | Asterinas ext4 PageCache | direct-read cache | Linux PageCache | 说明 |
|------|----------------------|--------------------------|-------------------|-----------------|------|
| D1 | 否 | 否 | 否 | 否 | `direct=1,page_cache=0`，纯 O_DIRECT 守底 |
| D2 | 是 | 否 | 否 | 是 | `direct=0,page_cache=0`，buffered I/O 但 Asterinas PageCache 关闭 |
| D3 | 是 | 是 | 否 | 是 | `direct=0,page_cache=1`，专门测试 PageCache-on buffered 路径 |
| D4 | 理论上否 | 是 | 否 | 理论上否 | `direct=1,page_cache=1`，测试 PageCache 开启时 O_DIRECT 是否受 coherency 协议拖累 |

| case | rw | direct | page_cache | bs | Asterinas MB/s | Linux MB/s | ratio |
|------|----|-------:|-----------:|----|---------------:|-----------:|------:|
| D1-write | write | 1 | 0 | 1M | 1808.0 | 3243.0 | 55.75% |
| D1-read | read | 1 | 0 | 1M | 2786.0 | 2798.0 | 99.57% |
| D2-write | write | 0 | 0 | 1M | 38.4 | 640.0 | 6.00% |
| D2-read-cold | read-cold | 0 | 0 | 1M | 122.0 | 4006.0 | 3.05% |
| D2-read-warm | read-warm | 0 | 0 | 1M | 123.0 | 5965.0 | 2.06% |
| D3-write | write | 0 | 1 | 1M | 10.8 | 622.0 | 1.74% |
| D3-read-cold | read-cold | 0 | 1 | 1M | 20.3 | 3615.0 | 0.56% |
| D3-read-warm | read-warm | 0 | 1 | 1M | 4211.0 | 10000.0 | 42.11% |
| D4-write | write | 1 | 1 | 1M | 42.3 | 3261.0 | 1.30% |
| D4-read | read | 1 | 1 | 1M | 2538.0 | 3788.0 | 67.00% |

分析：PageCache-on 能显著提升 warm buffered read 绝对值，`4211 MB/s` 对比 PageCache-off 的 `123 MB/s` 是明确收益。但 PageCache-on 对 direct 守底路径影响很差，尤其 D4-write 只有 1.30%。这说明 PageCache/direct coherency、discard/invalidate、writeback 协议仍是 Phase 4 hardening 重点。

## 9. E 组：fsync sweep

参数：

| 参数项 | 取值 |
|--------|------|
| 测试对象 | raw `/dev/vda`, ext4 journaled, ext4 nojournal |
| `rw` | `write` |
| `direct` | `1` |
| `bs` | `16K`, `1M` |
| `numjobs` | `1` |
| `fsync` | `none`, `4`, `16`, `64` |
| PageCache | `EXT4_PAGE_CACHE=0` |
| direct-read cache | `EXT4_DIRECT_READ_CACHE=0` |
| 对照方式 | 每个 fsync 周期分别与 Linux 对比 |
| 诊断目的 | 观察 durable write / flush / fsync 频率对吞吐的影响 |
| 注意 | fsync-heavy 结果只用于语义成本分析，不作为普通顺序吞吐主结论 |

Cache 使用情况：

| cache 类型 | 是否使用 | 说明 |
|------------|----------|------|
| fio / 文件 PageCache | 否 | 全部 fsync 组合均为 `direct=1` |
| Asterinas ext4 PageCache | 否 | `EXT4_PAGE_CACHE=0` |
| Asterinas ext4 direct-read cache | 否 | `EXT4_DIRECT_READ_CACHE=0` |
| Linux ext4 PageCache | 否 | Linux ext4 对照同样 `direct=1` |
| raw block cache | 不使用文件 cache | raw case 走 `/dev/vda`，fsync/flush 用于块设备或文件持久化成本对照 |

| case | target | journal | bs | fsync | Asterinas MB/s | Linux MB/s | ratio |
|------|--------|---------|----|-------|---------------:|-----------:|------:|
| E-raw-16K-none | raw | none | 16K | none | 446.0 | 796.0 | 56.03% |
| E-ext4j-16K-none | ext4 | journaled | 16K | none | 173.0 | 764.0 | 22.64% |
| E-ext4n-16K-none | ext4 | nojournal | 16K | none | 172.0 | 788.0 | 21.83% |
| E-raw-16K-4 | raw | none | 16K | 4 | 25.4 | 29.5 | 86.10% |
| E-ext4j-16K-4 | ext4 | journaled | 16K | 4 | 36.6 | 16.7 | 219.16% |
| E-ext4n-16K-4 | ext4 | nojournal | 16K | 4 | 29.7 | 25.8 | 115.12% |
| E-raw-16K-16 | raw | none | 16K | 16 | 101.0 | 77.7 | 129.99% |
| E-ext4j-16K-16 | ext4 | journaled | 16K | 16 | 74.2 | 51.7 | 143.52% |
| E-ext4n-16K-16 | ext4 | nojournal | 16K | 16 | 103.0 | 76.0 | 135.53% |
| E-raw-16K-64 | raw | none | 16K | 64 | 178.0 | 190.0 | 93.68% |
| E-ext4j-16K-64 | ext4 | journaled | 16K | 64 | 130.0 | 130.0 | 100.00% |
| E-ext4n-16K-64 | ext4 | nojournal | 16K | 64 | 133.0 | 193.0 | 68.91% |
| E-raw-1M-none | raw | none | 1M | none | 2119.0 | 3547.0 | 59.74% |
| E-ext4j-1M-none | ext4 | journaled | 1M | none | 1819.0 | 3334.0 | 54.56% |
| E-ext4n-1M-none | ext4 | nojournal | 1M | none | 1787.0 | 3344.0 | 53.44% |
| E-raw-1M-4 | raw | none | 1M | 4 | 462.0 | 612.0 | 75.49% |
| E-ext4j-1M-4 | ext4 | journaled | 1M | 4 | 484.0 | 384.0 | 126.04% |
| E-ext4n-1M-4 | ext4 | nojournal | 1M | 4 | 491.0 | 474.0 | 103.59% |
| E-raw-1M-16 | raw | none | 1M | 16 | 648.0 | 777.0 | 83.40% |
| E-ext4j-1M-16 | ext4 | journaled | 1M | 16 | 647.0 | 544.0 | 118.93% |
| E-ext4n-1M-16 | ext4 | nojournal | 1M | 16 | 643.0 | 715.0 | 89.93% |
| E-raw-1M-64 | raw | none | 1M | 64 | 1394.0 | 1415.0 | 98.52% |
| E-ext4j-1M-64 | ext4 | journaled | 1M | 64 | 982.0 | 961.0 | 102.19% |
| E-ext4n-1M-64 | ext4 | nojournal | 1M | 64 | 1303.0 | 1102.0 | 118.24% |

分析：fsync-heavy 测试下 Asterinas 经常超过 Linux，是因为 Linux 对照被同步持久化成本压低；这些结果只能用于解释持久化成本，不能混入普通 direct 顺序吞吐结论。趋势上 fsync 周期放宽后吞吐上升，1M/fsync=64 已经接近或超过 90%。

## 10. F 组：numjobs sweep

参数：

| 参数项 | 取值 |
|--------|------|
| 测试对象 | raw `/dev/vda`, ext4 journaled, ext4 nojournal |
| `rw` | `write`, `read` |
| `direct` | `1` |
| `bs` | `1M` |
| `numjobs` | `1`, `2`, `4` |
| `fsync` | `none`，仅保留 `fsync_on_close=1` |
| PageCache | `EXT4_PAGE_CACHE=0` |
| direct-read cache | `EXT4_DIRECT_READ_CACHE=0` |
| 对照方式 | 每个 `numjobs`、target、rw 组合都与 Linux 对比 |
| 诊断目的 | 判断单 job 同步提交是否限制 direct I/O，寻找合理队列深度 |

Cache 使用情况：

| cache 类型 | 是否使用 | 说明 |
|------------|----------|------|
| fio / 文件 PageCache | 否 | 全部 `numjobs` 组合均为 `direct=1` |
| Asterinas ext4 PageCache | 否 | `EXT4_PAGE_CACHE=0` |
| Asterinas ext4 direct-read cache | 否 | `EXT4_DIRECT_READ_CACHE=0` |
| Linux ext4 PageCache | 否 | Linux ext4 对照同样 `direct=1` |
| raw block cache | 不使用文件 cache | raw case 用于块设备并发提交能力对照 |

| case | target | journal | rw | jobs | Asterinas MB/s | Linux MB/s | ratio |
|------|--------|---------|----|-----:|---------------:|-----------:|------:|
| F-raw-write-nj1 | raw | none | write | 1 | 2228.0 | 3828.0 | 58.20% |
| F-ext4j-write-nj1 | ext4 | journaled | write | 1 | 1800.0 | 3345.0 | 53.81% |
| F-ext4n-write-nj1 | ext4 | nojournal | write | 1 | 1810.0 | 2759.0 | 65.60% |
| F-raw-read-nj1 | raw | none | read | 1 | 2474.0 | 4703.0 | 52.60% |
| F-ext4j-read-nj1 | ext4 | journaled | read | 1 | 2798.0 | 2778.0 | 100.72% |
| F-ext4n-read-nj1 | ext4 | nojournal | read | 1 | 2746.0 | 2543.0 | 107.98% |
| F-raw-write-nj2 | raw | none | write | 2 | 4611.0 | 4422.0 | 104.27% |
| F-ext4j-write-nj2 | ext4 | journaled | write | 2 | 3856.0 | 6202.0 | 62.17% |
| F-ext4n-write-nj2 | ext4 | nojournal | write | 2 | 3792.0 | 4333.0 | 87.51% |
| F-raw-read-nj2 | raw | none | read | 2 | 5238.0 | 5500.0 | 95.24% |
| F-ext4j-read-nj2 | ext4 | journaled | read | 2 | 2884.0 | 4231.0 | 68.16% |
| F-ext4n-read-nj2 | ext4 | nojournal | read | 2 | 2772.0 | 4339.0 | 63.89% |
| F-raw-write-nj4 | raw | none | write | 4 | 4628.0 | 4319.0 | 107.15% |
| F-ext4j-write-nj4 | ext4 | journaled | write | 4 | 3824.0 | 4025.0 | 95.01% |
| F-ext4n-write-nj4 | ext4 | nojournal | write | 4 | 3717.0 | 4196.0 | 88.58% |
| F-raw-read-nj4 | raw | none | read | 4 | 5018.0 | 5786.0 | 86.73% |
| F-ext4j-read-nj4 | ext4 | journaled | read | 4 | 3056.0 | 4241.0 | 72.06% |
| F-ext4n-read-nj4 | ext4 | nojournal | read | 4 | 2920.0 | 4257.0 | 68.59% |

分析：`numjobs` 是本轮最关键变量。raw write 从 58.20% 提升到 104.27%/107.15%，journaled ext4 write 在 `numjobs=4` 达到 95.01%。这说明单 job direct 写低并不代表底层能力不足，队列深度、并发提交和请求 overlap 是明确优化方向。读方向不同：ext4 read 单 job 达标，但多 job 下降，说明 ext4 direct read 的并发扩展性需要单独分析，不能盲目增大 numjobs。

## 11. G 组：功能回归

参数：

| 测试项 | 参数 / 范围 |
|--------|-------------|
| crash matrix | Phase 4 part3 crash suite，2 轮，9 个场景，共 18 个 verify |
| `phase4_good` | xfstests phase4 good 子集 + syscall tests |
| `pagecache_phase4` | PageCache Phase 4 xfstests 子集 + syscall tests |
| `phase3_base_guard` | Phase 3 base guard xfstests 子集 + syscall tests |
| `phase6_good` | Phase 6 good xfstests 子集 + syscall tests |
| `jbd_phase1` | JBD Phase 1 xfstests 子集 + syscall tests |
| Phase 2 concurrency | `seed=78`, `workers=4`, `rounds=8`, 7 个并发场景 |
| `jbd_phase3_fsync_durability` | fsync / fdatasync / flush 相关 xfstests 子集 + syscall tests |
| host-crash fsync matrix | `host_crash_fsync_size_durability`, `host_crash_fdatasync_metadata`, `host_crash_rename_fsync_dst`, `host_crash_concurrent_fsync` |
| lmbench | 本轮关闭，`RUN_LMBENCH=0` |

共同参数：使用 `phase4_part3` initramfs；KVM/tap/vhost 与 fio 矩阵一致；目标是确认性能数据没有建立在 correctness 回退之上。

Cache 使用情况：

| cache 类型 | 是否统一启用 | 说明 |
|------------|--------------|------|
| fio O_DIRECT cache-off 口径 | 不适用 | G 组不是 fio 性能矩阵，而是功能回归 |
| Asterinas ext4 PageCache | 按子测试启用/覆盖 | `pagecache_phase4` 专门覆盖 PageCache；其他 xfstests 按脚本默认语义运行 |
| direct-read cache | 不作为 G 组变量 | 本组不评估 direct-read cache 性能收益 |
| Linux PageCache | 不作为 G 组变量 | G 组重点是 Asterinas correctness，不做 Linux 性能对照 |
| crash/fsync 持久化状态 | 使用 | crash matrix 和 host-crash fsync matrix 重点验证持久化语义，而不是缓存加速 |

| 测试集 | 结果 | log | note |
|--------|------|-----|------|
| crash matrix | 18/18 PASS | `G_correctness/crash/phase4_part3_crash_summary_20260518_234727.tsv` | create/rename/truncate/large/fsync/multi-file/dir-tree 等两轮全过 |
| `phase4_good` | PASS, pass_rate=100% | `G_correctness/phase4_good_20260518_234727.log` | syscall tests passed |
| `pagecache_phase4` | PASS, pass_rate=100% | `G_correctness/pagecache_phase4_20260518_234727.log` | PageCache 守底通过 |
| `phase3_base_guard` | PASS, pass_rate=100% | `G_correctness/phase3_base_guard_20260518_234727.log` | Phase 3 base 守底通过 |
| `phase6_good` | rc=0, pass_rate=96% | `G_correctness/phase6_good_20260518_234727.log` | `generic/011` output mismatch，需要复查；其余 24 项通过 |
| `jbd_phase1` | PASS, pass_rate=100% | `G_correctness/jbd_phase1_20260518_234727.log` | JBD Phase 1 守底通过 |
| Phase 2 concurrency | 7/7 PASS | `G_correctness/jbd_phase2_concurrency_20260518_234727.log` | 并发正确性通过 |
| `jbd_phase3_fsync_durability` | PASS, pass_rate=100% | `G_correctness/jbd_phase3_fsync_durability_20260518_234727.log` | generic/388 shutdown 日志但 rc=0 |
| host-crash fsync matrix | 4/4 PASS | `G_correctness/crash/phase4_part3_crash_summary_20260519_010329.tsv` | host_crash fsync/fdatasync/rename/concurrent 全过 |

分析：功能回归整体阈值通过，但不能写成完全无风险。`phase6_good generic/011` 出现目录压力 output mismatch：`rm ... Is a directory`。下一步做性能优化前建议先单独复跑 `phase6_good` 或至少 `generic/011`，判断是偶发压力问题还是目录一致性隐患。

## 12. 汇总结论

| 结论项 | 结果 |
|--------|------|
| 官方 O_DIRECT read 是否达到 90% | 是，A2 为 101.91% |
| 官方 O_DIRECT write 是否达到 90% | 否，A1 为 51.60% |
| 合理参数下 direct write 是否可达 90% | 是，F 组 `ext4 journaled write bs=1M numjobs=4` 为 95.01% |
| raw write 是否可达 90% | 是，`numjobs=2/4` 分别为 104.27%/107.15% |
| JBD2 是否是单 job 主要瓶颈 | 不是，B 组 journaled/nojournal 写几乎相同 |
| JBD2 是否影响并发写 | 可能有影响，`numjobs=2` journaled 62.17% 低于 nojournal 87.51%，但 `numjobs=4` journaled 又达 95.01% |
| 最合理 bs | direct read 用 1M 最稳定；write 需要结合 numjobs，单 job 下 256K raw 接近 90% |
| fsync sweep 结论 | 可解释同步成本，不适合宣传普通顺序吞吐 |
| PageCache buffered warm read | 有明显绝对收益，但仍低于 Linux warm read |
| buffered write / direct+PageCache | 仍是 Phase 4 hardening 点 |

## 13. 后续优化方向

1. 优先做 direct write 并发提交路径分析。证据是 raw/ext4 write 在 `numjobs=4` 可以达标，说明核心瓶颈不是绝对 I/O 能力，而是单 job 下请求 overlap 不足、队列深度不足或同步提交串行化。
2. 保留 `bs=1M,numjobs=4,direct=1` 作为可解释的达标参数候选，但不能替代官方单 job 守底。答辩时应同时展示单 job 未达标和多 job 达标，说明我们没有藏数据。
3. 对 ext4 direct read 多 job 退化做二级排查。单 job read 达标，多 job 下降，可能涉及 inode/extent 状态、direct readahead、锁粒度或 fio 多文件/同文件竞争模式。
4. PageCache 方向暂时不要用来解释 direct fio。D 组已经显示 PageCache-on 的 buffered warm read 有收益，但 direct+PageCache 守底非常差，需要先修 coherency/invalidation/writeback 对 O_DIRECT 的干扰。
5. 功能侧先复查 `phase6_good generic/011`。本轮 G 组整体通过阈值，但该单项失败不应忽略，建议下一轮优化前先单测确认是否偶发。

## 14. 原始日志索引

| 类型 | 路径 |
|------|------|
| 参数 sweep 脚本 | `asterinas/test/initramfs/src/benchmark/fio/run_parameter_sweep_summary.sh` |
| 汇总 TSV | `asterinas/benchmark/logs/fio_parameter_sweep_20260518_222437/fio_parameter_sweep_summary.tsv` |
| A-F 单项日志 | `asterinas/benchmark/logs/fio_parameter_sweep_20260518_222437/*.log` |
| G 回归日志目录 | `asterinas/benchmark/logs/fio_parameter_sweep_20260518_222437/G_correctness/` |
| phase6 generic/011 失败日志 | `asterinas/benchmark/logs/fio_parameter_sweep_20260518_222437/G_correctness/phase6_good_20260518_234727.log` |

## 15. 待办

- [ ] 单独复跑 `phase6_good generic/011` 或完整 `phase6_good`，确认是否偶发。
- [ ] 若赛题/老师允许，把 `numjobs=4` 纳入合理参数说明。
- [ ] 对 direct write 单 job 路径做 profile：bio submit、virtio queue、block request overlap、inode/extent 锁、JBD handle/commit。
- [ ] 对 ext4 direct read 多 job 下降做 profile。
- [ ] 修复或解释 PageCache-on direct 守底退化。
- [ ] 将本轮关键结论同步到 `benchmark.md` 与 `feature_pagecache_phase4_milestone.md`。
