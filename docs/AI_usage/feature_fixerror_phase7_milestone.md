# ext4 功能正确性 Phase 7 Milestone 记录（Official xfstests 错误修复主线）

配套计划：`feature_fixerror_phase7_plan.md`
工作分支：`feature-fixerror-phase-7`
起点日期：2026-06-12

## 0. 当前状态

Phase 7 已立项，目标是修复 official xfstests 暴露出的错误，并保证 Phase 3/4/5/6 既有守底不回退。

最新进展（2026-06-18）：

- `official_20260617_121742.log`：full official 被外层 21600s run timeout 在 `generic/558` 附近终止，未生成最终 official summary；但在被终止前已完成 69 个 case，其中 60 个有效样例为 57 PASS / 3 FAIL，当前有效通过率 95.00%。
- 本轮关键共因已修：`generic/269` 满盘后 umount ENOSPC 导致的 scratch 残留挂载已消除，`269/273/275/308/309/313/320/339/340/344/345/346/354/371/393/406/412/...` 在 full-order 下均恢复 PASS/NOTRUN 正常分类。
- `generic/452` 的 busybox `ls` applet 复制后 argv0 失效问题已修，单测 PASS（`official_20260618_113426.log`）。按当前已完成集合折算，`452` 转绿后有效样例为 58 PASS / 2 FAIL / 60，约 96.67%。
- 剩余明确项：`generic/074` 为 600s timeout 长尾（1800s 诊断 PASS），`generic/532` 为 `chattr`/FS_IOC flags 能力缺口；`generic/558` 及其后 case 仍需一轮更长 run 或从 `558` 续跑确认。

起点证据：

- runner 提交：`356f51279 Add official xfstests runner`
- 起点日志：`asterinas/benchmark/logs/official_20260612_082930.log`
- run 结果：`rc=1`
- 已完成：47 cases
- PASS：35 cases
- FAIL：12 cases
- 中断点：`generic/320`
- 中断症状：`Failed to allocate a large slot` / heap allocation error

## 1. 起点 official xfstests 结果

| 时间 | 分支/提交 | 日志 | 完成数 | PASS | FAIL | 中断/备注 |
|------|-----------|------|--------|------|------|-----------|
| 2026-06-12 | `feature-fixerror-phase-7` / `356f51279` | `asterinas/benchmark/logs/official_20260612_082930.log` | 47 | 35 | 12 | `generic/320` 期间 heap allocation error，full run 未完成 |
| 2026-06-13 | `feature-fixerror-phase-7` / working tree | `asterinas/benchmark/logs/official_20260613_032449.log` | 69 | 52 | 17 | 跑到 `generic/558` 时 kernel panic；`generic/014`/`027`/`030` 已恢复 PASS，说明全局 PageCache 退化已被撤销 |
| 2026-06-17 | `feature-fixerror-phase-7` / working tree | `asterinas/benchmark/logs/official_20260617_071904.log` | 3 | 3 | 0 | 小集合 `generic/014,027,030` 统一 PageCache 配置验证 PASS |
| 2026-06-17 | `feature-fixerror-phase-7` / working tree | `asterinas/benchmark/logs/official_20260617_072559.log` | 9 | 9 | 0 | mmap 相关集合 `141,246,248,340,344,354,428,437,438` 统一配置验证 PASS |
| 2026-06-17 | `feature-fixerror-phase-7` / working tree | `asterinas/benchmark/logs/official_20260617_121742.log` | 69 | 57/60 有效 | 3/60 有效 | 外层 21600s timeout 于 `generic/558` 前终止，无最终 summary；有效样例达 95.00%，FAIL=`074/452/532`，其中 `452` 后续已修 |
| 2026-06-18 | `feature-fixerror-phase-7` / working tree | `asterinas/benchmark/logs/official_20260618_113426.log` | 1 | 1 | 0 | `generic/452` 单测 PASS；busybox `ls` applet 复制执行问题已修 |

## 2. FAIL case 跟踪表

分类依据：plan §3 根因分析报告（2026-06-12），按根因簇（R/A/B/C/D/E/F/G）记账。

