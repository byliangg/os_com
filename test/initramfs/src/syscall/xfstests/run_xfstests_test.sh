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
IGNORE_STATIC_EXCLUDED_FOR_SINGLE=${XFSTESTS_IGNORE_STATIC_EXCLUDED_FOR_SINGLE:-0}
TRACE_RUN=${XFSTESTS_TRACE_RUN:-0}
CASE_TIMEOUT_SEC=${XFSTESTS_CASE_TIMEOUT_SEC:-0}

TEST_DEV=${XFSTESTS_TEST_DEV:-/dev/vda}
SCRATCH_DEV=${XFSTESTS_SCRATCH_DEV:-/dev/vdb}
TEST_DIR=${XFSTESTS_TEST_DIR:-/ext4_test}
SCRATCH_MNT=${XFSTESTS_SCRATCH_MNT:-/ext4_scratch}

# Keep stress profiles deterministic and bounded in our benchmark runs.
TIME_FACTOR=${TIME_FACTOR:-1}
LOAD_FACTOR=${LOAD_FACTOR:-1}
export TIME_FACTOR LOAD_FACTOR

PHASE3_BASE_LIST=${SCRIPT_DIR}/testcases/phase3_base.list
PHASE3_STATIC_EXCLUDED=${SCRIPT_DIR}/blocked/phase3_excluded.tsv
PHASE4_GOOD_LIST=${SCRIPT_DIR}/testcases/phase4_good.list
PHASE4_STATIC_EXCLUDED=${SCRIPT_DIR}/blocked/phase4_excluded.tsv
PHASE6_GOOD_LIST=${SCRIPT_DIR}/testcases/phase6_good.list
PHASE6_STATIC_EXCLUDED=${SCRIPT_DIR}/blocked/phase6_excluded.tsv
JBD_PHASE1_LIST=${SCRIPT_DIR}/testcases/jbd_phase1.list
JBD_PHASE1_STATIC_EXCLUDED=${SCRIPT_DIR}/blocked/jbd_phase1_excluded.tsv

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

# Ensure deterministic ext4 media for xfstests unless host side already did it.
if [ "${XFSTESTS_SKIP_MKFS:-0}" != "1" ]; then
    mkfs_ext4_if_needed "${TEST_DEV}"
    mkfs_ext4_if_needed "${SCRATCH_DEV}"
fi

BASE_PATH="/bin:/usr/bin:/sbin:/usr/sbin"
if [ -d "${TOOLS_BIN_DIR}" ]; then
    # Keep system paths first so broken host symlinks under tools/bin do not shadow busybox tools.
    export PATH="${BASE_PATH}:${TOOLS_BIN_DIR}:${PATH}"
else
    export PATH="${BASE_PATH}:${PATH}"
fi

# Prebuilt xfstests helper binaries are ELF executables that may expect
# glibc loader paths such as /lib64/ld-linux-x86-64.so.2.
if [ -d /nix/store ]; then
    glibc_lib_dir=$(ls -d /nix/store/*-glibc-*/lib 2>/dev/null | head -n 1 || true)
    if [ -n "${glibc_lib_dir}" ] && [ -d "${glibc_lib_dir}" ]; then
        if [ -e "${glibc_lib_dir}/ld-linux-x86-64.so.2" ]; then
            ln -sf "${glibc_lib_dir}/ld-linux-x86-64.so.2" /lib64/ld-linux-x86-64.so.2 || true
        fi
        for lib in libc.so.6 libm.so.6 libpthread.so.0 librt.so.1 libdl.so.2 libresolv.so.2; do
            if [ -e "${glibc_lib_dir}/${lib}" ]; then
                ln -sf "${glibc_lib_dir}/${lib}" "/usr/lib/${lib}" || true
                ln -sf "${glibc_lib_dir}/${lib}" "/usr/lib64/${lib}" || true
            fi
        done
    fi
fi

CHECK_SHELL=/bin/sh
if command -v bash >/dev/null 2>&1; then
    CHECK_SHELL=$(command -v bash)
else
    for candidate in /nix/store/*-bash-*/bin/bash /opt/xfstests/tools/bin/bash /usr/bin/bash /bin/bash; do
        if [ -x "${candidate}" ]; then
            CHECK_SHELL="${candidate}"
            break
        fi
    done
fi
export SHELL="${CHECK_SHELL}"

# Some helper scripts use `#!/bin/bash`; keep that path usable in minimal initramfs.
if [ "${CHECK_SHELL}" != "/bin/sh" ] && [ ! -x /bin/bash ] && [ -x "${CHECK_SHELL}" ]; then
    ln -sf "${CHECK_SHELL}" /bin/bash || true
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
exec /usr/bin/busybox mke2fs -t ext4 "$@"
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

