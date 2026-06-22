# Asterinas ext4 JBD2 功能实现 Phase 3 — 计划（已收口）

首次更新时间：2026-05-06（Asia/Shanghai）
收口更新时间：2026-05-11（Asia/Shanghai）

## 阶段状态

**Phase 3 功能线已结束。** 本阶段按“先语义、后性能”的原则，完成 raw block fd、virtio-blk 与 ext4 regular-file 的 `fsync` / `fdatasync` / flush 持久化语义收口，并形成 Tier 1 shutdown xfstests、自研 host-crash fsync matrix 与 fsync-heavy fio 证据链。

普通 O_DIRECT write 在 Phase 3 后仍低于性能红线，且 Step 7 多个小实验已证明它不是单纯的 JBD2 soft limit、bitmap goal、software queue 或 naive page-SG zero-copy 问题。该项从 Phase 3 功能验收中移出，作为后续性能 hardening 独立推进。

收口证据摘要：

| 项目 | 结果 | 说明 |
|------|------|------|
| `jbd_phase3_fsync_flush` | 11 PASS / 1 NOTRUN / 0 FAIL | 默认 2G scratch 口径 |
| `generic/048` | PASS | 12G scratch 单点复跑 |
| host-crash fsync matrix | 4/4 PASS | fsync size、fdatasync metadata、rename+dir fsync、concurrent fsync |
| fsync-heavy fio | 已复跑并修正单位解析 | 真实 flush 成本单独记录，不作为普通吞吐宣传 |
| 普通 O_DIRECT read | 127.06% | 通过 |
| 普通 O_DIRECT write | 39.18% | 后续性能 hardening blocker |

## 目标

在 JBD2 Phase 2 correctness 收口的基础上，单独收口 **`fsync` / `fdatasync` / block flush / 持久化语义与 Linux ext4 的差异**。本目标已完成，具体过程和证据见 `feature_jbd2_phase3_milestone.md`。

本阶段不以提升普通顺序写吞吐为第一目标。Phase 3 的核心问题来自预研测试：

1. raw block file 的 `fsync(/dev/vda)` 可能没有下发到底层 flush；
2. ext4 regular-file `fsync` 当前只做轻量 JBD2 commit / lazy checkpoint，不等价于 Linux 的设备持久化屏障语义；
3. virtio-blk `FLUSH` feature 判断和 `flush()` 分支存在明显可疑点；
4. `bs=16K + fsync=4` 的 Asterinas 高吞吐更像语义缺口暴露，不能作为性能优势宣传。

阶段目标按优先级排序：

1. environment：把 Phase 3 fsync/flush 测试入口固化到 Docker 与仓库资产中，让 clone 后可复现；
2. correctness：raw block fd、virtio-blk、ext4 regular-file 的 `fsync` / `fdatasync` / flush 语义边界清楚；
3. durability：JBD2 commit、ordered mode data drain、block device flush/barrier 的顺序可解释、可测试；
4. coverage：用 xfstests 可运行子集、自研 crash matrix 与 fio fsync-heavy 预研测试共同覆盖；
5. performance：记录真实持久化语义下的 fsync-heavy 性能变化，但不把它与普通 fio 顺序写 90% 目标混淆。

## 当前代码审计锚点

本计划基于 2026-05-06 对当前代码的审计，后续实现前需再次确认：

| 路径 | 当前观察 |
|------|----------|
| `kernel/src/syscall/fsync.rs` | `sys_fsync` / `sys_fdatasync` 分别调用 VFS `sync_all()` / `sync_data()` |
| `kernel/src/fs/utils/inode.rs` | 默认 `sync_all()` / `sync_data()` 返回 `Ok(())`，容易让未覆盖 inode 退化为 no-op |
| `kernel/src/device/registry/block.rs` | `OpenBlockFile` 只实现 read/write/ioctl，当前没有 block file sync hook |
| `kernel/src/fs/ext4/inode.rs` | ext4 regular file 的 `sync_all()` / `sync_data()` 都进入 `fsync_regular_file()` |
| `kernel/src/fs/ext4/fs.rs` | `fsync_regular_file()` 只调用两次 `commit_pending_jbd2_transactions()`，而该路径依赖 `commit_ready()` |
| `kernel/libs/ext4_rs/src/ext4_impls/jbd2/journal.rs` | `commit_ready()` 要求目标 TX 无 active handle；当前没有 inode -> TID 等价追踪和 force-commit 等待语义 |
| `kernel/libs/ext4_rs/src/ext4_impls/jbd2/mod.rs` | journal descriptor / metadata / commit block 写入之间当前没有显式 device flush |
| `kernel/comps/block/src/impl_block_device.rs` | `BlockDevice::sync()` 使用 `BioType::Flush` + `submit_and_wait()` |
| `kernel/comps/virtio/src/device/block/mod.rs` | `VIRTIO_BLK_F_FLUSH` 判断写成 `features & FLUSH == 1`，对 `1 << 9` 明显可疑 |
| `kernel/comps/virtio/src/device/block/device.rs` | `flush()` 分支当前在 `support_flush=true` 时直接 complete，`support_flush=false` 时才下发 `ReqType::Flush`，语义反了 |
| `tools/qemu_args.sh` | 当前 `-drive` 未显式设置 cache mode，Phase 3 host/device persistence 证据必须单独设计 |
| `test/initramfs/src/syscall/xfstests/run_xfstests_test.sh` | 当前把 `src/godown` 包成 ext4 fallback：`sync` + `xfstests_ext4_needs_recovery` marker；这不是内核 `EXT4_IOC_SHUTDOWN`，不能作为 Phase 3 shutdown/fsync durability 证据 |
| `kernel/src/fs/ext4/inode.rs` | 当前未见 ext4-specific ioctl；`EXT4_IOC_SHUTDOWN` / `XFS_IOC_GOINGDOWN` 语义尚未接入 |

