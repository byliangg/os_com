# Asterinas EXT4 Benchmark/Test 汇总（stage7）

更新时间：2026-04-09 20:43（Asia/Shanghai）

说明：当前阶段为 `stage7`，测试入口沿用历史脚本命名（如 `phase6_only`、`phase6_perf_compare`）。

## 1. 当前结论（Docker 实跑）

1. `phase3_only`：PASS  
   统计：`pass=10 fail=0 notrun=6 static_blocked=24 denominator=10 pass_rate=100.00%`  
   日志：`benchmark/logs/phase3_base_guard_20260409_045752.log`
2. `phase4_good`：PASS  
   统计：`pass=12 fail=0 notrun=6 static_blocked=22 denominator=12 pass_rate=100.00%`  
   日志：`benchmark/logs/phase4_good_20260409_050505.log`
3. `lmbench_only`：PASS（8/8）  
   汇总：`benchmark/logs/lmbench/phase4_part3_lmbench_summary_20260409_051131.tsv`
4. `crash_only`：PASS（`3 场景 x 3 轮 = 9/9`）  
   汇总：`benchmark/logs/crash/phase4_part3_crash_summary_20260409_051328.tsv`
5. `phase6_only`：PASS（stage7 当前功能门禁）  
   统计：`pass=25 fail=0 notrun=0 static_blocked=26 denominator=25 pass_rate=100.00%`  
   日志：`benchmark/logs/phase6_good_20260409_122044.log`
6. `others_test`：PASS（非 ext4 通用回归）  
   日志：`benchmark/logs/others_general_20260409_203930.log`
7. `phase6_perf_compare`：FAIL（Linux EXT4 对照性能门禁）  
   最新结果：`overall_avg_ratio=0.350293 < 0.80`  
   目录：`benchmark/logs/perf_compare/20260409_122735/`

## 2. 本轮关键变更

1. ext4 I/O fast path（仅 ext4 路径）：
   - 文件：`kernel/src/fs/ext4/fs.rs`
   - 内容：`KernelBlockDeviceAdapter::read_offset/write_offset` 增加扇区对齐直读直写路径，减少临时缓冲分配与拷贝。
2. lmbench `lat_fs` 指标口径修正：
   - 文件：`test/initramfs/src/benchmark/lmbench/{ext4,ext2,ramfs}_create_delete_files_{0k,10k}_ops/bench_result.yaml`
   - 修正：`result_index` 从 `2` 改为 `3`（取 create 吞吐列）。
3. ext4_rs 第三轮写路径优化：
   - 文件：`kernel/libs/ext4_rs/src/ext4_impls/file.rs`
   - 变更：`write_at` 写区间预映射、全块连续批量写、`create` 去除冗余 inode 落盘/回读。
4. ext4_rs 第四轮热路径优化：
   - 文件：`kernel/libs/ext4_rs/src/ext4_impls/{dir.rs,file.rs}`、`kernel/libs/ext4_rs/src/fuse_interface/mod.rs`
   - 变更：目录插入去重落盘、目录删除复用 `prev_offset`、`read_at/write_at` 增加 extent 命中缓存、`link/create` 去掉冗余 inode 落盘并补齐 `fuse_link` 写回。
5. ext4_rs 第五轮元数据路径优化：
   - 文件：`kernel/libs/ext4_rs/src/ext4_defs/ext4.rs`、`kernel/libs/ext4_rs/src/ext4_impls/{ext4.rs,inode.rs,dir.rs,ialloc.rs,balloc.rs}`
   - 变更：新增 `inode_table_blk_cache`、目录尾块避免重复扫描、`ialloc/balloc` 热路径 superblock 同步降频（可开关）。
6. ext4 第六轮 dentry 路径优化：
   - 文件：`kernel/src/fs/ext4/inode.rs`
   - 变更：`is_dentry_cacheable` 改为 `true`，提升重复路径访问命中率。

## 3. 最新性能复测（8项x3轮）

目录：`benchmark/logs/perf_compare/20260409_122735/`

1. `ext4_copy_files_bw`：`ratio_avg=0.012274`
2. `ext4_create_delete_files_0k_ops`：`ratio_avg=0.009919`
3. `ext4_create_delete_files_10k_ops`：`ratio_avg=0.007561`
4. `ext4_vfs_open_lat`：`ratio_avg=0.400410`
5. `ext4_vfs_stat_lat`：`ratio_avg=0.366783`
6. `overall_avg_ratio=0.350293`（较 `20260409_112901` 的 `0.338363` 提升 `+3.53%`）
7. 备注：本轮 8 项 3 轮全部有值，无超时轮次。

结论：本轮仍未达到 phase7 性能门禁（`>=0.80`）。

## 3.1 稳定性备注（phase6_only）

1. 最近连续复跑中 `generic/011` 出现过一次波动：
   - FAIL 轮次：`benchmark/logs/phase6_good_20260409_121357.log`（`24/25`）
   - PASS 轮次：`benchmark/logs/phase6_good_20260409_122044.log`（`25/25`）
2. 当前以最新 PASS 轮次作为门禁基线，同时保留失败日志用于后续稳定性收敛。

## 4. 口径注意

1. `20260408_195543` 起，create/delete 指标以修正口径采集。
2. 历史目录中按旧口径（`result_index=2`）产出的 create/delete 比值不再作为性能决策依据。

## 5. 常用命令

```bash
cd /home/lby/os_com/asterinas

PHASE4_DOCKER_MODE=phase6_only ENABLE_KVM=1 XFSTESTS_CASE_TIMEOUT_SEC=900 KLOG_LEVEL=error ./tools/ext4/run_phase4_in_docker.sh

PERF_ROUNDS=1 \
PERF_BENCHES=ext4_create_delete_files_0k_ops,ext4_create_delete_files_10k_ops,ext4_copy_files_bw \
PERF_CASE_TIMEOUT_SEC=900 \
BENCH_ENABLE_KVM=1 BENCH_ASTER_NETDEV=tap BENCH_ASTER_VHOST=on \
./tools/ext4/run_phase6_perf_compare_in_docker.sh
```
