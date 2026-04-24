# Asterinas EXT4 Environment（Current, stage-8）

更新时间：2026-04-11（Asia/Shanghai）

## 1. 当前工作目录

1. 主工作树：`/home/lby/os_com_codex/asterinas`
2. 顶层文档目录：`/home/lby/os_com_codex`
3. 当前目标：继续推进 ext4 P1，主看 `fio/ext4_seq_read_bw`
4. 当前推荐执行位置：Docker 容器

## 2. 当前 benchmark 口径

1. fio 常用参数指纹：
   - `BENCH_ENABLE_KVM=1`
   - `BENCH_ASTER_NETDEV=tap`
   - `BENCH_ASTER_VHOST=on`
2. lmbench 对比常用参数指纹：
   - `PERF_ROUNDS=1`
   - `PERF_CASE_TIMEOUT_SEC=600`
   - `BENCH_ENABLE_KVM=1`
   - `BENCH_ASTER_NETDEV=tap`
   - `BENCH_ASTER_VHOST=on`
3. P1 排障允许临时单边跑：
   - `BENCH_RUN_ONLY=asterinas`
   - `BENCH_RUN_ONLY=linux`
   - 默认仍应是 `BENCH_RUN_ONLY=both`
4. 正式写入 milestone 的结果仍应使用双边对照。

## 3. 当前已确认的环境坑

### 3.1 宿主机 benchmark 不稳定，优先不用

1. `prepare_host.sh` 默认会写 `/opt/linux_binary_cache`，当前宿主无权限。
   - 需要显式设置：
   - `LINUX_DEPENDENCIES_DIR=/home/lby/os_com_codex/asterinas/.cache/linux_binary_cache`
2. `make run_kernel` 必须显式带：
   - `VDSO_LIBRARY_DIR=/home/lby/os_com_codex/asterinas/.local/linux_vdso`
3. 宿主历史 target 中存在 root-owned 产物，会导致 `cargo-osdk` 报 `Permission denied`。
   - 典型位置：
   - `asterinas/target/osdk/`
   - `asterinas/target/release-lto/` 下部分文件
   - 规避方式：
   - `CARGO_TARGET_DIR=/home/lby/os_com_codex/asterinas/.target_bench`
4. 宿主机 `~/.local/bin/qemu-system-x86_64` 是旧 wrapper。
   - 当前内容仍指向 `/home/lby/os_com/asterinas/.local/qemu-root2/...`
   - 该真实 QEMU bundle 已不存在
   - 结论：宿主 benchmark 入口当前不可信，优先走 Docker
5. 迁目录后，`cargo-osdk` 如果不是从当前树重新安装，会混入旧路径 `/home/lby/os_com/asterinas/...`。
   - 必须重装为当前树版本
   - 而且要在**安装 cargo-osdk 时**带 `OSDK_LOCAL_DEV=1`
   - 否则 run-base 会把 `ostd/osdk-frame-allocator/osdk-heap-allocator` 写成 crates.io 版本，出现双份 `ostd` 冲突

### 3.2 Docker benchmark 更稳，但也有两个固定坑

1. `test/initramfs/build/initramfs` 和 `initramfs.cpio.gz` 可能残留为旧目录/旧 symlink。
   - 每次重跑前建议先清：
   - `rm -rf /root/asterinas/test/initramfs/build/initramfs /root/asterinas/test/initramfs/build/initramfs.cpio.gz`
2. 由于当前机器代理环境和 `/etc/resolv.conf` 口径，容器里会看到 DNS fallback 警告。
   - 目前不影响 benchmark 继续执行

## 4. 当前建议的固定准备动作

在重复跑 benchmark 前，先做下面这些：

```bash
cd /home/lby/os_com_codex/asterinas

mkdir -p .cache/linux_binary_cache .target_bench

OSDK_LOCAL_DEV=1 cargo install --locked cargo-osdk --path ./osdk --force
```

如果只做本地编译检查：

```bash
cd /home/lby/os_com_codex/asterinas/kernel
VDSO_LIBRARY_DIR=/home/lby/os_com_codex/asterinas/.local/linux_vdso cargo osdk check
```

## 5. 当前推荐的 Docker fio 复跑命令

单边 Asterinas read：

```bash
cd /home/lby/os_com_codex/asterinas

docker run --rm --privileged --network=host --device=/dev/kvm \
  -v /dev:/dev \
  -v /home/lby/os_com_codex/asterinas:/root/asterinas \
  -w /root/asterinas \
  -e http_proxy=http://127.0.0.1:7890 \
  -e https_proxy=http://127.0.0.1:7890 \
  -e all_proxy=socks5://127.0.0.1:7890 \
  -e BENCH_RUN_ONLY=asterinas \
  -e BENCH_ENABLE_KVM=1 \
  -e BENCH_ASTER_NETDEV=tap \
  -e BENCH_ASTER_VHOST=on \
  -e CARGO_TARGET_DIR=/root/asterinas/.target_bench \
  -e VDSO_LIBRARY_DIR=/root/asterinas/.local/linux_vdso \
  -e LINUX_DEPENDENCIES_DIR=/root/asterinas/.cache/linux_binary_cache \
  asterinas/asterinas:0.17.0-20260227 \
  bash -lc '
    rm -rf /root/asterinas/.target_bench/osdk \
           /root/asterinas/test/initramfs/build/initramfs \
           /root/asterinas/test/initramfs/build/initramfs.cpio.gz
    OSDK_LOCAL_DEV=1 cargo install --locked cargo-osdk --path /root/asterinas/osdk --force
    bash test/initramfs/src/benchmark/bench_linux_and_aster.sh fio/ext4_seq_read_bw
  '
```

如果要正式双边对照，把 `BENCH_RUN_ONLY=asterinas` 去掉或改回 `both`。

快速双项复跑（ext4 write + read，默认静默）：

```bash
cd /home/lby/os_com_codex
./asterinas/test/initramfs/src/benchmark/fio/run_ext4_summary.sh
```

说明：

1. 该脚本会顺序执行 `fio/ext4_seq_write_bw` 和 `fio/ext4_seq_read_bw`。
2. 默认不向终端输出 benchmark 过程日志，只在结束后打印 `Asterinas`、`Linux` 和 `ratio` 摘要。
3. 如需保留临时日志用于排障，可执行：

```bash
cd /home/lby/os_com_codex
KEEP_LOGS=1 ./asterinas/test/initramfs/src/benchmark/fio/run_ext4_summary.sh
```

## 6. 当前现场状态（2026-04-11）

1. `asterinas/test/initramfs/build/` 当前状态：
   - `initramfs -> /nix/store/...-initramfs`
   - `initramfs.cpio.gz -> /nix/store/...-initramfs-image`
2. 可写 benchmark target：
   - `asterinas/.target_bench`
3. 历史 target 仍保留，但不建议 benchmark 继续复用：
   - `asterinas/target/osdk`
   - `asterinas/target_lby`
4. 当前只保留了一个长期容器：
   - `asterinas/asterinas:0.17.0-20260227`
   - `sleep infinity`
   - 这是历史常驻容器，不是本轮 benchmark 残留

## 7. 后续约定

1. 后续凡是 benchmark 入口、代理、`cargo-osdk`、QEMU、target 目录有变化，都优先更新本文件。
2. 提交前如果不再需要单边 benchmark，加回默认双边口径使用方式。
