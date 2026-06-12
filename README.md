# 面向 RustOS 的高性能强一致性 EXT4 文件系统（Asterinas）

> 2026 年全国大学生计算机系统能力大赛·操作系统设计赛·OS 功能挑战赛道
> 赛题：面向 RustOS 的高性能强一致性文件系统研究（A 类·学术型，蚂蚁技术研究院）

本仓库在 Rust framekernel 操作系统 [Asterinas](https://github.com/asterinas/asterinas) 上实现了兼容 POSIX、支持 **Extent 连续块管理**与 **JBD2 完整日志**的 EXT4 文件系统，并完成了一条以**延迟归因 profiling 为方法论**的系统性性能优化主线。所有性能数字均为**诚实口径**：cache-off、host drop-caches 公平基线、Asterinas 与 Linux 同机同口径对照。

- 核心实现：`kernel/src/fs/ext4/`（内核集成层）、`kernel/libs/ext4_rs/`（EXT4 核心库，含 JBD2）
- 测试环境：QEMU/KVM + virtio-blk，与赛题规定一致；对照系统为同环境 Linux ext4

## 1. 完成度对照（赛题四大评审项）

| 评审项 | 优秀档要求 | 现状 |
|---|---|---|
| **实现完整度（40%）** | JBD2 完整功能（日志刷盘/事务管理/全量崩溃恢复）、多文件并发读写无错乱丢失、xfstests 核心用例 ≥95% | ✅ 全部达成：JBD2 完整事务管理与全量恢复；崩溃恢复矩阵 18/18 PASS + host-crash fsync 4/4；并发正确性双层验证（自研确定性 hash 校验 7/7 + xfstests concurrency 10/10）；策划用例集通过率 100%（详见 §3 测试矩阵） |
| **文档完整性（20%）** | 架构/用户/测试文档 + 差异化分析 + 性能优化技术研究报告（量化数据 + 学术结论） | ⚠️ 工程文档齐备（本仓库 `docs/`，含技术分析报告、全套 plan/milestone/benchmark 报告）；**学术型研究报告尚未成稿**（素材与量化数据已全部就绪，见 §5 未完成项） |
| **Demo 质量（20%）** | 运行稳定、性能达 Linux EXT4 90%+、优化技术 ≥5% 提升、并发无数据问题 | ⚠️ 部分达成：并发读写 nj2/4 已**反超 Linux**（写 165–187%）；单 job O_DIRECT 为 75–123%（小块缺口归因 virtio 平台层，裸盘自身仅 52–79%，详见 §2.1）；优化提升远超 5%（SQLite 7.4×、小块读 ×2.6–5.2）；并发无数据问题（双层验证） |
| **创新性（20%）** | 1–2 种面向 RustOS 架构的优化技术，可复用、数据可信 | ✅ 素材充足：设备块缓存（JBD2 overlay 下的 write-through 镜像）、unwritten extent 预分配 + 写时转换、写路径 ext2 形态化（内存区间集）、dio overwrite 共享锁并发、四层延迟归因 profiling 方法论（含三次"先核实省实现"的负结果案例） |

## 2. 关键性能数据

### 2.1 fio O_DIRECT 顺序读写（诚实口径，`direct=1 nj=1`，对比 Linux ext4）

| bs | read | write |
|----|-----:|------:|
| 4K | 82% | 75% |
| 16K | 86% | 75% |
| 64K | 88% | 81% |
| 256K | 90% | 121% |
| 1M | 122–140% | 82–87% |

优化前小块仅 11–28%。**剩余小块缺口的归因证据**：同口径裸盘（virtio-blk 无文件系统）仅为 Linux 的 52–79%，且我们 ext4 的比值全面**高于**裸盘比值——ext4 模块自身已无短板，差距来自 virtio 平台层（跨文件系统通用，参考实现 ext2 同样止步 82–85%）。

### 2.2 多文件并发（fio numjobs 同文件，1M，本仓库 dio overwrite 共享锁优化后）

| nj | 写：优化前 → 后（vs Linux） | 读：优化前 → 后 |
|----|---|---|
| 2 | 2708 → **6024 MB/s**（65% → **165%**）| 3531 → 7456 MB/s |
| 4 | 2724 → **5139 MB/s**（63% → **187%**）| 3640 → 13200 MB/s |

优化前 ext4 并发被 per-inode 互斥锁钉死在 ~2800 MB/s；改为读写锁 + 纯覆盖 O_DIRECT 持共享锁（Linux shared `i_rwsem` dio overwrite 的对应实现）后随并发线性扩展并反超 Linux。并发**正确性**始终由双层测试把关，无数据错乱与丢失。

### 2.3 SQLite 真实应用（speedtest1 --size 1000，赛题指定的真实负载维度）

| 优化阶段 | TOTAL | vs Linux |
|---|---:|---:|
| 起点（功能收口时）| 2010.7s | 2.97% |
| unwritten extent + 写时预分配 | 1332.2s | 3.86% |
| 设备块缓存（元数据读 98.5% 命中）| 454.3s | 11.26% |
| 写快路径 ext2 形态化 | 243.9s | 20.88% |
| lean prepare（当前）| **234.9s** | **约 22%** |

累计 **7.4×**，全程 `PRAGMA integrity_check` PASS。三 FS 诊断三角（本平台 ext2 = 94.91%、ramfs = 95.87%）证明平台地板很薄，剩余差距的理论归因（每追加写的结构性日志开销 ≈ 549K 次 × ~240us）与上限分析见技术报告 §7.5。

## 3. 功能与测试矩阵

| 测试 | 结果 |
|---|---|
| JBD2 崩溃恢复矩阵（9 场景 × 2 轮，杀 VM + 重放校验）| 18/18 PASS |
| host-crash fsync 持久化（4 自研场景）| 4/4 PASS |
| fsync/flush 语义（xfstests Tier1，含 generic/388 shutdown 循环）| 100% pass rate |
| 并发正确性（自研多文件 hash 校验 / xfstests concurrency 套件）| 7/7 + 10/10 PASS |
| xfstests 策划用例集（phase3 base / phase4 / phase6 / pagecache / jbd）| 全部 0 FAIL |
| SQLite integrity_check | PASS（每轮性能测试后验证） |
| e2fsck 互操作（unwritten extent 镜像）| 干净（exit 0） |

### 3.1 快速复现

前提：宿主机有 Docker 与 `/dev/kvm`，拉取镜像 `asterinas/asterinas:0.17.0-20260227`。所有入口均在宿主机执行、脚本自管容器：

```bash
# 功能回归（PHASE4_DOCKER_MODE 可选：crash_only / phase6_with_guard / concurrency /
#           jbd_phase1 / jbd_phase2_concurrency / jbd_phase3_host_crash / jbd_phase3_fsync_flush ...）
PHASE4_DOCKER_MODE=crash_only ENABLE_KVM=1 BENCH_ENABLE_KVM=1 \
  BENCH_ASTER_NETDEV=tap BENCH_ASTER_VHOST=on bash tools/ext4/run_phase4_in_docker.sh

# fio 96-case 参数广度测试（SWEEP_GROUPS=F 只跑并发组；RUN_G_CORRECTNESS=0 跳过附带回归）
bash test/initramfs/src/benchmark/fio/run_parameter_sweep_summary.sh

# fio O_DIRECT 守底（单 job 诚实口径）
EXT4_DIRECT_READ_CACHE=0 EXT4_PAGE_CACHE=0 LOG_LEVEL=error BENCH_ENABLE_KVM=1 \
  BENCH_ASTER_NETDEV=tap BENCH_ASTER_VHOST=on bash test/initramfs/src/benchmark/fio/run_ext4_summary.sh

# SQLite 真实应用（FS_LIST="ext4 ext2 ramfs" 可跑三 FS 诊断三角）
FS_LIST=ext4 PAGE_CACHE_LIST=1 LOG_LEVEL=error bash test/initramfs/src/benchmark/sqlite/run_sqlite_summary.sh
```

完整环境约定（KVM/代理/initramfs/常见问题）见 **[docs/environment.md](docs/environment.md)**；各 benchmark 的精确口径、可调参数与历史汇总见 **[docs/benchmark.md](docs/benchmark.md)**（§0 为最新结果快照）。

## 4. 主要优化技术（创新点摘要）

1. **设备块缓存**：在块设备适配层（JBD2 overlay 之下）做设备内容的 write-through 镜像，将"每次元数据访问一次 virtio 往返"（实测一个写 33GB 的负载读了 166GB）压到 98.5% 内存命中，一致性推理局部化到单层。
2. **unwritten extent 语义 + 写时预分配**：EXT4 核心库实现 unwritten 读返零/写转换/左合并（200 次追加仅产生 2 个 extent），预分配尾段以 unwritten 落盘，e2fsck/debugfs 互操作验证。
3. **写快路径 ext2 形态化**：per-inode 内存 allocated 区间集（覆盖判定 = 一次 BTreeMap 查询，不变量 coverage ⊆ truth）+ 用户数据直拷页缓存，纯覆盖写零元数据开销。
4. **dio overwrite 共享锁并发**：per-inode 锁读写化，经映射验证的纯覆盖 O_DIRECT 持共享锁并行提交，对应 Linux dio overwrite 共享锁语义，解除并发串行化。
5. **四层延迟归因 profiling 方法论**：FS/virtio/锁/JBD2 四层 ns 级计时（门控开关，诚实口径零开销），每步优化先归因后动手；包含三次"动手前被数据否决"的负结果记录，方法论本身可复用。

## 5. 未完成项与已知差距（诚实清单）

1. **学术型性能研究报告未成稿**（文档完整性 20% 的优秀档要件）：量化数据、归因图表、负结果案例均已就绪于 `docs/`，待整合成学术文体。
2. **JBD2 revoke 记录未写入 journal**（技术报告 C1/C2）：特定"元数据块释放后复用 + 崩溃重放"序列下存在理论上的静默覆盖风险；现有崩溃矩阵未覆盖该场景。修复方案（revoke 块写入 + 重放序列号过滤）已设计，未实施。
3. **SQLite 距 Linux 90% 的差距**：当前约 22%，类别内（写时同步转换架构）理论上限约 43–46%、现实可达约 24–27%；突破需 delalloc 类机制，其前置依赖（安全的非 fsync 点写回）是平台级缺口。完整理论依据见技术报告 §7.5——该分析本身构成研究结论的一部分。
4. 压力场景下两处低频潜伏问题（fsstress 满盘垃圾节点家族）：已加防御钳制（损坏降级 EIO，不再 panic），共同根因仍在追踪。
5. fio 单 job 小块 90% 线：受 virtio 平台层钉顶（见 §2.1），属跨 FS 平台优化范畴。

## 6. 文档索引

| 文档 | 内容 |
|---|---|
| **[docs/technical_report.md](docs/technical_report.md)** | **系统技术分析报告（主文档）**：架构总览、关键数据路径、问题清单（C1–C9/P1–P6 带代码位置）、性能归因、优化执行结果增编（§7）与上限理论分析。更详细的内容以此为准 |
| **[docs/fio_direct_parameter_sweep_report_phase6.md](docs/fio_direct_parameter_sweep_report_phase6.md)** | **fio 96-case 参数广度测试报告（最新）**：A–F 组全量数据、优化前后对照、并发分析。更详细的性能数据以此为准 |
| [docs/fio_direct_parameter_sweep_report_phase5.md](docs/fio_direct_parameter_sweep_report_phase5.md) | 上一轮 sweep 基线（含裸盘地板专测 §8.5） |
| [docs/sqlite_benchmark_report.md](docs/sqlite_benchmark_report.md) | SQLite 真实应用基准报告 |
| [docs/feature_sqlite_phase6_plan.md](docs/feature_sqlite_phase6_plan.md) / [milestone](docs/feature_sqlite_phase6_milestone.md) | 性能优化主线计划与全过程记录（含负结果与教训） |
| [docs/feature_jbd2_phase1_plan.md](docs/feature_jbd2_phase1_plan.md) 等 | JBD2 / 并发 / fsync / PageCache 各功能阶段的 plan、analysis、milestone 系列 |
| [docs/benchmark.md](docs/benchmark.md) | benchmark 口径与阶段性汇总 |
| [docs/environment.md](docs/environment.md) | 环境搭建、Docker/KVM 约定、复现清单 |
| [docs/赛题要求.md](docs/赛题要求.md) | 赛题原文与评审标准 |

## 7. 口径声明

- 所有对比均为 Asterinas 与 Linux **同机、同 QEMU/KVM、同 virtio-blk、同 drop-caches** 公平基线。
- 历史上 speculative data cache 开启时的 `read 127% / write 39%` 不作为成绩引用。
- 性能数字标注单轮/中位数口径；崩溃一致性结果均含完整重放校验。

## 来源说明

本工程基于 Asterinas 社区项目进行 EXT4/JBD2 赛题方向开发，保留原工程结构与许可证信息。
