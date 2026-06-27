# ext4 性能测试结果与优化方向

## 1. 测试结果

### 1.1 SQLite 整体结果

| 阶段 | Asterinas TOTAL | Linux TOTAL | Asterinas / Linux | 完整性 |
| --- | ---: | ---: | ---: | --- |
| Phase 6  | 2022 s | 60.2 s | 2.97% | PASS |
| 当前结果 | 234.9 s | - | 21.92% | PASS |

从 Phase 6 起点到当前结果，Asterinas SQLite 总时间从 2022 s 降到 234.9 s，约提升 8.6 倍，耗时降低约 88.4%。

### 1.2 文件系统对照结果

| 文件系统 | Asterinas TOTAL | Linux TOTAL | Asterinas / Linux | 完整性 |
| --- | ---: | ---: | ---: | --- |
| ext4 journaled | 2010.7 s | 60.8 s | 3.02% | PASS |
| ext2 | 62.5 s | 59.3 s | 94.91% | PASS |
| ramfs | 55.6 s | 53.3 s | 95.87% | PASS |

ext2 和 ramfs 都接近 Linux，说明主要瓶颈不在 Asterinas 平台层或 virtio 基础路径，而集中在 ext4 journaled 写路径。

### 1.3 主要瓶颈归因

| 瓶颈来源 | 耗时占比 |
| --- | ---: |
| fast overwrite 中重复 `map_blocks` 和全局 runtime lock | 41% |
| journaled allocation 慢路径 | 32% |
| commit / journal / fsync / writeback bio | 24% |
| read 路径 | 1% |

读路径不是主要问题。SQLite 慢主要来自写入、追加、新块分配、建索引和 VACUUM 等写密集路径。



## 2. 结论

1. Asterinas ext4 的 SQLite 性能瓶颈主要在 ext4 journaled 写路径，不是基础平台性能不足。
2. 当前优化已经把 SQLite 总时间从 2022 s 降到 234.9 s，性能提升约 8.6 倍。
3. ext2 和 ramfs 结果接近 Linux，进一步证明 Asterinas 的通用 PageCache 和基础 I/O 路径不是主因。
4. 主要开销来自重复块映射、journaled allocation、commit/fsync/writeback。读路径占比很低，不是优化重点。
5. 完整 delalloc 理论收益最大，但目前受限于 Asterinas 缺少安全的后台 writeback / mid-flight writeback 机制。之前测试中 dirty page throttle 可以避免 OOM，但 mid-flight writeback 会导致 SQLite corruption，因此不能直接启用完整 delalloc。
6. 当前优化路线更偏向“安全写路径优化”：在保证 SQLite integrity 和 crash consistency 的前提下，逐步减少 metadata 读写、锁竞争和 journaled allocation 成本。

## 3. 优化方向

### 3.1 短期可继续推进

1. 继续扩大 metadata / device block cache 覆盖面，减少重复读块和重复解析。
2. 继续优化 write fast path，减少 hot write 场景下的 `map_blocks`、锁和事务开销。
3. 优化 fsync / commit batching，在保证 crash consistency 的前提下降低提交频率和同步成本。
4. 继续完善 unwritten extent 和 preallocation，减少 SQLite 追加写和新块分配的慢路径成本。
5. 保留并完善 O_DIRECT overwrite 并发共享锁优化，用于 fio 并发场景。

### 3.2 中长期方向

1. 补齐安全的后台 flusher / mid-flight writeback 机制，然后重新评估完整 delalloc。
2. 强化 ENOSPC、extent insert、revoke 和 crash recovery 等边界场景，避免性能优化破坏一致性。
3. 针对 commit、journal 和 writeback bio 做更细粒度 profile，确认 P5a 之后新的主瓶颈。
4. 所有后续优化都需要继续保留 SQLite integrity、fio O_DIRECT、fsync flush、crash matrix 和 pagecache coherency 等回归测试。
