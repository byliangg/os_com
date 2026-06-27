# Asterinas ext4 JBD2 功能实现 Phase 1 — 问题分析

## 测试现状

| 测试项 | 当前结果 | 优秀档要求 | 差距 |
|--------|----------|-----------|------|
| crash_only | PASS (6/6，3 场景 × prepare/verify) | 多场景全覆盖 | 需扩至 ≥ 8 个场景 |
| phase3_base | runner 口径 PASS (100%)，但需按原始日志复核 | — | 不能只看 `rc=0` |
| phase4_good | runner 口径 PASS (100%)，但需按原始日志复核 | — | 不能只看通过率 |
| phase6_good（自定义） | runner 口径 PASS / 部分达标，需结合失败用例与内核日志复核 | — | 不能只看通过率 |
| xfstests（官方 jbd_phase1 子集） | 未建立 | ≥ 95% | 需抽取列表 + 建立运行环境 |
| 并发读写 | 未测试 | 无数据错乱（Phase 2 目标） | Phase 1 不覆盖 |
| `e2fsck -n` 对 journal 的识别 | 不通过（自研格式） | 通过（标准 JBD2） | 需实现标准格式 |


---

## 优先级汇总

| 优先级 | 方向 | 对应 Gap | 预期收益 | 实现难度 |
|--------|------|----------|----------|----------|
| P0 | JBD2 on-disk 数据结构与 journal 设备初始化 | G1 | 打通基础，所有后续工作的前置 | 中 |
| P0 | 事务与 block-level metadata 日志写入 | G2 | 替代 CrashJournal，覆盖所有 metadata 路径 | 高 |
| P0 | Commit 流程与 checkpoint | G2、G3 | 让 journal 可持续运行 | 高 |
| P0 | 标准 JBD2 recovery（scan + revoke + replay） | G4 | 全量崩溃恢复的核心 | 中 |
| P1 | xfstests jbd_phase1 列表 + 多场景 crash tests | G5 | 建立优秀档验收基线 | 中（含人工抽取） |
| P2 | 旧 CrashJournal 的替换与 kernel cmdline 开关整理 | G6 | 降低维护成本，避免双轨 | 低 |

---
