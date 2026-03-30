#!/bin/sh

# SPDX-License-Identifier: MPL-2.0

set -e

SYSCALL_TEST_SUITE=${SYSCALL_TEST_SUITE:-ltp}
LTP_DIR=/opt/ltp
GVISOR_DIR=/opt/gvisor
XFSTESTS_DIR=/opt/xfstests
EXT4_CRASH_DIR=/opt/ext4_crash

if [ "${SYSCALL_TEST_SUITE}" == "ltp" ]; then
    echo "Running LTP syscall tests..."
    if ! "${LTP_DIR}/run_ltp_test.sh"; then
        echo "Error: LTP syscall tests failed." >&2
        exit 1
    fi
elif [ "${SYSCALL_TEST_SUITE}" == "gvisor" ]; then
    echo "Running gVisor syscall tests..."
    if ! "${GVISOR_DIR}/run_gvisor_test.sh"; then
        echo "Error: gVisor syscall tests failed." >&2
        exit 2
    fi
elif [ "${SYSCALL_TEST_SUITE}" == "xfstests" ]; then
    echo "Running xfstests..."
    if ! "${XFSTESTS_DIR}/run_xfstests_test.sh"; then
        echo "Error: xfstests failed." >&2
        exit 4
    fi
elif [ "${SYSCALL_TEST_SUITE}" == "ext4_crash" ]; then
    echo "Running ext4 crash-recovery tests..."
    if ! "${EXT4_CRASH_DIR}/run_ext4_crash_test.sh"; then
        echo "Error: ext4 crash-recovery tests failed." >&2
        exit 5
    fi
else
    echo "Error: Unknown test suite '${SYSCALL_TEST_SUITE}'." >&2
    exit 3
fi

echo "All syscall tests passed."
exit 0