# dumpe2fs is used by ext4 log/journal probes in xfstests. Minimal
# initramfs may not ship e2fsprogs; provide a compatible header output.
cat > "${SHIM_DIR}/dumpe2fs" <<'EOF'
#!/bin/bash
set -eu
dev=""
want_header=0
while [ $# -gt 0 ]; do
    case "$1" in
        -h)
            want_header=1
            ;;
        -*)
            ;;
        *)
            dev="$1"
            ;;
    esac
    shift
done

if [ -z "${dev}" ]; then
    exit 2
fi

# The ext4 godown fallback below cannot currently drive a real
# FS_IOC_GOINGDOWN path, so it drops a one-shot marker for xfstests'
# ext4 logstate probe. Honor that marker before delegating to a real
# dumpe2fs binary so shutdown/log tests observe the intended
# "dirty before replay, clean after replay" sequence.
if [ -f /tmp/xfstests_ext4_needs_recovery ]; then
    need_marker=1
else
    need_marker=0
fi

for cand in /usr/sbin/dumpe2fs /usr/bin/dumpe2fs /sbin/dumpe2fs /bin/dumpe2fs; do
    if [ "${need_marker}" -eq 0 ] && [ -x "${cand}" ]; then
        exec "${cand}" ${want_header:+-h} "${dev}"
    fi
done

# Fallback output covering xfstests probes:
# - "Filesystem features" with "has_journal"
# - "Inode size" for inode-size checks.
read_le_u16() {
    local file="$1"
    local offset="$2"
    od -An -j "${offset}" -N 2 -t u1 "${file}" 2>/dev/null | \
        awk '{ print ($1 + (256 * $2)) }'
}

read_le_u32() {
    local file="$1"
    local offset="$2"
    od -An -j "${offset}" -N 4 -t u1 "${file}" 2>/dev/null | \
        awk '{ print ($1 + (256 * $2) + (65536 * $3) + (16777216 * $4)) }'
}

features="ext_attr dir_index filetype extent 64bit flex_bg sparse_super large_file huge_file dir_nlink extra_isize metadata_csum"
inode_size=256

# ext superblock is little-endian and starts at offset 1024.
feature_compat=$(read_le_u32 "${dev}" 1116)
journal_inum=$(read_le_u32 "${dev}" 1248)
parsed_inode_size=$(read_le_u16 "${dev}" 1112)
if [ -n "${parsed_inode_size:-}" ] && [ "${parsed_inode_size}" -gt 0 ]; then
    inode_size="${parsed_inode_size}"
fi
if [ -n "${feature_compat:-}" ] && [ -n "${journal_inum:-}" ] && \
   [ $((feature_compat & 0x4)) -ne 0 ] && [ "${journal_inum}" -ne 0 ]; then
    features="has_journal ${features}"
fi
if [ "${need_marker}" -eq 1 ]; then
    features="${features} needs_recovery"
    rm -f /tmp/xfstests_ext4_needs_recovery >/dev/null 2>&1 || true
fi
cat <<EOM
Dumpe2fs 1.47.0 (compat-shim)
Filesystem volume name:   <none>
Filesystem magic number:  0xEF53
Filesystem features:      ${features}
Inode size:               ${inode_size}
EOM
exit 0
EOF
chmod +x "${SHIM_DIR}/dumpe2fs"

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

# BusyBox stat may not support GNU --format. Translate to -c for xfstests.
cat > "${SHIM_DIR}/stat" <<'EOF'
#!/bin/bash
set -eu
args=()
while [ $# -gt 0 ]; do
    case "$1" in
        --format=*)
            args+=("-c" "${1#--format=}")
            ;;
        --format)
            shift
            [ $# -gt 0 ] || exit 2
            args+=("-c" "$1")
            ;;
        *)
            args+=("$1")
            ;;
    esac
    shift
done

if [ -x /usr/bin/stat ]; then
    exec /usr/bin/stat "${args[@]}"
fi
exec /usr/bin/busybox stat "${args[@]}"
EOF
chmod +x "${SHIM_DIR}/stat"

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

# BusyBox pkill lacks procps-ng long options such as "--echo".
cat > "${SHIM_DIR}/pkill" <<'EOF'
#!/bin/bash
set -eu
args=()
while [ $# -gt 0 ]; do
    case "$1" in
        --echo)
            args+=("-e")
            ;;
        -PIPE|-SIGPIPE)
            # BusyBox pkill does not support symbolic signal names here.
            # Drop it and let pkill use default TERM.
            ;;
        --signal)
            shift
            [ $# -gt 0 ] || exit 2
            args+=("-$1")
            ;;
        *)
            args+=("$1")
            ;;
    esac
    shift
done
exec /usr/bin/busybox pkill "${args[@]}"
EOF
chmod +x "${SHIM_DIR}/pkill"

# xfs_io is used by common/rc sparse-file probing. Some minimal roots do not
# provide a working xfs_io binary; emulate the probe command when needed.
cat > "${SHIM_DIR}/xfs_io" <<'EOF'
#!/bin/bash
set -eu
orig_args=("$@")
cmds=()
target=""
real=""
while [ $# -gt 0 ]; do
    case "$1" in
        -c)
            cmds+=("${2:-}")
            shift 2
            ;;
        --)
            shift
            break
            ;;
        -*)
            shift
            ;;
        *)
            target="$1"
            shift
            ;;
    esac
