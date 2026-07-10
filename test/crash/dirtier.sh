#!/bin/sh

# SPDX-License-Identifier: MPL-2.0
#
# The --dirty-start stage-1 prologue (see run_matrix.sh): dirties the
# journal, then erases its own traces so the SAME baked workload list can
# run again in stage 2 on the hard-cut disk.
#
# Installed by run_matrix.sh as `.crash/00-dirty-dirtier.sh` — the name
# sorts FIRST, so this runs before every corpus workload and the cut point
# (the first FLUSH where this script's cleanup is durable) predates any
# corpus activity: the cut image carries corpus-free state, a dirty journal
# whose replay stage 2's mount performs on camera, and no marker-byte
# residue that could confuse the stage-2 oracle.
#
# Runs like any crash workload: CWD = wd_00-dirty-dirtier on the test disk.
#
# Three acts:
#   1. journal dirt: fsynced creates, a rename and an unlink — committed
#      transactions that lazy checkpoint keeps journal-only, so the
#      stage-2 mount has real replay work;
#   2. self-cleanup: remove the working directory (the guest runner
#      mkdir-s it and would abort stage 2 on a survivor);
#   3. cut beacon: persist the cleanup and drop a sentinel file whose
#      durable presence (checked post-replay by run_matrix's cut search,
#      together with needs_recovery still set) marks the usable cut.

set -eu

for i in 1 2 3 4 5 6 7 8; do
    xfs_io -f -c "pwrite -S 0x37 0 8192" -c "fsync" "f$i" > /dev/null
done
mkdir sub
mv f1 sub/g1
xfs_io -r -c "fsync" .
rm f2
xfs_io -r -c "fsync" .

cd ..
rm -rf wd_00-dirty-dirtier
echo DIRTY-START-CUT-v1 > .dirty_cut_sentinel
xfs_io -r -c "fsync" .dirty_cut_sentinel
xfs_io -r -c "fsync" .