## 设计原则

1. **先环境，后实现**：Step 0 只固化 clone-ready Docker 测试入口，不修改内核语义。
2. **先语义，后性能**：修完 flush 后 `fsync=4` 吞吐下降是合理现象；不能为了维持高吞吐保留 no-op flush。
3. **最小持久化边界**：regular-file `fsync` 不应退化成每次全文件系统 checkpoint sweep；JBD2 commit durable 即可，不要求每次把全部 metadata 写回 home block。
4. **区分 guest crash 与 host/device 持久化**：现有 crash matrix 主要证明 guest 重启后的 journal replay；Phase 3 需要补设备 flush/barrier 语义证据。
5. **blocked 透明化**：依赖 `dm-flakey` / `dm-log-writes` 的 xfstests 先进入目标清单和 blocked 清单；不把环境缺口算作 PASS。
6. **复用现有资产**：优先使用 `benchmark/assets/xfstests-prebuilt`、`xfstests-src`、`initramfs`、`linux_vdso` 与现有 Docker runner；避免每次从互联网下载。
7. **不扩大功能范围**：Phase 3 不实现 hardlink/symlink/xattr/DAX/reflink/device-mapper 等非本阶段目标，除非它们是 fsync/flush 验证的必要前置。
8. **不把 QEMU 进程退出误当 host cache 丢失**：`pkill qemu-system` 可以模拟 guest powercut/reboot 边界，但宿主 Linux page cache 不会因 QEMU 退出自动丢失；设备持久化测试必须显式约定 QEMU cache 参数，并依赖 host-side fault/log backend、flush 计数闭环或等价的可丢弃未 flush 写入机制。

## 验收标准

### 固定回归不能回退

| 测试项 | 最低要求 |
|--------|----------|
| `phase3_base_guard` | 不回退，严格关键词扫描为空 |
| `phase4_good` | 不回退，严格关键词扫描为空 |
| `phase6_good` | `25/25 PASS`，严格关键词扫描为空 |
| `jbd_phase1` | 有效样本通过率 100% |
| JBD2 crash matrix | 默认场景 100% PASS |
| Phase 2 concurrency baseline | `7/7 PASS`，严格关键词扫描为空 |

### 新增 Phase 3 验收

- `jbd_phase3_fsync_flush` Docker mode 可一键运行，且 clone 后默认不依赖 `.local` 目录；
- raw `/dev/vda` `fsync` / `fdatasync` 能触达底层 `BlockDevice::sync()`；不支持硬 flush 的设备必须有明确 warn-once 兼容语义；
- virtio-blk `FLUSH` feature 判断正确，支持 flush 时下发 `ReqType::Flush`；
- ext4 regular-file `fsync` 对调用者写入提供目标 TID force commit + ordered data drain + device flush/barrier 语义；
- `fdatasync` 与 `fsync` 的当前差异或等价边界被文档明确记录；
- `EXT4_IOC_SHUTDOWN` 支持 Phase 3 所需 shutdown 语义，`src/godown` 不再依赖 sync-marker shim 伪造 recovery 状态；
- Tier 1 shutdown xfstests、`jbd_phase1`、自研 crash matrix 与 raw/ext4 `bs=16K fsync=4` 预研测试形成阶段证据；
- Tier 2 / Tier 3 dm 依赖 case 有清晰 blocked 原因或自研等价替代测试。
- 普通 fio O_DIRECT write/read 与 fsync-heavy fio 分开验收；普通 ext4 write ratio 若因 Phase 3 改动跌破 `75%`，必须追加 group commit / commit batching 分析或明确环境原因。

## Step 0：Phase 3 环境与测试资产固化

**状态：** 待执行
**目标：** 让 Phase 3 fsync/flush 测试入口在 Docker 内可复现，别人 clone 仓库后不需要手工拼环境。

### 方案

1. 新增或规划 `PHASE4_DOCKER_MODE=jbd_phase3_fsync_flush`：
   - 使用现有 `tools/ext4/run_phase4_in_docker.sh`；
   - 复用 `tools/ext4/run_phase4_part3.sh`、xfstests runner、fio 预研脚本；
   - 默认只跑 Phase 3 相关测试，不夹带 lmbench 或普通 fio 全量。
2. 固化 xfstests 资产：
   - 首选 `benchmark/assets/xfstests-prebuilt`；
   - 兜底使用 `benchmark/assets/xfstests-src` 在 Docker 内重建；
   - 不要求每次联网下载 xfstests。
3. 新增 Phase 3 xfstests 目标清单与 blocked 清单：
   - 新建 `phase3_fsync_durability.list` 或 `jbd_phase3_good.list`；
   - Tier 1：依赖 `_require_scratch_shutdown` / `src/godown`，待 `EXT4_IOC_SHUTDOWN` 接入后直接运行；
   - Tier 2：依赖 `dm-flakey`，默认 blocked，但必须有自研 crash 补位；
   - Tier 3：依赖 `dm-log-writes` / thin-pool / reflink / dm-error 等，默认推迟。
