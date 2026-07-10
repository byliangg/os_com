#!/bin/sh

# SPDX-License-Identifier: MPL-2.0
#
# protocol point: multi-descriptor transaction intermediate states (P7a).
#
# RAW workload (taken verbatim by run_matrix.sh, no oracle instrumentation:
# the J-lang subset cannot express batch creation at this scale). Builds one
# large uncommitted metadata batch — 4500 inodes across three dirs, no
# intermediate persistence — so the committing transaction's tag list spills
# over one 4K descriptor block (csum_v3 tag3 is 16 bytes: >254 blocks needs
# a second descriptor). The crash sweep then enumerates power cuts between
# the descriptor/commit writes of a multi-descriptor chain.
#
# Where the multi-descriptor form actually lands (measured 2026-07-10):
#   - DEFAULT journal: the batch must outrun the 5s age trigger; measured
#     to succeed — the protocol-library round produced a committed
#     transaction with 2 descriptor blocks (seq 333 of that recording).
#     Creation-rate dependent, so verify per recording (below).
#   - --journal-size 4 does NOT help, contrary to first intuition: the
#     quarter-journal commit trigger fires on RESERVED credits (worst-case
#     per-op estimates, P7d enforced credits), so transactions commit long
#     before 254 ACTUAL journaled blocks accumulate — all 222 commits of
#     the tiny-journal round were single-descriptor.
# Verify post-hoc against the recorded WRITE LOG (not the final image's
# journal window, and never `logdump -O` on a wrapped journal — it re-walks
# stale wraps and once emitted 15G): count jbd2 type-1 blocks per committed
# sequence, e.g. with walcheck.py's read_log as in age_evidence.py.
#
# Batches are split across three directories so the degraded-linear insert
# scan stays O(1500^2) per dir instead of O(4500^2).

set -eu

mkdir d1 d2 d3
seq -f "d1/f%04g" 1 1500 | xargs touch
seq -f "d2/f%04g" 1 1500 | xargs touch
seq -f "d3/f%04g" 1 1500 | xargs touch
xfs_io -r -c "fsync" d1
xfs_io -r -c "fsync" d2
xfs_io -r -c "fsync" d3
