# AI 使用披露材料

本文件夹按赛事要求集中存放 AI 工具使用的披露说明及其交互记录证据，自成一体。

## 主文档

- [AI使用说明.md](AI使用说明.md)：披露 AI 工具与模型名称、使用场景、AI 的工作成果、与 AI 的交互记录和诚信声明。先读这一份。

## 交互记录文档

下列为各开发阶段与 AI 协作产生的计划、分析与进度文档，按开发阶段组织。详细对应关系见主文档第 4 节。

| 阶段 | 文档 |
|---|---|
| ext4 基础集成 | asterinas_ext4_phase1_rs_requirements、asterinas_ext4_phase1_rs_integration_report、analysis_phase1、optimize_plan_phase1、optimize_phase1_milestone |
| JBD2 日志 | feature_jbd2_phase1_analysis / _plan / _milestone |
| 并发正确性 | feature_jbd2_phase2_analysis / _plan / _lock_order / _milestone |
| 持久化语义 | feature_jbd2_phase3_pretest / _plan / _milestone |
| PageCache | feature_pagecache_phase4_plan / _milestone |
| 性能优化 | feature_perf_phase5_plan / _milestone |
| SQLite 真实应用 | feature_sqlite_phase6_plan / _milestone |
| official 修复 | feature_fixerror_phase7_plan / _milestone |