| Case | 起点状态 | 分类（簇） | 当前状态 | 根因摘要 | 修复提交/文件 | 验证 |
|------|----------|------|----------|----------|----------------|------|
| `ext4/042` | FAIL | C | 已修 | statfs 需区分 `minixdf`/`bsddf`，并使用 live superblock counters | `kernel/src/fs/ext4/fs.rs`、`kernel/libs/ext4_rs/src/ext4_impls/ext4.rs` | PASS：`official_20260612_143424.log` |
| `generic/030` | FAIL | B+F | 已修 | official 统一启用 mmap PageCache 能力，但普通 I/O 默认不走 PageCache；已有 mmap 状态时 read/write 切回 PageCache 保 coherency；last-close/umount 保守写回 resident mmap 页 | `tools/ext4/run_phase4_part3.sh`、`test/initramfs/src/syscall/xfstests/run_xfstests_test.sh`、`kernel/src/fs/ext4/fs.rs`、`kernel/src/fs/ext4/inode.rs`、`kernel/src/fs/utils/page_cache.rs` | PASS：`official_20260617_071416.log`、组合 PASS：`official_20260617_071904.log` |
| `generic/074` | PASS at 1800s / likely TIMEOUT at 600s | B/A/D | correctness 已修复（需守底） | PageCache panic/heap abort 已解除；`0a f3` extent-header 数据损坏在整树 truncate 修复后未再出现；该 case 是长 mmap/truncate 压力项，1800s 诊断跑 PASS，但 95% official 收敛仍采用 600s 标准 timeout | 待填 | corruption：`official_20260617_073751.log`；timeout：`official_20260617_095156.log`、`official_20260617_100940.log`；PASS：`official_20260617_102211.log` |
| `generic/141` | FAIL | B | 已修 | official 统一提供 mmap PageCache 能力，不再按 case 写 mount option 白名单 | `tools/ext4/run_phase4_part3.sh`、`kernel/src/fs/ext4/*`、`kernel/src/fs/utils/page_cache.rs` | PASS：`official_20260617_072559.log` |
| `generic/246` | FAIL | A+B+F | 已修（mmap 子因） | 起点 mmap ENODEV；统一 PageCache 配置后单 case 转绿，空间泄漏簇需继续在 full run 中观察 | 同上 | PASS：`official_20260617_072559.log` |
| `generic/248` | FAIL | A | 已修（mmap 子因） | 起点 mmap ENODEV；统一 PageCache 配置后单 case 转绿，空间泄漏簇需继续在 full run 中观察 | 同上 | PASS：`official_20260617_072559.log` |
| `generic/249` | FAIL | A | 已修/需守底 | 先前 TEST_DIR 永久 ENOSPC；full-order 下已恢复 PASS | ENOSPC 回滚/释放、umount ENOSPC detach 修复链 | PASS：`official_20260617_121742.log` |
| `generic/257` | FAIL | A | 已修/需守底 | 先前 TEST_DIR 永久 ENOSPC；full-order 下已恢复 PASS | 同上 | PASS：`official_20260617_121742.log` |
| `generic/273` | FAIL | D(+R) | 已修/需守底 | 先前受 `269` 后 scratch 残留挂载/ENOSPC 污染；umount ENOSPC 降级后恢复 PASS | `kernel/src/fs/ext4/fs.rs` | PASS：`official_20260617_121742.log` |
| `generic/275` | FAIL | C+D+F | 已修/需守底 | 先前受满盘后清理失败与工具链叠加影响；full-order 下恢复 PASS | ENOSPC detach + dd/statfs 等既有修复 | PASS：`official_20260617_121742.log` |
| `generic/309` | FAIL | A | 已修/需守底 | 先前 TEST_DIR 永久 ENOSPC 连锁；full-order 下恢复 PASS | 同上 | PASS：`official_20260617_121742.log` |
| `generic/313` | FAIL | A | 已修/需守底 | 先前 TEST_DIR 永久 ENOSPC；full-order 下恢复 PASS | 同上 | PASS：`official_20260617_121742.log` |
| `generic/320` | 未完成 | G(+D+R) | 已修/需守底 | 起点 heap abort；full-order 下已越过 `320` 并 PASS，说明主要是前置资源/满盘清理连锁 | page-cache writeback 封顶、umount ENOSPC detach 修复链 | PASS：`official_20260617_121742.log` |
| `generic/340`/`344`/`354`/`428`/`437`/`438` | 后半段新增 FAIL | B | 已修 | full rerun 暴露的 mmap ENODEV；统一 PageCache 配置后全部转绿，不再依赖按 case mount option 白名单 | `tools/ext4/run_phase4_part3.sh`、`kernel/src/fs/ext4/*`、`kernel/src/fs/utils/page_cache.rs` | PASS：`official_20260617_072559.log` |
| `generic/345`/`346` | 后半段新增 FAIL | B(+PageCache 并发) | 已修 | PageCache-on 后 page fault dirty 标记路径在 atomic 上下文等待 Mutex panic；`update_page` 改为非阻塞 dirty 标记，ext4 sync/close/evict 侧保守标脏 resident pages 兜底 | `kernel/src/fs/utils/page_cache.rs`、`kernel/src/fs/ext4/fs.rs` | PASS：`345`=`official_20260617_073309.log`；`346`=`official_20260617_073603.log` |
| `generic/452` | 后半段新增 FAIL | F | 已修 | `type -P ls` 取到 busybox，复制到 scratch 并改名为 `ls_on_scratch` 后 busybox 按 argv0 找 applet 失败 | `test/initramfs/src/syscall/xfstests/run_xfstests_test.sh` | PASS：`official_20260618_113426.log` |
| `generic/532` | 后半段新增 FAIL | F/E | 待修或确认 NOTRUN | `chattr +i/+a` 因 FS_IOC flags 不支持输出 `Inappropriate ioctl for device`，污染 golden output | 待填 | FAIL：`official_20260617_121742.log` |
| `generic/558` | 后半段未跑到/后续 panic | G | 记录纠正：待重新确认 | 先前 milestone 称“移除 close 回调阻塞 cleanup”与实际 diff 不符；当前未把 558 作为已修项，需在新 PageCache close flush 后重新单跑 | 待填 | 历史 TIMEOUT/no panic：`official_20260613_050405.log`，结论作废待复验 |
| `generic/214` | NOTRUN | A | 待修 | O_DIRECT 探针因 TEST_DIR ENOSPC 误报"不支持" | 待填 | 待填 |
| `generic/002`/`089`/`236` | NOTRUN | E | 待修 | hardlink 未实现（`Ext4Inode` 无 `link()`） | 待填 | 待填 |
| `generic/005`/`023`/`109` | NOTRUN | E | 待修 | symlink stage1 未实现（`inode.rs:509-511`） | 待填 | 待填 |
| `generic/312` | NOTRUN | E | 待修 | 需 5G scratch，runner 默认 2G | 待填 | 待填 |

