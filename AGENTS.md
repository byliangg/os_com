# Agents Guidelines for Asterinas

Asterinas is a Linux-compatible, general-purpose OS kernel
written in Rust using the framekernel architecture.
`unsafe` Rust is confined to OSTD (`ostd/`);
the kernel (`kernel/`) is entirely safe Rust.

## Repository Layout

| Directory    | Purpose                                                  |
|--------------|----------------------------------------------------------|
| `kernel/`    | Safe-Rust OS kernel (syscalls, VFS, networking, etc.)    |
| `ostd/`      | OS framework — the only crate permitted to use `unsafe`  |
| `osdk/`      | `cargo-osdk` CLI tool for building/running/testing       |
| `test/`      | Regression and syscall tests (C user-space programs)     |
| `distro/`    | Asterinas NixOS distribution configuration               |
| `tools/`     | Utility scripts (formatting, Docker, benchmarking, etc.) |
| `book/`      | The Asterinas Book (mdBook documentation)                |

## Building and Running

All development is done inside the project Docker container:

```bash
docker run -it --privileged --network=host --device=/dev/kvm -v /dev:/dev \
  -v $(pwd)/asterinas:/root/asterinas \
  asterinas/asterinas:0.17.0-20260227
```

Key Makefile targets:

| Command              | What it does                                         |
|----------------------|------------------------------------------------------|
| `make kernel`        | Build initramfs and the kernel                       |
| `make run_kernel`    | Build and run in QEMU                                |
| `make test`          | Unit tests for non-OSDK crates (`cargo test`)        |
| `make ktest`         | Kernel-mode unit tests via `cargo osdk test` in QEMU |
| `make check`         | Full lint: rustfmt, clippy, typos, license checks    |
| `make format`        | Auto-format Rust, Nix, and C code                    |
| `make docs`          | Build rustdocs for all crates                        |

Set `OSDK_TARGET_ARCH` to `x86_64` (default), `riscv64`, or `loongarch64`.

## Toolchain

- **Rust nightly** pinned in `rust-toolchain.toml` (nightly-2025-12-06).
- **Edition:** 2024.
- `rustfmt.toml`: imports grouped as Std / External / Crate
  (`imports_granularity = "Crate"`, `group_imports = "StdExternalCrate"`).
- Clippy lints are configured in the workspace `Cargo.toml`
  under `[workspace.lints.clippy]`.
  Every member crate must have `[lints] workspace = true`.

## Coding Guidelines

The full coding guidelines live in
`book/src/to-contribute/coding-guidelines/`.
Below is a condensed summary of the most important rules.

### General

- **Be descriptive.** No single-letter names or ambiguous abbreviations.
- **Explain why, not what.** Comments that restate code are noise.
- **One concept per file.** Split when files grow long.
- **Organize for top-down reading.** High-level entry points first.
- **Hide implementation details.** Narrowest visibility by default.
- **Validate at boundaries, trust internally.**
  Validate at syscall entry; trust already-validated values inside.

### Rust

- **Naming:** CamelCase with title-cased acronyms (`IoMemoryArea`).
  Closure variables end in `_fn`.
- **Functions:** Keep small and focused; minimize nesting (max 3 levels);
  use early returns, `let...else`, and `?`.
  Avoid boolean arguments — use an enum or split into two functions.
- **Types:** Use types to enforce invariants.
  Prefer enums over trait objects for closed sets.
  Encapsulate fields behind getters.
- **Unsafety:**
  - Every `unsafe` block requires a `// SAFETY:` comment.
  - Every `unsafe fn` or `unsafe trait` requires a `# Safety` doc section.
  - All crates under `kernel/` must have `#![deny(unsafe_code)]`.
    Only `ostd/` may contain unsafe code.
- **Modules:** Default to `pub(super)` or `pub(crate)`;
  use `pub` only when truly needed.
  Always use `workspace.dependencies`.
