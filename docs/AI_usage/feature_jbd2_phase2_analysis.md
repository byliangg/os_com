# Asterinas ext4 JBD2 功能实现 Phase 2 — 并发正确性问题分析


## Phase 2 主要风险分组

| 编号 | 风险 | 影响 | 优先级 |
|------|------|------|--------|
| G1 | `runtime_block_size` 是全局变量 | 多挂载点/并发 ext4 调用可能解析错误 block size | P0 |
| G2 | `active_handles.front_mut()` 代表当前 handle | 并发 handle 下 metadata 记账串 transaction | P0 |
| G3 | `OP_ALLOCATED_BLOCKS` 是全局集合 | 并发 allocator 记录互相污染，重复分配检测失效 | P0 |
| G4 | allocator bitmap/counter 无细粒度并发协议 | 重复 pblock、free counter 错、crash 后不一致 | P0 |
| G5 | 同 inode write/truncate/extent tree 缺少显式互斥 | 映射丢失、释放后读、size 回退 | P0 |
| G6 | 目录 rename/create/unlink 锁顺序未定义 | dentry/link count 不一致或死锁 | P0 |
| G7 | direct read cache 与写侧失效竞争 | 读到旧 mapping 或旧数据 | P1 |
| G8 | timestamp cache 与 journal 成功语义松耦合 | 时间戳漏写或 crash 后状态异常 | P1 |
| G9 | commit/checkpoint 锁顺序与 I/O 混合 | 高并发下死锁或长尾延迟 | P1 |
| G10 | 当前测试未覆盖真实多文件并发 | 删除全局锁后缺少可信验收 | P0 |
| G11 | active handle 按 transaction id / FIFO 匹配 | 同一 transaction 多 handle 下 stop/metadata/data-sync 归属错误 | P0 |
| G12 | buffered I/O / PageCache 失效协议未定义 | truncate/unlink/rename/direct write 后读到旧数据 | P1 |
| G13 | fsync group commit 与 orphan inode 语义未验收 | fsync 测试预期错误，unlink-while-open 泄漏或复用腐败 | P1 |
| G14 | ordered-mode data drain 误按单文件处理 | 同一 TX 中其他文件 metadata 已提交但 data 未落盘 | P0 |
| G15 | `jbd2_runtime` / `jbd2_journal` 锁序未固定 | commit/checkpoint 与写路径死锁 | P0 |
| G16 | 多 block group 锁序未固定 | allocator 并发死锁 | P1 |
| G17 | 多 handle 共用 TX 时 credit/admission 未审计 | TX 过大、journal 空间不足或 rotation 时机错误 | P1 |


