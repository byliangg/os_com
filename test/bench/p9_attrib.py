#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
#
# P9b-b1 三向归因：读 p9_ledger.tsv（单一真值源），按 case 取中位数，产
# 归因表（P9b_plan §1.1）。
#
# 每 case 报 journaled ratio（aster/linux 中位数）；在 1M 顺序读写这两个
# 有完整 raw/nojournal/journaled 三 target 覆盖的代表 case 上分解四比值：
#   T_j  = aster-nojournal ÷ aster-journaled   （journal 税；>1 = journal 在扣）
#   T_f  = aster-raw ÷ aster-nojournal          （fs 层税）
#   W    = linux-raw ÷ aster-raw                （平台/块层墙）
#   E_L  = linux-ext4 ÷ linux-raw               （对照系自身折损）
# 恒等式近似：linux-ext4 ÷ aster-journaled ≈ T_j × T_f × W × E_L。
# 归因规则 = 最大因子命名层；W 主导 → 豁免材料而非立项。
#
# 用法：python3 test/bench/p9_attrib.py [ledger.tsv]（默认 test/bench/p9_ledger.tsv）
# 输出：markdown 归因表到 stdout；激活建议按 P9b_plan §1.1 决策表。

import sys
from collections import defaultdict
from statistics import median

LEDGER = sys.argv[1] if len(sys.argv) > 1 else "test/bench/p9_ledger.tsv"

# (side, target, case, phase) -> [values]  |  sqlite: metric=time
rows = defaultdict(list)
with open(LEDGER) as f:
    for line in f:
        parts = line.rstrip("\n").split("\t")
        if len(parts) < 16 or parts[0].startswith("#") or parts[0] == "date":
            continue
        (date, commit, rnd, side, target, case, kind, rw, bs, nj, fsync,
         size, metric, phase, value, unit) = parts[:16]
        status = parts[16] if len(parts) > 16 else ""
        # 只取 surgery 后的 after 批（8ccb1fb89 起；4c48679fc/b68391d14 不
        # 改热路径，与 8ccb1fb89 同一性能面，合并为中位数样本）。
        if commit not in ("8ccb1fb89", "4c48679fc", "b68391d14"):
            continue
        if "OK" not in status and metric != "integrity":
            continue
        try:
            v = float(value)
        except ValueError:
            continue
        key = (side, target, case, phase or "-", metric)
        rows[key].append(v)


def med(side, target, case, phase="-", metric="bw"):
    vs = rows.get((side, target, case, phase, metric))
    return median(vs) if vs else None


def ratio(a, l, invert=False):
    if a is None or l is None or l == 0 or a == 0:
        return None
    return (l / a) if invert else (a / l)


def fmt(x, pct=True):
    if x is None:
        return "-"
    return f"{x * 100:.0f}%" if pct else f"{x:.2f}"


cases = sorted({(t, c, p) for (s, t, c, p, m) in rows if m in ("bw", "time")})
print("## b1 归因表（中位数口径；样本 = after 批全部轮次）\n")
print("| case | phase | aster | linux | ratio | 归因/激活 |")
print("|---|---|---:|---:|---:|---|")

WRITE_HINT = {
    "c_write_4k": "b2（小 bs 写，分配器+每页往返）",
    "c_write_16k": "b2",
    "c_write_64k": "b2 边际",
    "c_write_256k": "b2 边际",
    "d_write_1m": "≈达标（单轮方差内）",
    "e_write_fsync4": "候选池①②判据输入（flush 计数）",
    "e_write_fsync16": "达标",
    "e_write_fsync64": "候选池①②判据输入",
    "f_write_nj2": "b2 + 锁粒度观察（表④判据输入）",
    "f_write_nj4": "同上",
    "nj_write_1m": "⚠ 异常（比 journaled 慢 6×）——b1 专项归因",
    "raw_write": "W（块层墙）→ 豁免材料",
    "raw_read": "W → 豁免材料",
    "sqlite_speedtest": "b2（分配质量/元数据密集）+ b1 profile",
}

for (t, c, p) in cases:
    m = "time" if c == "sqlite_speedtest" else "bw"
    a = med("aster", t, c, p, m)
    l = med("linux", t, c, p, m)
    r = ratio(a, l, invert=(m == "time"))
    hint = ""
    if p == "cold":
        hint = "b3 readahead（头号杠杆）"
    elif p == "warm":
        hint = "达标" if (r or 0) >= 0.9 else "b3 v2 观察"
    else:
        hint = WRITE_HINT.get(c, "")
    unit = "s" if m == "time" else "KiB/s"
    print(f"| {c} | {p} | {a:,.0f} {unit} | {l:,.0f} {unit} | {fmt(r)} | {hint} |"
          if a and l else f"| {c} | {p} | - | - | - | {hint} |")

print("\n## 四比值分解（1M 顺序代表 case）\n")
print("| 方向 | T_j (nj/j) | T_f (raw/nj) | W (l-raw/a-raw) | E_L (l-ext4/l-raw) | 恒等式核对 |")
print("|---|---:|---:|---:|---:|---:|")
for rw, jcase, njcase, rawcase, phase in (
    ("write", "d_write_1m", "nj_write_1m", "raw_write", "-"),
    ("read-cold", "d_read_1m", "nj_read_1m", "raw_read", "cold"),
    ("read-warm", "d_read_1m", "nj_read_1m", "raw_read", "warm"),
):
    aj = med("aster", "journaled", jcase, phase)
    anj = med("aster", "nojournal", njcase, phase)
    araw = med("aster", "raw", rawcase, "-")
    lj = med("linux", "journaled", jcase, phase)
    lraw = med("linux", "raw", rawcase, "-")
    tj = ratio(anj, aj)
    tf = ratio(araw, anj)
    w = ratio(lraw, araw)
    el = ratio(lj, lraw)
    lhs = ratio(lj, aj)
    prod = tj * tf * w * el if None not in (tj, tf, w, el) else None
    print(f"| {rw} | {fmt(tj, False)} | {fmt(tf, False)} | {fmt(w, False)} | "
          f"{fmt(el, False)} | lhs={fmt(lhs, False)} vs ∏={fmt(prod, False)} |")

print("""
> sanity：raw ≥ nojournal ≥ journaled 若被违反（如 nj_write 异常慢于
> journaled），该方向的 T_j/T_f 分解**不可用**，先归因异常本身。
""")
