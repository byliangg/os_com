# Asterinas ext4 功能正确性 Phase 7 — 计划（Official xfstests 错误修复主线）

创建时间：2026-06-12
根因分析报告：2026-06-12（基于起点日志 + 代码联合分析，见 §3）
工作分支：`feature-fixerror-phase-7`
配套进度：`feature_fixerror_phase7_milestone.md`

## 0. 阶段定位

Phase 7 承接 Phase 6 收口后的功能正确性修复工作。Phase 6 已完成 SQLite 真实应用写优化、fio O_DIRECT 守底、并发守底与 crash/host-crash 回归；Phase 7 不再扩大性能优化范围，主目标是把官方功能测试暴露出的 ext4 错误逐项修掉。

本阶段起点来自官方 xfstests runner：

- runner 提交：`356f51279 Add official xfstests runner`
- 当前分支：`feature-fixerror-phase-7`
- 起点日志：`asterinas/benchmark/logs/official_20260612_082930.log`（26MB / 128,920 行，其中 127,463 行是内核 ENOSPC ERROR 刷屏）
- 执行结果：run `rc=1`，55 个 case 启动（35 PASS / 12 FAIL / 8 NOTRUN），执行到 `generic/320` 时出现 `Failed to allocate a large slot` / heap allocation error（`ostd/src/mm/heap/slot.rs:89`，188KB 连续分配失败），日志在此截断，official.list 共 82 个 case，**后 35 个（generic/320–708）从未运行**。

## 1. 目标

### 1.1 必达目标

1. official xfstests runner 能稳定跑完整个 `official.list`，不因 kernel heap allocation error、runner 超时、日志爆炸或宿主端资源问题提前中断。
2. 起点已暴露的 12 个 FAIL 逐项修复或形成明确、可复现、符合赛题要求的不可支持说明。
3. 8 个 NOTRUN 逐个甄别：真实能力缺口（实现它或形成正式说明）vs 探针误判（修探针/环境）。**`generic/214` 的 "O_DIRECT is not supported" 与 fio O_DIRECT 天天在跑直接矛盾，已确认是被前序 case 毒化（见 §3.4），必须修复而非接受。**
4. 修复后不回退既有守底：JBD2、crash recovery、fsync/flush、PageCache、concurrency、SQLite integrity 与 fio O_DIRECT。
5. 每个修复都保留单 case 复现证据、bad output 摘要、根因、涉及文件、回归结果。

### 1.2 非目标

- 不以牺牲 Phase 6 性能成果换取 testcase 通过。
- 不为了绕过 FAIL 随意扩大 `official.list` 排除项（当前 `blocked/official_excluded.tsv` 为空，必须保持）；若确需排除，必须先对照 `赛题要求.md` 并人工确认。
- 不在没有单 case 复现和 bad output 的情况下做大范围重构。

## 2. 工作方法

### 2.1 复现入口

全量 official：

```bash
cd /home/lby/os_com_codex/asterinas
tools/ext4/run_official_xfstests.sh
```

单 case：

```bash
cd /home/lby/os_com_codex/asterinas
XFSTESTS_SINGLE_TEST=generic/030 tools/ext4/run_official_xfstests.sh
```

常用环境变量：`ENABLE_KVM=1`、`XFSTESTS_CASE_TIMEOUT_SEC=600`、`OFFICIAL_THRESHOLD=100`。

**宿主侧 e2fsck 取证（本阶段新增的关键手段）**：TEST/SCRATCH 镜像是宿主文件（`test/initramfs/build/ext2.img` = TEST_DEV(/dev/vda)、`test/initramfs/build/exfat.img` = SCRATCH_DEV(/dev/vdb)，见 `tools/ext4/run_phase4_part3.sh:231-234`）。任意单 case 跑完后在宿主执行：

```bash
e2fsck -nf test/initramfs/build/ext2.img    # TEST 设备
e2fsck -nf test/initramfs/build/exfat.img   # SCRATCH 设备
```

e2fsck 的 "Free blocks count wrong"（计数错）与 "Unattached/orphan blocks"（不可达块）输出能直接区分"计数维护 bug"与"块真泄漏"，是 §3 多个根因簇的判别证据。

### 2.2 修复原则

1. 先看 bad output / full log，再看代码；本计划 §3 已给出代码级根因或候选机制，执行时仍须用单 case + e2fsck 验证后再动手。
2. 每次只修一个根因簇；如果多个 case 共享根因，统一记录到 milestone。
3. 触及持久化、writeback、journal、PageCache coherency 的改动必须追加 crash / fsync / SQLite integrity 守底。
4. 触及 VFS/stat/mmap 的改动必须至少跑相关单 case + Phase 4 PageCache 守底。
5. runner 或测试环境问题要和内核语义问题分开记录，避免把 harness 修复误记成 ext4 修复。
6. 涉及 inode/块回收（§3.3）的改动是本阶段风险最高的部分，必须小步提交、每步全守底。

## 3. 起点根因分析报告（2026-06-12）