## 3. Step 记录

### Step 0：Phase 7 立项与索引同步

状态：已完成

改动概要：

- 创建 `feature_fixerror_phase7_plan.md`
- 创建 `feature_fixerror_phase7_milestone.md`
- 同步 `CLAUDE.md` / `AGENTS.md` 当前阶段索引
- 同步 `asterinas/CLAUDE.md` / `asterinas/AGENTS.md` 仓库内入口
- 同步 `asterinas/docs/` 文档副本

涉及文件：

- `feature_fixerror_phase7_plan.md`
- `feature_fixerror_phase7_milestone.md`
- `CLAUDE.md`
- `AGENTS.md`
- `asterinas/docs/feature_fixerror_phase7_plan.md`
- `asterinas/docs/feature_fixerror_phase7_milestone.md`
- `asterinas/CLAUDE.md`
- `asterinas/AGENTS.md`

验证：

- 文档阶段，无内核代码改动。

### Step 1：runner 完整性与 `generic/320` 中断

状态：未开始

待办：

- 单独复现 `generic/320`
- 检查 full-run 累积资源、日志体积、heap allocation error 触发位置
- 明确是 runner 稳定性问题还是 ext4/kernel bug

记录：

| 时间 | 命令 | 日志 | 结果 | 结论 |
|------|------|------|------|------|
| 待填 | 待填 | 待填 | 待填 | 待填 |