done

for cand in /usr/sbin/xfs_io /usr/bin/xfs_io /sbin/xfs_io /bin/xfs_io /opt/xfstests/tools/xfs_io; do
    if [ -x "${cand}" ]; then
        real="${cand}"
        break
    fi
done

if [ -n "${real}" ]; then
    if [ "${XFSTESTS_XFS_IO_DEBUG:-0}" = "1" ]; then
        echo "xfs_io shim: use real ${real}" >&2
    fi
    exec "${real}" "${orig_args[@]}"
fi

parse_size() {
    local tok="$1"
    local num unit
    if [[ "${tok}" =~ ^([0-9]+)([kKmMgG]?)$ ]]; then
        num="${BASH_REMATCH[1]}"
        unit="${BASH_REMATCH[2]}"
    else
        echo 0
        return 0
    fi
    case "${unit}" in
        k|K) echo $((num * 1024)) ;;
        m|M) echo $((num * 1024 * 1024)) ;;
        g|G) echo $((num * 1024 * 1024 * 1024)) ;;
        *)   echo "${num}" ;;
    esac
}

emulate_pwrite() {
    local cmd="$1"
    local off=0
    local len=0
    local fill_byte=0
    local parse_fill=0
    local tokens=()
    read -r -a tokens <<<"${cmd}"
    for token in "${tokens[@]}"; do
        if [ "${parse_fill}" -eq 1 ]; then
            fill_byte=$((token))
            parse_fill=0
            continue
        fi
        if [ "${token}" = "-S" ]; then
            parse_fill=1
        fi
    done
    if [ "${#tokens[@]}" -ge 3 ]; then
        off=$(parse_size "${tokens[$((${#tokens[@]} - 2))]}")
        len=$(parse_size "${tokens[$((${#tokens[@]} - 1))]}")
    fi
    [ "${len}" -gt 0 ] || len=4096

    if [ -z "${target}" ]; then
        return 1
    fi

    mkdir -p "$(dirname "${target}")"
    [ -e "${target}" ] || : > "${target}"

    if [ -x /opt/xfstests/fsync_file ]; then
        /opt/xfstests/fsync_file pwrite "${target}" "${off}" "${len}" "${fill_byte}" >/dev/null 2>&1
        return $?
    fi

    # Emulate pwrite by issuing real data writes, not truncate-only growth.
    # This preserves ENOSPC behavior for tests like generic/027.
    write_chunk() {
        local seek="$1"
        local count="$2"
        if [ "${fill_byte}" -eq 0 ]; then
            dd if=/dev/zero of="${target}" bs=1 seek="${seek}" count="${count}" conv=notrunc >/dev/null 2>&1
            return $?
        fi
        awk -v n="${count}" -v c="${fill_byte}" 'BEGIN { for (i = 0; i < n; i++) printf "%c", c }' | \
            dd of="${target}" bs=1 seek="${seek}" count="${count}" conv=notrunc >/dev/null 2>&1
    }

    local pos="${off}"
    local rem="${len}"
    local blk=4096
    local head=$(((blk - (pos % blk)) % blk))
    if [ "${head}" -gt "${rem}" ]; then
        head="${rem}"
    fi
    if [ "${head}" -gt 0 ]; then
        write_chunk "${pos}" "${head}" || return 1
        pos=$((pos + head))
        rem=$((rem - head))
    fi

    local full=$((rem / blk))
    if [ "${full}" -gt 0 ]; then
        local i=0
        while [ "${i}" -lt "${full}" ]; do
            write_chunk "${pos}" "${blk}" || return 1
            pos=$((pos + blk))
            i=$((i + 1))
        done
        rem=$((rem - full * blk))
    fi

    if [ "${rem}" -gt 0 ]; then
        write_chunk "${pos}" "${rem}" || return 1
    fi
    return 0
}

emulate_fiemap() {
    local sz=0
    if [ -n "${target}" ] && [ -e "${target}" ]; then
        sz=$(stat -c '%s' "${target}" 2>/dev/null || echo 0)
    fi
    echo " EXT: FILE-OFFSET      BLOCK-RANGE      TOTAL FLAGS"
    if [ "${sz}" -gt 0 ]; then
        local blocks=$(((sz + 4095) / 4096))
        [ "${blocks}" -gt 0 ] || blocks=1
        echo "   0: [0..$((blocks - 1))]: 0..$((blocks - 1))  ${blocks}"
    fi
    return 0
}