4. 把 shutdown ioctl 作为 Phase 3 入口的显式前置：
   - Linux UAPI：`EXT4_IOC_SHUTDOWN = _IOR('X', 125, __u32)`，本机头文件展开为 `0x8004587d`；
   - 需要支持 `EXT4_GOING_FLAGS_DEFAULT`、`EXT4_GOING_FLAGS_LOGFLUSH`、`EXT4_GOING_FLAGS_NOLOGFLUSH`；
   - Phase 3 真正考核使用 `NOLOGFLUSH`：拒绝后续 I/O，不主动 flush journal，后续 mount 必须靠 replay 恢复；
   - `LOGFLUSH` 更接近 clean shutdown / unmount 边界，用于对照，不作为 fsync 紧语义主证据；
   - Phase 3 runner 不能继续用 `sync + needs_recovery marker` 的 godown shim 作为 PASS 证据；实现前应 fail-fast 或明确 NOTRUN。
5. 纳入 fio fsync-heavy 预研入口：
   - raw write `bs=16K fsync=4`；
   - ext4 journaled write `bs=16K fsync=4`；
   - ext4 nojournal write `bs=16K fsync=4`；
   - 这些测试用于暴露持久化语义，不作为普通吞吐宣传。
6. 更新 `environment.md`、`benchmark.md`、README 与 AGENTS/CLAUDE 索引，明确当前 Phase 3 工作入口。

### xfstests 分层

| 层级 | 用例 | 能否跑 | 跑了验证什么 | 前置条件 |
|------|------|--------|--------------|----------|
| Tier 1 | `generic/043/044/045/046/047/048/049/052/054/055/388/392` | `EXT4_IOC_SHUTDOWN` 后应直接跑 | shutdown 后 journal replay、fsync/fdatasync metadata durability、JBD2 sequence/replay idempotency | `src/godown` 走真实 shutdown ioctl；`xfs_io` 支持所需 `fsync/fdatasync/pwrite/fpunch/fiemap` 子命令 |
| Tier 1 conditional | `generic/506` | 默认 NOTRUN，quota 完成后再跑 | project quota id 持久化 | quota / project id 支持 |
| Carry-over | `generic/051` | 可作为 shutdown stress 回归 | log recovery stress；不作为 Phase 3 最小 fsync 语义项 | 同 Tier 1 |
| Tier 2 | `generic/311/321/322/335/341/342/376` | 当前 blocked | dm-flakey powerfail 下 fsync/dir fsync/rename replay | Asterinas guest device-mapper，或自研 crash 补位 |
| Tier 3 | `generic/455/457/482/648` | 推迟 | dm-log-writes prefix replay、thin-pool、reflink、dm-error 深层一致性 | dm-log-writes / thin-pool / reflink / dm-error |

Tier 1 里最关键的是 `generic/047` 与 `generic/392`：前者是 force-commit 后 inode size 真进 journal 的最小验证，后者直接区分 `fsync` / `fdatasync` 恢复后 metadata 边界。

### 验收

- clone 后进入 `asterinas/`，能通过一条 Docker 命令启动 Phase 3 mode；
- 缺少 prebuilt 时，Docker 内自动准备或给出明确错误；
- `phase3_fsync_durability.list` / `jbd_phase3_good.list` 已定义，Tier 1 / Tier 2 / Tier 3 分类写入 milestone；
- Phase 3 mode 对 `EXT4_IOC_SHUTDOWN` 前置做 fail-fast 或明确 NOTRUN，不使用 godown sync-marker shim 伪造 PASS；
- dm 依赖 case 不混入 pass rate；
- milestone 记录当前资产状态、命令、blocked 清单与日志路径；
- 本 step 不修改 raw block / virtio / ext4 fsync 实现。

## Step 1：建立 fsync/flush 当前语义基线与观测点

**状态：** 待执行
**目标：** 在改代码前，把当前 no-op / lightweight fsync 行为转成可重复证据。

### 方案

1. 复跑 `feature_jbd2_phase3_pretest.md` 中两组测试：
   - 6-test raw/ext4/nojournal read/write；
   - `bs=16K + fsync=4` write-only 测试。
2. 增加或启用低噪声观测点：
   - raw block fd `sync_all` / `sync_data` 是否被调用；
   - `BlockDevice::sync()` 调用次数；
   - `BioType::Flush` 提交次数；
   - virtio `ReqType::Flush` 下发次数；
   - ext4 regular-file `fsync_regular_file()` commit/checkpoint/flush 行为；
   - 每次 `fsync_regular_file()` 入口/出口的 JBD2 `sequence` / running TID / prev_running TID / committed TID；
   - `commit_ready()` 在 fsync 入口是否为 true，若为 false，是因为无 dirty metadata、running transaction 仍有 active handle，还是 prev_running 正在等待 commit；
   - fsync 目标 inode 对应的 dirty metadata TID 是否已经前进到 committed/checkpointed 状态；
   - 当前 ext4 buffered write 是否经过 VFS `PageCache`，以及 fsync 路径是否会触达 page-cache writeback。代码现状与 ext2 不同：ext4 `Ext4Inode` 未暴露 `page_cache()`，非 `O_DIRECT` 写入直接进入 `Ext4Fs::write_at()` / `ext4_rs` 同步块 I/O；若后续引入 ext4 PageCache，fsync 必须先 drain dirty pages。