### Step 2：配置/能力错配类 FAIL

状态：部分完成

候选：

- `generic/030`
- `ext4/042`
- `generic/141`

记录：

| 时间 | Case | 命令 | bad output 摘要 | 结论 | 验证 |
|------|------|------|-----------------|------|------|
| 2026-06-12 | `ext4/042` | `XFSTESTS_SINGLE_TEST=ext4/042 tools/ext4/run_official_xfstests.sh` | `minixdf`/`bsddf` 下 `f_blocks` 不符合预期 | 实现 mount option 解析；`minixdf` 返回总块数，默认/`bsddf` 扣除 system zone + journal overhead；使用 allocator live counters 返回 free blocks | PASS：`official_20260612_143424.log` |
| 2026-06-12 | `generic/141` | `XFSTESTS_SINGLE_TEST=generic/141 tools/ext4/run_official_xfstests.sh` | 起点为 mmap/pagecache 能力缺失 | official mode 启用 `ext4fs.page_cache=1` | PASS：`official_20260612_143521.log` |
| 2026-06-12 | `generic/030` | `XFSTESTS_SINGLE_TEST=generic/030 tools/ext4/run_official_xfstests.sh` | 先后暴露 `od -t x1z` 缺失、truncate/mmap 写 remount 后丢失 | 增加 GNU `od -Ax -t x1z` 兼容 shim；official mode 启用 PageCache；truncate 不再预先 flush/clean pagecache，避免 writable mmap 后续写不重新置 dirty | PASS：`official_20260612_142922.log` |

### Step 3：剩余 FAIL 根因簇修复

状态：进行中

记录：