证据来源：起点日志全文、xfstests 测试源码（`asterinas/.local/xfstests-src/tests/`）、内核与 ext4_rs 源码。下文所有 `file:line` 均已人工核对。

### 3.0 总览：12 FAIL + 8 NOTRUN + 1 中断 → 7 个根因簇

| 簇 | 性质 | 覆盖 case | 置信度 |
|----|------|-----------|--------|
| R：runner/平台前置（umount-by-device 失败、mount 内省失效） | harness+内核 | 间接影响所有 case 的 scratch 生命周期 | 已实锤 |
| A：空间永久泄漏 / orphan 清理缺失 | 内核 ext4 | `generic/246`、`248`、`249`、`257`、`309`、`313`（6 FAIL）+ `generic/214` NOTRUN | 已实锤（机制有 5 处，触发主链见 §3.4） |
| B：mmap ENODEV（official 跑在 page_cache=0） | 配置错配 | `generic/030`、`074`、`141`（3 FAIL，246 也叠加） | 已实锤 |
| C：statfs 冻结快照 + f_blocks 不扣 overhead | 内核 ext4 | `ext4/042`、`generic/275`（部分） | 已实锤 |
| D：分配器过早/持久 ENOSPC | 内核 ext4_rs | `generic/275`（主因）、`273`、`320`（ENOSPC 部分） | 机制候选已列出，需判别实验 |
| E：能力缺口 NOTRUN | 功能缺失 | `generic/002`、`089`、`236`（hardlink）、`005`、`023`、`109`（symlink）、`312`（scratch 2G < 5G） | 已实锤 |
| F：guest 工具链（busybox od/dd 缺特性） | 环境 | `generic/030`、`275`（即使内核修好也会因输出差异 FAIL） | 已实锤 |
| G：generic/320 中断（内核 heap 188KB 分配失败） | 内核内存 | full run 完整性 | 次生于 A/D 的概率大，需单跑判别 |

### 3.1 簇 R：umount-by-device 失败贯穿全程，scratch 生命周期被破坏

**日志证据**：`umount: can't unmount /dev/vdb: Invalid argument` 出现 16 次（含一次 `/dev/vda`）；run 开头 `xfstests probe: shim grep rc=1 ... shim findmnt rc=1`；所有 fsstate 行 `TEST_DEV_MOUNTS='<none>' SCRATCH_MOUNTS='<none>'`；整个日志看不到任何一次 e2fsck 实际运行的输出。

**代码定位**：
- `asterinas/kernel/src/syscall/umount.rs:10-41`：`sys_umount` 把参数当路径解析后对其 `get_top_path().unmount()`。传入 `/dev/vdb` 时解析到 devfs 设备节点（不是挂载点）→ EINVAL。Linux 语义是 umount 设备路径会先解析为对应挂载点。
- xfstests `_scratch_unmount` 用的就是设备路径（`umount $SCRATCH_DEV`）。busybox umount 会先查 `/etc/mtab`（runner 已 symlink 到 `/proc/mounts`，`run_xfstests_test.sh:62-64`）把设备翻译成挂载点，但 `/proc/mounts` 的 source 字段不是 `/dev/vdb`（`kernel/src/fs/procfs/pid/task/mounts.rs:86`：`mount.source().unwrap_or("none")`，ext4 mount 的 source 记录情况待验证），翻译失败 → 裸设备路径直传 umount(2) → EINVAL。

**后果链**（这是为什么它必须最先修）：
1. 每个 case 结束 scratch 卸不掉 → 下一个 case 的 `_scratch_mkfs` 在**带活挂载**的设备上 mkfs → 旧挂载实例的内存元数据与新 fs 不一致，旧实例的延迟写回/缓存可能污染新 fs（§3.6 簇 D 的候选机制之一）。
2. 每次 `_scratch_mount` 叠加新挂载，旧 `Ext4Fs` 实例（含 inode 元数据缓存、extent map 缓存、PageCache 状态、correctness 锁表）永不释放 → 长 run 内存只增不减（§3.9 簇 G 的候选机制之一）。
3. xfstests check 每 case 后的 `_check_filesystems`（e2fsck）需要先 unmount，卸不掉 → fsck 静默跳过 → **35 个 PASS 没有 fsck 背书，可信度打折**。
4. `findmnt`/`grep /proc/mounts` 探针失效 → `_require_scratch` 之类的前置检查行为不可信。

**修复方案**（两层，都做）：
- 内核：`sys_umount` 支持设备路径——解析结果若是块设备节点，反查挂载表找到以该设备为 source 的最顶层挂载点再 unmount（对齐 Linux）。同时核查 ext4 mount 是否把 source 设备名记入 `mount.source()`（`/proc/mounts` 第一列应显示 `/dev/vdb`），让 busybox/findmnt 的标准路径也能工作。
- runner 兜底（先行，低风险）：在 `run_xfstests_test.sh` 的 `SHIM_DIR`（已在 PATH 最前，line 1089）放一个 `umount` shim：参数是 `/dev/vda`、`/dev/vdb` 时翻译成 `/ext4_test`、`/ext4_scratch` 再调真 umount。这一步可以立刻恢复 scratch 生命周期，先于内核修复生效。