3. 对比 Linux 侧 fsync latency，记录 Asterinas 当前差异。
4. 明确当前 crash matrix 与 Linux fsync 持久化语义的覆盖差异。

### 验收

- milestone 记录当前 raw/ext4/nojournal `fsync=4` 结果；
- 能证明 raw block fd 当前是否真正触达底层 sync；
- 能证明 ext4 regular-file 当前是否执行设备级 flush；
- 能证明并发写入下 `commit_ready=false` 时 fsync 是否可能静默 no-op；
- 形成 Step 2/3/4 的实现前基线。

## Step 2：raw block fd `fsync` / `fdatasync` 接入底层 sync

**状态：** 待执行
**目标：** 让 `/dev/vda` 这类 block device file 的 `fsync` 不再落到通用 no-op inode sync。

### 方案

1. 审计 `sys_fsync` / `sys_fdatasync` 到 VFS inode `sync_all` / `sync_data` 的路径。
2. 为 block device inode 或 opened block file 增加 sync 行为：
   - `sync_all()` 调用底层 `BlockDevice::sync()`；
   - `sync_data()` 当前可与 `sync_all()` 等价；
   - 不支持 flush 的设备采用保守兼容策略：等待本调用路径上已提交 write 完成，返回成功并 warn-once 记录“不具备硬 flush 能力”；不要把普通应用的 `fsync` 直接变成错误返回，也不要静默 no-op。
3. 保证普通 `read_at` / `write_at` 路径不被额外 flush 影响。
4. 增加 raw block `fsync=4` 观测，确认 sync latency 不再是纳秒级 no-op。

### 验收

- raw `/dev/vda` `fsync` 能触达 `BlockDevice::sync()`；
- 不支持 flush 的设备行为有统一策略与日志证据；
- `raw_write_16k_fsync4` 结果与 sync latency 变化被记录；
- 不破坏 raw read/write 基本 benchmark；
- phase3/phase4/phase6 smoke 不回退。

## Step 3：virtio-blk flush feature 与请求路径修正

**状态：** 待执行
**目标：** 让 `BlockDevice::sync()` 在 virtio-blk 上真正表达设备 flush。

### 方案

1. 修正 `VIRTIO_BLK_F_FLUSH` feature 判断：
   - `FLUSH` 是 `1 << 9`；
   - 判断应检查 bit 是否非零，而不是等于 `1`。
2. 修正 `flush()` 分支语义：
   - 支持 flush 时下发 `ReqType::Flush`；
   - 不支持 flush 时才走明确降级策略。
3. 复核 descriptor 数、response completion、错误处理与日志。
4. 增加观测或统计，证明 `BioType::Flush` 到 `ReqType::Flush` 的路径成立。
5. 复核同步等待闭环：
   - `BlockDevice::sync()` 使用 `Bio::submit_and_wait()`；
   - virtio flush 请求进入 `submitted_requests`；
   - IRQ completion 读取 response 后调用 `bio.complete(BioStatus::Complete)`；
   - 等待方确实在 flush 完成后才返回。

### 验收

- feature 判断单元或代码审计通过；
- 支持 flush 的 virtio 设备实际收到 `ReqType::Flush`；
- `block_device.sync()` 不再被错误短路；
- `block_device.sync()` 的同步等待路径在 virtio flush IRQ completion 上完整闭环；
- raw/ext4 fsync-heavy 测试结果变化可解释。

## Step 4：ext4 regular-file `fsync` / `fdatasync` 持久化语义收口

**状态：** ✅ 4a-1 / 4a-2 / 4b / 4d 已完成（2026-05-07）；4c（commit block 前 PREFLUSH 严格 ordered-mode）留作后续
**目标：** 对普通文件提供接近 Linux ext4 ordered-mode 的 fsync 语义，而不是只依赖同步 DMA 与写序。

### 子步拆分（按风险/收益排序）

| 子步 | 内容 | 改动量 | 风险 | 直接收益 |
|---|---|---|---|---|
| **4a-1** | `Ext4Inode::sync_all/sync_data` 末尾加 `block_device.sync()`（VFS 层 flush，对齐 ext2 模式） | 极小（~5 行）| 低 | 单线程 ext4 fsync latency 立刻从 ~50us 升到 ms 级 |
| **4a-2** | inode→TID 表 + JBD2 `force_commit_for_tid` + WaitQueue 等待原语；fsync 用 force-commit 替换 `commit_pending_jbd2_transactions()` | 中（~150 行，多文件）| 中（并发正确性、新等待原语）| 修复并发下 fsync 静默 no-op 的语义违规 |
| **4b** | `EXT4_IOC_SHUTDOWN` ioctl（`NOLOGFLUSH` / `LOGFLUSH` / `DEFAULT` 三种 flag）+ shutdown 状态机 | 中（~80 行）| 低-中 | Tier 1 xfstests 不再 NOTRUN，可真实跑 generic/047 等 |
| **4c** | commit block 前一次 flush（PREFLUSH 等价）；改 `journal.write_commit_plan_with_hook` 提交序列 | 小-中（~30 行）| 低（不改算法）| 严格 ordered-mode 持久化；host crash 也安全 |
| **4d** | 跑 Tier 1 xfstests + 16K fsync4 benchmark + 自研 host-crash + 写 milestone | — | — | Step 4 验收闭环 |

