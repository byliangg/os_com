# SPDX-License-Identifier: MPL-2.0
"""P9 sweep 输出解析器：从一次 boot 的控制台日志提取测量行。

用法：p9_parse.py <log> <mode>；stdout 每行一条 TSV 后缀列：
    metric \t phase \t value \t unit \t status \t notes

协议（guest 侧 fio/ext4_p9_param/run.sh 与 sqlite/ext4_benchmarks/run.sh）：
测量段夹在 `P9CASE <名>` / `P9DONE <名>` 之间；fio 段取 READ:/WRITE: 汇总行
的 bw=（归一化 KiB/s），sqlite 段取 `real` 时间（秒）与 integrity 的 "ok"。
段开了没关 = INCOMPLETE（崩溃中途，如实入账）；整个日志一个标记都没有 =
NO_MARKER（boot 没跑到 job，环境问题不是测量值）。
"""

import re
import sys

BW_RE = re.compile(r"^\s*(READ|WRITE):\s+bw=([\d.]+)([KMG]i?B)/s")
TIME_RE = re.compile(r"^real\s+(\d+)m\s*([\d.]+)s")
SPEEDTEST_STEP_RE = re.compile(r"^\s*(\d+) - ")

UNIT_TO_KIB = {
    "KiB": 1.0,
    "MiB": 1024.0,
    "GiB": 1024.0 * 1024.0,
    # fio 汇总行主值用二进制单位；十进制括号值不取。保底映射防万一：
    "KB": 1000.0 / 1024.0,
    "MB": 1000.0 * 1000.0 / 1024.0,
    "GB": 1000.0**3 / 1024.0,
}


def emit(metric, phase, value, unit, status, notes=""):
    print(f"{metric}\t{phase}\t{value}\t{unit}\t{status}\t{notes}")


def segments(lines):
    """(名, 段内行, 是否闭合) 列表。"""
    out = []
    name, buf = None, []
    for line in lines:
        line = line.rstrip("\r\n")
        if line.startswith("P9CASE "):
            if name is not None:
                out.append((name, buf, False))
            name, buf = line[len("P9CASE ") :].strip(), []
        elif line.startswith("P9DONE "):
            if name is not None:
                out.append((name, buf, True))
                name, buf = None, []
        elif name is not None:
            buf.append(line)
    if name is not None:
        out.append((name, buf, False))
    return out


def phase_of(seg_name):
    if seg_name.endswith("-cold"):
        return "cold"
    if seg_name.endswith("-warm"):
        return "warm"
    return "-"


def parse_fio_segment(seg_name, buf, closed):
    for line in buf:
        m = BW_RE.match(line)
        if m:
            _, val, unit = m.groups()
            kib = float(val) * UNIT_TO_KIB[unit]
            status = "OK" if closed else "INCOMPLETE"
            emit("bw", phase_of(seg_name), f"{kib:.0f}", "KiB/s", status)
            return
    emit("bw", phase_of(seg_name), "-", "-", "INCOMPLETE" if not closed else "NO_BW",
         f"segment {seg_name} has no fio summary line")


def parse_sqlite_segment(seg_name, buf, closed):
    if seg_name == "sqlite-speedtest1":
        secs = None
        last_step = ""
        for line in buf:
            m = TIME_RE.match(line)
            if m:
                secs = int(m.group(1)) * 60 + float(m.group(2))
            m = SPEEDTEST_STEP_RE.match(line)
            if m:
                last_step = line.strip()[:48]
        if secs is not None and closed:
            emit("time", "-", f"{secs:.1f}", "s", "OK")
        else:
            emit("time", "-", "-", "s", "INCOMPLETE", f"last: {last_step}")
    elif seg_name == "sqlite-integrity":
        ok = any(line.strip() == "ok" for line in buf)
        if ok and closed:
            emit("integrity", "-", "1", "ok", "OK")
        else:
            emit("integrity", "-", "-", "-", "INCOMPLETE" if not closed else "FAIL")


def main():
    log_path, mode = sys.argv[1], sys.argv[2]
    with open(log_path, errors="replace") as f:
        lines = f.readlines()

    segs = segments(lines)
    if not segs:
        emit("-", "-", "-", "-", "NO_MARKER", "boot produced no P9CASE marker")
        return

    for seg_name, buf, closed in segs:
        if mode == "sqlite":
            parse_sqlite_segment(seg_name, buf, closed)
        else:
            parse_fio_segment(seg_name, buf, closed)

    if not any("P9ALLDONE" in line for line in lines):
        emit("-", "-", "-", "-", "NO_ALLDONE", "job did not run to completion")


if __name__ == "__main__":
    main()