**验证**：单 case 日志中 `umount` 不再报 Invalid argument；fsstate 行能看到挂载状态变化；e2fsck（guest 内 check 的 `_check_filesystems` 或宿主侧 §2.1 方法）开始产生输出。

### 3.2 簇 B：official 模式跑在 `page_cache=0`，mmap 必然 ENODEV（030/074/141，246 叠加）

**日志证据**：四个 case 的 bad output 都有 `mmap: No such device`（xfs_io 的 mmap 命令失败，errno=ENODEV）。`generic/074` 的非 mmap 子项全 PASS，到 `-m`（mmap 模式）子项 child exit 1，症状指向性极强。

**代码定位（完整因果链）**：
1. `tools/ext4/run_phase4_part3.sh:223-226`：`ext4_page_cache=0`，只有 `mode = pagecache_phase4` 才是 1 → official 模式内核启动参数是 `ext4fs.page_cache=0`。
2. `kernel/src/fs/ext4/fs.rs:1678-1680`：`page_cache_enabled_from_kcmdline` 默认 false。
3. `kernel/src/fs/ext4/inode.rs:292-301`：`page_cache()` 在 `!fs.page_cache_enabled()` 时返回 `None`。
4. `kernel/src/fs/inode_handle.rs:355-364`：`page_cache()` 为 `None` → `return_errno!(ENODEV, "the file is not mappable")`。

**2026-06-17 修正后的方案**：放弃按 case 名选择 mount option 的白名单做法，避免“runner 识别测试名再调配置”的绕测嫌疑。official 统一启动 `ext4fs.page_cache=1,ext4fs.page_cache_io=0`：

- `page_cache=1`：所有 official case 都具备 mmap/PageCache 能力。
- `page_cache_io=0`：普通 buffered read/write 默认仍走 ext4 原路径，避免 `generic/014`/`027`/`030` 这类基础压力用例被全局 PageCache I/O 慢路径拖垮。
- 一旦 inode 已存在 mmap PageCache state，普通 read/write 自动切到 PageCache 路径，保证 mmap 与普通 I/O coherency。
- ext4 sync/last-close/evict 前保守标脏 resident PageCache pages，补偿共享 mmap 写入没有再次触发 write fault 的 dirty tracking 缺口。
- `Pager::update_page` 改为非阻塞 dirty 标记，避免 page fault atomic 上下文等待 `Mutex` 导致 `generic/345`/`346` panic。

已验证：`generic/014,027,030` 组合 PASS（`official_20260617_071904.log`）；`generic/141,246,248,340,344,354,428,437,438` PASS（`official_20260617_072559.log`）；`generic/345`/`346` PASS（`official_20260617_073309.log`、`official_20260617_073603.log`）。`generic/074` 已不再 heap/panic；`0a f3` extent-header 数据损坏经 O_TRUNC 整树释放修复后未再复现，1800s 诊断跑 PASS（`official_20260617_102211.log`）。当前按 95% 目标恢复 600s official case timeout，优先修短平快 FAIL，不继续用长 timeout 追慢 case。

**风险与守底**：这已经触及通用 PageCache dirty 标记和 ext4 writeback/close 语义，必须补跑 PageCache Phase 4、Phase 6 guard、SQLite integrity、fsync/flush/crash 守底；`page_cache_io=0` 不改变 fio/Phase 6 O_DIRECT 宣传口径。

### 3.3 簇 A：空间永久泄漏——orphan 清理缺失 + 多个释放路径断链（246/248/249/257/309/313）

**日志证据**：时间线上 `generic/246`（约 1657 行）之前所有写 TEST_DIR 的 case 正常；从 246 起**每一个**碰 TEST_DIR 的写操作（echo 几十字节、pwrite、mkdir、touch）全部 ENOSPC，持续到 run 结束。TEST_DEV 只在 runner 启动时 mkfs 一次（`run_xfstests_test.sh:152-155`），全程被 55 个 case 共用——一旦泄漏就是永久的。

**代码级泄漏点（5 处，全部人工核对）**：

