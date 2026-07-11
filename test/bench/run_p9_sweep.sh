#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
#
# ================================================================
#  P9 计分板 sweep —— Asterinas vs Linux 同轮矩阵跑批 + TSV 台账
# ================================================================
#
# 【做什么】把 P9_plan §7.1 计分板的每个 case 在 Asterinas 和参照 Linux 上
# 各跑一遍（同 QEMU+KVM+virtio-blk、同一块每 case 重新 mke2fs 的盘、boot 前
# host drop_caches），解析两侧输出，追加进单一 TSV 台账（test.md §6.5 的
# "单一真值源"）。取代 run_ext4_bench.sh 的单 job 手跑做矩阵。
#
# 【怎么参数化】guest 侧只有一个参数化 job（fio/ext4_p9_param），参数经
# ext2 盘根目录的 /ext2/p9cfg 传递（host 用 debugfs 免挂载写入）——改参数
# 零重建、aster/linux 两侧同通道。SQLite 走独立 job（sqlite/ext4_benchmarks，
# 已绑 integrity_check）。
#
# 【用法】dev 容器内、仓库根：
#   bash test/bench/run_p9_sweep.sh                 # 全计分板 × both × 1 轮
#   ONLY='^d_' bash test/bench/run_p9_sweep.sh      # 只跑 D 组
#   SIDES=aster ROUNDS=3 bash ...                   # 单侧 / 多轮（中位数口径）
#   ONLY=selfcheck_hang bash ...                    # 超时自校准（必须记 HANG）
# 环境旋钮：ONLY(case 名正则) SIDES(aster,linux) ROUNDS TIMEOUT_S(默认 1800)
#           LEDGER EVIDENCE_DIR MEM(默认 8G) RELEASE(默认 1)
#
# 【口径纪律（test.md §6.3/§6.4）】
#   * 必须串行独占——别在别处同时跑 QEMU/构建/矩阵。
#   * host drop_caches 必须成功，失败整批中止（冷读口径的前提；本容器已实测
#     有特权——P9_milestone §2 环境发现）。
#   * aster 侧默认 RELEASE=1（对 Linux 公平；debug 内核数字不进台账）。
#   * 镜像默认全特性（与崩溃矩阵/xfstests 同口径，P9_plan §10-⑥）；
#     nojournal target 仅归因用。
#   * 台账只追加不改写；解析失败/挂死/崩溃如实记 status 行，不静默丢。
# ================================================================

set -uo pipefail

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$REPO"
BUILD=test/initramfs/build

FIO_JOB=fio/ext4_p9_param
SQLITE_JOB=sqlite/ext4_benchmarks
PARSER=test/bench/p9_parse.py

ONLY="${ONLY:-.}"
SIDES="${SIDES:-aster,linux}"
ROUNDS="${ROUNDS:-1}"
TIMEOUT_S="${TIMEOUT_S:-1800}"
MEM="${MEM:-8G}"
RELEASE="${RELEASE:-1}"
LINUX_KERNEL="${LINUX_KERNEL:-/opt/linux_binary_cache/vmlinuz}"
COMMIT=$(git rev-parse --short HEAD)
STAMP=$(date +%Y%m%d-%H%M)
LEDGER="${LEDGER:-test/bench/p9_ledger.tsv}"
EVIDENCE_DIR="${EVIDENCE_DIR:-test/bench/evidence/${STAMP}-${COMMIT}}"
mkdir -p "$EVIDENCE_DIR"

