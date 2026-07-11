# test/bench — P9 性能计分板跑批与台账（host 侧）

P9 性能线的单一真值台账（test.md §6.5）与跑批工具。guest 侧的参数化 job 在
`test/initramfs/src/benchmark/fio/ext4_p9_param/`（经 `/ext2/p9cfg` 传参，见其头注）。

| 文件 | 说明 |
|---|---|
| `run_p9_sweep.sh` | 计分板 sweep（P9_plan §7.1 case 表 × aster/linux × 轮次）；用法见头注 |
| `p9_parse.py` | 控制台日志 → TSV 测量行（P9CASE/P9DONE 协议） |
| `p9_ledger.tsv` | **单一真值台账**（追加式，带 commit/口径全字段；报告只读它） |
| `evidence/` | 每轮 sweep 的逐 boot 原始日志（`<stamp>-<commit>/<case>-<side>-r<n>.log`） |

口径纪律（违者数字不进台账）：串行独占；host drop_caches 必须成功（特权容器）；
aster 侧 RELEASE=1；镜像全特性默认（nojournal 仅归因）；中位数 ≥3 轮才做 ratio
结论；HANG/CRASH/INCOMPLETE 如实入账。判官自校准：`ONLY=selfcheck_hang` 必须
产出 HANG 行（能红），`ONLY=d_write_1m` 两侧数量级对上 test.md §6.6 旧地板（能绿）。