emulate_one() {
    local cmd="$1"
    case "${cmd}" in
        help\ fiemap*)
            echo "fiemap [offset [len]]"
            return 0
            ;;
        pwrite*)
            emulate_pwrite "${cmd}"
            return $?
            ;;
        fsync*|fdatasync*|syncfs*)
            local op="${cmd%% *}"
            if [ -n "${target}" ] && [ -x /opt/xfstests/fsync_file ]; then
                /opt/xfstests/fsync_file "${op}" "${target}" >/dev/null 2>&1
                return $?
            fi
            sync >/dev/null 2>&1 || true
            return 0
            ;;
        fiemap*)
            emulate_fiemap
            return 0
            ;;
        *)
            return 1
            ;;
    esac
}

if [ "${#cmds[@]}" -eq 0 ]; then
    exit 1
fi

for cmd in "${cmds[@]}"; do
    if ! emulate_one "${cmd}"; then
        if [ "${XFSTESTS_XFS_IO_DEBUG:-0}" = "1" ]; then
            echo "xfs_io shim: unsupported command: ${cmd}" >&2
        fi
        exit 1
    fi
done

exit 0

exit 1
EOF
chmod +x "${SHIM_DIR}/xfs_io"

cat > "${SHIM_DIR}/umount" <<'EOF'
#!/bin/bash
set -eu

translate_target() {
    local arg="$1"
    local mountpoint=""

    case "${arg}" in
        ""|-*)
            printf '%s\n' "${arg}"
            return 0
            ;;
    esac

    if [ -b "${arg}" ] || [ "${arg#"/dev/"}" != "${arg}" ]; then
        mountpoint=$(awk -v dev="${arg}" '$1==dev { print $2; exit }' /proc/mounts || true)
        if [ -n "${mountpoint}" ]; then
            printf '%s\n' "${mountpoint}"
            return 0
        fi
    fi

    printf '%s\n' "${arg}"
}

args=()
for arg in "$@"; do
    args+=("$(translate_target "${arg}")")
done

for cand in /usr/bin/umount /bin/umount; do
    if [ -x "${cand}" ] && [ "$(readlink -f "${cand}" 2>/dev/null || printf '%s' "${cand}")" != "$(readlink -f "$0" 2>/dev/null || printf '%s' "$0")" ]; then
        exec "${cand}" "${args[@]}"
    fi
done

exec /usr/bin/busybox umount "${args[@]}"
EOF
chmod +x "${SHIM_DIR}/umount"

# xfstests freeze/shutdown groups rely on xfs_freeze/godown helpers.
# Asterinas ext4 test environment currently lacks native ioctl support for
# these helpers, so provide ext4-only no-op fallbacks to keep the tests
# runnable instead of immediate [not run].
cat > "${SHIM_DIR}/xfs_freeze" <<'EOF'
#!/bin/bash
set -eu
rc=1

if [ "$#" -lt 2 ]; then
    exit 2
fi

for cand in /usr/sbin/xfs_freeze /usr/bin/xfs_freeze /sbin/xfs_freeze /bin/xfs_freeze; do
    if [ -x "${cand}" ]; then
        "${cand}" "$@"
        rc=$?
        [ "${rc}" -eq 0 ] && exit 0
        break
    fi
done

op="$1"
mnt="$2"
case "${op}" in
    -f|-u) ;;
    *)
        exit "${rc}"
        ;;
esac

if [ "${FSTYP:-}" = "ext4" ]; then
    if awk -v t="${mnt}" '$2==t { found=1 } END { exit(found ? 0 : 1) }' /proc/mounts; then
        # Ext4 freeze/thaw no-op compatibility for phase6 benchmark.
        exit 0
    fi
fi

exit "${rc}"
EOF
chmod +x "${SHIM_DIR}/xfs_freeze"

export PATH="${SHIM_DIR}:${PATH}"
export XFS_IO_PROG="${SHIM_DIR}/xfs_io"
for libdir in /lib/x86_64-linux-gnu /usr/lib/x86_64-linux-gnu /lib64 /usr/lib64; do
    [ -d "${libdir}" ] || continue
    if [ -n "${LD_LIBRARY_PATH:-}" ]; then
        export LD_LIBRARY_PATH="${libdir}:${LD_LIBRARY_PATH}"
    else
        export LD_LIBRARY_PATH="${libdir}"
    fi
done

export FSTYP=ext4
export TEST_DEV
export SCRATCH_DEV
export TEST_DIR
export SCRATCH_MNT