1. **inode 永不回收**：`kernel/libs/ext4_rs/src/ext4_impls/ext4.rs:220-268` `unlink()` 中 `free_child` 初始化为 `false` 后从未置 true，行 261-265 的 `ialloc_free_inode` 是死代码。注释明说 "We currently do not have close-time orphan cleanup, so avoid immediate inode bitmap recycle"——递延变成了永不。
2. **open-unlink 的文件块永不回收**：VFS 只在 unlink 当下机会主义清理一次（`kernel/src/fs/path/dentry.rs:410-429`：`nlinks==0 && Arc::strong_count==1` 才调 `cleanup_unlinked()`）；`cleanup_unlinked_file`（`kernel/src/fs/ext4/fs.rs:4896-4920`）遇到 `has_open_file_handles` 直接返回；而最后一个 fd 关闭时 `on_close_file_handle`（fs.rs:4877-4887）只递减计数，**没有任何补偿清理**。fsstress（generic/013/014 已 PASS 但在泄漏）大量使用 open-unlink 模式。
3. **rmdir 的目录块+inode 永不回收**：`unlink()` 的 is_dir 分支（ext4.rs:231-244）只把 nlink 置 0 写回，目录自身的数据块从未释放；`rmdir_at`（fs.rs:4922-4960）调完 `ext4_rmdir_at` 后也没有任何回收动作。
4. **rename 覆盖普通文件不截断**：`kernel/libs/ext4_rs/src/simple_interface/mod.rs:353-359`，覆盖非目录时直接 `unlink` 被覆盖者，不像目录分支（349 行）那样先 `truncate_inode(0)`；除非 VFS 层 rename 对被覆盖 inode 另有 cleanup 调用（**待验证**），否则被覆盖文件的块全漏。
5. **fallocate/写路径 ENOSPC 无回滚**：`allocate_range`（`kernel/libs/ext4_rs/src/ext4_impls/file.rs:1415-1485`）里 `ensure_write_range_mapped(...)?` 失败直接上抛，已在位图里置位但尚未挂进 extent 树的块没有任何回收；日志中 `[convert_unwritten_span] written piece insert failed` ×3 是同族证据（位图已分配、extent 插入失败 → 块从此不可达，连 truncate 都救不回）。

**触发主链（高置信推断，需单 case 验证）**：`generic/213`（PASS）在 TEST_DIR 上做 `fallocate len=3441631232`（3.2GB > 2GB 盘），日志 971.231s 显示它真的 grind 到 ENOSPC 才失败——若失败路径不回滚（泄漏点 5）+ 删除文件时不可达块救不回，TEST_DEV 从此接近全满。下一个 case `generic/214` 的 `_require_odirect` 探针（xfstests `common/rc:3155`，本质是 `xfs_io -d pwrite 0 20k`）因 ENOSPC 失败 → 被误报成 "O_DIRECT is not supported" NOTRUN。再后面 246/248/249/257/309/313 凡写 TEST_DIR 全灭。**generic/213 单跑 + 宿主 e2fsck TEST 镜像，"Free blocks count wrong / unattached blocks" 的具体数字可以一次性证实或证伪这条链。**

**修复方案**（按风险递增排序，逐步做）：
1. fallocate/写路径 ENOSPC 回滚：审计 `ensure_write_range_mapped` 与 `balloc_alloc_block_batch`（`balloc.rs:714-`）所有部分失败路径，未挂树的块当场 `balloc_free_blocks` 还回去。低风险，独立可验证（单跑 213 + e2fsck 干净）。
2. close 时 orphan 补偿：`on_close_file_handle` 计数归零时检查 `nlink==0 && S_IFREG` → 触发与 `cleanup_unlinked_file` 相同的 truncate(0) 清理。注意锁序：先放 `open_file_handles` 锁再拿 inode correctness 锁。
3. rename 覆盖路径：非目录覆盖在 nlink 归零时补 `truncate_inode(0)`（或确认 VFS 层会对被覆盖 inode 调 cleanup，二选一，不要双重释放）。
4. rmdir 目录块回收：rmdir 成功后对子目录 inode `truncate_inode(0)`。
5. inode 位图回收（**风险最高，放最后**）：在上述 1-4 稳定后，把 `ialloc_free_inode` 接回 "nlink==0 且无打开句柄且完成 truncate" 的收尾点（含 `cleanup_unlinked_file` 和 close 补偿路径），同时回收 `free_inodes_count`。原注释担心的 inode 复用竞态要用现有 `open_file_handles` + correctness 锁封住；做完必须全量跑 crash matrix + fsstress 单 case（013/014）+ SQLite integrity。

**判别/验收**：修复每步后单跑 `generic/213`，宿主 `e2fsck -nf` TEST 镜像必须干净；最终 246/248/249/257/309/313 六个 case 在 full run（不重 mkfs TEST_DEV）里全 PASS，`generic/214` 不再 NOTRUN。

### 3.4 簇 C：statfs 是 mount 时的冻结快照，且 f_blocks 不扣 overhead（ext4/042、275 部分）

**日志证据**：
- `ext4/042`：`bsd f_blocks has value of 524288, NOT in range 493150.68..503113.32` —— f_blocks 返回裸设备块数。
- `generic/275`：dd 实际写到 ENOSPC 后，df 显示 Used=104624K（恰好是 fresh-mkfs 基线）且 rm 前后纹丝不动 → "could not sufficiently fill filesystem"。

