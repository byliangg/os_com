# SPDX-License-Identifier: MPL-2.0
"""台账 → 逐 case 双侧配对与 ratio 速览（人读；正式口径=中位数，报告期再算）。

用法：p9_ratio.py [ledger.tsv]。同 key 多轮取最新行（速览用）；ratio 口径：
bw = aster/linux，time = linux/aster（都是"越高越好"的百分比）。
"""

import collections
import csv
import sys


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else "test/bench/p9_ledger.tsv"
    rows = list(csv.DictReader(open(path), delimiter="\t"))
    data = collections.defaultdict(dict)
    for r in rows:
        if r["status"] != "OK" or r["metric"] not in ("bw", "time"):
            continue
        key = (r["case"], r["phase"], r["target"], r["metric"])
        data[key][r["side"]] = float(r["value"])

    print(f'{"case":<18}{"phase":<6}{"target":<11}{"aster":>12}{"linux":>12}{"ratio":>8}')
    for (case, phase, target, metric), sides in sorted(data.items()):
        a, l = sides.get("aster"), sides.get("linux")
        if a and l:
            ratio = f"{(l / a if metric == 'time' else a / l) * 100:.0f}%"
        else:
            ratio = "-"
        fa = f"{a:,.0f}" if a else "-"
        fl = f"{l:,.0f}" if l else "-"
        print(f"{case:<18}{phase:<6}{target:<11}{fa:>12}{fl:>12}{ratio:>8}")


if __name__ == "__main__":
    main()