| 时间 | 根因簇 | 覆盖 case | 修改文件 | 单 case 验证 | guard 验证 | 备注 |
|------|--------|-----------|----------|--------------|------------|------|
| 2026-06-13 | B：mmap/PageCache per-case enable | `030`、`141`、`246`、`248`、`340`、`344`、`354`、`428`、`437`、`438` | `test/initramfs/src/syscall/xfstests/run_xfstests_test.sh` | PASS：`030`=`official_20260613_005005.log`；`141/246/248/340/344`=`official_20260613_052453.log`；`354/428/437/438`=`official_20260613_052915.log` | `bash -n`、`git diff --check` PASS | official 默认保持 PageCache-off，避免 `014/027` 退化；仅安全 mmap case 通过 mount option 局部启用 |
| 2026-06-13 | G：close/drop atomic panic | `generic/558` | `kernel/src/fs/ext4/inode.rs` | 单跑不再 kernel panic，但外层 timeout：`official_20260613_050405.log` | `cargo check -p aster-kernel --target x86_64-unknown-none` PASS | 移除 close 回调中的阻塞 `cleanup_unlinked_file`；open-unlink 最终回收需后续异步化 |
| 2026-06-13 | B/G：PageCache-on 风险隔离 | `generic/074`、`345`、`346` | `test/initramfs/src/syscall/xfstests/run_xfstests_test.sh` | `074` PageCache-on heap abort：`official_20260613_053244.log`；`345` PageCache-on panic：`official_20260613_052453.log` | 待补 | 暂不放入白名单，避免 full official 被 panic/heap abort 截断；需要确认是否允许改通用 PageCache 或做 ext4 专用 pager |
| 2026-06-17 | B：去除 per-case PageCache 白名单 | `014`、`027`、`030`、`141`、`246`、`248`、`340`、`344`、`354`、`428`、`437`、`438` | `tools/ext4/run_phase4_part3.sh`、`tools/ext4/run_phase4_in_docker.sh`、`test/initramfs/src/syscall/xfstests/run_xfstests_test.sh`、`kernel/src/fs/ext4/fs.rs`、`kernel/src/fs/ext4/inode.rs` | `014/027/030` PASS：`official_20260617_071904.log`；mmap 9-case PASS：`official_20260617_072559.log` | `bash -n`、`git diff --check`、`cargo check -p aster-kernel --target x86_64-unknown-none` PASS | official 统一 `page_cache=1,page_cache_io=0`，不按 case 白名单；已有 mmap state 的 inode 走 PageCache 保 coherency |
| 2026-06-17 | B/G：mmap dirty tracking atomic panic | `generic/345`、`generic/346` | `kernel/src/fs/utils/page_cache.rs`、`kernel/src/fs/ext4/fs.rs` | PASS：`345`=`official_20260617_073309.log`；`346`=`official_20260617_073603.log` | `cargo check` PASS | `Pager::update_page` 在 page fault 中改非阻塞；ext4 flush/close/evict 前保守标脏 resident pages，补偿 missed dirty |
| 2026-06-17 | B→A/D：074 降级定位 | `generic/074` | 同上 | FAIL/no panic：`official_20260617_073751.log` | 待补 | 失败点已从 PageCache heap/panic 降级为 `fstest.2` 非 mmap hole-file 并发数据损坏，下一步按洞文件/并发写一致性排查 |
| 2026-06-17 | A/D：074 extent tree truncate 修复 | `generic/074`、`generic/030` | `kernel/libs/ext4_rs/src/ext4_impls/balloc.rs`、`kernel/libs/ext4_rs/src/ext4_impls/extents.rs`、`kernel/libs/ext4_rs/src/ext4_impls/file.rs`、`kernel/src/fs/ext4/fs.rs`、`tools/ext4/run_phase4_part3.sh`、`tools/ext4/run_phase4_in_docker.sh` | `030` PASS：`official_20260617_100520.log`；`074` 600s timeout：`official_20260617_095156.log`、`official_20260617_100940.log`；`074` 1200s timeout 但跑完 fstest.2/fstest.3 child loops：`official_20260617_092905.log`；`074` 1800s PASS：`official_20260617_102211.log` | `git diff --check`、`cargo check -p aster-kernel --target x86_64-unknown-none` PASS | `0a f3` 指向 extent leaf header 被当作数据读；新增 full-truncate 整树释放和批量 block free，修正分配器避免把本 inode extent metadata 当候选；当前 95% 策略恢复 600s 标准 timeout，不为慢 case 拉长 full run |
| 2026-06-17 | G：069 负结果诊断 | `generic/069` | 无保留代码改动 | `069` 诊断：`official_20260617_110011.log`/`110156.log` 暴露 `page_cache_io=1` 下 close atomic panic；临时 close no-op 又导致 `030` post-remount 数据丢失（`official_20260617_111842.log`），已撤回；`030` 恢复 PASS：`official_20260617_112357.log` | `cargo check`、`git diff --check` PASS | 结论：不为 069 牺牲 mmap close 写回语义；069 作为 3,000,000 次 4-byte O_APPEND 慢 case，按 95% 策略低优先级 |

### Step 4：full official 收敛重跑

状态：进行中

记录：

| 时间 | 提交 | 日志 | 总数 | PASS | FAIL | NOTRUN/SKIP | 结论 |
|------|------|------|------|------|------|-------------|------|
| 2026-06-17 | working tree | `official_20260617_121742.log` | 69 done before outer timeout | 57/60 valid | 3/60 valid | 9 NOTRUN | `269` 共因修复有效，full-order 有效通过率 95.00%；外层 run timeout 于 `generic/558` 前终止，需续跑/加长 run timeout 补完整 summary |
| 2026-06-18 | working tree | `official_20260618_113426.log` | 1 | 1 | 0 | 0 | `generic/452` 单测 PASS；折算已完成集合约 58/60 valid PASS=96.67% |

### Step 5：守底回归

状态：未开始

