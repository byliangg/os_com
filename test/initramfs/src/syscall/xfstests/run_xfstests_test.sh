#!/bin/sh

# SPDX-License-Identifier: MPL-2.0

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
XFSTESTS_ROOT=${XFSTESTS_ROOT:-/opt/xfstests}
XFSTESTS_DEV_DIR=${XFSTESTS_DEV_DIR:-${XFSTESTS_ROOT}/xfstests-dev}
TOOLS_BIN_DIR=${XFSTESTS_TOOLS_BIN_DIR:-${XFSTESTS_ROOT}/tools/bin}
MODE=${XFSTESTS_MODE:-phase3_base}
THRESHOLD_PERCENT=${XFSTESTS_THRESHOLD_PERCENT:-90}
RESULTS_DIR=${XFSTESTS_RESULTS_DIR:-/tmp/xfstests_results}
SINGLE_TEST=${XFSTESTS_SINGLE_TEST:-}
TRACE_RUN=${XFSTESTS_TRACE_RUN:-0}
CASE_TIMEOUT_SEC=${XFSTESTS_CASE_TIMEOUT_SEC:-0}

TEST_DEV=${XFSTESTS_TEST_DEV:-/dev/vda}
SCRATCH_DEV=${XFSTESTS_SCRATCH_DEV:-/dev/vdb}
TEST_DIR=${XFSTESTS_TEST_DIR:-/ext4_test}
SCRATCH_MNT=${XFSTESTS_SCRATCH_MNT:-/ext4_scratch}

PHASE3_BASE_LIST=${SCRIPT_DIR}/testcases/phase3_base.list
PHASE3_STATIC_EXCLUDED=${SCRIPT_DIR}/blocked/phase3_excluded.tsv
PHASE4_GOOD_LIST=${SCRIPT_DIR}/testcases/phase4_good.list
PHASE4_STATIC_EXCLUDED=${SCRIPT_DIR}/blocked/phase4_excluded.tsv

BASE_LIST=""
STATIC_EXCLUDED=""

CHECK_BIN=${XFSTESTS_DEV_DIR}/check
RESULTS_FILE=${RESULTS_DIR}/${MODE}_results.tsv
SUMMARY_FILE=${RESULTS_DIR}/${MODE}_summary.tsv
EXCLUDED_FILE=${RESULTS_DIR}/${MODE}_excluded.tsv

mkdir -p "${RESULTS_DIR}" "${TEST_DIR}" "${SCRATCH_MNT}"
if [ ! -e /etc/fstab ]; then
    mkdir -p /etc
    : > /etc/fstab
fi
if [ ! -e /etc/mtab ] && [ -e /proc/mounts ]; then
    ln -sf /proc/mounts /etc/mtab
fi

unmount_target_if_needed() {
    target="$1"
    if [ -z "${target}" ]; then
        return 0
    fi
    if awk -v t="${target}" '$2==t { found=1; exit 0 } END { exit(found ? 0 : 1) }' /proc/mounts; then
        umount "${target}" >/dev/null 2>&1 || true
    fi
}

unmount_dev_if_needed() {
    dev="$1"
    if [ -z "${dev}" ]; then
        return 0
    fi
    awk -v d="${dev}" '$1==d { print $2 }' /proc/mounts | while IFS= read -r mnt; do
        [ -n "${mnt}" ] || continue
        [ "${mnt}" = "/" ] && continue
        umount "${mnt}" >/dev/null 2>&1 || true
    done
}

# Shell login profile may pre-mount /dev/vda and /dev/vdb for ext2/exfat smoke.
# xfstests requires raw devices and dedicated mount points.
unmount_target_if_needed "${TEST_DIR}"
unmount_target_if_needed "${SCRATCH_MNT}"
unmount_dev_if_needed "${TEST_DEV}"
unmount_dev_if_needed "${SCRATCH_DEV}"

