# Asterinas EXT4 Benchmark 
 
## 1. 环境准备

### 1.1 快速复现（拿到仓库即可执行）

前提：宿主机有 Docker 与 `/dev/kvm`，拉取镜像 `asterinas/asterinas:0.17.0-20260227`。所有入口在宿主机仓库根执行、脚本自管容器：

```bash
# 1) 功能回归（PHASE4_DOCKER_MODE 可选：crash_only / phase6_with_guard / concurrency /
#    jbd_phase1 / jbd_phase2_concurrency / jbd_phase3_host_crash / jbd_phase3_fsync_flush ...）
PHASE4_DOCKER_MODE=crash_only ENABLE_KVM=1 BENCH_ENABLE_KVM=1 \
  BENCH_ASTER_NETDEV=tap BENCH_ASTER_VHOST=on bash tools/ext4/run_phase4_in_docker.sh

# 2) fio 96-case 参数广度测试（SWEEP_GROUPS=F 只跑并发组；RUN_G_CORRECTNESS=0 跳过附带回归）
bash test/initramfs/src/benchmark/fio/run_parameter_sweep_summary.sh

# 3) fio O_DIRECT 守底（单 job，诚实口径）
EXT4_DIRECT_READ_CACHE=0 EXT4_PAGE_CACHE=0 LOG_LEVEL=error BENCH_ENABLE_KVM=1 \
  BENCH_ASTER_NETDEV=tap BENCH_ASTER_VHOST=on bash test/initramfs/src/benchmark/fio/run_ext4_summary.sh

# 4) SQLite 真实应用（FS_LIST="ext4 ext2 ramfs" 可跑三 FS 诊断三角）
FS_LIST=ext4 PAGE_CACHE_LIST=1 LOG_LEVEL=error bash test/initramfs/src/benchmark/sqlite/run_sqlite_summary.sh
```

### 1.2 环境要求与首次准备

- 宿主参考：Ubuntu 24.04 / 内核 6.8 / QEMU 8.2 / e2fsprogs 1.47 / Rust nightly + cargo-osdk 0.17（容器内自带，宿主只需 Docker + KVM）。
- 首次本机开发（非 Docker）可运行 `./tools/setup_dev_env.sh`（装 rust-src/rustfmt/rustc-dev/llvm-tools + 内核 target + VDSO；`--no-vdso` 只补组件）。
- 可选代理（仅下载超时时）：`export http_proxy=http://127.0.0.1:7890 https_proxy=... all_proxy=socks5://127.0.0.1:7890`。
- 宿主直跑（排障用）统一环境变量：`CARGO_TARGET_DIR=$(pwd)/target_lby VDSO_LIBRARY_DIR=$(pwd)/.local/linux_vdso BOOT_METHOD=qemu-direct OVMF=off RELEASE_LTO=1 CONSOLE=ttyS0`；Docker/KVM 复跑推荐 `ENABLE_KVM=1 BENCH_ENABLE_KVM=1 BENCH_ASTER_NETDEV=tap BENCH_ASTER_VHOST=on`。

### 1.3 本机生成物（不进 Git，克隆后可重建）

| 目录 | 说明 |
|---|---|
| `target_lby/`、`.target_bench/` | 构建产物 |
| `benchmark/logs/` | 本地 run 产物（已 gitignore；需版本化的证据用 `git add -f`）|
| `.local/linux_vdso`、`.local/xfstests-*` | VDSO 与 xfstests 预构建（缺失时 `tools/ext4/prepare_xfstests_prebuilt.sh` 重建）|
| `.cache/linux_binary_cache/vmlinuz` | fio Linux 对照侧内核（~24MB；缺失时 `prepare_host.sh` 自动从 `asterinas/linux_binary_cache` 下载，离线环境手工预置同名文件）|

仓库已跟踪可复用 initramfs：`benchmark/assets/initramfs/initramfs_phase{3,4_part3}.cpio.gz`（runner 按需重打包，重建入口 `tools/ext4/prepare_phase4_part3_initramfs.sh`）。

### 1.4 判定口径

- xfstests：单 case 看 `xfstests case done: <case> rc=0`，总体看 `All syscall tests passed.`。
- crash matrix：看末尾 `Crash summary` 表全 PASS。
- 已知 flake：QEMU hostfwd 随机端口偶发碰撞（`Could not set up host forwarding rule`），crash runner 已加场景间隔缓解；遇到即重跑该模式。