- **Error handling:** Propagate errors with `?`.
  Do not `.unwrap()` where failure is possible.
- **Logging:** Use `log` crate macros only (`trace!`..`error!`).
  No `println!` in production code.
- **Concurrency:** Establish and document lock order.
  Never do I/O or blocking under a spinlock.
  Avoid casual use of atomics.
- **Performance:** Avoid O(n) on hot paths.
  Minimize unnecessary copies, allocations, and `Arc::clone`s.
  No premature optimization without benchmark evidence.
- **Macros and attributes:** Prefer functions over macros.
  Suppress lints at the narrowest scope.
  Prefer `#[expect(...)]` over `#[allow(...)]`.
- **Doc comments:** First line uses third-person singular present
  ("Returns", "Creates"). End sentence comments with punctuation.
  Wrap identifiers in backticks.
- **Arithmetic:** Use checked or saturating arithmetic.

### Git

- **Subject line:** Imperative mood, at or below 72 characters.
  Common prefixes: `Fix`, `Add`, `Remove`, `Refactor`, `Rename`,
  `Implement`, `Enable`, `Clean up`, `Bump`.
- **Atomic commits:** One logical change per commit.
- **Separate refactoring from features** into distinct commits.
- **Focused PRs:** One topic per PR. Ensure CI passes before review.

### Testing

- **Add regression tests for every bug fix** (with issue reference).
- **Test user-visible behavior** through public APIs, not internals.
- **Use assertion macros**, not manual output inspection.
- **Clean up resources** after every test (fds, temp files, child processes).

### Assembly

- Use `.balign` over `.align` for unambiguous byte-count alignment.
- Add `.type` and `.size` for Rust-callable functions.
- Use unique label prefixes to avoid name clashes in `global_asm!`.

## Architecture Notes

- **Framekernel:** The kernel is split into a safe upper half (`kernel/`)
  and an unsafe lower half (`ostd/`).
  This is a hard architectural boundary — never add `unsafe` to `kernel/`.
- **Components** (`kernel/comps/`): block, console, network, PCI, virtio, etc.
  Each is a separate crate.
- **OSTD** (`ostd/`): memory management, page tables, interrupt handling,
  synchronization primitives, task scheduling, boot, and arch-specific code.
- **Architectures:** x86-64 (primary), RISC-V 64, LoongArch 64.
  Arch-specific code lives in `ostd/src/arch/` and `kernel/src/arch/`.

## CI

CI runs in the project Docker container with KVM.
Key test matrices:
- x86-64: lint, compile, usermode tests, kernel tests, integration tests
  (boot, syscall, general), multiple boot protocols, SMP configurations.
- RISC-V 64, LoongArch 64, and Intel TDX have dedicated workflows.
- License headers and SCML validation are also checked.

---

# ext4 性能优化项目 — Agent 工作指南

## 项目概览

在 Asterinas OS 中优化 ext4 文件系统性能，并实现优秀档功能要求（JBD2 完整日志、并发读写、崩溃恢复）。

- **性能目标**：fio 顺序读写 >= 90%。诚实口径（cache-off + extent_map/inode 缓存 + drop 公平基线，`direct=1, nj=1`，中位数）下 Phase 5 读优化已收口：**read 4K/16K/64K/256K/1M = 86/84/87/95/123%，write = 76/76/84/121/88%**（优化前小块 16–24% / write 4K 20%、1M 63%）。ext4 域内 per-op 固定开销已榨干，剩余瓶颈在 Asterinas virtio 设备往返（平台层、跨 FS 通用）。历史 `read 127% / write 39%` 是 speculative data cache **开**的不诚实数，不能用于答辩。
- **功能目标**：JBD2 完整事务管理、全量崩溃恢复、多文件并发读写、fsync/flush 持久化语义、PageCache 集成、性能优化全线（Phase 5 + Phase 6 S/P/C 系列）**均已收口**：SQLite 234.9s = Linux 21.92%（起点 2.97%，7.4×）、fio O_DIRECT 守底 75–121%、并发 C1 后 write nj2/4 = 165%/187% 反超 Linux，守底全绿，已推送 GitHub main（a38bda464）。**当前任务：revoke 正确性修复（technical_report C1/C2）**——revoke 记录从不写入 journal + recovery 无序列号上界，两者必须一起修（修一个洞开一个洞）；修法与验证用例设计见 technical_report §5 C1/C2 + 附录 A-1、feature_sqlite_phase6_plan.md §并行正确性任务。次优先：学术研究报告（20% 文档分）。