各子步要求"独立可回退"：每做完一步即 commit 并跑对应 benchmark/smoke，确认 ext4 fsync latency 与 phase4_good/phase6_good 不回退。

### 方案

0. 先补 ext4 shutdown ioctl 前置：
   - 在 ext4 inode/file ioctl 路径识别 `EXT4_IOC_SHUTDOWN`；
   - `NOLOGFLUSH`：丢弃在途写入，不主动 flush journal，直接进入 forced shutdown；这是 Phase 3 模拟硬掉电与验证 replay 的主路径；
   - `LOGFLUSH`：先 force commit + flush journal，再进入 shutdown 状态，用作 clean-ish shutdown / unmount 边界对照；
   - `DEFAULT`：按 Linux ext4 默认 goingdown 语义独立处理，先尽力 sync 可写部分再 forced shutdown；它不等同于 `NOLOGFLUSH`，不能作为最硬 crash 模拟；
   - shutdown 后普通 read/write/create/rename/truncate/fsync 应返回一致错误，remount 后通过 recovery 清除 shutdown 状态；
   - `src/godown` 必须走真实 ioctl，不能继续用 runner shim 的 `sync` marker。
1. 明确 `fsync_regular_file(ino)` 最小承诺：
   - 调用前已完成的该文件写入对应 data block 已落原位置；
   - 必要 metadata 所在 JBD2 transaction 已被 force commit；
   - commit block 与相关写入经过设备 flush/barrier 后对崩溃恢复可见。
2. 引入 inode -> JBD2 TID 追踪：
   - 记录每个 inode 的 `sync_tid` / `datasync_tid` 等价字段，语义对齐 Linux `i_sync_tid` / `i_datasync_tid`；
   - 不直接照搬成“字段挂在当前 `Ext4Inode` 对象上”：当前 ext4 `make_inode()` / `lookup()` 会为同一 ino 构造多个 `Ext4Inode` wrapper，尚无 ext2 block-group inode cache 那种单例对象。v1 采用 `Ext4Fs` 内的 per-ino TID 状态表，或先引入 inode cache 后再挂到真实 inode state；
   - 参考 ext2 `Dirty<T>` 的 per-object dirty pattern，但以“同一 ino 共享状态”为硬约束；
   - 扩展 `JournaledOp` 或 `finish_jbd2_handle()` 的上下文，明确本次 handle 影响的 inode 集合；不要仅靠 `MetadataWriter` 的 raw block offset 反推 inode；
   - regular-file write/truncate/mtime/ctime/extent/inode-size 更新必须能让 fsync 找到需要等待的 TID；
   - directory fsync/rename 场景后续也要能映射到相关目录 inode 的 TID。
3. 引入 force commit 语义，而不是只调用 `commit_pending_jbd2_transactions()`：
   - 参考 Linux `fs/ext4/fsync.c::ext4_sync_file`、`fs/jbd2/transaction.c::jbd2_journal_force_commit_nested`、`fs/jbd2/journal.c::__jbd2_log_start_commit`、`EXT4_I(inode)->i_sync_tid / i_datasync_tid`；
   - 新增 JBD2 层 `force_commit_for_tid(tid)` 或 `force_commit_for_inode(ino)`；
   - 若目标 TID 仍是 running 且存在别的 active handle，主动 rotate 到 `prev_running`，阻止新 handle 继续加入该 TX；
   - 等待该 TID 的 active handles 退出、commit plan 写完、`finish_commit()` 完成；等待必须使用明确等待原语或可解释的调度点，不允许在锁内忙等；
   - 等待原语优先使用 Asterinas 已有 `WaitQueue` / `Condvar` 风格的 `JournalCommitNotifier`：fsync 等待 `committed_tid >= target_tid`，commit 完成后 wake/broadcast；禁止 `loop { yield/spin }` 等轮询等待；
   - 修复后重新判断 `fsync_regular_file()` 中连续两次 `commit_pending_jbd2_transactions()` 是否仍有必要，能删除则删除，不能删除必须写明语义。
4. 明确 ordered mode 的设备顺序：
   - 沿用 Phase 2 已完成的 per-TX data drain，本步只在 drain 之后叠加 device flush/barrier；
   - v1 至少使用两次显式 flush：fs-internal commit block 前一次，VFS inode sync 末尾一次；三次 flush 可作为保守 debug 模式，不作为默认实现目标；
   - fs-internal force commit 顺序：data block 写到 home/original location 并等待 I/O 完成；写 journal descriptor + metadata data blocks；随后执行 commit block 前 flush，确保 data 与 journal payload 都不会落在 commit block 之后；
   - 写 commit block 以及必要 journal superblock 更新后返回给 VFS 层；
   - `Ext4Inode::sync_all()` / `sync_data()` 必须 mirror ext2 `impl_for_vfs/inode.rs` 的模式：调用 fs-internal `fsync_regular_file()` 后，在 VFS inode 层末尾再调用 `fs.block_device().sync()`，作为 commit block 后 flush / FUA 等价物；
   - 若后续 block 层支持 PREFLUSH/FUA，可把 commit block 前 flush + commit block 后 flush 优化成等价屏障，但当前计划以显式 `BioType::Flush` 为准；
   - 当前 ext4 buffered write 不走 VFS PageCache；如果后续接入 PageCache，fsync 必须先调用 `page_cache.evict_range(0..file_size)` 等价原语 drain dirty pages，再进入 force commit。
