#!/bin/sh

# SPDX-License-Identifier: MPL-2.0

set -eu

# RUNTIME_PATH is substituted by the Nix build.
export PATH=__RUNTIME_PATH__

XFSTESTS_DIR=/opt/xfstests
cd "$XFSTESTS_DIR"

TEST_DEV=${XFSTESTS_TEST_DEV:-/dev/vdc}
SCRATCH_DEV=${XFSTESTS_SCRATCH_DEV:-/dev/vdd}
export TEST_DEV SCRATCH_DEV

# FSTYP comes from local.config — the same file ./check reads — so the mounts
# below always use the filesystem the suite is configured for.
. "$XFSTESTS_DIR/local.config"

# Mount the test image with explicit error checking so a mount failure is not
# silently skipped (which would cause ./check to run against empty directories
# and still print the "all passed" success line).
#
# The scratch device is deliberately NOT pre-mounted: xfstests wants
# SCRATCH_DEV unmounted between tests (./check re-mkfses it per test via
# _scratch_mkfs), and pre-mounting couples every run to whatever filesystem the
# previous run's last scratch test left on the image — e.g. a sized-down
# _scratch_mkfs_sized leftover that the kernel under test may rightly refuse.
for dev in "$TEST_DEV" "$SCRATCH_DEV"; do
    if [ ! -b "$dev" ]; then
        echo "Expected $dev to be a block device for xfstests" >&2
        exit 1
    fi
done
if ! mount -t "$FSTYP" "$TEST_DEV" "$XFSTESTS_DIR/test"; then
    echo "Failed to mount $TEST_DEV on $XFSTESTS_DIR/test (test)" >&2
    exit 1
fi
if ! mountpoint -q "$XFSTESTS_DIR/test"; then
    echo "test dir is not a mountpoint after mount(8) succeeded" >&2
    exit 1
fi

# Crash-workload mode (test/crash/run_matrix.sh): the recorded test disk
# arrives pre-populated with workload scripts under /.crash (mke2fs -d); run
# each in its own directory and exit — the crash states are reconstructed and
# judged on the host from the blklogwrites log, not in here.
if [ -n "${CRASH_WORKLOADS:-}" ]; then
    cd "$XFSTESTS_DIR/test"
    fails=0
    for w in .crash/*.sh; do
        [ -f "$w" ] || continue
        wd="wd_$(basename "$w" .sh)"
        mkdir "$wd"
        # A single workload script failing (e.g. GNU mv refusing a same-inode
        # rename, or any op a translation can't express faithfully) must NOT
        # abort the whole recording: the writes it already emitted are on the
        # log and every crash prefix is still judged on the host, and the
        # oracle marks an unfinished workload UNAVAIL rather than green. Log it
        # and keep going so one quirky workload cannot lose the other hundreds.
        if ! (cd "$wd" && sh "../$w"); then
            echo "crash workload $w failed (continuing)" >&2
            fails=$((fails + 1))
        fi
    done
    cd /
    echo "crash workloads done ($fails script failure(s))"
    exit 0
fi

RUNLIST_FILE=""
TEST_ARGS=""

# Parse -R flag and collect direct test names.
# Test names are simple identifiers (e.g. "generic/001") so accumulating
# them in a space-separated string is safe.
while [ $# -gt 0 ]; do
  case "$1" in
    -R|--runlist)
      if [ $# -lt 2 ]; then
        echo "Error: -R|--runlist requires a filename argument." >&2
        exit 2
      fi
      RUNLIST_FILE="$2"
      shift 2
      ;;
    --)
      shift
      TEST_ARGS="$TEST_ARGS $*"
      break
      ;;
    *)
      TEST_ARGS="$TEST_ARGS $1"
      shift
      ;;
  esac
done

if [ -n "$RUNLIST_FILE" ]; then
  if [ ! -f "$RUNLIST_FILE" ]; then
    echo "Run list file not found: $RUNLIST_FILE" >&2
    exit 2
  fi
  while IFS= read -r test; do
    case "$test" in
      ""|\#*) continue ;;
    esac
    TEST_ARGS="$TEST_ARGS $test"
  done < "$RUNLIST_FILE"
fi

# Prepend block-list exclusion so blocked tests are skipped.
if [ -f "$XFSTESTS_DIR/block.list" ]; then
    TEST_ARGS="-E $XFSTESTS_DIR/block.list $TEST_ARGS"
fi

# Word-splitting is intentional here: TEST_ARGS contains only test names
# and the -E flag, none of which contain whitespace or shell metacharacters.
# shellcheck disable=SC2086
./check $TEST_ARGS
