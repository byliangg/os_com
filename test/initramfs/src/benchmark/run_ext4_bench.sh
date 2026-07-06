#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
#
# ================================================================
#  ext4 buffered 基准 runner —— Asterinas vs Linux 同轮对标
# ================================================================
#
# 【做什么】把一个 ext4 buffered（-direct=0）基准 job 在 **Asterinas 和参照
# Linux 内核**上各跑一遍：同一 QEMU + KVM + virtio-blk + 同一块新格式化的
# ext4 盘，打印两侧原始数（bw / 时间），供人工算 ratio。方法学见 test.md §6，
# 最新一轮实测快照见 test.md §6.6。
#
# 【为什么只有 buffered】本 port **尚未实现 O_DIRECT**（写拒 EOPNOTSUPP、读静默
# 走页缓存 —— P9，ledger `ext4-vs-ext2-feature-regression`）。所以老树那套
# A/B/C/E/F 的 O_DIRECT sweep **跑不了**，当前只有 buffered D 组能真实测量。
# 本脚本只跑 buffered job。
#
# 【前置】asterinas dev 镜像自带参照 Linux 内核（6.16，virtio+ext4 编进内核）
# 于 /opt/linux_binary_cache/vmlinuz（Dockerfile 下好；prepare_host.sh 也指向它）。
# ext4 job 已登记在 fio/summary.yaml，各带 bench_result.yaml 解析配置。
#
# 【用法】仓库根目录、dev 容器内：
#   bash test/initramfs/src/benchmark/run_ext4_bench.sh <suite/job> [both|aster|linux]
#     <suite/job> = fio/ext4_buffered_seq_write_bw
#                 | fio/ext4_buffered_seq_read_bw   （读分 cold/warm 两次）
#                 | sqlite/ext4_benchmarks          （speedtest1；见下警告）
#     侧           = both（默认）| aster | linux
#   例： bash test/initramfs/src/benchmark/run_ext4_bench.sh fio/ext4_buffered_seq_write_bw
#
# 【ext4 盘怎么来】benchmark 默认 boot 只挂 ext2.img/exfat.img、不挂 ext4 盘。
# 本脚本复用 xfstests 的盘槽：先把 xfstests_test.img **mke2fs 成一块干净 ext4**，
# 再用 ATTACH_XFSTESTS_IMAGES=true 把它挂到 /dev/vdc，job 的 run.sh 去 mount vdc。
#
# 【必须串行】基准一次只能跑一个 boot（并发会污染吞吐数）；本脚本本身串行，
#   但**你别在别处同时跑 QEMU/构建**。
#
# 【口径警告 —— 见 test.md §6.4】
#   * host drop_caches 需要特权容器；没特权时 **冷读**被宿主镜像的 page cache
#     污染（不可信）。只有 **write 和 warm-read** 的 ratio 干净。
#   * **SQLite speedtest1 跑不完**：崩于 test 150 CREATE INDEX（reserialize
#     mega-buffer 堆耗尽 —— P9 `p9-perf-basket`/`extent-flatten-storm`）。作为
#     P9 头号债的实测证人保留；跑它是为了记录崩点，不是拿完成时间。
# ================================================================

set -e

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)   # -> asterinas/
cd "$REPO"
BUILD=test/initramfs/build
JOB="${1:?用法: run_ext4_bench.sh <suite/job> [both|aster|linux]}"
SIDE="${2:-both}"
LINUX_KERNEL="${LINUX_KERNEL:-/opt/linux_binary_cache/vmlinuz}"

# 每次跑前把 ext4 测试盘重置成干净 2G 卷（可挂的保守特性集）。
prep_ext4_disk() {
    rm -f "$BUILD/xfstests_test.img"
    truncate -s 2G "$BUILD/xfstests_test.img"
    mke2fs -F -q -t ext4 -b 4096 \
        -O has_journal,extent,filetype,^metadata_csum,^dir_index,^64bit,^flex_bg \
        "$BUILD/xfstests_test.img"
    sync
    echo 3 >/proc/sys/vm/drop_caches 2>/dev/null || \
        echo "  (drop_caches 被拒：容器无特权 —— 冷读口径不可信，见脚本头)" >&2
}

run_aster() {
    prep_ext4_disk
    ATTACH_XFSTESTS_IMAGES=true make run_kernel "BENCHMARK=$JOB"
}

run_linux() {
    prep_ext4_disk
    # 手挂 ext4 盘到 vdc（0x9=第 3 块 virtio-blk）；user-net（fio 本地盘不需要网）。
    timeout 1200 qemu-system-x86_64 --no-reboot -smp 1 -m 8G \
        -machine q35,kernel-irqchip=split --enable-kvm \
        -cpu Icelake-Server,-pcid,+x2apic \
        -kernel "$LINUX_KERNEL" -initrd "$BUILD/initramfs.cpio.gz" \
        -drive if=none,format=raw,id=x0,file="$BUILD/ext2.img" \
        -device virtio-blk-pci,bus=pcie.0,addr=0x6,drive=x0,serial=vext2,disable-legacy=on,disable-modern=off \
        -drive if=none,format=raw,id=x1,file="$BUILD/exfat.img" \
        -device virtio-blk-pci,bus=pcie.0,addr=0x7,drive=x1,serial=vexfat,disable-legacy=on,disable-modern=off \
        -drive if=none,format=raw,id=x2,file="$BUILD/xfstests_test.img" \
        -device virtio-blk-pci,bus=pcie.0,addr=0x9,drive=x2,serial=vxfstest,disable-legacy=on,disable-modern=off \
        -netdev user,id=net01 \
        -device virtio-net-pci,netdev=net01,disable-legacy=on,disable-modern=off \
        -append "console=ttyS0 rdinit=/benchmark/common/bench_runner.sh $JOB linux mitigations=off hugepages=0 transparent_hugepage=never quiet" \
        -nographic
}

case "$SIDE" in
    aster) run_aster ;;
    linux) run_linux ;;
    both)
        echo "======== ASTERINAS: $JOB ========"
        run_aster
        echo "======== LINUX ($LINUX_KERNEL): $JOB ========"
        run_linux
        ;;
    *) echo "未知侧: $SIDE（应为 both|aster|linux）" >&2; exit 2 ;;
esac
