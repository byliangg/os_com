#!/bin/sh

# SPDX-License-Identifier: MPL-2.0
#
# P9 参数化 FIO job（guest 侧）——一个 job 目录跑完整个计分板矩阵。
#
# 参数不走构建期 init-args（asterinas 侧改参数就要重建内核），而是由 host 的
# run_p9_sweep.sh 在每次 boot 前用 debugfs 写进 ext2 盘根目录的 /ext2/p9cfg
# （shell 变量格式），本脚本 source 它。/ext2 两侧都有：asterinas 内核自动挂，
# Linux 侧 bench_runner.sh 的 prepare_system 挂——同一通道、零重建、两侧对称。
#
# 输出协议（host 侧 test/bench/p9_parse.py 消费）：每个测量段夹在
# `P9CASE <名>` / `P9DONE <名>` 之间，fio 的人类可读输出原样打印，host 从段内
# 提取 READ:/WRITE: 汇总行的 bw。脚本尾打 `P9ALLDONE`——没有它 = 中途死亡。
#
# MODE 取值：
#   write  挂 /dev/vdc 顺序写（BS/NJ/FSYNC 生效；fsync_on_close=1 恒开）
#   read   先 prep 写 1G，umount/mount 清 guest 页缓存后 cold 读，再 warm 读
#   raw    不建 fs，fio 直打 /dev/vdc（裸盘地板，三向归因的 raw 腿）
#   hang   故意挂死（sweep 的 per-case 超时自校准用——判官要能红）

set -e

CFG=/ext2/p9cfg
if [ ! -f "$CFG" ]; then
    echo "P9ERR missing $CFG"
    exit 1
fi
. "$CFG"

MODE="${MODE:?p9cfg missing MODE}"
RW="${RW:-write}"
BS="${BS:-1M}"
NJ="${NJ:-1}"
FSYNC="${FSYNC:-0}"
SIZE="${SIZE:-1G}"
echo "P9CFG mode=$MODE rw=$RW bs=$BS nj=$NJ fsync=$FSYNC size=$SIZE"

FIO=/benchmark/bin/fio
FSYNC_OPT=""
if [ "$FSYNC" != "0" ]; then
    FSYNC_OPT="-fsync=$FSYNC"
fi

case "$MODE" in
hang)
    echo "P9CASE hang"
    # 永不结束：host 侧 timeout 必须把本 boot 记成 HANG 而不是挂死整批。
    while true; do sleep 60; done
    ;;
raw)
    echo "P9CASE raw-$RW"
    $FIO -rw="$RW" -filename=/dev/vdc -name="raw-$RW" -size="$SIZE" -bs="$BS" \
        -ioengine=sync -direct=0 -numjobs="$NJ" $FSYNC_OPT
    echo "P9DONE raw-$RW"
    ;;
write)
    mkdir -p /ext4
    mount -t ext4 /dev/vdc /ext4
    echo "P9CASE write"
    $FIO -rw=write -filename=/ext4/fio-test -name=p9write -size="$SIZE" -bs="$BS" \
        -ioengine=sync -direct=0 -numjobs="$NJ" $FSYNC_OPT -fsync_on_close=1
    echo "P9DONE write"
    ;;
read)
    mkdir -p /ext4
    mount -t ext4 /dev/vdc /ext4
    $FIO -rw=write -filename=/ext4/fio-test -name=prep -size="$SIZE" -bs=1M \
        -ioengine=sync -direct=0 -numjobs=1 -fsync_on_close=1 > /dev/null
    # umount/mount 丢弃 guest 页缓存；host 页缓存由 sweep 的 boot 前 drop 保证，
    # 二者齐备 cold 才是真设备路径（test.md §6.4）。
    umount /ext4
    mount -t ext4 /dev/vdc /ext4
    echo "P9CASE read-cold"
    $FIO -rw=read -filename=/ext4/fio-test -name=p9read-cold -size="$SIZE" -bs="$BS" \
        -ioengine=sync -direct=0 -numjobs="$NJ"
    echo "P9DONE read-cold"
    echo "P9CASE read-warm"
    $FIO -rw=read -filename=/ext4/fio-test -name=p9read-warm -size="$SIZE" -bs="$BS" \
        -ioengine=sync -direct=0 -numjobs="$NJ"
    echo "P9DONE read-warm"
    ;;
directwrite)
    mkdir -p /ext4
    mount -t ext4 /dev/vdc /ext4
    echo "P9CASE direct-write"
    $FIO -rw=write -filename=/ext4/fio-test -name=p9dwrite -size="$SIZE" -bs="$BS" \
        -ioengine=sync -direct=1 -numjobs="$NJ" $FSYNC_OPT
    echo "P9DONE direct-write"
    ;;
directread)
    mkdir -p /ext4
    mount -t ext4 /dev/vdc /ext4
    # Buffered prep lays the file down; the measured read is O_DIRECT, which
    # bypasses the page cache by construction — no cold/warm split.
    $FIO -rw=write -filename=/ext4/fio-test -name=prep -size="$SIZE" -bs=1M \
        -ioengine=sync -direct=0 -numjobs=1 -fsync_on_close=1 > /dev/null
    echo "P9CASE direct-read"
    $FIO -rw=read -filename=/ext4/fio-test -name=p9dread -size="$SIZE" -bs="$BS" \
        -ioengine=sync -direct=1 -numjobs="$NJ"
    echo "P9DONE direct-read"
    ;;
*)
    echo "P9ERR unknown MODE=$MODE"
    exit 1
    ;;
esac

echo "P9ALLDONE"