| 类别 | 入口 | 最近结果 | 日志 | 备注 |
|------|------|----------|------|------|
| Official xfstests | `tools/ext4/run_official_xfstests.sh` | 待填 | 待填 | full run |
| 修复单 case | `XFSTESTS_SINGLE_TEST=...` | 待填 | 待填 | 修过的 case |
| Phase 4 PageCache | `PHASE4_DOCKER_MODE=pagecache_phase4` | 待填 | 待填 | mmap/PageCache 相关改动必跑 |
| Phase 6 guard | `PHASE4_DOCKER_MODE=phase6_with_guard` | 待填 | 待填 | 必跑 |
| 并发守底 | `RUN_PHASE2_CONCURRENCY=1` / `PHASE4_DOCKER_MODE=concurrency` | 待填 | 待填 | 触及锁/并发必跑 |
| JBD2/crash | `jbd_phase1` + crash matrix | 待填 | 待填 | 触及 journal 必跑 |
| fsync/flush | Phase 3 fsync/flush + host-crash fsync | 待填 | 待填 | 触及持久化必跑 |
| fio O_DIRECT | Phase 6 fio guard | 待填 | 待填 | 不低于既有红线 |
| SQLite | speedtest1 / `integrity_check` | 待填 | 待填 | 触及写回/PageCache/journal 必跑 |

## 4. 变更日志

| 日期 | Step | 改动概要 | 涉及文件 | 测试结果 | 备注 |
|------|------|----------|----------|----------|------|
| 2026-06-12 | Step 0 | Phase 7 立项，创建 plan/milestone 模板并同步根目录与仓库内索引 | `feature_fixerror_phase7_*`、`AGENTS.md`、`CLAUDE.md`、`asterinas/AGENTS.md`、`asterinas/CLAUDE.md`、`asterinas/docs/feature_fixerror_phase7_*` | 文档改动，无内核测试；模板副本一致性检查通过 | 起点来自 `official_20260612_082930.log` |
| 2026-06-12 | Step 2 | 修复 statfs 口径、official PageCache 开关、`od -t x1z` shim、truncate/mmap dirty 保持 | `kernel/src/fs/ext4/fs.rs`、`kernel/libs/ext4_rs/src/ext4_impls/ext4.rs`、`tools/ext4/run_phase4_part3.sh`、`test/initramfs/src/syscall/xfstests/run_xfstests_test.sh` | `ext4/042` PASS、`generic/030` PASS、`generic/141` PASS；`cargo check -p ext4_rs` PASS；`git diff --check` PASS | `generic/074` 已从 mmap ENODEV 变为 timeout/perf 类，仍待修 |
| 2026-06-13 | Step 3 | 撤销 official 全局 PageCache，改为安全 mmap case 局部 mount option；修正 `MOUNT_OPTIONS` quoting；避免 close/drop atomic 路径阻塞 cleanup | `test/initramfs/src/syscall/xfstests/run_xfstests_test.sh`、`kernel/src/fs/ext4/inode.rs`、`feature_fixerror_phase7_plan.md`、`feature_fixerror_phase7_milestone.md`、`asterinas/docs/feature_fixerror_phase7_*` | `014/027/030` 小集合 PASS：`official_20260613_005333.log`；mmap 子集 PASS：`official_20260613_052453.log`、`official_20260613_052915.log`；`558` 不再 panic 但 timeout：`official_20260613_050405.log` | `074/345/346` PageCache-on 会触发更底层风险，已隔离并记录为需确认项 |
| 2026-06-17 | Step 3 | 按 DeepSeek V4 Pro review 去掉 per-case PageCache 白名单，改为 official 统一 `page_cache=1,page_cache_io=0`；修复 mmap/普通 read coherency、mmap dirty close/sync 写回、PageCache update_page atomic panic | `kernel/src/fs/ext4/fs.rs`、`kernel/src/fs/ext4/inode.rs`、`kernel/src/fs/utils/page_cache.rs`、`tools/ext4/run_phase4_part3.sh`、`tools/ext4/run_phase4_in_docker.sh`、`test/initramfs/src/syscall/xfstests/run_xfstests_test.sh` | `014/027/030` 3/3 PASS：`official_20260617_071904.log`；mmap 9-case 9/9 PASS：`official_20260617_072559.log`；`345`/`346` PASS：`official_20260617_073309.log`/`073603.log`；`074` no panic but FAIL：`official_20260617_073751.log` | 已消除 per-case PageCache 绕测嫌疑；`074` 转入洞文件/并发数据损坏调查 |
| 2026-06-17 | Step 3 | 修复 `074` 中 O_TRUNC 后 extent tree 残留/元数据块读作数据的风险；新增 truncate-to-empty 整树释放、批量 free runs，并将 nlink=0 unlink/close 的 PageCache 处理改为 discard；`flush_all` 不再强制全文件标脏 | `kernel/libs/ext4_rs/src/ext4_impls/balloc.rs`、`kernel/libs/ext4_rs/src/ext4_impls/extents.rs`、`kernel/libs/ext4_rs/src/ext4_impls/file.rs`、`kernel/src/fs/ext4/fs.rs`、`tools/ext4/run_phase4_part3.sh`、`tools/ext4/run_phase4_in_docker.sh` | `030` PASS：`official_20260617_100520.log`；`074` 1800s 诊断跑 PASS：`official_20260617_102211.log` | 当前 `074` correctness 收口；95% 收敛恢复 600s case timeout，仍需跑 full official 和 PageCache/Phase6/crash/fsync 守底 |
| 2026-06-18 | Step 4 | 满盘 fs-wide sync ENOSPC 降级，避免 `generic/269` 后 umount 失败导致 scratch 残留挂载并污染后续 case | `kernel/src/fs/ext4/fs.rs` | `generic/269` 单测 PASS：`official_20260617_121256.log`；partial full：`official_20260617_121742.log` 中 `269/273/275/308/309/313/320/...` 恢复 PASS/NOTRUN 正常分类 | `cargo check -p aster-kernel --target x86_64-unknown-none` PASS；full 未生成最终 summary（outer timeout） |
| 2026-06-18 | Step 4/F | 增加可复制执行的 `ls` shim，避免 busybox 被复制为 `ls_on_scratch` 后 applet argv0 失效 | `test/initramfs/src/syscall/xfstests/run_xfstests_test.sh` | `generic/452` PASS：`official_20260618_113426.log` | `bash -n`、`git diff --check` PASS |

