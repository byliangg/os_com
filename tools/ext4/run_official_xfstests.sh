#!/usr/bin/env bash

# Run the official competition functional-correctness xfstests list
# (test/initramfs/src/syscall/xfstests/testcases/official.list) inside Docker.
#
# Usage:
#   tools/ext4/run_official_xfstests.sh
#
# Useful env overrides:
#   ENABLE_KVM=1                  (default; set 0 to disable KVM)
#   XFSTESTS_SINGLE_TEST=generic/001
#   XFSTESTS_CASE_TIMEOUT_SEC=600
#   OFFICIAL_THRESHOLD=100

set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)

export PHASE4_DOCKER_MODE=official
export ENABLE_KVM=${ENABLE_KVM:-1}

exec "${ROOT_DIR}/tools/ext4/run_phase4_in_docker.sh"