## 工作树约定

- 当前唯一有效工作树是 `/home/lby/os_com_codex`
- 后续代码修改、benchmark、文档同步默认都在 `/home/lby/os_com_codex` 下进行
- 如果出现多个目录副本，以 `/home/lby/os_com_codex` 为准

## 关键文件索引

| 文件 | 用途 |
|------|------|
| `benchmark.md` | benchmark 与复现唯一指引（§0 最新快照、§1 环境准备与快速复现、§4–6 精确跑法；原 environment.md 已并入后删除）；根目录与 `asterinas/benchmark/`、`asterinas/docs/` 三处副本需同步维护 |
| `benchmark.md` / `benchmark/benchmark.md` / `docs/benchmark.md` | benchmark 指引与最新结果快照（含 §6.7 SQLite speedtest1 确切跑法 / 入口 `run_sqlite_summary.sh` / 最新 2.97%）；三处副本须同步 |
| `analysis_phase1.md` | 性能优化 Phase 1 诊断报告（已完成，95.79%）|
| `optimize_plan_phase1.md` | 性能优化 Phase 1 计划（已完成）|
| `optimize_phase1_milestone.md` | 性能优化 Phase 1 进度跟踪（已完成）|
| `feature_jbd2_phase1_analysis.md` | JBD2 功能 Phase 1 问题分析 |
| `feature_jbd2_phase1_plan.md` | JBD2 功能 Phase 1 实现计划 |
| `feature_jbd2_phase1_milestone.md` | JBD2 功能 Phase 1 进度跟踪（已完成） |
| `feature_jbd2_phase2_analysis.md` / `docs/feature_jbd2_phase2_analysis.md` | JBD2 功能 Phase 2 并发正确性问题分析 |
| `feature_jbd2_phase2_plan.md` / `docs/feature_jbd2_phase2_plan.md` | JBD2 功能 Phase 2 实现计划（先 correctness，再性能） |
| `feature_jbd2_phase2_lock_order.md` / `docs/feature_jbd2_phase2_lock_order.md` | JBD2 功能 Phase 2 锁顺序、同步原语与回退约定 |
| `feature_jbd2_phase2_milestone.md` / `docs/feature_jbd2_phase2_milestone.md` | JBD2 功能 Phase 2 进度跟踪模板 |
| `feature_jbd2_phase3_pretest.md` / `docs/feature_jbd2_phase3_pretest.md` | JBD2 功能 Phase 3 预研测试：fsync/flush 语义风险与性能现象 |
| `feature_jbd2_phase3_plan.md` / `docs/feature_jbd2_phase3_plan.md` | JBD2 功能 Phase 3 实现计划：环境固化、raw/virtio/ext4 fsync/flush 语义收口 |
| `feature_jbd2_phase3_milestone.md` / `docs/feature_jbd2_phase3_milestone.md` | JBD2 功能 Phase 3 进度跟踪模板 |
| `feature_pagecache_phase4_plan.md` / `docs/feature_pagecache_phase4_plan.md` | PageCache Phase 4 实现计划：ext4 buffered I/O / mmap 接入 Asterinas PageCache，自研 cache 退役边界 |
| `feature_pagecache_phase4_milestone.md` / `docs/feature_pagecache_phase4_milestone.md` | PageCache Phase 4 进度、代码审计、回归与 benchmark 记录 |
| `feature_perf_phase5_plan.md` / `docs/feature_perf_phase5_plan.md` | 性能优化 Phase 5 计划（已完成）：延迟归因驱动，O_DIRECT write / 小块 / 读并发优化 |
| `feature_perf_phase5_milestone.md` / `docs/feature_perf_phase5_milestone.md` | 性能优化 Phase 5 进度、占比表、回归与 benchmark 记录（已完成）|
| `feature_sqlite_phase6_plan.md` / `docs/feature_sqlite_phase6_plan.md` | Phase 6 计划（已收口）：S/P/C 系列全过程 + §P 系列 90% 上限分析 + §并行任务 revoke 修复设计 |
| `feature_sqlite_phase6_milestone.md` / `docs/feature_sqlite_phase6_milestone.md` | Phase 6 进度（已收口）：全部变更日志、负结果教训、守底记录 |
| `sqlite_benchmark_report.md` / `docs/sqlite_benchmark_report.md` | SQLite speedtest1 真实应用 benchmark 报告（读追平 / 写瓶颈 / 崩溃 bug / 端到端跑通）|
| `fio_direct_parameter_sweep_report.md` | Phase 5 基线证据：fio direct 全量参数 sweep（A–G 组），三瓶颈分解 |
| `fio_direct_senior_feedback_response.md` | Phase 5 基线证据：学长性能优化指导与三方对齐结论 |
| `赛题要求.md` | 比赛评审标准 |