mkfs_ext4_if_needed() {
    dev="$1"
    mkfs_log="/tmp/xfstests_mkfs_$(basename "${dev}" | tr -c '[:alnum:]' '_').log"
    if [ ! -b "${dev}" ]; then
        echo "Error: device not found: ${dev}" >&2
        return 1
    fi
    : >"${mkfs_log}"
    if [ -x /usr/sbin/mkfs.ext4 ]; then
        if /usr/sbin/mkfs.ext4 -F "${dev}" >"${mkfs_log}" 2>&1; then
            return 0
        fi
        rc=$?
        echo "mkfs attempt failed: /usr/sbin/mkfs.ext4 -F ${dev} (rc=${rc})" >&2
    fi
    if [ -x /usr/bin/mkfs.ext4 ]; then
        if /usr/bin/mkfs.ext4 -F "${dev}" >"${mkfs_log}" 2>&1; then
            return 0
        fi
        rc=$?
        echo "mkfs attempt failed: /usr/bin/mkfs.ext4 -F ${dev} (rc=${rc})" >&2
    fi
    if command -v mkfs.ext4 >/dev/null 2>&1; then
        if mkfs.ext4 -F "${dev}" >"${mkfs_log}" 2>&1; then
            return 0
        fi
        rc=$?
        echo "mkfs attempt failed: mkfs.ext4 -F ${dev} (rc=${rc})" >&2
    fi
    if [ -x /usr/sbin/mke2fs ]; then
        if /usr/sbin/mke2fs -F "${dev}" >"${mkfs_log}" 2>&1; then
            return 0
        fi
        rc=$?
        echo "mkfs attempt failed: /usr/sbin/mke2fs -F ${dev} (rc=${rc})" >&2
    fi
    if [ -x /usr/bin/mke2fs ]; then
        if /usr/bin/mke2fs -F "${dev}" >"${mkfs_log}" 2>&1; then
            return 0
        fi
        rc=$?
        echo "mkfs attempt failed: /usr/bin/mke2fs -F ${dev} (rc=${rc})" >&2
    fi
    if command -v mke2fs >/dev/null 2>&1; then
        if mke2fs -F "${dev}" >"${mkfs_log}" 2>&1; then
            return 0
        fi
        rc=$?
        echo "mkfs attempt failed: mke2fs -F ${dev} (rc=${rc})" >&2
    fi
    echo "Error: no working mkfs.ext4/mke2fs available for ${dev}" >&2
    if [ -s "${mkfs_log}" ]; then
        echo "mkfs log (${mkfs_log}):" >&2
        sed -n '1,80p' "${mkfs_log}" >&2 || true
    fi
    return 1
}

# Ensure deterministic ext4 media for xfstests.
mkfs_ext4_if_needed "${TEST_DEV}"
mkfs_ext4_if_needed "${SCRATCH_DEV}"

BASE_PATH="/bin:/usr/bin:/sbin:/usr/sbin"
if [ -d "${TOOLS_BIN_DIR}" ]; then
    # Keep system paths first so broken host symlinks under tools/bin do not shadow busybox tools.
    export PATH="${BASE_PATH}:${TOOLS_BIN_DIR}:${PATH}"
else
    export PATH="${BASE_PATH}:${PATH}"
fi
export XFSTESTS_TOOLS_BIN_DIR="${TOOLS_BIN_DIR}"

CHECK_SHELL=/bin/sh
if command -v bash >/dev/null 2>&1; then
    CHECK_SHELL=$(command -v bash)
fi
export SHELL="${CHECK_SHELL}"

# Most local helper shims below use "#!/bin/bash". In minimal initramfs,
# /bin/bash may be absent while bash is available elsewhere in PATH
# (for example /opt/xfstests/tools/bin/bash wrapper). Ensure /bin/bash
# is executable so those helper scripts can run.
if [ ! -x /bin/bash ]; then
    if [ "${CHECK_SHELL}" != "/bin/bash" ]; then
        ln -sf "${CHECK_SHELL}" /bin/bash >/dev/null 2>&1 || true
    fi
fi
if [ ! -x /bin/bash ]; then
    cat > /bin/bash <<EOF
#!/bin/sh
exec ${CHECK_SHELL} "\$@"
EOF
    chmod +x /bin/bash >/dev/null 2>&1 || true
fi

# Place shims under /opt so they stay executable in environments where /tmp is mounted noexec.
SHIM_DIR="${XFSTESTS_ROOT}/shims/bin"
mkdir -p "${SHIM_DIR}"

cat > "${SHIM_DIR}/mkfs" <<'EOF'
#!/bin/bash
set -eu
fstyp=${FSTYP:-ext4}
if [ "${1:-}" = "-t" ]; then
    fstyp="${2:-${fstyp}}"
    shift 2
fi
if [ "${1:-}" = "--" ]; then
    shift
fi
case "${fstyp}" in
    ext4|ext2)
        if [ -x /usr/sbin/mkfs.ext4 ]; then
            exec /usr/sbin/mkfs.ext4 "$@"
        fi
        if [ -x /usr/bin/mkfs.ext4 ]; then
            exec /usr/bin/mkfs.ext4 "$@"
        fi
        if [ -x /usr/bin/mke2fs ]; then
            exec /usr/bin/mke2fs "$@"
        fi
        echo "mkfs shim: no mkfs.ext4/mke2fs in initramfs" >&2
        exit 127
        ;;
    *)
        echo "mkfs shim: unsupported fs type '${fstyp}'" >&2
        exit 2
        ;;
