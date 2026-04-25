# Asterinas ext4 JBD2 Phase 2 — 锁顺序与同步原语约定

首次更新时间：2026-04-24（Asia/Shanghai）

## 目标

Phase 2 会逐步把 Phase 1 依赖的全局串行化拆成显式同步。本文件固定实现前必须遵守的锁顺序，避免并发读写、JBD2 commit/checkpoint、allocator 与 cache 路径形成死锁。

## 总原则

1. 不持有 spin lock 做阻塞操作、磁盘 I/O、等待 QEMU/block device、等待其他进程或可能睡眠的操作。
2. cache map 锁只保护内存 map 的短临界区，不在持锁状态下进入 ext4_rs、JBD2 或块设备 I/O。
3. 多资源锁按稳定 key 排序获取；需要补锁时，如果新 key 可能排在已持锁之前，必须释放后按完整顺序重取。
4. `EXT4_RS_RUNTIME_LOCK` 在 Step 7 前仍是 safety fence，不能在 Step 2 后提前缩空或删除。
5. Step 4.5 通过前，Step 3/4 的 handle-local / operation-local 结论只视为全局串行 fence 下成立；不得据此进入 Step 5 的实现阶段或 Step 7 的拆锁阶段。

## 全局锁顺序

从外到内固定为：

1. VFS / file operation 层已有锁
2. per-directory / per-inode correctness 锁
3. cache 全局 map 锁
   - `dir_entry_cache`
   - `inode_direct_read_cache`
   - `inode_atime_cache`
   - `inode_ctime_cache`
   - `inode_mtime_ctime_cache`
4. ext4_rs coordination
   - Phase 2 前半段：`EXT4_RS_RUNTIME_LOCK`、`Ext4Fs::inner`
   - Step 7 后：per-inode/per-dir/per-block-group 等更细粒度锁
5. allocator block group locks
6. `jbd2_runtime`
7. `jbd2_journal`
8. `jbd2_checkpoint_lock`
9. block device I/O / `sync()`

任何新增路径如果需要反向获取，必须先重构 critical section，不能引入例外。

## JBD2 锁序

- `jbd2_runtime` 必须先于 `jbd2_journal` 获取。
- commit/checkpoint 路径如果已经持有 `jbd2_journal` 又需要修改 runtime，必须先释放 journal 锁，再回头按 `jbd2_runtime -> jbd2_journal` 顺序获取。
- `jbd2_checkpoint_lock` 只用于串行 checkpoint 执行；不要在持有它时回头获取 higher-level inode/dir/cache 锁。
- Phase 2 默认 commit/checkpoint 仍为 inline；若 Step 8 引入后台线程，必须重审锁序、唤醒条件、shutdown/unmount 交互。

## Directory / Rename 锁序

目录 mutation 的顺序：

1. 按 inode number 升序锁定 `old_parent` / `new_parent`。
2. 在 parent 锁内解析 `src` 与可选 `dst`。
3. 若 `dst` 存在，按 inode number 升序锁定 `src` / `dst`。
4. 若 `dst` 不存在，使用明确的 absent-dst 语义，不允许在后续路径反向补锁。

必须覆盖：

- same-dir rename
- cross-dir rename
- overwrite existing dst
- dst absent

## Allocator / Block Group 锁序

- 单个 block group 内的 bitmap RMW、group free counter、superblock free counter 由该 group 的 allocator 锁保护。
- 多 block group 操作必须按 group number 升序获取 per-bg 锁。
- 不允许在持有较大 group number 锁时再补较小 group number 锁。
- 如果 fallback 分配发现需要新 group，释放当前锁集合，重新按完整升序集合获取。

## JBD2 Handle 可见性

- 同一 running transaction 内多个 handle 共享 metadata overlay view。
- `overlay_metadata_read()` 不按 handle id 过滤。
- 跨 transaction 读取顺序保持 `running -> prev_running -> committing -> checkpoint_list`，不能跨过未完成 commit 看到 future transaction。
- `JournalHandle` 必须引入唯一 handle id / generation；`remove_active_handle()`、data sync 标记、metadata write 归属都按 handle id，不按 transaction id 或 FIFO front。
- current handle 不能依赖跨 operation 的全局 single-slot mutex；Step 4.5 必须将 metadata write / data-sync 归属改为显式上下文，或在过渡期使用严格 scoped push/pop 并标注剩余风险。

## Operation Context 可见性

- allocator guard 必须按 operation id 访问，不能依赖跨 operation 的全局 current operation single-slot。
- “切换当前 operation”与“丢弃某 operation 已分配块集合”是不同语义，API 命名和调用点必须区分。
- 无 journal 路径也必须有明确 operation context；不得回退到全局默认 operation id 共享状态。
- nested `run_ext4` / active handle 路径必须证明不会清空外层 operation guard。

## Ordered Mode / fsync

- ordered-mode data-before-metadata 约束以 transaction 为单位。
- commit 前必须 drain 该 transaction 内全部 handle 触及的 dirty data block。
- `fsync(file A)` 可以 group commit 其他 ready transaction 或同 TX 的其他 handle，但对调用者的最小承诺是：A 在 fsync 前完成的写入 durable。
- 测试不能假设 fsync 只提交当前文件。

## Buffered I/O / Cache

- Phase 2 不接入 ext4 PageCache；buffered I/O 仍直通 `fs.read_at()` / `fs.write_at()`。
- Step 5 correctness 阶段采用保守 read 同步：read 与同 inode write/truncate 互斥，或只基于持锁期间取得的稳定 mapping snapshot 读取。
- mapping generation 与更细粒度 read/write 并发留到 Step 8 性能优化。
- write/truncate/unlink/rename 失败或成功后，都必须保证 direct read cache 不返回旧 mapping。

## 回退策略

如果某 step 引入死锁、重复 pblock、metadata 归属错误或 crash recovery 回退：

1. 先恢复上一层保守锁范围；
2. 保留失败日志与 seed/worker/round 参数；
3. 在 milestone 写清楚重新打开并发的前置条件。
