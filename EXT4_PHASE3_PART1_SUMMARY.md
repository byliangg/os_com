# EXT4 阶段3 第一部分总结报告

## 1. 报告范围与约束

- 报告范围：本次夜间阶段3推进（以 `xfstests` 双轨中的基础轨为主，观测轨做现状记录）。
- 执行约束：严格遵守“**不改 benchmark，只改 ext4/内核逻辑**”。
- 代码修改范围：仅在 `kernel/src/fs/ext4/`（`fs.rs`、`inode.rs`）进行逻辑修复与优化。
- 本次用户要求已执行：所有在跑的测试进程已手动停止。

## 2. 本轮主要问题、处理过程与结果

### 问题A：`generic/006` 长时间超时（核心阻塞）

- 现象：`xfstests` 基础轨中 `generic/006` 多轮出现 `rc=124`（timeout），导致阶段3无法稳定通过。
- 根因分析：
- 目录查找路径存在高频线性扫描，`lookup` 在大量创建/校验流程下开销过高。
- `readdir` offset 语义与 VFS 调用约定存在偏差，目录遍历可能反复/回退，放大耗时问题。
- 代码处理：
- 在 `kernel/src/fs/ext4/fs.rs` 增加目录项缓存（按父目录 inode 维护 `name -> ino`），并在 `lookup/create/mkdir/unlink/rmdir` 后同步缓存状态。
- 在 `kernel/src/fs/ext4/inode.rs` 修正 `readdir_at` 的 offset 行为：向 visitor 传递 `entry.next_offset`，返回值改为“本次消费偏移增量”，与 inode handle 的累加协议对齐。
- 结果：`generic/006` 从 timeout 转为稳定 `rc=0`。

### 问题B：测试链路不稳定（非功能性阻塞）

- 现象：多次出现“命令可执行但结果不可复现”的链路问题。
- 主要阻塞点与处理：
- `nix-build` 不在 PATH：补齐 `PATH`。
- `target/osdk` 权限导致构建失败：切换 `CARGO_TARGET_DIR` 到可写目录。
- OVMF 默认路径指向 root 目录导致权限问题：使用 `BOOT_METHOD=qemu-direct OVMF=off`。
- `make run_kernel` 会覆盖手工 initramfs：采用 `-o initramfs` + `INITRAMFS_SKIP_GZIP=1` 固定使用目标镜像。
- 残留 QEMU 进程锁镜像：测试前后清理旧进程。
- 结果：阶段3基础轨可重复执行，并能连续得到一致结论。

### 问题C：`generic_quick` 观测轨出现“写路径映射失败”风险信号

- 现象：观测轨日志多次出现：
- `[Write] Failed to get physical block for logical block ... ENOENT (logical block not mapped)`
- 并伴随观测任务终止（超时/人工终止）。
- 当前判断：这不是 benchmark 改动问题，属于 ext4 写路径在更激进负载下的语义/分配边界风险，需在下一步继续定位。

## 3. 本轮代码侧关键改动（仅 ext4 逻辑）

1. `kernel/src/fs/ext4/fs.rs`
- 新增目录项缓存结构与查询状态机（Hit/Miss/Unknown）。
- `lookup_at` 先查缓存，必要时一次性填充目录缓存，再回退 ext4_rs 查询。
- `create_at/mkdir_at/unlink_at/rmdir_at` 成功后维护缓存一致性。

2. `kernel/src/fs/ext4/inode.rs`
- 调整 `readdir_at` 的起始定位策略（基于 `next_offset` 定位）。
- 调整 visitor 回调 offset 与返回值语义，避免重复迭代和错误推进。

## 4. 已完成里程碑

### 里程碑1：`generic/006` 从失败转为通过

- 证据：
- `stage3_ext4_logs_local/single_generic006_after_readdirfix2_20260326_095621.log`
- 日志关键结论：`xfstests case done: generic/006 rc=0`。

### 里程碑2：`phase3_base` 连续两轮达标

- Round 1 日志：`stage3_ext4_logs_local/phase3_base_round1_after_fix_20260326_095857.log`
- Round 2 日志：`stage3_ext4_logs_local/phase3_base_round2_retry_20260326_100617.log`
- 两轮关键结论均为：
- `xfstests phase3_base passed: pass_rate=100.00% >= 90%`
- `All syscall tests passed.`

### 里程碑3：执行约束符合要求

- 本轮未改 benchmark 用例或 benchmark 工具逻辑。
- 修改集中在 ext4 内核实现。

## 5. 当前未完成事项

1. 观测轨 `generic_quick` 未形成完整稳定收敛结论
- 已有风险信号（logical block not mapped）。
- 需要下一轮针对写路径映射/块分配边界继续排查。

2. 阶段2 LMbench 8项回归（本轮未完成）
- 本次中断点前主要资源用于打通并稳定 phase3_base。
- 需要在下一轮补跑 8/8 并出回归结果表。

3. 阶段3结果文档需要更新为“第一部分已完成、第二部分进行中”
- 需补齐观测轨失败归因映射与后续修复计划。

## 6. 本轮最终状态（截至当前）

- 已按要求停止当前测试进程。
- 阶段3第一部分（基础轨阻塞目标）已完成：`phase3_base` 连续两轮 `>=90%`（当前为 100%）。
- 阶段3第二部分（观测轨 + 全量回归）尚未完成，已识别到下一阶段的重点风险入口。