**代码定位**：
- `kernel/src/fs/ext4/fs.rs:6177-6199` `sb()`：读 `self.lock_inner().super_block` —— 这是 mount 时加载进 `Ext4` 结构体的快照字段；而分配/释放路径维护的是另一份 `allocator_locks.lock_superblock_counter()`（`ext4_rs/src/ext4_impls/ext4.rs:14-42`，`ext4_defs/ext4.rs:35`）。两者从不同步（fs.rs 全文只有 needs_recovery 标志会写回 inner.super_block），所以 df 永远显示 mount 时的值。
- `blocks` 直接取 `blocks_count()`（fs.rs:6180），未减 overhead；Linux ext4 的 `statfs` 返回 `blocks_count - s_overhead`（每组元数据 + journal）。`bavail == bfree`（fs.rs:6191）也没扣 root 保留块，顺手一起对齐。

**修复方案**：
1. `sb()` 的 bfree/ffree 改从 `lock_superblock_counter()` 读（ext4_rs 暴露一个只读访问方法）。
2. mount 时一次性计算 overhead（复用 `ext4_rs` 已有的 `num_base_meta_blocks` / `get_system_zone` 几何逻辑 + journal inode 块数），`blocks = blocks_count - overhead`；`bavail = bfree - r_blocks_count`。
3. 对照验证：同一镜像 Linux loop mount 的 `df -k` / `stat -f` 输出与我们一致（容差 ±1%）。

**验收**：`ext4/042` PASS；`generic/275` 的 df 随写入/删除变动（275 完全 PASS 还需簇 D + 簇 F）。

### 3.5 簇 D：分配器过早且持久的 ENOSPC（275 主因、273、320 的 ENOSPC 部分）——未定癥，先做判别实验

**日志证据（含金量最高的一条）**：`generic/275` 在**新 mkfs 的 2GB scratch**上，单线程 `dd bs=1M` 只写了 253MB 就 ENOSPC，且之后连 4K 都分配不出（tmp3 "0+0 records out"）——过早、持久、单线程即可触发。`generic/273`（50 并发 porter cp）和 `generic/320`（100 并发 worker cp，2/3 盘容量的拷贝压力）大面积 `No free blocks available in all block groups`（`balloc.rs:308/417/460/581`）。

**已排除**：alloc_guard 是 operation-scoped、finish 即清（`alloc_guard.rs:106-121`），单线程不解释 275；块组描述符写回是 64B 精确偏移（`block_group.rs:180-188`），无整块互踩。

**候选机制（按嫌疑排序，留给判别实验定癥）**：
1. **簇 R 的 mkfs-over-live-mount 污染**：scratch 卸不掉（§3.1），`_scratch_mkfs_sized` 在带活挂载的设备上重建，旧挂载实例的延迟元数据写回 / JournalIoBridge overlay（fs.rs:1163-1227）把旧世界的"满"组描述符写回新 fs → 分配器每次 `Ext4BlockGroup::load_new` 直读设备（balloc.rs:297/449/751）看到假满。**如果簇 R 修完 275 就好了，此机制实锤。**
2. **组描述符 free count 维护错误**：`balloc_alloc_block_batch` 的 `max_to_find = min(remaining, free_blocks)`（balloc.rs:776）依赖 desc free count 的正确性；任何一次多扣（或释放路径少加）都会让组被永久跳过（`free_blocks==0` 即 skip，balloc.rs:301/453/756），且 statfs 冻结（簇 C）使问题不可见。
3. **泄漏点 5（§3.3）在写路径上的体现**：extent 插入失败丢块，叠加 dd 大批量分配，位图实占远超文件实长。
4. 并发毒点（只影响 273/320，不影响 275）：`balloc_alloc_block_from` 的 find_clr 命中 alloc_guard 持有块时直接跳整组（balloc.rs:548-573 fallthrough 到 576），高并发下所有组都可能被跳 → 假 ENOSPC。

**判别实验（执行 agent 第一批活，先于任何分配器改动）**：
1. 修完簇 R 的 umount shim 后单跑 `generic/275` → 若 PASS，机制 1 实锤，分配器本体无罪。
2. 仍 FAIL 则加诊断（kcmdline 门控 `ext4fs.balloc_debug=1`）：balloc 四个 ENOSPC return 点触发时，dump 全部 16 个组的 desc free count + 位图 popcount + superblock counter + alloc_guard stats（`debug_stats()` 已有）。一次单跑直接分辨"desc 说 0 但位图有空"（机制 2）vs"位图真满"（机制 3）。
3. 宿主 `e2fsck -nf` scratch 镜像，看 "Free blocks count wrong" 的方向和数量。

**修复方案**：按实验结论修对应机制；机制 4 无论如何都该修（find_clr 跳过被 guard 的位后应继续在本组内搜索，而不是放弃整组）。

### 3.6 簇 E：能力缺口 NOTRUN——hardlink、symlink、scratch 尺寸