5. 避免每次 regular-file `fsync` 都全量 checkpoint：
   - committed journal transaction durable 即可；
   - 禁止 per-fsync 触发非目标 TID 的 commit / checkpoint sweep；同一目标 TID 内包含其他 inode 的 metadata 属于 JBD2 group commit 的自然结果，可以一起提交；
   - checkpoint 继续按空间/内存阈值批量化；
   - 只有 unmount / filesystem sync / journal 空间压力需要全量 drain。
6. 明确 `fdatasync` 当前语义：
   - 若实现上仍与 `fsync` 等价，文档记录为保守实现；
   - 明确不采用 ext2 当前 `fdatasync` 路径作为 ext4 参考：ext2 `sync_data()` 只 evict page cache，不同步 inode size / block metadata；这与 Linux `fdatasync` 需要持久化“读回该文件所必需 metadata”的语义存在偏差，`generic/392` 会暴露；
   - 审计 atime/ctime/mtime 更新时间是否进入 journal；若 read-atime 会产生 metadata TX，确认 `fdatasync` 不被纯 atime 更新拖成不必要 force commit；
   - 后续可再细化只刷数据与必要 metadata。
7. 复核目录 fsync、rename 后 fsync、unlink-while-open 与 group commit 边界。
8. 保持 Phase 2 concurrency 的同 inode fsync/write/truncate 互斥策略不回退。

### 验收

- `generic/047` 与 `generic/392` 作为 critical 用例通过或有明确 blocker；
- Tier 1 `generic/043/044/045/046/047/048/049/052/054/055/388/392` 形成真实 shutdown ioctl 证据；
- `generic/506` 若因 quota NOTRUN，必须写明 quota/project-id 前置缺口；
- `jbd_phase1` 有效样本不回退；
- crash matrix 中 fsync durability 场景不回退；
- 并发 active handle 存在时，fsync 能 force rotate 并等待目标 TID commit 完成；
- JBD2 sequence / committed TID 在 fsync 前后可观测前进；
- `ext4_journaled_write_16k_fsync4` latency 上升有合理解释；
- 不引入 per-fsync full checkpoint sweep 导致 `generic/047` / `ext4/045` 级别长尾不可接受。

## Step 5：dm 依赖 xfstests 的替代验证与 blocked 策略

**状态：** 待执行
**目标：** 对 Linux fsync crash replay 相关 xfstests 给出可信处理：能跑则跑，不能跑则明确 blocked 并用自研场景补位。

### 方案

1. 审计并分类以下目标 case：
   - Tier 1 shutdown ioctl：`generic/043/044/045/046/047/048/049/052/054/055/388/392`；
   - `generic/311`：随机写、随机 fsync、故障后 md5/fsck；
   - `generic/321`：directory fsync corner cases；
   - `generic/322`：rename + fsync crash replay；
   - `generic/335/341/342/376`：目录/rename/link 与 fsync 后 powerfail replay；
   - `generic/506`：quota/project-id 持久化，quota 未完成时保持 NOTRUN；
   - `generic/455/457/482/648`：dm-log-writes / thin-pool / reflink / dm-error 深层 prefix replay，推迟；
   - 其他 `dm-log-writes` / `dm-flakey` replay case 作为扩展目标。
2. 判断是否在 Asterinas guest 内实现 device-mapper 能力：
   - 默认不作为 Step 5 必须项；
   - 若成本过高，继续 blocked。
3. 用自研 crash matrix 增补等价场景：
   - `host_crash_fsync_size_durability`：对标 `generic/047/311`，write + fsync + crash + remount 后 size/md5 校验；
   - `host_crash_fdatasync_metadata`：对标 `generic/392`，验证 fdatasync 至少恢复 i_size，fsync 恢复更完整 metadata；
   - `host_crash_rename_fsync_dst`：对标 `generic/322/335/376`，rename + fsync(dst/parent) 后 crash；
   - `host_crash_concurrent_fsync`：Phase 2 concurrency baseline 加 crash 变体，验证 force-commit 在并发 active handle 下不漏 commit；
   - fsync 后 guest powercut / kill QEMU，再 mount verify，用于覆盖 guest crash + journal replay。
4. 补 host/device persistence 方法学：
   - 审计并记录当前 `tools/qemu_args.sh` 中 `-drive` cache 参数；当前若未显式设置 cache，不能把结果当作稳定介质证明；
   - 规划可选 `QEMU_DRIVE_CACHE_MODE=writeback|none|directsync` 等入口，明确哪些模式用于性能、哪些用于语义；
   - host 持久化负向用例必须依赖 host-side `dm-log-writes` / `dm-flakey`、可丢弃未 flush 写入的测试 backend，或等价的 virtio flush 计数 + fault injection；单纯 kill QEMU 不算“host cache 丢失”证明；
   - 修复前应能观察到“fsync 返回但未发 flush / 目标 TID 未 commit”的负向证据；修复后同流程必须消失。
5. 每个 blocked case 必须有替代验证或明确“不在当前功能范围”的说明。

