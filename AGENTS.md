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

- **性能目标**：fio 顺序读写 >= 90%（Phase 1 已达成：read 95.79%、write 90.48%）
- **功能目标**：JBD2 完整事务管理、全量崩溃恢复、多文件并发读写（feature_jbd2 阶段推进）

## 工作树约定

- 当前唯一有效工作树是 `/home/lby/os_com_codex`
- 后续代码修改、benchmark、文档同步默认都在 `/home/lby/os_com_codex` 下进行
- 如果出现多个目录副本，以 `/home/lby/os_com_codex` 为准

## 关键文件索引

| 文件 | 用途 |
|------|------|
| `environment.md` | 环境指引：记录当前推荐工作目录、Docker/代理/KVM 约定、benchmark 执行注意事项；根目录与仓库内对应副本需同步维护 |
| `benchmark.md` | benchmark 指引与最新结果快照 |
| `analysis_phase1.md` | 性能优化 Phase 1 诊断报告（已完成，95.79%）|
| `optimize_plan_phase1.md` | 性能优化 Phase 1 计划（已完成）|
| `optimize_phase1_milestone.md` | 性能优化 Phase 1 进度跟踪（已完成）|
| `feature_jbd2_phase1_analysis.md` | JBD2 功能 Phase 1 问题分析 |
| `feature_jbd2_phase1_plan.md` | JBD2 功能 Phase 1 实现计划 |
| `feature_jbd2_phase1_milestone.md` | JBD2 功能 Phase 1 进度跟踪 |
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

- 根目录文档 `environment.md`、`benchmark.md` 是顶层工作指引。
- 仓库内存在对应副本：`asterinas/environment.md`、`asterinas/benchmark/benchmark.md`。
- 修改环境或 benchmark 口径时，默认需要同步检查并更新这两处文档，避免根目录说明与仓库内记录不一致。

## 每阶段工作流程

每个 Phase 严格按以下流程执行：

### 1. 规划阶段

- 性能优化阶段：阅读 `optimize_plan_phase1.md` 和 `analysis_phase1.md`
- JBD2 功能阶段：阅读 `feature_jbd2_phase1_plan.md` 和 `feature_jbd2_phase1_analysis.md`
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

- 将结果写入对应 milestone 文件（性能阶段：`optimize_phase1_milestone.md`；JBD2 阶段：`feature_jbd2_phase1_milestone.md`）对应 Step 下：
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

### JBD2 功能系列（feature_jbd2）

| Phase | 目标 | 状态 |
|-------|------|------|
| feature_jbd2_phase1 | JBD2 事务管理、日志刷盘、全量崩溃恢复 | 进行中 |
| feature_jbd2_phase2 | 多文件并发读写、xfstests 全量 >= 95% | 未开始 |

## 注意事项

- fio 使用 `direct=1`（O_DIRECT），PageCache 对 fio 测试无效，必须实现 bio 直接 I/O
- lmbench 走缓冲 I/O，PageCache + inode 缓存对其有效
- 全局锁 `EXT4_RS_RUNTIME_LOCK` 的根因是 ext4_rs 的全局 `runtime_block_size` 变量
- ext2 是最好的参考实现，位于 `asterinas/kernel/src/fs/ext2/`
- 每次改动后必须确认 phase3/phase4/phase6/crash 功能测试不回归
