# AI 使用说明

> 赛题：2026 全国大学生计算机系统能力大赛 · 操作系统设计赛 · OS 功能挑战赛道
> 项目：在 Asterinas（Rust framekernel）上用 Rust 实现兼容 POSIX、支持 Extent 与 JBD2 日志机制的 EXT4 文件系统，并探索面向 RustOS 架构的性能优化技术。
> 代码基线：分支 `feature-sqlite-phase-6`。
> 文档日期：2026-06-22。

本文件按赛事要求披露项目开发过程中 AI 工具的使用情况，包括工具与模型名称、使用场景、人机分工、AI 辅助形成的材料，以及可追溯的交互记录。本文档用于说明 AI 如何参与开发辅助，不替代项目设计文档、代码提交记录和测试报告。

---

## 1. 披露范围

项目以上游 Asterinas 内核为代码基线。上游社区已有代码不属于本队参赛新增工作，本文件只说明本队在 EXT4 文件系统实现、测试、性能分析和文档整理过程中对 AI 工具的使用情况。

本项目使用 AI 的基本方式是：参赛队负责需求拆分、功能实现、关键代码修改、测试取舍、结果验收和最终提交；AI 工具主要用于沟通实现思路、检查代码风险、排查运行 bug、生成和维护自动化测试脚本、执行耗时测试流程、整理测试数据表格，以及总结阶段状态供队内讨论。所有进入仓库的代码和结论均由参赛队审阅、修改和确认。

---

## 2. AI 工具与模型清单

| 项 | 内容 |
|---|---|
| 交互与执行工具 | Claude Code（命令行交互与执行 harness） |
| 使用的大模型 | DeepSeek V4 Pro |
| 使用周期 | 2026-03 至 2026-06，覆盖 EXT4 基础集成、JBD2、PageCache、性能优化和 SQLite 验证等阶段 |
| 使用方式 | 本地终端交互；由参赛队提出任务、审阅输出、选择是否采纳，并人工完成提交 |
| 提交方式 | Git commit 由参赛队执行；AI 输出不作为未经审查的最终交付内容 |

---

## 3. 使用场景

AI 工具主要用于以下辅助场景：

- **思路沟通与代码检查**：围绕参赛队已经实现或准备实现的功能，讨论可能影响的模块、边界条件和风险点，用作人工检查清单。
- **运行问题排查**：当功能运行不正确、测试失败或出现 panic/timeout 时，辅助分析日志和报错信息，缩小 bug 排查范围。
- **自动化测试辅助**：由于 xfstests、crash recovery、SQLite、fio 等测试耗时较长，AI 辅助编写和维护测试脚本、整理运行命令，并按参赛队要求执行重复测试。
- **测试数据整理**：将多轮 benchmark、profile、PASS/FAIL 结果整理为表格，便于比较不同阶段的性能变化和正确性状态。
- **阶段状态总结**：根据已运行的测试结果和当前代码状态，总结阶段完成情况、遗留问题和下一步计划，方便队内沟通。
- **文档整理**：辅助整理 plan、analysis、milestone、benchmark 复现说明和本披露材料，最终内容由参赛队确认。

AI 未被用于无人值守地决定技术路线或替代功能实现。涉及文件系统语义、崩溃一致性、锁顺序、性能口径、测试结论和是否合入提交的决定，均以参赛队判断为准。

---

## 4. 人机分工

### 4.1 参赛队负责的工作

- 确定阶段目标和优先级，例如 JBD2 日志、PageCache、O_DIRECT、SQLite 写性能等开发顺序。
- 完成功能实现和关键代码修改，包括 extent 映射、事务边界、dirty page 写回、fsync/fdatasync、并发锁和 crash recovery 相关逻辑。
- 审查代码改动是否符合项目设计，确认是否能够进入提交历史。
- 选择测试口径，确认 xfstests、fio、SQLite、crash matrix、host-crash 等验证是否可信。
- 对性能结果做取舍，避免采用不能反映真实文件系统能力的缓存口径或临时数据。
- 决定最终提交内容，维护 Git 分支和 commit 记录。

### 4.2 AI 辅助的工作

- 根据参赛队描述的问题，归纳可能受影响的代码路径和检查点。
- 根据报错日志、测试输出和运行现象辅助定位问题。
- 编写或调整测试脚本，帮助重复运行耗时测试和回归矩阵。
- 把多轮测试数据整理成表格或阶段总结，便于队伍判断下一步方向。
- 对部分关键路径提供代码检查建议，供参赛队判断是否采纳。

这种分工下，AI 是测试、调试、记录和沟通辅助工具，不是项目核心功能实现的独立完成者。

---

## 5. 交互记录与可追溯性

与 AI 的交互记录以阶段过程文档的形式保存在 `docs/AI_usage/`。这些文档记录了各阶段的计划、分析、测试结果和阶段总结，可与对应分支的代码提交记录互相核对。

| 阶段 | 分支 | 主要交互文档 | 大致内容 |
|---|---|---|---|
| EXT4 基础集成 | `stage1-ext4` 到 `stage-8` | [需求](asterinas_ext4_phase1_rs_requirements.md)、[集成报告](asterinas_ext4_phase1_rs_integration_report.md)、[诊断分析](analysis_phase1.md)、[优化计划](optimize_plan_phase1.md)、[进度](optimize_phase1_milestone.md) | ext4_rs 接入 Asterinas、单文件读写、Extent、初步 benchmark |
| JBD2 日志 | `jbd-phase-1` | [分析](feature_jbd2_phase1_analysis.md)、[计划](feature_jbd2_phase1_plan.md)、[进度](feature_jbd2_phase1_milestone.md) | 事务管理、日志刷盘、崩溃恢复验证 |
| 并发正确性 | `jbd-phase-2` | [分析](feature_jbd2_phase2_analysis.md)、[计划](feature_jbd2_phase2_plan.md)、[锁顺序](feature_jbd2_phase2_lock_order.md)、[进度](feature_jbd2_phase2_milestone.md) | 多文件并发、锁顺序、xfstests 回归 |
| 持久化语义 | `jbd-phase-3` | [预研](feature_jbd2_phase3_pretest.md)、[计划](feature_jbd2_phase3_plan.md)、[进度](feature_jbd2_phase3_milestone.md) | fsync/fdatasync/flush 语义和验证 |
| PageCache | `jbd-phase-4-pagecache` | [计划](feature_pagecache_phase4_plan.md)、[进度](feature_pagecache_phase4_milestone.md) | buffered I/O、mmap、PageCache coherency |
| 性能优化 | `jbd-phase-5-optimize` | [计划](feature_perf_phase5_plan.md)、[进度](feature_perf_phase5_milestone.md) | profile、瓶颈归因、小块读写优化 |
| SQLite 真实应用 | `feature-sqlite-phase-6` | [计划](feature_sqlite_phase6_plan.md)、[进度](feature_sqlite_phase6_milestone.md) | SQLite speedtest1、写路径优化、回归守底 |
| official 修复 | `feature-fixerror-phase-7` | [计划](feature_fixerror_phase7_plan.md)、[进度](feature_fixerror_phase7_milestone.md) | official xfstests 错误修复记录 |

---

## 6. 诚信声明

本文件如实披露了项目中使用的 AI 工具、模型名称和主要使用场景。AI 参与的是思路沟通、代码问题检查、bug 排查、自动化测试脚本、测试运行辅助、数据表格整理和文档总结等工作；参赛队负责功能实现、核心设计判断、代码审查、测试验收和最终提交。本文档未刻意夸大 AI 贡献，也不隐瞒 AI 辅助使用情况。