# ---- 计分板 case 表（P9_plan §7.1）----------------------------------------
# 格式：name|job|mode|rw|bs|nj|fsync|size|target
#   target: journaled(全特性) / nojournal(归因) / raw(裸盘地板，不建 fs)
# D 组主战场；C 组 bs 扫描（1M 由 D 覆盖）；E 组 fsync 语义线；F 组并发；
# raw/nojournal 归因腿；selfcheck_hang 是判官自校准（永不进计分，必须 HANG）。
CASES="
d_write_1m|$FIO_JOB|write|write|1M|1|0|1G|journaled
d_read_1m|$FIO_JOB|read|read|1M|1|0|1G|journaled
c_write_4k|$FIO_JOB|write|write|4K|1|0|1G|journaled
c_read_4k|$FIO_JOB|read|read|4K|1|0|1G|journaled
c_write_16k|$FIO_JOB|write|write|16K|1|0|1G|journaled
c_read_16k|$FIO_JOB|read|read|16K|1|0|1G|journaled
c_write_64k|$FIO_JOB|write|write|64K|1|0|1G|journaled
c_read_64k|$FIO_JOB|read|read|64K|1|0|1G|journaled
c_write_256k|$FIO_JOB|write|write|256K|1|0|1G|journaled
c_read_256k|$FIO_JOB|read|read|256K|1|0|1G|journaled
e_write_fsync4|$FIO_JOB|write|write|1M|1|4|1G|journaled
e_write_fsync16|$FIO_JOB|write|write|1M|1|16|1G|journaled
e_write_fsync64|$FIO_JOB|write|write|1M|1|64|1G|journaled
f_write_nj2|$FIO_JOB|write|write|1M|2|0|1G|journaled
f_write_nj4|$FIO_JOB|write|write|1M|4|0|1G|journaled
raw_write|$FIO_JOB|raw|write|1M|1|0|1G|raw
raw_read|$FIO_JOB|raw|read|1M|1|0|1G|raw
nj_write_1m|$FIO_JOB|write|write|1M|1|0|1G|nojournal
nj_read_1m|$FIO_JOB|read|read|1M|1|0|1G|nojournal
sqlite_speedtest|$SQLITE_JOB|sqlite|-|-|-|-|1000|journaled
selfcheck_hang|$FIO_JOB|hang|-|-|-|-|-|journaled
"

# ---- 台账 -----------------------------------------------------------------
if [ ! -f "$LEDGER" ]; then
    printf 'date\tcommit\tround\tside\ttarget\tcase\tmode\trw\tbs\tnj\tfsync\tsize\tmetric\tphase\tvalue\tunit\tstatus\tnotes\n' > "$LEDGER"
fi

record() { # round side target name mode rw bs nj fsync size metric phase value unit status notes
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$STAMP" "$COMMIT" "$@" >> "$LEDGER"
}

# ---- 口径与盘准备 ----------------------------------------------------------
drop_host_caches() {
    sync
    if ! echo 3 > /proc/sys/vm/drop_caches 2>/dev/null; then
        echo "FATAL: drop_caches 失败——冷读口径不成立，整批中止（容器需特权）" >&2
        exit 3
    fi
}

prep_disk() { # target
    rm -f "$BUILD/xfstests_test.img"
    truncate -s 2G "$BUILD/xfstests_test.img"
    case "$1" in
    raw) : ;; # 裸盘地板：不建 fs
    journaled)
        mke2fs -F -q -t ext4 -b 4096 -I 256 \
            -O has_journal,extent,filetype,metadata_csum,dir_index,64bit,flex_bg,^inline_data \
            "$BUILD/xfstests_test.img" ;;
    nojournal)
        mke2fs -F -q -t ext4 -b 4096 -I 256 \
            -O ^has_journal,extent,filetype,metadata_csum,dir_index,64bit,flex_bg,^inline_data \
            "$BUILD/xfstests_test.img" ;;
    *) echo "FATAL: unknown target $1" >&2; exit 2 ;;
    esac
    sync
}

write_cfg() { # mode rw bs nj fsync size
    local tmp
    tmp=$(mktemp)
    printf 'MODE=%s\nRW=%s\nBS=%s\nNJ=%s\nFSYNC=%s\nSIZE=%s\n' "$@" > "$tmp"
    debugfs -w -R "rm p9cfg" "$BUILD/ext2.img" >/dev/null 2>&1 || true
    debugfs -w -R "write $tmp p9cfg" "$BUILD/ext2.img" >/dev/null 2>&1
    # 写入核验：读回与写入逐字节一致，否则中止（配置错 = 测错 case，比不跑更糟）。
    if ! debugfs -R "cat p9cfg" "$BUILD/ext2.img" 2>/dev/null | diff -q - "$tmp" >/dev/null; then
        echo "FATAL: p9cfg 写入 ext2.img 后读回不一致" >&2
        rm -f "$tmp"; exit 2
    fi
    rm -f "$tmp"; sync
}