### 验收

- blocked 清单与替代测试一一对应；
- 自研 crash 场景覆盖 Phase 3 关键 fsync/flush 风险；
- Tier 1 shutdown ioctl 用例与 Tier 2/Tier 3 blocked 用例分开统计；
- host/device persistence 证据与 guest crash replay 证据分开记录；
- 文档不把 blocked case 计入 pass rate；
- 若某个 dm case 被解除 blocked，必须记录环境能力、命令与日志。

## Step 6：Phase 3 全量回归、benchmark 与报告更新

**状态：** 待执行
**目标：** 在修复持久化语义后，重新给出可信功能与性能口径。

### 方案

1. 跑固定功能回归：
   - `phase3_base_guard`；
   - `phase4_good`；
   - `phase6_good`；
   - `jbd_phase1`；
   - crash matrix；
   - Phase 2 concurrency baseline。
2. 跑 Phase 3 专项：
   - `jbd_phase3_fsync_flush`；
   - raw/ext4/nojournal `bs=16K fsync=4`；
   - Tier 1 shutdown ioctl xfstests；
   - 4 个自研 host-crash fsync 场景。
3. 更新 benchmark：
   - 普通 fio O_DIRECT read/write 继续单独记录；
   - fsync-heavy fio 单独记录，不与普通顺序吞吐混成一个指标；
   - 若普通 ext4 write ratio 从当前 `87.01%` 跌破 `75%`，必须追加 group commit / commit batching / flush 合并分析；
   - 若 write ratio 变化，说明是 flush 语义修正还是普通路径回退。
4. 更新技术报告：
   - 说明 Phase 3 前后的语义差异；
   - 解释 JBD2 ordered mode + device flush 的实现边界；
   - 记录 dm 依赖 xfstests 的处理策略。

### 验收

- Phase 3 milestone 完整；
- benchmark 与 technical report 不再把 no-op fsync 结果当作性能优势；
- Step 6 统计分两栏：guest crash + journal replay（原 crash matrix + Tier 1 xfstests）与 host/device persistence（4 个自研 host-crash + flush 证据），不混算；
- 赛题优秀档“日志刷盘、事务管理、全量崩溃恢复”在持久化语义上有闭环证据；
- 后续若继续追 fio write >= 90%，作为性能 hardening 单独规划。

## 推荐验证命令

所有命令默认在 `/home/lby/os_com_codex/asterinas` 下执行。

### Phase 3 专项入口（规划）

```bash
PHASE4_DOCKER_MODE=jbd_phase3_fsync_flush \
ENABLE_KVM=1 \
BENCH_ENABLE_KVM=1 \
BENCH_ASTER_NETDEV=tap \
BENCH_ASTER_VHOST=on \
XFSTESTS_CASE_TIMEOUT_SEC=1200 \
XFSTESTS_RUN_TIMEOUT_SEC=5400 \
bash tools/ext4/run_phase4_in_docker.sh
```

### fsync-heavy fio 预研复跑

```bash
cd /home/lby/os_com_codex
KEEP_LOGS=1 bash ./asterinas/test/initramfs/src/benchmark/fio/run_write_16k_fsync4_summary.sh
```

### 固定功能回归

```bash
PHASE4_DOCKER_MODE=phase6_only \
ENABLE_KVM=1 \
BENCH_ENABLE_KVM=1 \
BENCH_ASTER_NETDEV=tap \
BENCH_ASTER_VHOST=on \
XFSTESTS_CASE_TIMEOUT_SEC=1200 \
bash tools/ext4/run_phase4_in_docker.sh
```

```bash
PHASE4_DOCKER_MODE=jbd_phase1 \
ENABLE_KVM=1 \
BENCH_ENABLE_KVM=1 \
BENCH_ASTER_NETDEV=tap \
BENCH_ASTER_VHOST=on \
XFSTESTS_CASE_TIMEOUT_SEC=1200 \
XFSTESTS_RUN_TIMEOUT_SEC=5400 \
bash tools/ext4/run_phase4_in_docker.sh
```

```bash
PHASE4_DOCKER_MODE=jbd_phase2_concurrency \
EXT4_PHASE2_WORKERS=4 \
EXT4_PHASE2_ROUNDS=8 \
EXT4_PHASE2_SEED=78 \
ENABLE_KVM=1 \
BENCH_ENABLE_KVM=1 \
BENCH_ASTER_NETDEV=tap \
BENCH_ASTER_VHOST=on \
bash tools/ext4/run_phase4_in_docker.sh
```

## 阶段管理与回退策略

- Step 0 只做环境/测试入口，不改内核语义。
- Step 2/3/4 每一步都必须能独立回退，并保留前后 benchmark 对照。
- 修 flush 后 fsync-heavy 性能下降不直接视为回退；需要结合语义判断。
- 若设备 flush 导致某个 xfstests 长尾不可接受，先判断是否 per-fsync full checkpoint sweep，再决定优化点。
- 若 dm 依赖 case 无法运行，必须写入 blocked 清单和替代验证，不允许静默跳过。

## Step 7：普通 O_DIRECT write hardening

**状态：** 执行中
**目标：** 在 Phase 3 fsync/flush 语义不回退的前提下，定位并提升普通 fio `ext4_seq_write_bw`，优先把 write 拉回 `75%` hardening 红线，再评估是否能继续冲击 `90%`。

