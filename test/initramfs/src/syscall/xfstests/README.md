# xfstests Syscall Suite (Phase3)

This suite integrates `xfstests` into Asterinas `AUTO_TEST=syscall` pipeline.

## Required host input

Set `XFSTESTS_PREBUILT_DIR` before building initramfs. The directory must contain:

1. `xfstests-dev/` (with executable `check` script)
2. Optional `tools/bin/` helper binaries used by xfstests scripts

## Runtime modes

1. `XFSTESTS_MODE=phase3_base` (default): blocking mode, computes pass-rate with phase3 rules.
2. `XFSTESTS_MODE=generic_quick`: observation-only mode, always exits success and records log.

## Pass-rate rules (phase3_base)

1. Denominator = `PASS + FAIL`
2. Numerator = `PASS`
3. `NOTRUN`/`STATIC_BLOCKED` do not count in denominator and are written with reasons
4. Threshold = `XFSTESTS_THRESHOLD_PERCENT` (default: `90`)

## Output files

Under `XFSTESTS_RESULTS_DIR` (default: `/tmp/xfstests_results`):

1. `<mode>_results.tsv`
2. `<mode>_summary.tsv`
3. `<mode>_excluded.tsv`
4. Per-test logs (`generic_001.log`, ...)
