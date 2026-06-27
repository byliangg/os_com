# ext4 性能测试

## 收尾 dump + 收割阶段占比表

### 写路径阶段占比（`[ext4-direct-write]` 平均 µs/写；device_wait 来自 `[block-profile]`）

| case | total | bio_wait | bio_copy | plan | prepare | touch | wait_after_complete | block device_wait |
|------|------:|---------:|---------:|-----:|--------:|------:|--------------------:|------------------:|
| ext4j-write-1M | 268 | **191 (71%)** | 52 (19%) | 0 | 0 | 0 | 9 | 178 |
| ext4j-write-16K | 34 | 31 (91%) | 0 | 0 | 0 | 0 | 7 | 22 |
| ext4j-write-4K | 30 | 28 (93%) | 0 | 0 | 0 | 0 | 7 | 19 |
| ext4n-write-1M | 359 | 259 (72%) | 71 (20%) | 0 | 0 | 0 | 12 | 240 |

### 读路径阶段占比（`[ext4-profile] direct-read` 平均 µs/读）

| case | plan | wait | copy | plan 占比 |
|------|-----:|-----:|-----:|----------:|
| ext4j-read-1M | 63 | 117 | 49 | 27% |
| ext4j-read-4K | **60** | 20 | 0 | **75%** |

### 归因结论

1. **① 大块单 job 写 = virtio device_wait，已钉死在 ext4 之下。** ext4j-write-1M 总 268µs 中 `bio_wait=191µs(71%) ≈ block device_wait=178µs`；ext4 `plan/prepare/touch = 0µs`；`wait_after_complete=9µs`（waiter 唤醒**不是**瓶颈）。与 sweep "ext4≈raw(105%)" 互相印证。
2. **② 小块瓶颈在"读"，且是 ext4 自己的 `plan`（extent 映射）。** ext4j-read-4K 的 `plan=60µs` 占 **75%**，而 `plan` 在 1M/4K 上都固定 ~60–63µs——**每次读重走 extent 映射的固定开销**，小块下无法摊薄。这正是 sweep 最差格子（4K read 11%）的根因。
3. **小块"写"反而不是 ext4 CPU 的锅**：4K write 的 `plan/prepare/touch` 同样 = 0，总时间 93% 在 `bio_wait`（per-request device 延迟）。即小块写差是 per-request 设备延迟无法摊薄，不是 ext4 元数据开销。
4. **③ 锁竞争在单 job 下不可见**：所有 case `avg_wait_us=0`（`max_hold_us` 的 33s 离群值是收尾 commit/checkpoint，非稳态）。读的并发退化（sweep nj2 100%→68%）需 `numjobs≥2` 才能在 profile 中显形——列入 Step 1b。
5. **JBD2 洗清（写）**：ext4j-write journaled_ops 整轮仅 165、overlay 命中 99.998%；nojournal 路径 journaled_ops=0 但 device_wait 反而更高，进一步说明写瓶颈在设备而非 JBD2。读路径有 atime 触发的 journaled 写（read-4K write_ops=262145），值得单独看是否可降。
6. **次要写优化点 `bio_copy`**：1M 写 52µs（19%）、nojournal 71µs，是用户 buffer→DMA 的 memcpy；profile 显示用户 buffer 为 256 个非连续物理页（`max_user_phys_run_pages=1`），零拷贝 SG 需 256 段。

### 优化候选排序

1. **读 extent-mapping plan 缓存**（砍 60µs 固定 `plan`）——射程内、故事性最好、直击 sweep 最差格子；与已退役的 DirectReadCache 的 mapping 缓存思路相关，但只缓存 metadata-only extent plan，不复活数据 cache。
2. **写 `bio_copy` 零拷贝**（1M 写 19%）——较难（256 非连续页）。


## 小块读全路径归因 + atime 按秒节流

| 部分 | µs | 占比 | 层 | ext4 可修 |
|------|---:|---:|----|:--:|
| VFS / syscall / framekernel（read_direct_at 之上）| 66 | 55% | 平台 | ❌ |
| **atime（每读一次 `stat(ino)`）** | 31 | 26% | ext4 | ✅ |
| virtio 往返（wait）| 21 | 18% | 块层 | ⚠️ |

对照 Linux 整次 4K read = 18µs。

### 85–90% 可行性判断：不现实（平台层）
framekernel 每-syscall 开销 66µs 即 Linux 整次读的 3.7×；即便 atime/virtio 清零，单这 66µs 也把 4K 卡在 ~27%。**小块 direct read 的根本限制是 framekernel per-syscall 开销（平台层，超 ext4 射程）**，作为"定位到平台瓶颈"的研究结论。ext4 射程内能榨的（extent 查找 60µs + atime 31µs）已榨完。



## ext4 inode 元数据缓存（小块读最大优化）


### profile + 吞吐（Asterinas-only，对照之前 / ext2）

| bs | 之前 MB/s | **inode 缓存后** | 提升 | ext2 MB/s |
|----|---------:|---------------:|-----:|----------:|
| 4K | ~35 | **152** | **4.3×** | — |
| 16K | ~140 | **539** | **3.6×** | 640 |
| 64K | ~524 | **1526** | **2.9×** | 1760 |

- 每读时间 4K 117µs→27µs：那 ~90µs 的"每读多次 inode-block stat 读盘"被消除；read_direct_at 内只剩 ~25µs（主要 virtio wait 23µs，平台地板，ext2 同）。
- **ext4 小块读达 ext2 的 ~84%**（之前 ~22%）——FS 专属差距基本填平。

| **1M write** | 63.44% | **76.31%** | +13pt |

**所有块大小读全部 84–95%——基本全线达标**；inode 缓存顺带把 write 路径（也每读 stat type/size）从 63% 拉到 76%。