### 背景与约束

Step 6 后普通 O_DIRECT read 已通过，但 write 跌破红线。后续性能优化必须和 fsync-heavy 语义测试分开：

- 普通 fio 参数仍为 `bs=1M direct=1 ioengine=sync numjobs=1 fsync_on_close=1 time_based=1 ramp_time=60 runtime=100`；
- `fsync_on_close=1` 只在关闭时触发一次 flush，不能把它和 `bs=16K fsync=4` 的每 4 次写 flush 混为一谈；
- 不允许为了普通 write 分数移除 fsync/flush、force-commit、PREFLUSH 等 Phase 3 语义修复；
- 若优化触及 block/virtio 公共层，必须同时观察 raw/ext2/ext4/nojournal，防止只优化一个路径却破坏其他文件系统。

### 当前瓶颈假设

基于 2026-05-08 profile，write hardening 不应只盯 JBD2：

1. **data bio 等待是主耗时**：ext4 direct write 的 1MiB data bio 平均约 350-400us，其中设备等待约 260-290us。
2. **用户数据 copy 到 DMA 是稳定成本**：每 1MiB write 约 70-80us；用户 buffer 通常分散成 256 个 4K 物理 run，直接零拷贝/SG 需要谨慎评估。
3. **JBD2/extent prepare 是 miss 路径主长尾**：journaled 比 nojournal 慢约 10-20%；Step 7d-1 证明首次 1GiB 布局的 1024 次 cache miss 平均约 21.9ms，其中 `prepare` 约 18.7ms，并与 JBD2 `avg_apply_us` 对齐。
4. **raw block write 不是干净下限**：raw `/dev/vda` 当前有 Vec 分配 + 双拷贝路径，不能直接代表 virtio 真实上限。
5. **virtio write 缺少 read fast-submit 等价路径**：当前 fast-submit 只允许 read；write 全部走 software request queue，但现有 profile 显示 queue wait 很小，预期收益需要实测。

### 实验顺序

1. **Step 7a：profile 基线固化**
   - 跑 Asterinas-only `ext2_seq_write_bw` / `ext4_seq_write_bw` / `ext4_nojournal_seq_write_bw`；
   - 打开 `EXT4_PHASE2_PROFILE=1`，记录 `[ext4-direct-write]`、`[block-profile]`、`[ext4-phase2]`；
   - 产出 raw/ext2/ext4/nojournal 对照表，明确是 FS 层、JBD2 层还是 block 层主导。
2. **Step 7b：virtio write fast-submit 小实验**
   - 参考 read prefetch fast-submit，只对大块单 segment write 尝试 bypass software queue；
   - 保留失败 fallback 到原 queue 路径；
   - 仅在 profile 证明有收益时保留，否则回退。
3. **Step 7c：raw block aligned write 双拷贝削减评估**
   - 针对 sector-aligned raw write，评估能否直接构造 bio/DMA，避免 `Vec<u8>` 中转；
   - 该项主要用于拆清 block 层上限，不直接作为 ext4 分数优化。
   - Step 7c-1 已验证 ext4 direct-write “用户页直接挂多 segment bio”无收益：1MiB write 被拆成约 5 个小 bio，实时吞吐约 1224MiB/s；后续除非能保持少量大 DMA segment，否则不再走每页 descriptor 零拷贝。
4. **Step 7d：ext4 write mapping / allocation fast path**
   - 先保留 Step 7d-1 的 hit/miss 细分 profile，作为后续实验判据；
   - Step 7d-2 已验证 previous-pblock physical goal 无收益且变慢，不再沿“bitmap 搜索起点”作为主线；
   - 区分纯 overwrite 与会扩展/分配的 write；
   - 评估写侧 mapping cache 或预分配更大 extent window，减少首次布局时 `ext4_prepare_write_at` 长尾；
   - 保持 inode TID 追踪正确，不把 metadata 修改漏出 fsync force-commit。
5. **Step 7e：JBD2 write credit / transaction admission 拆分**
   - 将“inode TID 追踪”与“大块 data length credit admission”解耦；
   - 只对真正会修改 extent/bitmap/inode metadata 的 write 预留较大 credit；
   - 不再做简单固定 credit cap，因为 Step 6 已验证 cap=32 会变慢。
   - 不再做单纯放大 running transaction 的调参，因为 Step 7e-1 已验证 soft limit 1024 -> 4096 虽将 rotations 从 342 降到 68，但 ext4 write 反降到 1422MiB/s，miss 长尾放大。
6. **Step 7f：完整回归**
   - 普通 fio：raw/ext2/ext4/ext4_nojournal 6-test；
   - 功能：`jbd_phase3_fsync_flush`、host-crash 4 case、phase6 smoke、jbd_phase1 或等价最小回归；
   - 文档：更新 `benchmark.md`、milestone 与 technical report。

### 验收

- 不破坏 Phase 3 fsync/flush 语义：Tier 1 fsync/flush、host-crash、raw/ext4 fsync-heavy 结果仍可解释；
- 普通 ext4 write ratio 回到 `>=75%` 才能解除 hardening blocker；
- 若达到 `>=90%`，可作为 Phase 3 退场性能结论；若达不到，必须给出 block/virtio 或 direct-IO 的硬瓶颈证据；
- 每个实验都有“改动、命令、日志、结果、保留/回退结论”。