## 仓库结构

| 路径 | 说明 |
|------|------|
| `asterinas/` | 主仓库（所有代码改动在此） |
| `asterinas/kernel/src/fs/ext4/` | ext4 内核集成层（fs.rs, inode.rs, mod.rs） |
| `asterinas/kernel/libs/ext4_rs/` | ext4 核心库（extent, balloc, file, dir 等） |
| `asterinas/kernel/src/fs/ext2/` | ext2 参考实现（PageCache、bio 集成的范例） |
| `asterinas/kernel/src/fs/utils/page_cache.rs` | Asterinas PageCache 基础设施 |
| `asterinas/kernel/comps/block/` | bio 层、BioSegment、请求合并 |
| `asterinas/test/` | 功能测试 |
| `asterinas/benchmark/` | 性能测试日志 |
| `ext4_rs/` | 合并前的原始 ext4_rs 仓库（仅供参考） |

补充说明：

- 根目录文档 `benchmark.md` 是顶层工作与复现指引（原 `environment.md` 已并入后删除）。
- 仓库内存在对应副本：`asterinas/benchmark/benchmark.md`、`asterinas/docs/benchmark.md`。
- 修改环境或 benchmark 口径时，默认需要同步检查并更新这两处文档，避免根目录说明与仓库内记录不一致。

## 每阶段工作流程

每个 Phase 严格按以下流程执行：

### 1. 规划阶段

- 性能优化阶段：阅读 `optimize_plan_phase1.md` 和 `analysis_phase1.md`
- JBD2 功能阶段：
  - Phase 1：阅读 `feature_jbd2_phase1_plan.md` 和 `feature_jbd2_phase1_analysis.md`
  - Phase 2：阅读 `feature_jbd2_phase2_plan.md` 和 `feature_jbd2_phase2_analysis.md`
  - Phase 3：阅读 `feature_jbd2_phase3_plan.md` 和 `feature_jbd2_phase3_pretest.md`
  - Phase 4：阅读 `feature_pagecache_phase4_plan.md` 和 `feature_pagecache_phase4_milestone.md`，并参考 ext2 PageCache 实现
  - Phase 5：阅读 `feature_perf_phase5_plan.md` 和 `feature_perf_phase5_milestone.md`，基线证据见 `fio_direct_parameter_sweep_report.md`；先收割已有 profile，再优化
  - Phase 6（当前）：阅读 `feature_sqlite_phase6_plan.md` 和 `feature_sqlite_phase6_milestone.md`，起点证据见 `sqlite_benchmark_report.md`；攻 SQLite 追加/新分配写，delalloc 主线，**profile 先行再优化**