**证据与定位**：
- hardlink（002/089/236 NOTRUN）：`Ext4Inode` 没有实现 `link()`，落到 trait 默认 `Err(ENOTDIR)`（`kernel/src/fs/utils/inode.rs:332-334`）→ xfstests 探针判 "No hardlink support"。真实缺口。
- symlink（005/023/109 NOTRUN）：`inode.rs:509-511` `read_link` 显式 `EOPNOTSUPP "symlink is not supported in stage1"`；`create()` 对 SymLink 类型也拒绝（inode.rs:383-385）。真实缺口。
- `generic/312` NOTRUN "Scratch device too small"：测试需要 5GB（`tests/generic/312:22` `fssize=$((2**30 * 5))`），runner 默认 `XFSTESTS_SCRATCH_IMG_SIZE=2G`（`run_phase4_in_docker.sh:64`）。
- 注意：未跑的 35 个 case（generic/339–708）里还有更多依赖 symlink/hardlink/xattr 的，修簇 E 同时解锁它们。

**与赛题对照**：`赛题要求.md` 的 POSIX 接口清单（create/open/close/read/write/truncate/lseek/mkdir/rmdir/unlink/rename/stat 等）没有点名 link/symlink，但 official.list 包含这些 case 且目标是全部通过 → **建议实现**（工作量评估：hardlink 小——`dir_add_entry` + links_count++ + journal 包装；symlink 中——ext4 fast symlink（target ≤60B 存 i_block）+ 长 symlink 数据块 + `read_link`/`write_link`/`create(SymLink)` 三个 VFS 入口；ext4_rs 的 dirent 层已认识 `EXT4_DE_SYMLINK`，`dir.rs:267`、`inode.rs:663`）。若人工决定不做，必须在 milestone 写正式的不支持说明并对照赛题确认。

**scratch 尺寸**：`XFSTESTS_SCRATCH_IMG_SIZE` 提到 8G（宿主镜像是 sparse 文件，成本可忽略），同步确认 `XFSTESTS_TEST_IMG_SIZE` 是否也该加大（TEST_DEV 2G 在簇 A 修复后理论够用，但 8G 提供裕量）。

### 3.7 簇 F：guest 工具链缺口——busybox od/dd 不支持 xfstests 依赖的参数

**证据**：
- `generic/030`/`246` 的 golden output 依赖 `od -t x1z`，busybox od 报 `od: invalid type string 'x1z'`。
- `generic/275` 的 `dd oflag=sync` 报 `dd: invalid argument 'sync' to 'oflag'`。

这两个 case 即使内核全修好也会因输出差异 FAIL。**这不是内核 bug，但也不能 skip，必须修镜像。**

**修复方案**：把 GNU coreutils 的静态 `od` 和 `dd` 放进 `SHIM_DIR`（`/opt/xfstests/shims/bin`，已在 PATH 最前，`run_xfstests_test.sh:1089`），通过 `prepare_xfstests_prebuilt.sh` 的产物链带进 initramfs。注意 `BASE_PATH` 在 `TOOLS_BIN_DIR` 之前（line 159-163）是刻意的，所以必须放 SHIM_DIR 而不是 tools/bin。完成后全量扫一遍 official.list 还有哪些 golden output 依赖的工具特性 busybox 不支持（重点：`hexdump`、`awk` 高级用法、`stat` 格式串），一次补齐。

### 3.8 簇 G：generic/320 中断——内核 heap 188KB 分配失败，判定次生还是独立 bug

**日志时间线**：320 于 1355s 启动 → 1355–1432s 约 6 万行 ENOSPC create 报错（scratch 又是秒满状态，即簇 D/R 的 degraded 模式）→ **1432–1698s 整整 267 秒静默** → 1698.6s `Failed to allocate a large slot`（`ostd/src/mm/heap/slot.rs:89`）+ `Heap allocation error size=0x2f080`（188KB，align 8）→ 日志终止，VM 死亡。

**分析**：致命点在刷屏停止 267 秒后，不是"打日志打死"的瞬时事件；588s < 600s 超时，也不是 timeout。三个候选，按嫌疑排序：
1. **次生于簇 R/A/D**：55 个 case 累积的未卸载挂载实例 + per-fs 无界缓存（inode 元数据缓存、extent map 缓存、correctness 锁表按 ino 只增不减）+ degraded 模式下 100 线程重试风暴。修完前置簇后大概率自愈。
2. 大分配点：非 page_cache 读路径 `vec![0u8; writer.avail()]`（`inode.rs:169`）和 `readdir` 全量收集 `Vec<SimpleDirEntry>`（fs.rs:5906-5918，rm -rf 一个 873 文件的目录时不小）；188KB 的连续分配在碎片化堆上失败。
3. 独立内核内存泄漏（最后才怀疑）。

**行动**：不先修它。修完簇 R + 日志降噪后单跑 `generic/320`（健康 fs 上 100 worker × cp 7MB × rm，本来就该过）；如果单跑就打死内核，再上内存统计定位。**独立的日志卫生项必须做**：`ext4_create_at failed`（`fs.rs` create 路径）和 `ext4 fallocate failed`（fs.rs:5888）对 ENOSPC 这类预期 errno 降为 debug 级——ENOSPC 是 xfstests 多个 case 的设计内行为（213/224/269 都是故意填满盘的 PASS case），12.7 万行 ERROR 既是宿主日志爆炸源也是堆压力源。