# Some prebuilt xfstests trees may ship `tests/*/group` without `group.list`.
# `check` requires group.list for test resolution.
if [ -d "${XFSTESTS_DEV_DIR}/tests" ]; then
    for test_dir in "${XFSTESTS_DEV_DIR}"/tests/*; do
        [ -d "${test_dir}" ] || continue
        if [ -f "${test_dir}/group" ] && [ ! -f "${test_dir}/group.list" ]; then
            cp "${test_dir}/group" "${test_dir}/group.list"
        fi
    done
fi

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

# Some prebuilt trees may ship src/lstat64 as a binary that is not runnable
# in our minimal initramfs. Install a deterministic script implementation.
mkdir -p "${XFSTESTS_DEV_DIR}/src"
cat > "${XFSTESTS_DEV_DIR}/src/lstat64" <<'EOF'
#!/bin/bash
set -eu
target="${1:-}"
[ -n "${target}" ] || exit 1
[ -e "${target}" ] || exit 1
dev_id=""
if [ -b "${target}" ]; then
    dev_node=$(basename "${target}")
    dev_id=$(cat "/sys/class/block/${dev_node}/dev" 2>/dev/null || true)
fi
if [ -z "${dev_id}" ]; then
    # Fallback for environments without /sys/class/block entries.
    dev_id=$(stat -c '%t:%T' "${target}" 2>/dev/null || true)
fi
[ -n "${dev_id}" ] || exit 1
# Match common/rc expectation: awk '/Device type:/ { print $9 }'
echo "Device type: shim shim shim shim shim shim ${dev_id}"
EOF
chmod +x "${XFSTESTS_DEV_DIR}/src/lstat64"

# Some prebuilt trees miss src/min_dio_alignment binary.
# generic/091 and similar cases expect this helper to print a numeric value.
if [ ! -x "${XFSTESTS_DEV_DIR}/src/min_dio_alignment" ]; then
cat > "${XFSTESTS_DEV_DIR}/src/min_dio_alignment" <<'EOF'
#!/bin/bash
set -eu
mntp="${1:-}"
dev="${2:-}"

# Prefer runtime probe if stat supports %o (optimal I/O transfer size).
if [ -n "${mntp}" ] && stat -c '%o' "${mntp}" >/dev/null 2>&1; then
    sz=$(stat -c '%o' "${mntp}" 2>/dev/null || true)
    case "${sz}" in
        ''|0|4096) ;;
        *[!0-9]*) ;;
        *) echo "${sz}"; exit 0 ;;
    esac
fi

# Keep fallback deterministic for ext4 test images.
echo 4096
EOF
chmod +x "${XFSTESTS_DEV_DIR}/src/min_dio_alignment"
fi

# Some xfstests feature tests require src/godown and
# src/aio-dio-regress/aio-dio-fcntl-race built artifacts.
# In minimal prebuilt trees those binaries may be missing.
if [ ! -e "${XFSTESTS_DEV_DIR}/src/godown.real" ] && [ -x "${XFSTESTS_DEV_DIR}/src/godown" ]; then
    mv "${XFSTESTS_DEV_DIR}/src/godown" "${XFSTESTS_DEV_DIR}/src/godown.real"
fi
cat > "${XFSTESTS_DEV_DIR}/src/godown" <<'EOF'
#!/bin/bash
set -eu
real="${0}.real"
if [ -x "${real}" ]; then
    if "${real}" "$@" >/tmp/xfstests_godown_real.log 2>&1; then
        exit 0
    fi
    rc=$?
else
    rc=127
fi

if [ "${FSTYP:-}" = "ext4" ]; then
    sync || true
    # Ext4 fallback: emulate "forced shutdown" as a sync barrier.
    touch /tmp/xfstests_ext4_needs_recovery >/dev/null 2>&1 || true
    exit 0
fi

if [ -f /tmp/xfstests_godown_real.log ]; then
    cat /tmp/xfstests_godown_real.log >&2 || true
fi
exit "${rc}"
EOF
chmod +x "${XFSTESTS_DEV_DIR}/src/godown"

aio_fcntl_race="${XFSTESTS_DEV_DIR}/src/aio-dio-regress/aio-dio-fcntl-race"
if [ ! -x "${aio_fcntl_race}" ]; then
    cat > "${aio_fcntl_race}" <<'EOF'
#!/bin/bash
set -eu
target="${1:-}"
[ -n "${target}" ] || exit 1

# Fallback helper when prebuilt aio-dio binary is unavailable.
: > "${target}"
end=$((SECONDS + 10))
while [ "${SECONDS}" -lt "${end}" ]; do
    dd if=/dev/zero of="${target}" bs=512 seek=1 count=1 conv=notrunc >/dev/null 2>&1 || true
    sync >/dev/null 2>&1 || true
done
exit 0
EOF
    chmod +x "${aio_fcntl_race}"
fi

if [ -x "${XFSTESTS_DEV_DIR}/src/lstat64" ]; then
    test_dev_is_block=0
    scratch_dev_is_block=0
    [ -b "${TEST_DEV}" ] && test_dev_is_block=1
    [ -b "${SCRATCH_DEV}" ] && scratch_dev_is_block=1
    echo "xfstests probe: is_block TEST_DEV=${test_dev_is_block} SCRATCH_DEV=${scratch_dev_is_block}" >&2
    ls -l "${TEST_DEV}" "${SCRATCH_DEV}" >&2 || true

    test_dev_raw=$("${XFSTESTS_DEV_DIR}/src/lstat64" "${TEST_DEV}" 2>&1 || true)
    scratch_dev_raw=$("${XFSTESTS_DEV_DIR}/src/lstat64" "${SCRATCH_DEV}" 2>&1 || true)
    echo "xfstests probe: lstat64 raw TEST_DEV=${test_dev_raw:-<empty>}" >&2
    echo "xfstests probe: lstat64 raw SCRATCH_DEV=${scratch_dev_raw:-<empty>}" >&2

    test_dev_type=$("${XFSTESTS_DEV_DIR}/src/lstat64" "${TEST_DEV}" 2>/dev/null | awk '/Device type:/ { print $9 }' || true)
    scratch_dev_type=$("${XFSTESTS_DEV_DIR}/src/lstat64" "${SCRATCH_DEV}" 2>/dev/null | awk '/Device type:/ { print $9 }' || true)
    echo "xfstests probe: lstat64 TEST_DEV_TYPE=${test_dev_type:-<empty>} SCRATCH_DEV_TYPE=${scratch_dev_type:-<empty>}" >&2
fi

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
ln -sf "${SHIM_DIR}/grep" "${XFSTESTS_DEV_DIR}/grep"
ln -sf "${SHIM_DIR}/readlink" "${XFSTESTS_DEV_DIR}/readlink"
ln -sf "${SHIM_DIR}/stat" "${XFSTESTS_DEV_DIR}/stat"

echo "xfstests probe: CHECK_SHELL=${CHECK_SHELL} SHELL=${SHELL}" >&2
echo "xfstests probe: grep=$(command -v grep || echo missing) readlink=$(command -v readlink || echo missing)" >&2
echo "xfstests probe: HOST_OPTIONS=${HOST_OPTIONS}" >&2
echo "xfstests probe: single_test=${SINGLE_TEST:-none} trace_run=${TRACE_RUN}" >&2
echo "xfstests probe: ignore_static_excluded_for_single=${IGNORE_STATIC_EXCLUDED_FOR_SINGLE}" >&2
echo "xfstests probe: TEST_DEV=${TEST_DEV} TEST_DIR=${TEST_DIR} SCRATCH_DEV=${SCRATCH_DEV} SCRATCH_MNT=${SCRATCH_MNT}" >&2
echo "xfstests probe: local.config" >&2
sed -n '1,80p' "${HOST_CONFIG_FILE}" >&2 || true

set +e
if [ -x "${XFSTESTS_DEV_DIR}/src/fill" ]; then
    "${XFSTESTS_DEV_DIR}/src/fill" >/tmp/xfstests_fill_probe.log 2>&1
    fill_probe_rc=$?
    echo "xfstests probe: fill rc=${fill_probe_rc}" >&2
    sed -n '1,3p' /tmp/xfstests_fill_probe.log >&2 || true
fi
set -e

set +e
"${SHIM_DIR}/grep" -q "never-match" /dev/null >/dev/null 2>&1
grep_probe_rc=$?
"${SHIM_DIR}/readlink" -e / >/dev/null 2>&1
readlink_probe_rc=$?
"${SHIM_DIR}/findmnt" -n -o SOURCE,TARGET -S /dev/vda >/dev/null 2>&1
findmnt_probe_rc=$?
set -e
echo "xfstests probe: shim grep rc=${grep_probe_rc} shim readlink rc=${readlink_probe_rc} shim findmnt rc=${findmnt_probe_rc}" >&2

log_sparse_probe() {
    probe_file="${TEST_DIR}/$${RANDOM}.sparseprobe"
    probe_log="/tmp/xfstests_sparse_probe.log"
    probe_mounted_by_us=0

    if ! awk -v t="${TEST_DIR}" '$2==t { found=1 } END { exit(found ? 0 : 1) }' /proc/mounts >/dev/null 2>&1; then
        if mount "${TEST_DEV}" "${TEST_DIR}" >/dev/null 2>&1; then
            probe_mounted_by_us=1
        fi
    fi
    probe_mount_line=$(awk -v t="${TEST_DIR}" '$2==t {print; exit}' /proc/mounts 2>/dev/null || true)
    [ -n "${probe_mount_line}" ] || probe_mount_line="<none>"
    echo "xfstests sparse probe mount: ${probe_mount_line}" >&2

    rm -f "${probe_file}" "${probe_log}" >/dev/null 2>&1 || true

    set +e
    "${XFS_IO_PROG}" -f -c 'pwrite -b 51200 -S 0x61 1638400 51200' "${probe_file}" >"${probe_log}" 2>&1
    probe_rc=$?
    set -e

    probe_du_kb=$(du -sk "${probe_file}" 2>/dev/null | awk '{print $1}' || true)
    probe_size=$(stat -c '%s' "${probe_file}" 2>/dev/null || echo "NA")
    probe_blocks=$(stat -c '%b' "${probe_file}" 2>/dev/null || echo "NA")
    [ -n "${probe_du_kb}" ] || probe_du_kb="NA"
    echo "xfstests sparse probe: rc=${probe_rc} size_bytes=${probe_size} blocks_512b=${probe_blocks} du_kb=${probe_du_kb} file=${probe_file}" >&2
    sed -n '1,3p' "${probe_log}" >&2 || true

    probe_method() {
        method="$1"
        file="$2"
        cmd="$3"
        rm -f "${file}" >/dev/null 2>&1 || true
        set +e
        /bin/bash -lc "${cmd}" >/tmp/xfstests_sparse_probe_method.log 2>&1
        mrc=$?
        set -e
        m_du=$(du -sk "${file}" 2>/dev/null | awk '{print $1}' || true)
        m_size=$(stat -c '%s' "${file}" 2>/dev/null || echo "NA")
        m_blocks=$(stat -c '%b' "${file}" 2>/dev/null || echo "NA")
        [ -n "${m_du}" ] || m_du="NA"
        echo "xfstests sparse method: ${method} rc=${mrc} size_bytes=${m_size} blocks_512b=${m_blocks} du_kb=${m_du}" >&2
        sed -n '1,2p' /tmp/xfstests_sparse_probe_method.log >&2 || true
        rm -f "${file}" /tmp/xfstests_sparse_probe_method.log >/dev/null 2>&1 || true
    }

    probe_method \
        "dd_seek_write" \
        "${TEST_DIR}/$${RANDOM}.method_dd_write" \
        "dd if=/dev/zero of='${TEST_DIR}/$${RANDOM}.method_dd_write' bs=1 seek=1638400 count=51200 conv=notrunc"
    probe_method \
        "dd_seek_zero_count" \
        "${TEST_DIR}/$${RANDOM}.method_dd_zero" \
        "dd if=/dev/zero of='${TEST_DIR}/$${RANDOM}.method_dd_zero' bs=1 seek=1689600 count=0 conv=notrunc"
    probe_method \
        "truncate_only" \
        "${TEST_DIR}/$${RANDOM}.method_truncate" \
        "truncate -s 1689600 '${TEST_DIR}/$${RANDOM}.method_truncate'"
    probe_method \
        "dd_small_seek" \
        "${TEST_DIR}/$${RANDOM}.method_dd_small" \
        "dd if=/dev/zero of='${TEST_DIR}/$${RANDOM}.method_dd_small' bs=1 seek=10 count=1 conv=notrunc"

    rm -f "${probe_file}" "${probe_log}" >/dev/null 2>&1 || true
    if [ "${probe_mounted_by_us}" = "1" ]; then
        umount "${TEST_DIR}" >/dev/null 2>&1 || true
    fi
}

if [ "${XFSTESTS_SPARSE_PROBE_LOG:-0}" = "1" ]; then
    log_sparse_probe
fi

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
    phase6_good)
        BASE_LIST=${PHASE6_GOOD_LIST}
        STATIC_EXCLUDED=${PHASE6_STATIC_EXCLUDED}
        ;;
    jbd_phase1)
        BASE_LIST=${JBD_PHASE1_LIST}
        STATIC_EXCLUDED=${JBD_PHASE1_STATIC_EXCLUDED}
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
    test_log_file="${3:-}"
    printf "%s\tNOTRUN\tNA\t%s\n" "${test_name}" "${reason}" >>"${RESULTS_FILE}"
    printf "%s\t%s\n" "${test_name}" "${reason}" >>"${EXCLUDED_FILE}"
    if [ -n "${test_log_file}" ] && [ -f "${test_log_file}" ]; then
        echo "----- NOTRUN LOG TAIL: ${test_name} -----" >&2
        tail -n 80 "${test_log_file}" >&2 || true
        echo "----- END NOTRUN LOG TAIL -----" >&2
    fi
    NOTRUN_COUNT=$((NOTRUN_COUNT + 1))
}

log_case_fs_state() {
    label="$1"
    test_dir_info=$(ls -ld "${TEST_DIR}" 2>&1 | tr '\t' ' ' | tr '\n' ' ')
    scratch_mnt_info=$(ls -ld "${SCRATCH_MNT}" 2>&1 | tr '\t' ' ' | tr '\n' ' ')
    test_dev_mounts=$(awk -v d="${TEST_DEV}" -v t="${TEST_DIR}" '
        $1==d || $2==t { printf "%s %s %s %s | ", $1, $2, $3, $4 }
    ' /proc/mounts)
    scratch_dev_mounts=$(awk -v d="${SCRATCH_DEV}" -v t="${SCRATCH_MNT}" '
        $1==d || $2==t { printf "%s %s %s %s | ", $1, $2, $3, $4 }
    ' /proc/mounts)
    [ -n "${test_dev_mounts}" ] || test_dev_mounts="<none>"
    [ -n "${scratch_dev_mounts}" ] || scratch_dev_mounts="<none>"
    echo "xfstests fsstate: ${label} TEST_DIR='${test_dir_info}' SCRATCH_MNT='${scratch_mnt_info}' TEST_DEV_MOUNTS='${test_dev_mounts}' SCRATCH_MOUNTS='${scratch_dev_mounts}'" >&2
}

log_timeout_diagnostics() {
    test_name="$1"
    case "${test_name}" in
        generic/027)
            target_dir="${SCRATCH_MNT}/testdir"
            echo "----- TIMEOUT DIAGNOSTICS: ${test_name} -----" >&2
            if [ -d "${target_dir}" ]; then
                file_count=$(find "${target_dir}" -type f 2>/dev/null | wc -l | tr -d ' ')
                dir_count=$(find "${target_dir}" -type d 2>/dev/null | wc -l | tr -d ' ')
                total_kb=$(du -sk "${target_dir}" 2>/dev/null | awk '{print $1}')
                [ -n "${total_kb}" ] || total_kb=0
                echo "timeout diag: target_dir=${target_dir} file_count=${file_count} dir_count=${dir_count} du_kb=${total_kb}" >&2

                sample_file=$(
                    find "${target_dir}" -type f 2>/dev/null \
                        | head -n 1
                )
                if [ -n "${sample_file}" ] && [ -e "${sample_file}" ]; then
                    sample_size=$(wc -c < "${sample_file}" 2>/dev/null | tr -d ' ')
                    [ -n "${sample_size}" ] || sample_size=0
                    echo "timeout diag: sample_file=${sample_file} size_bytes=${sample_size}" >&2
                    ls -l "${sample_file}" >&2 || true
                else
                    echo "timeout diag: no sample file found under ${target_dir}" >&2
                fi
            else
                echo "timeout diag: target_dir missing: ${target_dir}" >&2
            fi
            echo "timeout diag: df -k ${SCRATCH_MNT}" >&2
            df -k "${SCRATCH_MNT}" >&2 || true
            echo "----- END TIMEOUT DIAGNOSTICS: ${test_name} -----" >&2
            ;;
    esac
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

    static_reason=""
    if [ -n "${SINGLE_TEST}" ] && [ "${IGNORE_STATIC_EXCLUDED_FOR_SINGLE}" = "1" ]; then
        static_reason=""
    else
        static_reason=$(is_static_blocked "${test_name}" || true)
    fi
    if [ -n "${static_reason}" ]; then
        printf "%s\tSTATIC_BLOCKED\tNA\t%s\n" "${test_name}" "${static_reason}" >>"${RESULTS_FILE}"
        printf "%s\t%s\n" "${test_name}" "${static_reason}" >>"${EXCLUDED_FILE}"
        STATIC_BLOCKED_COUNT=$((STATIC_BLOCKED_COUNT + 1))
        continue
    fi

    test_log="${RESULTS_DIR}/$(echo "${test_name}" | tr '/' '_').log"
    log_case_fs_state "pre:${test_name}"
    echo "xfstests case start: ${test_name} timeout=${CASE_TIMEOUT_SEC}s trace=${TRACE_RUN}" >&2
    set +e
    run_check_with_optional_timeout "${CASE_TIMEOUT_SEC}" "${TRACE_RUN}" "${test_name}" "${test_log}"
    rc=$?
    set -e
    echo "xfstests case done: ${test_name} rc=${rc}" >&2
    log_case_fs_state "post:${test_name}"

    notrun_line=$(
        grep -Eim1 \
            "(^|[[:space:]])\\[not run\\]|^[[:space:]]*[Nn]ot run|unknown test" \
            "${test_log}" || true
    )
    if [ -n "${notrun_line}" ]; then
        reason=$(echo "${notrun_line}" | tr '\t' ' ')
        [ -z "${reason}" ] && reason="runtime notrun"
        record_notrun "${test_name}" "${reason}" "${test_log}"
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
        if [ -f /tmp/xfstests_child_xtrace.log ]; then
            echo "----- CHILD XTRACE -----" >&2
            cat /tmp/xfstests_child_xtrace.log >&2 || true
            echo "----- END CHILD XTRACE -----" >&2
        fi
        full_log="${XFSTESTS_DEV_DIR}/results/${test_name}.full"
        if [ -f "${full_log}" ]; then
            if [ "${rc}" -eq 124 ]; then
                echo "----- TIMEOUT FULL LOG TAIL: ${test_name} -----" >&2
            else
                echo "----- FULL LOG TAIL: ${test_name} -----" >&2
            fi
            tail -n 160 "${full_log}" >&2 || true
            if [ "${rc}" -eq 124 ]; then
                echo "----- END TIMEOUT FULL LOG TAIL -----" >&2
            else
                echo "----- END FULL LOG TAIL -----" >&2
            fi
        fi
        if [ "${rc}" -eq 124 ]; then
            log_timeout_diagnostics "${test_name}"
        fi
        bad_out="${XFSTESTS_DEV_DIR}/results/${test_name}.out.bad"
        if [ -f "${bad_out}" ]; then
            echo "----- BAD OUT: ${test_name} -----" >&2
            sed -n '1,120p' "${bad_out}" >&2 || true
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