# ---- 两侧 boot -------------------------------------------------------------
kill_leaked_qemu() {
    pkill -f "qemu-system-x86_64.*xfstests_test.img" 2>/dev/null || true
    sleep 2
}

boot_aster() { # job log
    # </dev/null：qemu(-nographic) 会读继承的 stdin——不隔离它就会吃掉
    # 外层 while read 的 case 表管道，第一个 case 后整批静默终止。
    ATTACH_XFSTESTS_IMAGES=true timeout -k 30 "$TIMEOUT_S" \
        make run_kernel "BENCHMARK=$1" "RELEASE=$RELEASE" "MEM=$MEM" \
        > "$2" 2>&1 < /dev/null
}

boot_linux() { # job log —— qemu 参数与 run_ext4_bench.sh run_linux 一致
    timeout -k 30 "$TIMEOUT_S" \
        qemu-system-x86_64 --no-reboot -smp 1 -m "$MEM" \
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
        -append "console=ttyS0 rdinit=/benchmark/common/bench_runner.sh $1 linux mitigations=off hugepages=0 transparent_hugepage=never quiet" \
        -nographic > "$2" 2>&1 < /dev/null
}

# ---- 主循环 ----------------------------------------------------------------
echo "== P9 sweep @ $COMMIT | rounds=$ROUNDS sides=$SIDES only='$ONLY' =="
echo "== 台账: $LEDGER | 证据: $EVIDENCE_DIR =="

# 预热：initramfs（打包新 job/sqlite3）先建好，别让第一个 case 的超时背构建账。
make initramfs "BENCHMARK=$FIO_JOB" > "$EVIDENCE_DIR/initramfs-build.log" 2>&1 \
    || { echo "FATAL: initramfs 构建失败，见 $EVIDENCE_DIR/initramfs-build.log" >&2; exit 2; }

for round in $(seq 1 "$ROUNDS"); do
  echo "$CASES" | grep -v '^[[:space:]]*$' | while IFS='|' read -r name job mode rw bs nj fsync size target; do
    [[ "$name" =~ $ONLY ]] || continue
    # 自校准 case 只在被显式点名时跑（全量 sweep 里它是纯烧超时的死时间）。
    if [[ "$name" == selfcheck_* && "$ONLY" == "." ]]; then continue; fi
    for side in ${SIDES//,/ }; do
        log="$EVIDENCE_DIR/${name}-${side}-r${round}.log"
        echo "-- [$round/$ROUNDS] $name ($side, target=$target) --"
        prep_disk "$target"
        if [ "$mode" != "sqlite" ]; then
            write_cfg "$mode" "$rw" "$bs" "$nj" "$fsync" "$size"
        fi
        drop_host_caches
        rc=0
        case "$side" in
            aster) boot_aster "$job" "$log" || rc=$? ;;
            linux) boot_linux "$job" "$log" || rc=$? ;;
        esac
        if [ "$rc" -eq 124 ] || [ "$rc" -eq 137 ]; then
            kill_leaked_qemu
            record "$round" "$side" "$target" "$name" "$mode" "$rw" "$bs" "$nj" "$fsync" "$size" \
                   "-" "-" "-" "-" "HANG" "timeout ${TIMEOUT_S}s"
            echo "   -> HANG (recorded)"
            continue
        fi
        # 解析该 boot 的输出段，逐 metric 追加台账（解析器出 TSV 后缀列）。
        if ! python3 "$PARSER" "$log" "$mode" | while IFS=$'\t' read -r metric phase value unit status notes; do
            record "$round" "$side" "$target" "$name" "$mode" "$rw" "$bs" "$nj" "$fsync" "$size" \
                   "$metric" "$phase" "$value" "$unit" "$status" "$notes"
        done; then
            record "$round" "$side" "$target" "$name" "$mode" "$rw" "$bs" "$nj" "$fsync" "$size" \
                   "-" "-" "-" "-" "PARSE_FAIL" "see $log"
        fi
    done
  done
done

echo "== sweep 完成，台账尾部: =="
tail -5 "$LEDGER"