- 确定要修改的文件和函数
- 如有需要，先阅读 ext2 对应实现作为参考

### 2. 实现阶段

- 在 `asterinas/` 仓库中进行代码修改
- 每个 Phase 聚焦一个明确目标，不做超出范围的改动
- 关键原则：
  - 不破坏现有功能测试
  - 参考 ext2 的实现模式
  - 优先最小改动、最大收益

### 3. 验证阶段

- 在 Docker 中运行性能测试和功能回归测试
- Docker 环境：`asterinas/asterinas:0.17.0-20260227`
- 进入方式：
  ```bash
  docker run -it --privileged --network=host --device=/dev/kvm -v /dev:/dev \
    -v $(pwd)/asterinas:/root/asterinas \
    asterinas/asterinas:0.17.0-20260227
  ```
- fio 测试参数：`BENCH_ENABLE_KVM=1 BENCH_ASTER_NETDEV=tap BENCH_ASTER_VHOST=on`
- lmbench 测试参数：`PERF_ROUNDS=1 PERF_CASE_TIMEOUT_SEC=600 BENCH_ENABLE_KVM=1 BENCH_ASTER_NETDEV=tap BENCH_ASTER_VHOST=on`

### 4. 记录阶段

- 将结果写入对应 milestone 文件（性能阶段：`optimize_phase1_milestone.md`；JBD2 Phase 1：`feature_jbd2_phase1_milestone.md`；JBD2 Phase 2：`feature_jbd2_phase2_milestone.md`；JBD2 Phase 3：`feature_jbd2_phase3_milestone.md`；PageCache Phase 4：`feature_pagecache_phase4_milestone.md`；性能 Phase 5：`feature_perf_phase5_milestone.md`；SQLite Phase 6：`feature_sqlite_phase6_milestone.md`）对应 Step 下：
  - **改动概要**：简述做了什么
  - **涉及文件**：列出修改的文件路径
  - **性能结果**：贴上新的性能数据表格，与基线对比
  - **功能回归**：记录功能测试是否通过
- 更新变更日志表格
- 如有必要，同步更新对应的 plan 和 analysis 文档

## 阶段总览

### 性能优化系列（optimize_phase）

| Phase | 目标 | 状态 |
|-------|------|------|
| optimize_phase1 | O_DIRECT speculative readahead，fio read/write >= 90% | ✅ 已完成（read 95.79%，write 90.48%） |
| feature_perf_phase5 | 延迟归因驱动优化：extent 映射缓存 / 全文件覆盖 / atime 节流 / inode 元数据缓存 / SQLite 崩溃 bug / 覆盖写快路径 | ✅ 已完成：fio 读写 75–123%（小块读 ×2.6–5.2、write 4K 20→76%）；SQLite 端到端跑通（4773→2022s，2.36×）；完整守底全绿 |
| feature_sqlite_phase6 | SQLite 真实应用追加/新分配写优化（delalloc 延迟分配主线），profile 先行 | 🔄 进行中：起点 2.97%（2022s），攻 INSERT 新块 / CREATE INDEX / VACUUM 慢路径 |

### JBD2 功能系列（feature_jbd2）