esac
EOF
chmod +x "${SHIM_DIR}/mkfs"

# xfstests probes mkfs.ext4 directly from PATH.
cat > "${SHIM_DIR}/mkfs.ext4" <<'EOF'
#!/bin/bash
set -eu
if [ -x /usr/sbin/mkfs.ext4 ]; then
    exec /usr/sbin/mkfs.ext4 "$@"
fi
if [ -x /usr/bin/mkfs.ext4 ]; then
    exec /usr/bin/mkfs.ext4 "$@"
fi
exec /usr/bin/busybox mke2fs "$@"
EOF
chmod +x "${SHIM_DIR}/mkfs.ext4"

cat > "${SHIM_DIR}/mke2fs" <<'EOF'
#!/bin/bash
set -eu
if [ -x /usr/sbin/mke2fs ]; then
    exec /usr/sbin/mke2fs "$@"
fi
if [ -x /usr/bin/mke2fs ]; then
    exec /usr/bin/mke2fs "$@"
fi
exec /usr/bin/busybox mke2fs "$@"
EOF
chmod +x "${SHIM_DIR}/mke2fs"

# xfstests common/config requires a perl command in PATH.
# Provide a minimal compatibility shim for common/rc _link_out_file_named().
cat > "${SHIM_DIR}/perl" <<'EOF'
#!/bin/bash
set -eu

if [ "${1:-}" = "-e" ] && [ $# -ge 2 ]; then
    script="$2"
    shift 2

    # xfstests common/rc calls perl -e with FEATURES to pick *.out suffix.
    case "${script}" in
        *"my %feathash"*'ENV{"FEATURES"}'*)
        features="${FEATURES:-}"
        /usr/bin/busybox awk -v features="${features}" '
BEGIN {
    n = split(features, parts, ",")
    for (i = 1; i <= n; i++) {
        gsub(/^[[:space:]]+|[[:space:]]+$/, "", parts[i])
        if (parts[i] != "") {
            have[parts[i]] = 1
        }
    }
    printed = 0
}
{
    line = $0
    sub(/[[:space:]]*#.*/, "", line)
    if (line == "") {
        next
    }

    split(line, kv, /[[:space:]]*:[[:space:]]*/)
    opts = kv[1]
    suffix = kv[2]
    gsub(/^[[:space:]]+|[[:space:]]+$/, "", suffix)
    if (suffix == "") {
        next
    }

    ok = 1
    m = split(opts, req, ",")
    for (j = 1; j <= m; j++) {
        gsub(/^[[:space:]]+|[[:space:]]+$/, "", req[j])
        if (req[j] == "") {
            continue
        }
        if (!(req[j] in have)) {
            ok = 0
            break
        }
    }

    if (ok == 1) {
        print suffix
        printed = 1
        exit 0
    }
}
END {
    if (printed == 0) {
        print "default"
    }
}
'
        exit 0
        ;;
    esac
fi

echo "perl shim invoked but perl is unavailable: $*" >&2
exit 127
EOF
chmod +x "${SHIM_DIR}/perl"

# xfstests requires xfs_io to exist even for ext4 runs.
# Provide a minimal shim so ext4 tests can run and unsupported commands become "notrun".
cat > "${SHIM_DIR}/xfs_io" <<'EOF'
#!/bin/bash
exit 1
EOF
chmod +x "${SHIM_DIR}/xfs_io"

# BusyBox grep does not support GNU-style short context options like "-1".
cat > "${SHIM_DIR}/grep" <<'EOF'
#!/bin/bash
set -eu

rewritten=""
while [ $# -gt 0 ]; do
    case "$1" in
        --extended-regexp|-E)
            # BusyBox grep in minimal initramfs may not support -E.
            # Most xfstests patterns here are BRE-compatible.
            ;;
        -*E*)
            # Normalize clustered short options like -Eqm1 -> -qm1.
            opt_body=$(printf '%s' "${1#-}" | tr -d 'E')
            if [ -n "${opt_body}" ]; then
                rewritten="${rewritten} \"-${opt_body}\""
            fi
            ;;
        -[0-9]*)
            n=${1#-}
            case "${n}" in
                ''|*[!0-9]*)
                    rewritten="${rewritten} \"$1\""
                    ;;
                *)
                    rewritten="${rewritten} \"-C\" \"${n}\""
                    ;;
            esac
            ;;
        *)
            rewritten="${rewritten} \"$1\""
            ;;
    esac
    shift
done
eval "set -- ${rewritten}"
exec /usr/bin/busybox grep "$@"
EOF
chmod +x "${SHIM_DIR}/grep"

