#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "${ROOT_DIR}"

SRC_XFSTESTS=${SRC_XFSTESTS:-"${ROOT_DIR}/benchmark/assets/xfstests-src"}
GENERIC_SRC="${SRC_XFSTESTS}/tests/generic"

mkdir -p benchmark/datasets/xfstests/{lists,blocked,samples/generic,licenses}
mkdir -p benchmark/datasets/results benchmark/logs benchmark/logs/lmbench

cp -f test/initramfs/src/syscall/xfstests/testcases/phase3_base.list benchmark/datasets/xfstests/lists/
cp -f test/initramfs/src/syscall/xfstests/testcases/phase4_good.list benchmark/datasets/xfstests/lists/
cp -f test/initramfs/src/syscall/xfstests/blocked/phase3_excluded.tsv benchmark/datasets/xfstests/blocked/
cp -f test/initramfs/src/syscall/xfstests/blocked/phase4_excluded.tsv benchmark/datasets/xfstests/blocked/

rm -f benchmark/datasets/xfstests/samples/generic/*
while IFS= read -r line; do
  case "$line" in ''|'#'*) continue;; esac
  case_id=${line#generic/}
  [ -f "${GENERIC_SRC}/${case_id}" ] && cp -f "${GENERIC_SRC}/${case_id}" benchmark/datasets/xfstests/samples/generic/
  for extra in "${GENERIC_SRC}/${case_id}".out "${GENERIC_SRC}/${case_id}".out.* "${GENERIC_SRC}/${case_id}".cfg; do
    [ -f "${extra}" ] && cp -f "${extra}" benchmark/datasets/xfstests/samples/generic/
  done
done < benchmark/datasets/xfstests/lists/phase4_good.list

cp -f "${SRC_XFSTESTS}/LICENSES/GPL-2.0" benchmark/datasets/xfstests/licenses/GPL-2.0 || true
cp -f "${SRC_XFSTESTS}/README" benchmark/datasets/xfstests/licenses/README.upstream || true

rev=$(git -C "${SRC_XFSTESTS}" rev-parse HEAD 2>/dev/null || echo unknown)
cat > benchmark/datasets/xfstests/README.md <<EOT
# xfstests 样例数据集（仓库内）

该数据集已提交到仓库，读取测试样例时不再依赖 \`.local\` 目录。

## 内容说明

- \`lists/\`：阶段候选用例清单。
- \`blocked/\`：静态排除用例及原因。
- \`samples/generic/\`：从上游 \`xfstests\` 拷贝的 \`tests/generic/*\` 脚本与期望输出（基于 \`phase4_good.list\`）。
- \`licenses/\`：上游许可与参考文件。

## 上游来源

- 同步来源目录：${SRC_XFSTESTS}
- 上游版本：${rev}

## 同步方式

当 list 或 excluded 变化后，执行 \`benchmark/sync_dataset.sh\` 重新同步。
EOT

copy_latest_if_exists() {
  local pattern="$1"
  local prune_glob="$2"
  local dest_dir="$3"
  local latest
  rm -f ${prune_glob} 2>/dev/null || true
  latest=$(ls -t ${pattern} 2>/dev/null | head -n 1 || true)
  if [ -n "${latest}" ]; then
    cp -f "${latest}" "${dest_dir}/"
  fi
}

# Sync latest benchmark outputs into git-friendly archive directory.
copy_latest_if_exists "benchmark/logs/phase4_good_*.log" "benchmark/datasets/results/phase4_good_*.log" "benchmark/datasets/results"
copy_latest_if_exists "benchmark/logs/phase3_base_guard_*.log" "benchmark/datasets/results/phase3_base_guard_*.log" "benchmark/datasets/results"
copy_latest_if_exists "benchmark/logs/lmbench/phase4_part3_lmbench_summary_*.tsv" "benchmark/datasets/results/phase4_part3_lmbench_summary_*.tsv" "benchmark/datasets/results"

echo "[DONE] benchmark dataset synced"