| Phase | 目标 | 状态 |
|-------|------|------|
| feature_jbd2_phase1 | JBD2 事务管理、日志刷盘、全量崩溃恢复 | ✅ 已完成（`jbd_phase1` 有效样本 100%，crash 9/9） |
| feature_jbd2_phase2 | 多文件并发读写、xfstests core >= 95%，先 correctness，再性能 | ✅ 已完成（Phase 2 concurrency 7/7，phase6 25/25，crash 18/18） |
| feature_jbd2_phase3 | fsync/fdatasync/block flush 与 Linux 持久化语义对齐 | ✅ 已完成（Tier 1 11 PASS / 1 NOTRUN / 0 FAIL，host-crash fsync 4/4） |
| feature_pagecache_phase4 | ext4 regular-file buffered I/O / mmap 接入 Asterinas PageCache，隔离自研 direct-read cache | ✅ 已收口（`pagecache_phase4` 9 PASS / 0 FAIL / 4 NOTRUN，守底回归全绿） |

## 注意事项

- fio 使用 `direct=1`（O_DIRECT），PageCache 对普通 fio 守底测试无效，必须继续维护 bio 直接 I/O；Phase 4 的 PageCache 指标需与 O_DIRECT 指标分开统计
- lmbench 走缓冲 I/O，PageCache + inode 缓存对其有效，是 Phase 4 重点观察项
- 全局锁 `EXT4_RS_RUNTIME_LOCK` 的历史直接根因是 ext4_rs 的全局 `runtime_block_size` 变量；Phase 2 已移除该全局 block size 状态，并完成 JBD2 handle context、operation-local alloc guard、inode/目录 correctness 锁与 allocator block group 协议；更激进拆锁列入后续 hardening
- Phase 3 已确认 raw block fd、virtio-blk 与 ext4 regular-file `fsync` / `fdatasync` / flush 的 Linux 等价持久化语义；`bs=16K fsync=4` 旧高性能结果不能用于性能宣传
- Phase 4 中 PageCache 只服务 buffered I/O / mmap / writeback；O_DIRECT 仍绕过 PageCache，但必须和 PageCache 建立 flush/discard 一致性协议
- ext2 是最好的参考实现，位于 `asterinas/kernel/src/fs/ext2/`
- 每次改动后必须确认 phase3/phase4/phase6/jbd_phase1/crash/Phase 2 concurrency/Phase 3 fsync-flush 功能测试不回归；Phase 4 还必须增加 buffered/direct coherency、mmap 与 dirty PageCache fsync 验证
- 并发功能正确性有两层互补证据，均不可回退：①自研 `phase2_concurrency.c`（多文件确定性数据完整性 hash 校验，`RUN_PHASE2_CONCURRENCY`）②标准 `concurrency` xfstests 套件（`testcases/concurrency.list`，fsstress 多进程 / 崩溃恢复 / 并发 dio，`PHASE4_DOCKER_MODE=concurrency`）
- **Phase 5（性能）**：profiling 基建已端到端建好（FS / virtio / 锁 / JBD2 四层 ns 级，门控 `ext4fs.phase2_profile=1`），**不要重造**；先收割占比表再优化。三瓶颈：①大块单 job 在 block/virtio（ext4≈raw，非 ext4 锅）②小块 per-request 开销在 ext4（最具优化故事）③读并发退化在锁。JBD2 与大块 ext4 路径数据上已洗清嫌疑。1M 大块定位（virtio vs ext4 成果）需与学长对齐答辩口径
- **Phase 6（SQLite 真实应用写）**：起点 SQLite speedtest1 TOTAL 2022s = Linux 的 2.97%（覆盖写快路径已落地）。剩余慢项全在**追加/新分配类写**（INSERT 新块 / CREATE INDEX / VACUUM）——每个新分配 4KB 页跑一遍完整 journaled 分配 + 每事务 fsync + 逐 4KB bio。主线 = **delalloc 延迟分配**（write 时不分配、writeback 批量分配大 extent + 大 bio）；备选 = 慢路径 journaled prepare 批量化（更轻）。**铁律：先 profile 再优化**（Phase 5 "写回批量化未 profile 先动手只拿 3% 被回退" 的教训）；delalloc/group-commit 触及持久化语义，必须过 crash matrix + SQLite `integrity_check`，不得回退 O_DIRECT 守底