# BusyBox readlink lacks GNU -e/-f behavior; provide a minimal compatible shim.
cat > "${SHIM_DIR}/readlink" <<'EOF'
#!/bin/bash
set -eu

# Prefer xfstests bundled readlink when available.
TOOLS_READLINK="${XFSTESTS_TOOLS_BIN_DIR:-/opt/xfstests/tools/bin}/readlink"
if [ -x "${TOOLS_READLINK}" ]; then
    exec "${TOOLS_READLINK}" "$@"
fi

mode="plain"
path=""
normalize_abs_path() {
    local input="$1"
    local out=""
    local oldifs seg

    # Keep behavior deterministic for absolute paths used by xfstests.
    case "${input}" in
        /*) ;;
        *) input="/${input}" ;;
    esac

    oldifs=${IFS}
    IFS='/'
    for seg in ${input}; do
        [ -n "${seg}" ] || continue
        [ "${seg}" = "." ] && continue
        if [ "${seg}" = ".." ]; then
            out=${out%/*}
            continue
        fi
        out="${out}/${seg}"
    done
    IFS=${oldifs}

    if [ -z "${out}" ]; then
        printf '/\n'
    else
        printf '%s\n' "${out}"
    fi
}
while [ $# -gt 0 ]; do
    case "$1" in
        -e)
            mode="exists"
            ;;
        -f)
            mode="follow"
            ;;
        --)
            shift
            break
            ;;
        -*)
            # Ignore unsupported flags in minimal environments.
            ;;
        *)
            path="$1"
            ;;
    esac
    shift
done

if [ -z "${path}" ] && [ $# -gt 0 ]; then
    path="$1"
fi
[ -n "${path}" ] || exit 1

case "${mode}" in
    exists)
        [ -e "${path}" ] || exit 1
        normalize_abs_path "${path}"
        exit 0
        ;;
    follow)
        # Keep minimal compatibility with readlink -f in test helpers.
        normalize_abs_path "${path}"
        exit 0
        ;;
    *)
        exec /usr/bin/busybox readlink "${path}"
        ;;
esac
EOF
chmod +x "${SHIM_DIR}/readlink"

# BusyBox realpath does not support GNU -q; ignore it for xfstests.
cat > "${SHIM_DIR}/realpath" <<'EOF'
#!/bin/bash
set -eu
args=()
while [ $# -gt 0 ]; do
    case "$1" in
        -q)
            ;;
        --)
            ;;
        "")
            ;;
        *)
            args+=("$1")
            ;;
    esac
    shift
done
if [ "${#args[@]}" -eq 0 ]; then
    exit 0
fi
exec /usr/bin/busybox realpath "${args[@]}"
EOF
chmod +x "${SHIM_DIR}/realpath"

# util-linux findmnt is required by xfstests common/rc but absent in minimal
# initramfs. Provide the subset of options used by xfstests.
cat > "${SHIM_DIR}/findmnt" <<'EOF'
#!/bin/bash
set -eu
source_filter=""
target_filter=""
output_fields="SOURCE,TARGET"
while [ $# -gt 0 ]; do
    case "$1" in
        -S)
            source_filter="${2:-}"
            shift 2
            ;;
        -M)
            target_filter="${2:-}"
            shift 2
            ;;
        -o)
            output_fields="${2:-SOURCE,TARGET}"
            shift 2
            ;;
        -n|-r|-c|-v)
            shift
            ;;
        --)
            shift
            break
            ;;
        *)
            shift
            ;;
    esac
done
awk -v src="${source_filter}" -v tgt="${target_filter}" -v out="${output_fields}" '
BEGIN {
    OFS=" "
    split(out, fields, ",")
}
{
    source=$1
    target=$2
    fstype=$3
    options=$4
    if (src != "" && source != src) next
    if (tgt != "" && target != tgt) next
    vals["SOURCE"]=source
    vals["TARGET"]=target
    vals["FSTYPE"]=fstype
    vals["OPTIONS"]=options
    line=""
    for (i=1; i<=length(fields); i++) {
        key=fields[i]
        gsub(/^[[:space:]]+|[[:space:]]+$/, "", key)
        if (i > 1) line = line OFS
        line = line vals[key]
    }
    print line
    matched=1
}
END {
    if (matched != 1) exit 1
}
' /proc/mounts
EOF
chmod +x "${SHIM_DIR}/findmnt"

export PATH="${SHIM_DIR}:${PATH}"

export FSTYP=ext4
export TEST_DEV
export SCRATCH_DEV
export TEST_DIR
export SCRATCH_MNT

# Keep xfstests config deterministic. Relying only on inherited env vars can
# leave TEST_DIR/SCRATCH_MNT unset after config re-sourcing.
HOST_CONFIG_FILE="${XFSTESTS_DEV_DIR}/local.config"
cat > "${HOST_CONFIG_FILE}" <<EOF
EMAIL=root@localhost
FSTYP=${FSTYP}
TEST_DEV=${TEST_DEV}
TEST_DIR=${TEST_DIR}
SCRATCH_DEV=${SCRATCH_DEV}
SCRATCH_MNT=${SCRATCH_MNT}
EOF
export HOST_OPTIONS="${HOST_CONFIG_FILE}"

# check uses "bash -c ... exec ./tests/..." to run each testcase. Ensure
# every non-interactive bash sees stable xfstests vars instead of inheriting
# stale state from parent shells.
BASH_ENV_SHIM="${XFSTESTS_ROOT}/shims/bash_env.sh"
cat > "${BASH_ENV_SHIM}" <<EOF
#!/bin/bash
unset CONFIG_INCLUDED
export FSTYP="\${FSTYP:-ext4}"
export TEST_DEV="\${TEST_DEV:-${TEST_DEV}}"
export SCRATCH_DEV="\${SCRATCH_DEV:-${SCRATCH_DEV}}"
export TEST_DIR="\${TEST_DIR:-${TEST_DIR}}"
export SCRATCH_MNT="\${SCRATCH_MNT:-${SCRATCH_MNT}}"
export HOST_OPTIONS="\${HOST_OPTIONS:-${HOST_CONFIG_FILE}}"
if [ "\${XFSTESTS_BASHENV_TRACE:-0}" = "1" ]; then
    {
        echo "pid=\$\$ ppid=\${PPID:-NA} argv0=\${0:-NA}"
        echo "CONFIG_INCLUDED=\${CONFIG_INCLUDED-<unset>}"
        echo "FSTYP=\${FSTYP-<unset>}"
        echo "TEST_DEV=\${TEST_DEV-<unset>}"
        echo "TEST_DIR=\${TEST_DIR-<unset>}"
        echo "SCRATCH_DEV=\${SCRATCH_DEV-<unset>}"
        echo "SCRATCH_MNT=\${SCRATCH_MNT-<unset>}"
        echo "HOST_OPTIONS=\${HOST_OPTIONS-<unset>}"
        echo "---"
    } >> /tmp/xfstests_bashenv_trace.log 2>/dev/null || true
fi
if [ "\${XFSTESTS_CHILD_XTRACE:-0}" = "1" ]; then
    case "\${0:-}" in
        ./tests/*|tests/*)
            exec 9>>/tmp/xfstests_child_xtrace.log
            export BASH_XTRACEFD=9
            set -x
            ;;
    esac
fi
EOF
chmod +x "${BASH_ENV_SHIM}"
export BASH_ENV="${BASH_ENV_SHIM}"
if [ "${TRACE_RUN}" = "1" ]; then
    export XFSTESTS_BASHENV_TRACE=1
    : >/tmp/xfstests_bashenv_trace.log
else
    unset XFSTESTS_BASHENV_TRACE
fi
if [ "${XFSTESTS_CHILD_XTRACE:-0}" = "1" ]; then
    : >/tmp/xfstests_child_xtrace.log
fi

# Some xfstests helper paths invoke "sh readlink"/"sh grep" unexpectedly.
# Put same-name wrappers in the test working directory so those calls resolve.
ln -sf /bin/bash "${SHIM_DIR}/bash"
ln -sf "${SHIM_DIR}/grep" "${XFSTESTS_DEV_DIR}/grep"
ln -sf "${SHIM_DIR}/readlink" "${XFSTESTS_DEV_DIR}/readlink"
ln -sf "${SHIM_DIR}/bash" "${XFSTESTS_DEV_DIR}/bash"
if [ -n "${TOOLS_BIN_DIR}" ] && [ -d "${TOOLS_BIN_DIR}" ]; then
    ln -sf "${SHIM_DIR}/grep" "${TOOLS_BIN_DIR}/grep"
    ln -sf "${SHIM_DIR}/readlink" "${TOOLS_BIN_DIR}/readlink"
fi

echo "xfstests probe: CHECK_SHELL=${CHECK_SHELL} SHELL=${SHELL}" >&2
echo "xfstests probe: grep=$(command -v grep || echo missing) readlink=$(command -v readlink || echo missing)" >&2
echo "xfstests probe: HOST_OPTIONS=${HOST_OPTIONS}" >&2
echo "xfstests probe: single_test=${SINGLE_TEST:-none} trace_run=${TRACE_RUN}" >&2
echo "xfstests probe: TEST_DEV=${TEST_DEV} TEST_DIR=${TEST_DIR} SCRATCH_DEV=${SCRATCH_DEV} SCRATCH_MNT=${SCRATCH_MNT}" >&2
echo "xfstests probe: local.config" >&2
sed -n '1,80p' "${HOST_CONFIG_FILE}" >&2 || true
set +e
"${SHIM_DIR}/grep" -q "never-match" /dev/null >/dev/null 2>&1
grep_probe_rc=$?
"${SHIM_DIR}/readlink" -e / >/dev/null 2>&1
readlink_probe_rc=$?
"${SHIM_DIR}/findmnt" -n -o SOURCE,TARGET -S /dev/vda >/dev/null 2>&1
findmnt_probe_rc=$?
set -e
echo "xfstests probe: shim grep rc=${grep_probe_rc} shim readlink rc=${readlink_probe_rc} shim findmnt rc=${findmnt_probe_rc}" >&2

resolve_exec() {
    for p in "$@"; do
        [ -n "${p}" ] || continue
        if [ -x "${p}" ]; then
            echo "${p}"
            return 0
        fi
    done
    return 1
}

if mkfs_prog=$(resolve_exec /usr/bin/mkfs.ext4 /bin/mkfs.ext4 /usr/sbin/mkfs.ext4 /usr/bin/mke2fs /bin/mke2fs /usr/sbin/mke2fs); then
    export MKFS_PROG="${mkfs_prog}"
elif command -v mkfs.ext4 >/dev/null 2>&1; then
    mkfs_prog=$(command -v mkfs.ext4)
    [ -x "${mkfs_prog}" ] && export MKFS_PROG="${mkfs_prog}"
elif command -v mke2fs >/dev/null 2>&1; then
    mkfs_prog=$(command -v mke2fs)
    [ -x "${mkfs_prog}" ] && export MKFS_PROG="${mkfs_prog}"
fi

if fsck_prog=$(resolve_exec /usr/bin/e2fsck /bin/e2fsck /usr/sbin/e2fsck); then
    export FSCK_PROG="${fsck_prog}"
elif command -v e2fsck >/dev/null 2>&1; then
    fsck_prog=$(command -v e2fsck)
    [ -x "${fsck_prog}" ] && export FSCK_PROG="${fsck_prog}"
fi

echo "xfstests env: MKFS_PROG=${MKFS_PROG:-unset} FSCK_PROG=${FSCK_PROG:-unset}" >&2

if [ ! -x "${CHECK_BIN}" ]; then
    echo "Error: xfstests check binary not found at ${CHECK_BIN}" >&2
    exit 2
fi

case "${MODE}" in
    generic_quick)
        QUICK_LOG=${RESULTS_DIR}/generic_quick.log
        echo "Running xfstests generic quick (observation-only)..."
        set +e
        (cd "${XFSTESTS_DEV_DIR}" && "${CHECK_SHELL}" ./check -g quick) >"${QUICK_LOG}" 2>&1
        QUICK_RC=$?
        set -e
        {
            echo "mode\trc\tlog"
            echo "generic_quick\t${QUICK_RC}\t${QUICK_LOG}"
        } >"${SUMMARY_FILE}"
        echo "generic quick done (non-blocking), rc=${QUICK_RC}, log=${QUICK_LOG}"
        exit 0
        ;;
    phase3_base)
        BASE_LIST=${PHASE3_BASE_LIST}
        STATIC_EXCLUDED=${PHASE3_STATIC_EXCLUDED}
        ;;
    phase4_good)
        BASE_LIST=${PHASE4_GOOD_LIST}
        STATIC_EXCLUDED=${PHASE4_STATIC_EXCLUDED}
        ;;
    *)
        echo "Error: unsupported XFSTESTS_MODE=${MODE}" >&2
        exit 3
        ;;
esac

if [ ! -f "${BASE_LIST}" ]; then
    echo "Error: base test list not found at ${BASE_LIST}" >&2
    exit 4
fi

: >"${EXCLUDED_FILE}"
printf "test\tstatus\trc\treason\n" >"${RESULTS_FILE}"

PASS_COUNT=0
FAIL_COUNT=0
NOTRUN_COUNT=0
STATIC_BLOCKED_COUNT=0

run_check_with_optional_timeout() {
    _timeout_sec="$1"
    _trace_mode="$2"
    _case_name="$3"
    _out_file="$4"

    if [ "${_timeout_sec}" -gt 0 ]; then
        (
            cd "${XFSTESTS_DEV_DIR}" || exit 1
            if [ "${_trace_mode}" = "1" ]; then
                "${CHECK_SHELL}" -x ./check "${_case_name}"
            else
                "${CHECK_SHELL}" ./check "${_case_name}"
            fi
        ) >"${_out_file}" 2>&1 &
        _case_pid=$!

        (
            sleep "${_timeout_sec}"
            kill -TERM "${_case_pid}" >/dev/null 2>&1 || true
            sleep 5
            kill -KILL "${_case_pid}" >/dev/null 2>&1 || true
        ) &
        _timer_pid=$!

        wait "${_case_pid}"
        _rc=$?
        kill "${_timer_pid}" >/dev/null 2>&1 || true
        wait "${_timer_pid}" >/dev/null 2>&1 || true

        if [ "${_rc}" -eq 143 ] || [ "${_rc}" -eq 137 ]; then
            return 124
        fi
        return "${_rc}"
    fi

    if [ "${_trace_mode}" = "1" ]; then
        (cd "${XFSTESTS_DEV_DIR}" && "${CHECK_SHELL}" -x ./check "${_case_name}") >"${_out_file}" 2>&1
    else
        (cd "${XFSTESTS_DEV_DIR}" && "${CHECK_SHELL}" ./check "${_case_name}") >"${_out_file}" 2>&1
    fi
}

is_static_blocked() {
    test_name="$1"
    if [ ! -f "${STATIC_EXCLUDED}" ]; then
        return 1
    fi
    reason=$(awk -F '\t' -v target="${test_name}" 'BEGIN{ret=""} $1==target {ret=$2; print ret; exit 0} END{if(ret=="") exit 1}' "${STATIC_EXCLUDED}" 2>/dev/null || true)
    if [ -n "${reason}" ]; then
        echo "${reason}"
        return 0
    fi
    return 1
}

record_notrun() {
    test_name="$1"
    reason="$2"
    printf "%s\tNOTRUN\tNA\t%s\n" "${test_name}" "${reason}" >>"${RESULTS_FILE}"
    printf "%s\t%s\n" "${test_name}" "${reason}" >>"${EXCLUDED_FILE}"
    NOTRUN_COUNT=$((NOTRUN_COUNT + 1))
}

while IFS= read -r test_name; do
    case "${test_name}" in
        "" | "#"*)
            continue
            ;;
    esac

    if [ -n "${SINGLE_TEST}" ] && [ "${test_name}" != "${SINGLE_TEST}" ]; then
        continue
    fi

    static_reason=$(is_static_blocked "${test_name}" || true)
    if [ -n "${static_reason}" ]; then
        printf "%s\tSTATIC_BLOCKED\tNA\t%s\n" "${test_name}" "${static_reason}" >>"${RESULTS_FILE}"
        printf "%s\t%s\n" "${test_name}" "${static_reason}" >>"${EXCLUDED_FILE}"
        STATIC_BLOCKED_COUNT=$((STATIC_BLOCKED_COUNT + 1))
        continue
    fi

    test_log="${RESULTS_DIR}/$(echo "${test_name}" | tr '/' '_').log"
    echo "xfstests case start: ${test_name} timeout=${CASE_TIMEOUT_SEC}s trace=${TRACE_RUN}" >&2
    set +e
    run_check_with_optional_timeout "${CASE_TIMEOUT_SEC}" "${TRACE_RUN}" "${test_name}" "${test_log}"
    rc=$?
    set -e
    echo "xfstests case done: ${test_name} rc=${rc}" >&2

    notrun_line=$(
        grep -Eim1 \
            "(^|[[:space:]])\\[not run\\]|^[[:space:]]*[Nn]ot run|unknown test" \
            "${test_log}" || true
    )
    if [ -n "${notrun_line}" ]; then
        echo "xfstests case notrun: ${test_name} :: ${notrun_line}" >&2
        sed -n '1,30p' "${test_log}" >&2 || true
        reason=$(echo "${notrun_line}" | tr '\t' ' ')
        [ -z "${reason}" ] && reason="runtime notrun"
        record_notrun "${test_name}" "${reason}"
        continue
    fi

    if [ "${rc}" -eq 0 ]; then
        printf "%s\tPASS\t0\t\n" "${test_name}" >>"${RESULTS_FILE}"
        PASS_COUNT=$((PASS_COUNT + 1))
    else
        reason=$(grep -E "Failures|failed|ERROR|error|^${test_name}" "${test_log}" | head -n 1 | tr '\t' ' ' || true)
        if [ -z "${reason}" ]; then
            reason=$(tail -n 1 "${test_log}" | tr '\t' ' ' || true)
        fi
        if [ "${rc}" -eq 124 ] && [ -n "${CASE_TIMEOUT_SEC}" ] && [ "${CASE_TIMEOUT_SEC}" -gt 0 ]; then
            reason="timeout ${CASE_TIMEOUT_SEC}s"
        fi
        [ -z "${reason}" ] && reason="rc=${rc}"
        printf "%s\tFAIL\t%s\t%s\n" "${test_name}" "${rc}" "${reason}" >>"${RESULTS_FILE}"
        echo "===== FAIL LOG: ${test_name} =====" >&2
        if [ "${TRACE_RUN}" = "1" ]; then
            cat "${test_log}" >&2 || true
            if [ -f /tmp/xfstests_bashenv_trace.log ]; then
                echo "----- BASH_ENV TRACE -----" >&2
                cat /tmp/xfstests_bashenv_trace.log >&2 || true
                echo "----- END BASH_ENV TRACE -----" >&2
            fi
        else
            sed -n '1,20p' "${test_log}" >&2 || true
        fi
        echo "----- PROCESS SNAPSHOT (fsstress) -----" >&2
        ps -ef | grep -E '[f]sstress' >&2 || true
        echo "----- END PROCESS SNAPSHOT -----" >&2
        if [ -f /tmp/xfstests_child_xtrace.log ]; then
            echo "----- CHILD XTRACE -----" >&2
            cat /tmp/xfstests_child_xtrace.log >&2 || true
            echo "----- END CHILD XTRACE -----" >&2
        fi
        if [ "${rc}" -eq 124 ]; then
            full_log="${XFSTESTS_DEV_DIR}/results/${test_name}.full"
            if [ -f "${full_log}" ]; then
                echo "----- TIMEOUT FULL LOG TAIL: ${test_name} -----" >&2
                tail -n 120 "${full_log}" >&2 || true
                echo "----- END TIMEOUT FULL LOG TAIL -----" >&2
            fi
        fi
        bad_out="${XFSTESTS_DEV_DIR}/results/${test_name}.out.bad"
        if [ -f "${bad_out}" ]; then
            echo "----- BAD OUT: ${test_name} -----" >&2
            sed -n '1,120p' "${bad_out}" >&2 || true
        fi
        full_log="${XFSTESTS_DEV_DIR}/results/${test_name}.full"
        if [ -f "${full_log}" ]; then
            echo "----- FULL LOG TAIL: ${test_name} -----" >&2
            tail -n 200 "${full_log}" >&2 || true
            echo "----- END FULL LOG TAIL -----" >&2
        fi
        echo "===== END FAIL LOG: ${test_name} =====" >&2
        FAIL_COUNT=$((FAIL_COUNT + 1))
    fi
done <"${BASE_LIST}"

DENOMINATOR=$((PASS_COUNT + FAIL_COUNT))
if [ "${DENOMINATOR}" -eq 0 ]; then
    PASS_RATE=0
else
    PASS_RATE=$(awk -v p="${PASS_COUNT}" -v d="${DENOMINATOR}" 'BEGIN{printf "%.2f", (p*100.0)/d}')
fi

{
    echo "mode\tpass\tfail\tnotrun\tstatic_blocked\tdenominator\tpass_rate_percent\tthreshold_percent"
    echo "${MODE}\t${PASS_COUNT}\t${FAIL_COUNT}\t${NOTRUN_COUNT}\t${STATIC_BLOCKED_COUNT}\t${DENOMINATOR}\t${PASS_RATE}\t${THRESHOLD_PERCENT}"
} >"${SUMMARY_FILE}"
cat "${SUMMARY_FILE}"
echo "===== ${MODE} detailed results ====="
cat "${RESULTS_FILE}"

if [ "${DENOMINATOR}" -eq 0 ]; then
    echo "Error: denominator is zero, no runnable xfstests in base set." >&2
    exit 5
fi

PASS_RATE_OK=$(awk -v r="${PASS_RATE}" -v t="${THRESHOLD_PERCENT}" 'BEGIN{ if ((r+0) >= (t+0)) print 1; else print 0; }')
if [ "${PASS_RATE_OK}" -ne 1 ]; then
    echo "xfstests ${MODE} failed: pass_rate=${PASS_RATE}% < threshold=${THRESHOLD_PERCENT}%" >&2
    echo "See summary: ${SUMMARY_FILE}" >&2
    exit 6
fi

echo "xfstests ${MODE} passed: pass_rate=${PASS_RATE}% >= ${THRESHOLD_PERCENT}%"
echo "summary=${SUMMARY_FILE}"
echo "results=${RESULTS_FILE}"
echo "excluded=${EXCLUDED_FILE}"
exit 0