### 3.9 重要交叉结论

1. **修复顺序有硬依赖**：簇 R（umount/生命周期）不修，275/273/320 的复现结果不可解释（无法区分分配器 bug 和 mkfs-over-live-mount 污染）；所以 R 必须第一个修，然后重测再定簇 D 的修法。
2. **一因多果**：簇 A 一个根因簇覆盖 6 个 FAIL + 1 个 NOTRUN；generic/246 是 A+B+F 三簇叠加，要全修完才 PASS；generic/275 是 C+D+F 三簇叠加；generic/030 是 B+F 两簇叠加。milestone 里按簇记账，不按 case 记账。
3. **35 个 PASS 含金量待补**：fsck 从未运行（簇 R 后果 3），全部修完后的 full run 必须带 per-case fsck 才能作为交付证据。

## 4. 分步计划（执行 agent 按此顺序，不得跳步）

### Step 0：基线与文档（已完成）

plan/milestone 创建、索引同步。本次更新：§3 根因分析报告写入。

### Step 1：判别实验与 runner 前置修复（簇 R + 诊断基建）

改动点：
1. `SHIM_DIR` umount shim（设备路径→挂载点翻译），`run_xfstests_test.sh`。
2. 内核 `sys_umount` 支持设备路径（`kernel/src/syscall/umount.rs`），并核查 ext4 mount 的 `source()` 记录，让 `/proc/mounts` 第一列显示设备路径。
3. ENOSPC 日志降噪（fs.rs create/fallocate 的 error! → ENOSPC 时 debug!）。
4. balloc ENOSPC 诊断 dump（kcmdline `ext4fs.balloc_debug=1` 门控）。

然后跑判别矩阵（每项单 case + 宿主 e2fsck 双镜像）：

| 实验 | 目的 |
|------|------|
| `generic/275` 单跑 | 区分簇 D 机制 1（umount 修后即愈）vs 机制 2/3（看 balloc dump + e2fsck 计数方向） |
| `generic/213` 单跑 + e2fsck TEST 镜像 | 证实/证伪 §3.3 的 fallocate 无回滚毒化链 |
| `generic/246` 单跑（fresh TEST_DEV） | 确认 A 簇 case 单跑即过（即失败纯靠累积态） |
| `generic/320` 单跑 | 区分簇 G 次生 vs 独立 |
| `generic/141` 单跑（先不开 page_cache） | 固化 mmap ENODEV 复现，作为 Step 2 的 before 证据 |

退出条件：umount 不再 EINVAL；判别矩阵结果记入 milestone，簇 D/G 的修法路径已确定。

### Step 2：低风险配置/语义修复批次（簇 B + C + E 的尺寸项）

1. official 模式 `ext4fs.page_cache=1`（`run_phase4_part3.sh:223-226`）→ 030/074/141 的 mmap 部分。
2. statfs 修复（`fs.rs sb()`：活计数 + overhead + bavail）→ ext4/042。
3. `XFSTESTS_SCRATCH_IMG_SIZE=8G`（`run_phase4_in_docker.sh:64`）→ generic/312 NOTRUN 解除。

验证：042/141/074 单 case PASS；`pagecache_phase4` 守底不回退；fio O_DIRECT phase6 守底不回退（确认 official 口径变化不影响性能口径）。

### Step 3：簇 A 空间回收收口（高风险，小步全守底）

按 §3.3 的 1→5 顺序：fallocate/写回滚 → close 时 orphan 补偿 → rename 覆盖截断 → rmdir 目录块回收 → inode 位图回收。每一小步：单 case（213/246/257/309）+ 宿主 e2fsck 干净 + crash matrix + fsstress 013/014 + SQLite integrity。

退出条件：full run（TEST_DEV 全程不重 mkfs）中 246/248/249/257/309/313 全 PASS、214 不再 NOTRUN、终态 e2fsck 双镜像干净。

### Step 4：簇 D 分配器修复（按 Step 1 实验结论）

机制 1 → 已被 Step 1 覆盖，只需回归确认；机制 2 → 修组计数维护/释放配平；机制 3 → 与 Step 3 第 1 项合并；机制 4（并发跳组）无论如何修掉。验证：275（还差簇 F 才全过）、273 单跑，273/320 在 full run 中 PASS。

2026-06-18 更新：`generic/269` 首因已收口。full official 中 `269` 原本只因满盘后 `umount /ext4_scratch` 返回 ENOSPC 而 FAIL，并把 scratch 残留挂载污染后续大量 case；ext4 fs-wide sync 对 page-cache writeback ENOSPC 降级后，`269` 单测 PASS（`official_20260617_121256.log`），partial full（`official_20260617_121742.log`）中 `269/273/275/308/309/313/320/...` 均恢复 PASS/NOTRUN 正常分类。当前不再把这些 case 作为独立 D/A 未修项，后续主要补守底与完整 run summary。

