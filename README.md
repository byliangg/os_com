# Asterinas EXT4 Competition Branch

## 1. 项目说明

本仓库用于 2026 全国大学生计算机系统能力大赛（OS 功能挑战赛道）“面向 RustOS 的高性能强一致性文件系统研究”赛题实践。

目标是在 Asterinas 上完成可验收的 EXT4 支持，并通过 xfstests 与基准测试证明功能正确性与工程可复现性。

## 2. 赛题目标对齐

赛题分档（简化）如下：

1. 基础完成度（及格）：核心 POSIX 接口 + 基础 EXT4 管理 + 单 Extent，xfstests 通过率 >= 90%。
2. 进阶完成度（良好）：完整 POSIX（含 mkdir/rmdir/unlink/rename）+ JBD2 基础能力 + 多 Extent，xfstests 通过率 >= 90%，并完成基础崩溃恢复验证。
3. 优秀完成度（优秀）：完整 JBD2、多场景恢复、并发稳定等。

当前状态（截至 2026-03-30）：

1. `phase3_base`：100%（基础档达标）。
2. `phase4_good`：100%（进阶目标集达标）。
3. 崩溃恢复基础测试（3 场景 * 2 轮）：6/6 PASS。
4. LMbench 8 项回归：8/8 PASS。

## 3. 我们改了什么

### 3.1 文件系统实现层

1. 新增并持续迭代 `kernel/src/fs/ext4/`：
   - `fs.rs`：Ext4Fs、块设备适配、错误映射、目录项缓存、最小事务记录与 mount replay。
   - `inode.rs`：VFS Inode 接口实现（create/read/write/lookup/readdir/unlink/rmdir/rename/truncate/sync）。
2. 在 `kernel/src/fs/mod.rs` 注册 `ext4::init()`，将 ext4 纳入内核文件系统类型。

### 3.2 EXT4 底层库

1. 使用独立 crate `ext4_rs` 作为 EXT4 逻辑实现。
2. 阶段5起目录迁移为：`kernel/libs/ext4_rs`（仍保持独立 crate，不并入 kernel 模块）。
3. workspace 依赖通过 `ext4_rs.workspace = true` 引用。

### 3.3 测试与回归脚本

1. xfstests 相关：
   - `test/initramfs/src/syscall/xfstests/`
   - `phase3_base.list`、`phase4_good.list`、排除清单等。
2. 崩溃恢复套件：
   - `test/initramfs/src/syscall/ext4_crash/run_ext4_crash_test.sh`
3. 一键回归脚本：
   - `tools/ext4/run_phase3_dual_track.sh`
   - `tools/ext4/run_phase4_part1.sh`
   - `tools/ext4/run_phase4_part2.sh`
   - `tools/ext4/run_phase4_part3.sh`

## 4. 关键调用链（当前实现）

运行时数据路径可概括为：

`syscall -> VFS(Inode) -> kernel/src/fs/ext4/{inode,fs}.rs -> ext4_rs (kernel/libs/ext4_rs) -> BlockDevice adapter -> virtio-blk`

说明：

1. `kernel/src/fs/ext4` 负责把 Asterinas VFS 语义映射到 ext4 操作。
2. `kernel/libs/ext4_rs` 负责 ext4 元数据/extent/目录项等底层逻辑。
3. ext4 适配层负责错误码映射、缓存、以及本项目的最小崩溃恢复闭环实现。

## 5. 运行环境（Docker 优先）

### 5.1 Docker 启动（推荐，和旧 README 一致）

在 `<repo_root>` 的上一级目录执行：

```bash
docker run --rm -it --privileged --network=host \
  -v /dev:/dev \
  -v "$(pwd)/asterinas:/root/asterinas" \
  -w /root/asterinas \
  asterinas/asterinas:0.17.0-20260227
```

如遇下载问题可加代理环境变量：

```bash
docker run --rm -it --privileged --network=host \
  -v /dev:/dev \
  -v "$(pwd)/asterinas:/root/asterinas" \
  -w /root/asterinas \
  -e http_proxy=http://127.0.0.1:7890 \
  -e https_proxy=http://127.0.0.1:7890 \
  -e all_proxy=socks5://127.0.0.1:7890 \
  asterinas/asterinas:0.17.0-20260227
```

容器内首次可执行：

```bash
OSDK_LOCAL_DEV=1 cargo install --path osdk --locked --force
```

### 5.2 统一复现实验口径（容器内/本机都适用）

```bash
cd <repo_root>

export PATH=$HOME/.local/bin:$PATH
export CARGO_TARGET_DIR=$(pwd)/target_lby
export VDSO_LIBRARY_DIR=$(pwd)/.local/linux_vdso
export BOOT_METHOD=qemu-direct
export ENABLE_KVM=0
export RELEASE_LTO=1
```

## 6. 如何跑测试与 benchmark

### 6.1 推荐：Phase4 全量回归（含崩溃恢复）

Docker 一键（推荐）：

```bash
cd <repo_root>
./tools/ext4/run_phase4_part3_docker.sh
```

Docker 一键（启用 Clash 7890 代理）：

```bash
cd <repo_root>
./tools/ext4/run_phase4_part3_docker.sh --proxy
```

容器内/本机直接执行：

```bash
cd <repo_root>

timeout 10800s tools/ext4/run_phase4_part3.sh
```

该脚本会串行执行：

1. crash suite（create_write / rename / truncate_append）
2. xfstests `phase4_good`
3. xfstests `phase3_base` guard
4. LMbench 8 项 guard

### 6.2 阶段性回归

```bash
# Part1：口径固化 + 护栏
./tools/ext4/run_phase4_part1.sh

# Part2：rename 收敛 + 护栏
./tools/ext4/run_phase4_part2.sh
```

### 6.3 Phase3 基础档复现

```bash
./tools/ext4/run_phase3_dual_track.sh
```

## 7. 输出日志位置

常用结果目录：

1. `stage3_ext4_logs_local/`
2. `stage4_ext4_logs_part1/`
3. `stage4_ext4_logs_part2/`
4. `stage4_ext4_logs_part3/`

其中 `stage4_ext4_logs_part3/` 包含：

1. `phase4_good_*.log`
2. `phase3_base_guard_*.log`
3. `crash/phase4_part3_crash_summary_*.tsv`
4. `lmbench/phase4_part3_lmbench_summary_*.tsv`

## 8. xfstests 模式说明

1. `phase3_base`：基础完成度验收集（基础档门槛）。
2. `phase4_good`：进阶完成度验收集（良好档门槛）。
3. `generic_quick`：观测轨（用于扩展风险扫描，不直接作为阶段门槛）。

## 9. 约束与边界

1. 不修改 benchmark/xfstests 用例逻辑和判定规则。
2. 只修改我们仓库代码（ext4 适配层 + ext4_rs + 测试编排脚本）。
3. 当前崩溃恢复为“最小可用闭环”，不等同于完整高性能 JBD2 协议级实现。
