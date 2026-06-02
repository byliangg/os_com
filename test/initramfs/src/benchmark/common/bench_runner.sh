#!/bin/sh

# SPDX-License-Identifier: MPL-2.0
# Entrypoint for the benchmark VM

set -e

BENCHMARK_ROOT="/benchmark"
READY_MESSAGE="The VM is ready for the benchmark."

BENCHMARK_NAME=$1
SYSTEM="${2:-asterinas}"
FIO_BS="${3:-}"
FIO_FSYNC="${4:-}"
FIO_SIZE="${5:-}"
FIO_NUMJOBS="${6:-}"
[ "${FIO_BS}" = "-" ] && FIO_BS=""
[ "${FIO_FSYNC}" = "-" ] && FIO_FSYNC=""
[ "${FIO_SIZE}" = "-" ] && FIO_SIZE=""
[ "${FIO_NUMJOBS}" = "-" ] && FIO_NUMJOBS=""
echo "Running benchmark: ${BENCHMARK_NAME} on ${SYSTEM}"
if [ -n "${FIO_BS}" ]; then
    export FIO_BS
    echo "Using FIO_BS=${FIO_BS}"
fi
if [ -n "${FIO_FSYNC}" ]; then
    export FIO_FSYNC
    echo "Using FIO_FSYNC=${FIO_FSYNC}"
fi
if [ -n "${FIO_SIZE}" ]; then
    export FIO_SIZE
    echo "Using FIO_SIZE=${FIO_SIZE}"
fi
if [ -n "${FIO_NUMJOBS}" ]; then
    export FIO_NUMJOBS
    echo "Using FIO_NUMJOBS=${FIO_NUMJOBS}"
fi

is_ext4_benchmark() {
    case "${BENCHMARK_NAME}" in
        */ext4_*|ext4_*)
            return 0
            ;;
        *)
            return 1
            ;;
    esac
}

is_raw_benchmark() {
    case "${BENCHMARK_NAME}" in
        */raw_*|raw_*)
            return 0
            ;;
        *)
            return 1
            ;;
    esac
}

is_mountpoint_present() {
    local mp="$1"
    awk -v p="${mp}" '$2==p { found=1; exit 0 } END { exit(found ? 0 : 1) }' /proc/mounts
}

print_help() {
    echo "Usage: $0 <benchmark_name> <system_type>"
    echo "  benchmark_name: The name of the benchmark to run."
    echo "  system_type: The type of system to run the benchmark on. 'linux' or 'asterinas'."
}

# Validate arguments
check_benchmark_name() {
    if [ -z "${BENCHMARK_NAME}" ] || [ -z "${SYSTEM}" ]; then
        echo "Error: Invalid arguments."
        print_help
        exit 1
    fi

    local full_path="${BENCHMARK_ROOT}/${BENCHMARK_NAME}"

    if ! [ -d "${full_path}" ]; then
        echo "Directory '${BENCHMARK_NAME}' does not exist in the benchmark directory."
        print_help
        exit 1
    fi
}

prepare_system() {
    if [ ! -d /tmp ]; then
        mkdir /tmp
    fi

    # System-specific preparation
    if [ "$SYSTEM" = "linux" ]; then
        # Mount necessary fs
        mount -t devtmpfs devtmpfs /dev
        # Enable network
        ip link set lo up
        ip link set eth0 up
        ifconfig eth0 10.0.2.15
        mkdir -p /ext2 /ext4
        if is_ext4_benchmark; then
            mount -t ext4 /dev/vda /ext4
            echo "Mounted ext4 benchmark fs on /dev/vda -> /ext4"
        elif is_raw_benchmark; then
            echo "Raw benchmark: skipping /dev/vda mount"
        else
            mount -t ext2 /dev/vda /ext2
            echo "Mounted ext2 benchmark fs on /dev/vda -> /ext2"
        fi
    elif [ "$SYSTEM" = "asterinas" ]; then
        if is_ext4_benchmark; then
            mkdir -p /ext4
            if ! is_mountpoint_present /ext4; then
                mount -t ext4 /dev/vda /ext4
            fi
            echo "Mounted ext4 benchmark fs on /dev/vda -> /ext4"
        fi
    else
        echo "Error: Unknown system type. Please set SYSTEM to 'linux' or 'asterinas'."
        exit 1
    fi
}

main() {
    # Check if the benchmark name is valid  
    check_benchmark_name

    # Prepare the system
    prepare_system
    
    # Message to notify the host script. It must align with the READY_MESSAGE in host_guest_bench_runner.sh.
    # DO NOT REMOVE THIS LINE!!!
    echo "${READY_MESSAGE}"

    # Run the benchmark
    BENCHMARK_SCRIPT=${BENCHMARK_ROOT}/${BENCHMARK_NAME}/run.sh
    chmod +x ${BENCHMARK_SCRIPT}
    ${BENCHMARK_SCRIPT}

    # Shutdown explicitly if running on Linux
    if [ "$SYSTEM" = "linux" ]; then
        poweroff -f
    fi
}

main "$@"