### Step 5：簇 E hardlink/symlink 实现（或经人工确认的不支持说明）

hardlink 先行（小）：ext4_rs `link_at`（dir_add_entry + links_count++，journal 包装）+ `Ext4Inode::link()`。symlink 跟上（中）：fast symlink + 数据块 symlink + `create(SymLink)`/`read_link`/`write_link`。验证：002/005/023/089/109/236 从 NOTRUN 转 PASS；unlink/rename 对 nlink>1 的行为回归（Step 3 的回收逻辑必须按 nlink 判断，连跑 crash matrix）。

### Step 6：簇 F guest 工具链

GNU 静态 od/dd 进 SHIM_DIR（经 `prepare_xfstests_prebuilt.sh`）；扫描 official.list 全部 golden output 的工具依赖一次补齐。验证：030/246/275 全 PASS。

2026-06-18 更新：`generic/452` 的 `ls_on_scratch` 已定位为 busybox applet 复制后 argv0 失效；新增可复制执行的 `ls` shim 后单测 PASS（`official_20260618_113426.log`）。剩余明确工具/能力缺口是 `generic/532`：`chattr +i/+a` 依赖 FS_IOC flags，当前会输出 `Inappropriate ioctl for device` 并污染 golden output；应实现最小 GETFLAGS/SETFLAGS 或经赛题要求确认后让该 case 正确 NOTRUN，不建议简单过滤 stdout。

### Step 7：full official 收敛 + 守底矩阵

完整 full run（带 per-case fsck），记录总数/PASS/FAIL/NOTRUN 与起点对比；新暴露 FAIL 回到 Step 1 的方法循环。然后跑完整守底矩阵（§5 表）。

退出条件：official.list 82 case 全部跑完，FAIL=0（或仅剩经人工确认的不可支持项），无中断，守底全绿。

2026-06-18 当前收敛口径：`official_20260617_121742.log` 因外层 21600s run timeout 在 `generic/558` 前终止，未生成最终 summary；但被终止前完成 69 个 case，其中有效样例 60 个，57 PASS / 3 FAIL = 95.00%。`generic/452` 后续已单测修复，按已完成集合折算约 58/60 = 96.67%。下一步优先级：补 `generic/532`，然后从 `generic/558` 起续跑或加长 full run timeout 拿完整 summary；`generic/074` 可作为 600s timeout 长尾保留，因 1800s 诊断已 PASS。

## 5. 守底回归矩阵

| 类别 | 入口 | 要求 |
|------|------|------|
| Official xfstests | `tools/ext4/run_official_xfstests.sh` | full run 无提前中断，FAIL 收敛，带 fsck |
| 单 case 回归 | `XFSTESTS_SINGLE_TEST=...` | 修过的 case 全 PASS |
| 宿主 e2fsck | `e2fsck -nf` 双镜像（§2.1） | 单 case 与 full run 终态均干净 |
| Phase 4 PageCache | `PHASE4_DOCKER_MODE=pagecache_phase4` | 不回退（Step 2/3 必跑） |
| Phase 6 guard | `PHASE4_DOCKER_MODE=phase6_with_guard` | 不回退 |
| 并发守底 | `RUN_PHASE2_CONCURRENCY=1` + `PHASE4_DOCKER_MODE=concurrency` | 不回退（Step 3/4 必跑） |
| JBD2/crash | `jbd_phase1` + crash matrix | 不回退（Step 3/5 必跑） |
| fsync/flush | Phase 3 fsync/flush + host-crash fsync | 不回退 |
| fio O_DIRECT | Phase 6 fio guard | 不低于既有红线（Step 2 确认口径隔离） |
| SQLite | speedtest1 / `integrity_check` | Step 3/4 必跑 |

## 6. 待人工确认问题

| 问题 | 背景 | 建议 |
|------|------|------|
| official 口径切到 `page_cache=1` 是否需要与学长/答辩口径对齐？ | 功能正确性口径与性能口径分离（§3.2） | 建议确认后固化进 benchmark.md 三处副本 |
| hardlink/symlink 是否纳入实现范围？ | 赛题 POSIX 清单未点名，但 official.list 包含且"等"字有解释空间（§3.6） | 建议实现；若否需正式说明 |
| `ext4_unlink_at`/inode 回收的并发安全边界 | ext4.rs:238-256 注释记录的历史顾虑 | Step 3 第 5 小步前人工 review 设计 |
| 35 个未跑 case（generic/339–708）中是否还有平台硬缺口（如 dm 依赖）？ | 记忆中 Asterinas 无 device-mapper | Step 7 第一次完整 run 后按同方法逐个归簇 |

## 7. 记录要求

所有结果写入 `feature_fixerror_phase7_milestone.md`：起点/重跑结果表、**按根因簇（R/A/B/C/D/E/F/G）记账的分类表**、每个修复的变更日志、守底矩阵、未解问题与人工确认项。根目录文档与 `asterinas/docs/` 副本必须同步。
