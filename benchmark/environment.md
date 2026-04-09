# Asterinas EXT4 Environment（stage7）

更新时间：2026-04-09 20:43（Asia/Shanghai）

## 1. 当前执行口径

1. 当前阶段：`stage7`（建议分支：`stage-7`）。
2. 执行方式：统一走 Docker（`tools/ext4/run_phase4_in_docker.sh`）。
3. 日志目录：`benchmark/logs/`。
4. 资产目录：`benchmark/assets/`（不依赖 `.local`）。
5. 命名说明：`phase6_only / phase6_perf_compare` 为历史入口名，stage7 继续沿用。

## 2. 最新结果

1. `phase6_only`：PASS（`25/25`）  
   日志：`benchmark/logs/phase6_good_20260409_122044.log`
2. `others_test`：PASS  
   日志：`benchmark/logs/others_general_20260409_203930.log`
3. `phase6_perf_compare`（8项x3轮）：FAIL  
   目录：`benchmark/logs/perf_compare/20260409_122735/`  
   结果：`overall_avg_ratio=0.350293`  
   备注：本轮 8 项 3 轮全部有值，无超时轮次。

## 3. 本轮关键变更

1. ext4 I/O fast path：`kernel/src/fs/ext4/fs.rs`（扇区对齐读写直通）。
2. lmbench `lat_fs` 口径修正：
   - `test/initramfs/src/benchmark/lmbench/{ext4,ext2,ramfs}_create_delete_files_{0k,10k}_ops/bench_result.yaml`
   - `result_index: 2 -> 3`（取 create 吞吐列）。
3. 注意：旧 perf 目录里按 `result_index=2` 的 create/delete 比值不再作为性能决策依据。
4. ext4_rs 第四轮热路径优化：
   - `kernel/libs/ext4_rs/src/ext4_impls/dir.rs`：目录插入去重落盘，目录删除复用 `prev_offset`。
   - `kernel/libs/ext4_rs/src/ext4_impls/file.rs`：`read_at/write_at` 增加 extent 命中缓存，`link/create` 去除冗余落盘。
   - `kernel/libs/ext4_rs/src/fuse_interface/mod.rs`：`fuse_link` 显式写回 parent/child inode。
5. ext4_rs 第五轮元数据热路径优化：
   - `kernel/libs/ext4_rs/src/ext4_defs/ext4.rs`：新增 `inode_table_blk_cache` 字段。
   - `kernel/libs/ext4_rs/src/ext4_impls/ext4.rs`：mount 阶段构建 inode table cache。
   - `kernel/libs/ext4_rs/src/ext4_impls/inode.rs`：`inode_disk_pos` 优先走缓存。
   - `kernel/libs/ext4_rs/src/ext4_impls/dir.rs`：目录 fast path 失败后不重复扫描尾块。
   - `kernel/libs/ext4_rs/src/ext4_impls/{ialloc.rs,balloc.rs}`：增加 `HOTPATH_SYNC_SUPERBLOCK` 开关并默认关闭。
6. ext4 第六轮 dentry 路径优化：
   - `kernel/src/fs/ext4/inode.rs`：`is_dentry_cacheable` 改为 `true`。

## 4. 常用命令

```bash
cd /home/lby/os_com/asterinas

# 功能门禁
PHASE4_DOCKER_MODE=phase6_only ENABLE_KVM=1 XFSTESTS_CASE_TIMEOUT_SEC=900 KLOG_LEVEL=error ./tools/ext4/run_phase4_in_docker.sh

# 性能全量对照
PERF_ROUNDS=3 \
PERF_CASE_TIMEOUT_SEC=600 \
BENCH_ENABLE_KVM=1 BENCH_ASTER_NETDEV=tap BENCH_ASTER_VHOST=on \
./tools/ext4/run_phase6_perf_compare_in_docker.sh

# 可选：先做 warmup，减少首轮 cold-start 干扰（脚本默认已开启）
PERF_WARMUP_BENCH=ext4_vfs_open_lat \
PERF_WARMUP_TIMEOUT_SEC=1200 \
./tools/ext4/run_phase6_perf_compare_in_docker.sh
```

## 5. 关键日志

1. `benchmark/logs/phase6_good_20260409_122044.log`
2. `benchmark/logs/perf_compare/20260409_122735/phase6_perf_compare_report.txt`
3. `benchmark/logs/others_general_20260409_203930.log`
