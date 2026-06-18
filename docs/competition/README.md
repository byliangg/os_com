# 竞赛交付文档集：面向 RustOS 的高性能强一致性 EXT4 文件系统

> 赛题：2026 全国大学生计算机系统能力大赛 · 操作系统设计赛 · OS 功能挑战赛道
> 赛题名称：面向 RustOS 的高性能强一致性文件系统研究
> 项目：在 Asterinas（Rust framekernel，OSTD 非特权运行，兼容 Linux ABI）上用 Rust 实现兼容 POSIX、支持 Extent 与 JBD2 完整日志的 EXT4 文件系统，并探索面向 RustOS 架构的性能优化技术。
> 代码基线：分支 `feature-sqlite-phase-6`（Phase 6 稳定版）。

---

## 文档清单（对应赛题「文档完整性」优秀档要求）

| # | 文档 | 对应评审档位 | 内容 |
|---|---|---|---|
| 01 | [架构设计文档](01_架构设计文档.md) | 基础档 | 分层结构、模块划分、磁盘结构、JBD2 子系统、并发模型、缓存体系、数据路径、崩溃一致性语义 |
| 02 | [用户手册](02_用户手册.md) | 基础档 | 环境要求、构建、挂载与配置、运行功能/性能测试、复现 benchmark、已知限制、故障排查 |
| 03 | [测试文档](03_测试文档.md) | 基础档 | 测试方法学、xfstests 功能正确性、崩溃恢复、并发、持久化语义、PageCache 一致性、性能结果、守底矩阵 |
| 04 | [开源 Rust EXT4 差异化分析](04_开源Rust_EXT4差异化分析.md) | 进阶档 | 与开源 `ext4_rs` 的逐项技术对比与分析结论 |
| 05 | [性能优化技术研究报告](05_性能优化技术研究报告.md) | 优秀档（学术型） | 量化测试数据、12 项优化技术、瓶颈归因、平台地板分析、上限理论、Rust vs C 对比、研究结论、负结果 |

---

## 成绩速览（诚实口径）

> 口径：cache-off（关闭自研投机数据缓存）、drop-caches 公平基线、`direct=1 nj=1` 中位数。历史 cache-on 的 read 127% / write 39% 已废弃，不用于结论。

| 维度 | 结果 |
|---|---|
| 功能 | Extent + 全量 POSIX + JBD2 完整日志（事务/刷盘/全量崩溃恢复）+ 多文件并发 + fsync/flush + PageCache/mmap |
| fio O_DIRECT 顺序读 | 4K–1M = 86 / 84 / 87 / 95 / **123%**（大块追平/反超） |
| fio O_DIRECT 顺序写 | 4K–1M = 76 / 76 / 84 / **121** / 88% |
| fio 并发同文件写（C1 后） | nj2 / nj4 = **165% / 187%**（反超 Linux） |
| SQLite speedtest1 真实应用 | 相对 Linux **2.97% → 21.92%（7.4×）**，墙钟 2022s → 234.9s，integrity PASS |
| 崩溃恢复 | guest-crash 全覆盖；crash matrix 18/18、host-crash fsync 4/4 |
| 并发正确性 | 自研 hash 校验 7/7 + xfstests concurrency 10/10 |
| 守底功能套件 | phase3/phase4/phase6 + jbd_phase1 + fsync Tier1 全部 0 FAIL |

---

## 已声明的已知边界（诚实清单）

| 项 | 说明 |
|---|---|
| JBD2 revoke 写盘 | 未实现：块复用 + checkpoint 前崩溃窗口理论上有覆盖风险（guest-crash 全覆盖场景不触发），列入后续 hardening |
| hardlink / symlink | 未实现 |
| ENOSPC 半成品元数据 | 待补 shutdown 只读保护 |
| official xfstests 全量 | 收口中（个别用例中断 / mmap 开页缓存触发更底层崩溃，已隔离） |
| SQLite 90% | 现实上限 ~24–27%（已达 21.92%），冲 90% 需 delalloc + 后台回写（赛后工程） |
| host 掉电 | 崩溃恢复对 host 掉电依赖 virtio 同步写序假设 |

---

## 证据与复现指引（仓库内）

| 主题 | 文件 |
|---|---|
| benchmark 最新快照 + 精确跑法 | `benchmark.md`（§0 快照、§1 复现） |
| 技术分析（问题清单 + 代码位置 + 路线执行结果） | `technical_report.md`（尤其 §4/§5/§7） |
| fio 参数 sweep 证据 | `fio_direct_parameter_sweep_report_phase6.md` |
| SQLite 真实应用报告 | `sqlite_benchmark_report.md` + `technical_report.md §7` |
| 三 FS 对照原始数据 | `core_results.md` |

> 本文档集为竞赛交付的清洁版；上述仓库文件为过程记录与原始证据，互为印证。