## 5. 开放问题

| 问题 | 当前判断 | 下一步 |
|------|----------|--------|
| `generic/320` 是真实内核 bug 还是 full-run 资源累积问题？ | 更像前置满盘/umount/资源累积的连锁问题；`official_20260617_121742.log` 已在 full-order 下 PASS | 续跑 `558` 以后 case，或加长 run timeout 跑完整 official summary |
| `generic/030` 是否因 official mode 未启用 PageCache/mmap 支持？ | 已确认并修复；后续又暴露 truncate/mmap dirty 语义问题，已修复 | 后续跑 full official 验证是否牵连 `generic/246` |
| `generic/074`/`345`/`346` 是否允许触及通用 PageCache？ | 已按赛题正确性优先原则做最小通用 PageCache 修复：`update_page` 非阻塞；`345/346` 已 PASS，`074` 在 1800s 诊断 timeout 下 PASS；95% official 收敛恢复 600s timeout | 补 Phase 4/Phase 6/PageCache/crash/fsync 守底，并跑 full official |
| `generic/532` 如何处理？ | 当前是 chattr/FS_IOC flags 能力缺口；不应靠过滤 stdout 假过 | 实现最小 FS_IOC_GETFLAGS/SETFLAGS 或确认该能力不在赛题要求后让 case NOTRUN |
| open-unlink close 后最终回收如何恢复？ | close 回调中的同步 cleanup 会在 atomic drop 路径 panic，已移除 | 设计 ext4 异步 orphan cleanup 队列，避免阻塞 fd drop 路径 |
| 是否有官方 list 中赛题不要求支持的能力项？ | 不先假设 | 逐 case 对照 `赛题要求.md`，必要时人工确认 |
