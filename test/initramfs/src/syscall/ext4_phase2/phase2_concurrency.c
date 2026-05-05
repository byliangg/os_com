// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <limits.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#define MAX_WORKERS 64
#define IO_BLOCK 1024
#define SMALL_BLOCK 256

static const uint64_t FNV_OFFSET = 1469598103934665603ULL;
static const uint64_t FNV_PRIME = 1099511628211ULL;

static const char *g_case_name = "multi_file_write_verify";
static const char *g_root = "/ext4_phase2/phase2";
static unsigned int g_workers = 4;
static unsigned int g_rounds = 8;
static uint64_t g_seed = 1;

static void failf(const char *fmt, ...)
{
    va_list ap;

    fprintf(stderr, "EXT4_PHASE2_FAIL case=%s seed=%" PRIu64 " workers=%u rounds=%u reason=\"",
            g_case_name, g_seed, g_workers, g_rounds);
    va_start(ap, fmt);
    vfprintf(stderr, fmt, ap);
    va_end(ap);
    fprintf(stderr, "\"\n");
    exit(1);
}

static uint64_t fnv_update(uint64_t hash, const unsigned char *data, size_t len)
{
    for (size_t i = 0; i < len; i++) {
        hash ^= (uint64_t)data[i];
        hash *= FNV_PRIME;
    }
    return hash;
}

static void make_pattern(unsigned char *buf, size_t len, unsigned int worker, unsigned int round,
                         unsigned int stream)
{
    uint64_t state = g_seed ^ ((uint64_t)worker << 32) ^ ((uint64_t)round << 16) ^ stream;

    for (size_t i = 0; i < len; i++) {
        state = state * 6364136223846793005ULL + 1442695040888963407ULL;
        buf[i] = (unsigned char)(state >> 56);
    }
}

static uint64_t expected_hash(unsigned int worker, unsigned int rounds, size_t len,
                              unsigned int stream)
{
    unsigned char buf[IO_BLOCK];
    uint64_t hash = FNV_OFFSET;

    if (len > sizeof(buf)) {
        failf("internal expected_hash len too large: %zu", len);
    }

    for (unsigned int round = 0; round < rounds; round++) {
        make_pattern(buf, len, worker, round, stream);
        hash = fnv_update(hash, buf, len);
    }
    return hash;
}

static uint64_t expected_hash_one(unsigned int worker, unsigned int round, size_t len,
                                  unsigned int stream)
{
    unsigned char buf[IO_BLOCK];
    uint64_t hash = FNV_OFFSET;

    if (len > sizeof(buf)) {
        failf("internal expected_hash_one len too large: %zu", len);
    }

    make_pattern(buf, len, worker, round, stream);
    return fnv_update(hash, buf, len);
}

static uint64_t file_hash(const char *path, off_t *size_out)
{
    unsigned char buf[4096];
    uint64_t hash = FNV_OFFSET;
    off_t total = 0;
    int fd = open(path, O_RDONLY);

    if (fd < 0) {
        failf("open for hash failed path=%s errno=%d", path, errno);
    }

    for (;;) {
        ssize_t nread = read(fd, buf, sizeof(buf));
        if (nread < 0) {
            int saved = errno;
            close(fd);
            failf("read for hash failed path=%s errno=%d", path, saved);
        }
        if (nread == 0) {
            break;
        }
        hash = fnv_update(hash, buf, (size_t)nread);
        total += nread;
    }
    if (close(fd) != 0) {
        failf("close after hash failed path=%s errno=%d", path, errno);
    }
    if (size_out != NULL) {
        *size_out = total;
    }
    return hash;
}

static void write_all(int fd, const void *buf, size_t len, const char *path)
{
    const unsigned char *cursor = buf;
    size_t remaining = len;

    while (remaining != 0) {
        ssize_t nwritten = write(fd, cursor, remaining);
        if (nwritten < 0) {
            failf("write failed path=%s errno=%d", path, errno);
        }
        if (nwritten == 0) {
            failf("write returned zero path=%s", path);
        }
        cursor += nwritten;
        remaining -= (size_t)nwritten;
    }
}

static void read_all_at(int fd, void *buf, size_t len, off_t offset, const char *path)
{
    unsigned char *cursor = buf;
    size_t remaining = len;

    if (lseek(fd, offset, SEEK_SET) < 0) {
        failf("lseek for read failed path=%s offset=%lld errno=%d",
              path, (long long)offset, errno);
    }

    while (remaining != 0) {
        ssize_t nread = read(fd, cursor, remaining);
        if (nread < 0) {
            failf("read_at failed path=%s offset=%lld errno=%d",
                  path, (long long)offset, errno);
        }
        if (nread == 0) {
            failf("short read path=%s offset=%lld remaining=%zu",
                  path, (long long)offset, remaining);
        }
        cursor += nread;
        remaining -= (size_t)nread;
    }
}

static void path_join(char *out, size_t out_len, const char *a, const char *b)
{
    int n = snprintf(out, out_len, "%s/%s", a, b);
    if (n < 0 || (size_t)n >= out_len) {
        failf("path too long base=%s name=%s", a, b);
    }
}

static void mkdir_if_needed(const char *path)
{
    if (mkdir(path, 0777) != 0 && errno != EEXIST) {
        failf("mkdir failed path=%s errno=%d", path, errno);
    }
}

static void ensure_case_dir(char *case_dir, size_t len)
{
    mkdir_if_needed(g_root);
    path_join(case_dir, len, g_root, g_case_name);
    mkdir_if_needed(case_dir);
}

static void wait_children(pid_t *pids, unsigned int count, const char *label)
{
    for (unsigned int i = 0; i < count; i++) {
        int status = 0;
        if (waitpid(pids[i], &status, 0) < 0) {
            failf("waitpid failed label=%s worker=%u errno=%d", label, i, errno);
        }
        if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
            failf("child failed label=%s worker=%u status=%d", label, i, status);
        }
    }
}

static void spawn_workers(void (*worker_fn)(const char *, unsigned int), const char *case_dir,
                          const char *label)
{
    pid_t pids[MAX_WORKERS];

    if (g_workers == 0 || g_workers > MAX_WORKERS) {
        failf("invalid workers=%u max=%u", g_workers, MAX_WORKERS);
    }
    for (unsigned int worker = 0; worker < g_workers; worker++) {
        pid_t pid = fork();
        if (pid < 0) {
            failf("fork failed label=%s worker=%u errno=%d", label, worker, errno);
        }
        if (pid == 0) {
            worker_fn(case_dir, worker);
            _exit(0);
        }
        pids[worker] = pid;
    }
    wait_children(pids, g_workers, label);
}

static void verify_file_hash(const char *path, uint64_t expected, off_t expected_size)
{
    off_t size = 0;
    uint64_t actual = file_hash(path, &size);

    if (size != expected_size || actual != expected) {
        failf("verify mismatch path=%s size=%lld expected_size=%lld hash=%" PRIu64
              " expected_hash=%" PRIu64,
              path, (long long)size, (long long)expected_size, actual, expected);
    }
}

static void worker_multi_file_write(const char *case_dir, unsigned int worker)
{
    unsigned char buf[IO_BLOCK];
    char path[PATH_MAX];
    char name[64];
    int fd;

    snprintf(name, sizeof(name), "worker_%02u.dat", worker);
    path_join(path, sizeof(path), case_dir, name);
    fd = open(path, O_CREAT | O_TRUNC | O_WRONLY, 0666);
    if (fd < 0) {
        failf("open write file failed worker=%u errno=%d", worker, errno);
    }
    for (unsigned int round = 0; round < g_rounds; round++) {
        make_pattern(buf, sizeof(buf), worker, round, 0x11);
        write_all(fd, buf, sizeof(buf), path);
    }
    if (fsync(fd) != 0) {
        failf("fsync write file failed worker=%u errno=%d", worker, errno);
    }
    if (close(fd) != 0) {
        failf("close write file failed worker=%u errno=%d", worker, errno);
    }
}

static void case_multi_file_write_verify(void)
{
    char case_dir[PATH_MAX];

    ensure_case_dir(case_dir, sizeof(case_dir));
    spawn_workers(worker_multi_file_write, case_dir, "multi_file_write");

    for (unsigned int worker = 0; worker < g_workers; worker++) {
        char path[PATH_MAX];
        char name[64];
        snprintf(name, sizeof(name), "worker_%02u.dat", worker);
        path_join(path, sizeof(path), case_dir, name);
        verify_file_hash(path, expected_hash(worker, g_rounds, IO_BLOCK, 0x11),
                         (off_t)g_rounds * IO_BLOCK);
    }
}

static void worker_read_write(const char *case_dir, unsigned int worker)
{
    if ((worker % 2) == 0) {
        worker_multi_file_write(case_dir, worker);
        return;
    }

    char stable[PATH_MAX];
    path_join(stable, sizeof(stable), case_dir, "stable.dat");
    for (unsigned int round = 0; round < g_rounds; round++) {
        off_t size = 0;
        uint64_t hash = file_hash(stable, &size);
        if (size != (off_t)IO_BLOCK || hash != expected_hash(0, 1, IO_BLOCK, 0x22)) {
            failf("stable read mismatch worker=%u round=%u size=%lld hash=%" PRIu64,
                  worker, round, (long long)size, hash);
        }
    }
}

static void case_multi_file_read_write(void)
{
    unsigned char buf[IO_BLOCK];
    char case_dir[PATH_MAX];
    char stable[PATH_MAX];
    int fd;

    ensure_case_dir(case_dir, sizeof(case_dir));
    path_join(stable, sizeof(stable), case_dir, "stable.dat");
    fd = open(stable, O_CREAT | O_TRUNC | O_WRONLY, 0666);
    if (fd < 0) {
        failf("open stable failed errno=%d", errno);
    }
    make_pattern(buf, sizeof(buf), 0, 0, 0x22);
    write_all(fd, buf, sizeof(buf), stable);
    if (fsync(fd) != 0 || close(fd) != 0) {
        failf("sync stable failed errno=%d", errno);
    }

    spawn_workers(worker_read_write, case_dir, "multi_file_read_write");
}

static void worker_create_unlink(const char *case_dir, unsigned int worker)
{
    unsigned char buf[SMALL_BLOCK];
    char path[PATH_MAX];
    char name[96];

    for (unsigned int round = 0; round < g_rounds; round++) {
        snprintf(name, sizeof(name), "tmp_w%02u_r%04u.dat", worker, round);
        path_join(path, sizeof(path), case_dir, name);
        int fd = open(path, O_CREAT | O_TRUNC | O_WRONLY, 0666);
        if (fd < 0) {
            failf("open tmp failed worker=%u round=%u errno=%d", worker, round, errno);
        }
        make_pattern(buf, sizeof(buf), worker, round, 0x33);
        write_all(fd, buf, sizeof(buf), path);
        if (close(fd) != 0) {
            failf("close tmp failed worker=%u round=%u errno=%d", worker, round, errno);
        }
        if (unlink(path) != 0) {
            failf("unlink tmp failed worker=%u round=%u errno=%d", worker, round, errno);
        }
    }

    snprintf(name, sizeof(name), "keep_%02u.dat", worker);
    path_join(path, sizeof(path), case_dir, name);
    int fd = open(path, O_CREAT | O_TRUNC | O_WRONLY, 0666);
    if (fd < 0) {
        failf("open keep failed worker=%u errno=%d", worker, errno);
    }
    make_pattern(buf, sizeof(buf), worker, g_rounds, 0x34);
    write_all(fd, buf, sizeof(buf), path);
    if (fsync(fd) != 0 || close(fd) != 0) {
        failf("sync keep failed worker=%u errno=%d", worker, errno);
    }
}

static void case_create_unlink_churn(void)
{
    char case_dir[PATH_MAX];

    ensure_case_dir(case_dir, sizeof(case_dir));
    spawn_workers(worker_create_unlink, case_dir, "create_unlink_churn");

    for (unsigned int worker = 0; worker < g_workers; worker++) {
        char path[PATH_MAX];
        char name[64];
        snprintf(name, sizeof(name), "keep_%02u.dat", worker);
        path_join(path, sizeof(path), case_dir, name);
        verify_file_hash(path, expected_hash_one(worker, g_rounds, SMALL_BLOCK, 0x34),
                         (off_t)SMALL_BLOCK);
    }
}

static void worker_rename(const char *case_dir, unsigned int worker)
{
    unsigned char buf[SMALL_BLOCK];
    char a[PATH_MAX];
    char b[PATH_MAX];
    char name[96];
    int fd;

    snprintf(name, sizeof(name), "rename_%02u_a.dat", worker);
    path_join(a, sizeof(a), case_dir, name);
    snprintf(name, sizeof(name), "rename_%02u_b.dat", worker);
    path_join(b, sizeof(b), case_dir, name);
    fd = open(a, O_CREAT | O_TRUNC | O_WRONLY, 0666);
    if (fd < 0) {
        failf("open rename initial failed worker=%u errno=%d", worker, errno);
    }
    make_pattern(buf, sizeof(buf), worker, 0, 0x44);
    write_all(fd, buf, sizeof(buf), a);
    if (close(fd) != 0) {
        failf("close rename initial failed worker=%u errno=%d", worker, errno);
    }

    for (unsigned int round = 0; round < g_rounds; round++) {
        const char *src = (round % 2 == 0) ? a : b;
        const char *dst = (round % 2 == 0) ? b : a;
        if (rename(src, dst) != 0) {
            failf("rename failed worker=%u round=%u errno=%d", worker, round, errno);
        }
    }
}

static void case_rename_churn(void)
{
    char case_dir[PATH_MAX];

    ensure_case_dir(case_dir, sizeof(case_dir));
    spawn_workers(worker_rename, case_dir, "rename_churn");

    for (unsigned int worker = 0; worker < g_workers; worker++) {
        char path[PATH_MAX];
        char name[96];
        snprintf(name, sizeof(name), "rename_%02u_%c.dat", worker,
                 (g_rounds % 2 == 0) ? 'a' : 'b');
        path_join(path, sizeof(path), case_dir, name);
        verify_file_hash(path, expected_hash(worker, 1, SMALL_BLOCK, 0x44),
                         (off_t)SMALL_BLOCK);
    }
}

static void worker_truncate_fsync(const char *case_dir, unsigned int worker)
{
    unsigned char buf[IO_BLOCK];
    char path[PATH_MAX];
    char name[64];
    int fd;

    snprintf(name, sizeof(name), "truncate_%02u.dat", worker);
    path_join(path, sizeof(path), case_dir, name);
    fd = open(path, O_CREAT | O_TRUNC | O_RDWR, 0666);
    if (fd < 0) {
        failf("open truncate file failed worker=%u errno=%d", worker, errno);
    }
    for (unsigned int round = 0; round < g_rounds; round++) {
        if (ftruncate(fd, 0) != 0) {
            failf("ftruncate zero failed worker=%u round=%u errno=%d", worker, round, errno);
        }
        make_pattern(buf, sizeof(buf), worker, round, 0x55);
        if (lseek(fd, 0, SEEK_SET) < 0) {
            failf("lseek failed worker=%u round=%u errno=%d", worker, round, errno);
        }
        write_all(fd, buf, sizeof(buf), path);
        if (fsync(fd) != 0) {
            failf("fsync full failed worker=%u round=%u errno=%d", worker, round, errno);
        }
        if (ftruncate(fd, SMALL_BLOCK) != 0) {
            failf("ftruncate small failed worker=%u round=%u errno=%d", worker, round, errno);
        }
        if (fsync(fd) != 0) {
            failf("fsync small failed worker=%u round=%u errno=%d", worker, round, errno);
        }
    }
    if (close(fd) != 0) {
        failf("close truncate file failed worker=%u errno=%d", worker, errno);
    }
}

static void case_write_truncate_fsync(void)
{
    char case_dir[PATH_MAX];

    ensure_case_dir(case_dir, sizeof(case_dir));
    spawn_workers(worker_truncate_fsync, case_dir, "write_truncate_fsync");

    for (unsigned int worker = 0; worker < g_workers; worker++) {
        unsigned char buf[IO_BLOCK];
        char path[PATH_MAX];
        char name[64];
        uint64_t expected = FNV_OFFSET;

        snprintf(name, sizeof(name), "truncate_%02u.dat", worker);
        path_join(path, sizeof(path), case_dir, name);
        make_pattern(buf, sizeof(buf), worker, g_rounds - 1, 0x55);
        expected = fnv_update(expected, buf, SMALL_BLOCK);
        verify_file_hash(path, expected, (off_t)SMALL_BLOCK);
    }
}

static void worker_unlink_while_open(const char *case_dir, unsigned int worker)
{
    unsigned char expected[IO_BLOCK];
    unsigned char actual[IO_BLOCK];
    unsigned char small[SMALL_BLOCK];
    char victim[PATH_MAX];
    char pressure[PATH_MAX];
    char name[96];
    struct stat victim_st;
    int fd;

    snprintf(name, sizeof(name), "open_unlink_%02u.dat", worker);
    path_join(victim, sizeof(victim), case_dir, name);
    fd = open(victim, O_CREAT | O_TRUNC | O_RDWR, 0666);
    if (fd < 0) {
        failf("open unlink victim failed worker=%u errno=%d", worker, errno);
    }

    make_pattern(expected, sizeof(expected), worker, 0, 0x66);
    write_all(fd, expected, sizeof(expected), victim);
    if (fsync(fd) != 0) {
        failf("fsync unlink victim failed worker=%u errno=%d", worker, errno);
    }
    if (fstat(fd, &victim_st) != 0) {
        failf("fstat unlink victim failed worker=%u errno=%d", worker, errno);
    }
    if (unlink(victim) != 0) {
        failf("unlink open victim failed worker=%u errno=%d", worker, errno);
    }
    int check_fd = open(victim, O_RDONLY);
    if (check_fd >= 0) {
        close(check_fd);
        failf("unlinked victim is still reachable worker=%u", worker);
    }
    if (errno != ENOENT) {
        failf("unexpected open errno for unlinked victim worker=%u errno=%d", worker, errno);
    }

    for (unsigned int round = 0; round < g_rounds; round++) {
        snprintf(name, sizeof(name), "pressure_%02u_%04u.dat", worker, round);
        path_join(pressure, sizeof(pressure), case_dir, name);
        int pressure_fd = open(pressure, O_CREAT | O_TRUNC | O_RDWR, 0666);
        if (pressure_fd < 0) {
            failf("open pressure file failed worker=%u round=%u errno=%d", worker, round, errno);
        }
        struct stat pressure_st;
        if (fstat(pressure_fd, &pressure_st) != 0) {
            failf("fstat pressure file failed worker=%u round=%u errno=%d", worker, round, errno);
        }
        if (pressure_st.st_ino == victim_st.st_ino) {
            failf("inode reused while old fd open worker=%u round=%u ino=%llu",
                  worker, round, (unsigned long long)victim_st.st_ino);
        }
        make_pattern(small, sizeof(small), worker, round, 0x67);
        write_all(pressure_fd, small, sizeof(small), pressure);
        if (fsync(pressure_fd) != 0 || close(pressure_fd) != 0) {
            failf("sync pressure file failed worker=%u round=%u errno=%d", worker, round, errno);
        }

        read_all_at(fd, actual, sizeof(actual), 0, victim);
        if (memcmp(actual, expected, sizeof(expected)) != 0) {
            failf("unlinked open fd content changed worker=%u round=%u", worker, round);
        }

        make_pattern(small, sizeof(small), worker, round, 0x68);
        if (lseek(fd, 0, SEEK_END) < 0) {
            failf("seek old fd end failed worker=%u round=%u errno=%d", worker, round, errno);
        }
        write_all(fd, small, sizeof(small), victim);
        if (fsync(fd) != 0) {
            failf("fsync old fd append failed worker=%u round=%u errno=%d", worker, round, errno);
        }
        memset(actual, 0, sizeof(small));
        read_all_at(fd, actual, sizeof(small),
                    (off_t)IO_BLOCK + (off_t)round * SMALL_BLOCK, victim);
        if (memcmp(actual, small, sizeof(small)) != 0) {
            failf("unlinked old fd append verify failed worker=%u round=%u", worker, round);
        }
    }

    if (close(fd) != 0) {
        failf("close unlinked victim fd failed worker=%u errno=%d", worker, errno);
    }
}

static void case_unlink_while_open(void)
{
    char case_dir[PATH_MAX];

    ensure_case_dir(case_dir, sizeof(case_dir));
    spawn_workers(worker_unlink_while_open, case_dir, "unlink_while_open");
}

static uint64_t expected_allocator_hash(unsigned int worker)
{
    unsigned char buf[IO_BLOCK];
    uint64_t hash = FNV_OFFSET;

    for (unsigned int round = 0; round < g_rounds; round++) {
        for (unsigned int chunk = 0; chunk < 4; chunk++) {
            make_pattern(buf, sizeof(buf), worker, round, 0x70 + chunk);
            hash = fnv_update(hash, buf, sizeof(buf));
        }
    }
    return hash;
}

static void worker_allocator_churn(const char *case_dir, unsigned int worker)
{
    unsigned char buf[IO_BLOCK];
    char keep[PATH_MAX];
    char tmp[PATH_MAX];
    char name[96];
    int keep_fd;

    snprintf(name, sizeof(name), "alloc_keep_%02u.dat", worker);
    path_join(keep, sizeof(keep), case_dir, name);
    keep_fd = open(keep, O_CREAT | O_TRUNC | O_RDWR, 0666);
    if (keep_fd < 0) {
        failf("open allocator keep failed worker=%u errno=%d", worker, errno);
    }

    for (unsigned int round = 0; round < g_rounds; round++) {
        for (unsigned int chunk = 0; chunk < 4; chunk++) {
            make_pattern(buf, sizeof(buf), worker, round, 0x70 + chunk);
            write_all(keep_fd, buf, sizeof(buf), keep);
        }
        if (fsync(keep_fd) != 0) {
            failf("fsync allocator keep failed worker=%u round=%u errno=%d",
                  worker, round, errno);
        }

        snprintf(name, sizeof(name), "alloc_tmp_%02u_%04u.dat", worker, round);
        path_join(tmp, sizeof(tmp), case_dir, name);
        int tmp_fd = open(tmp, O_CREAT | O_TRUNC | O_RDWR, 0666);
        if (tmp_fd < 0) {
            failf("open allocator tmp failed worker=%u round=%u errno=%d",
                  worker, round, errno);
        }
        for (unsigned int chunk = 0; chunk < 4; chunk++) {
            make_pattern(buf, sizeof(buf), worker, round, 0x80 + chunk);
            write_all(tmp_fd, buf, sizeof(buf), tmp);
        }
        if (fsync(tmp_fd) != 0 || close(tmp_fd) != 0) {
            failf("sync allocator tmp failed worker=%u round=%u errno=%d",
                  worker, round, errno);
        }
        if (unlink(tmp) != 0) {
            failf("unlink allocator tmp failed worker=%u round=%u errno=%d",
                  worker, round, errno);
        }
    }

    if (close(keep_fd) != 0) {
        failf("close allocator keep failed worker=%u errno=%d", worker, errno);
    }
}

static void case_allocator_churn(void)
{
    char case_dir[PATH_MAX];

    ensure_case_dir(case_dir, sizeof(case_dir));
    spawn_workers(worker_allocator_churn, case_dir, "allocator_churn");

    for (unsigned int worker = 0; worker < g_workers; worker++) {
        char path[PATH_MAX];
        char name[64];
        snprintf(name, sizeof(name), "alloc_keep_%02u.dat", worker);
        path_join(path, sizeof(path), case_dir, name);
        verify_file_hash(path, expected_allocator_hash(worker), (off_t)g_rounds * 4 * IO_BLOCK);
    }
}

static void parse_args(int argc, char **argv)
{
    for (int i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--case") == 0 && i + 1 < argc) {
            g_case_name = argv[++i];
        } else if (strcmp(argv[i], "--root") == 0 && i + 1 < argc) {
            g_root = argv[++i];
        } else if (strcmp(argv[i], "--workers") == 0 && i + 1 < argc) {
            g_workers = (unsigned int)strtoul(argv[++i], NULL, 10);
        } else if (strcmp(argv[i], "--rounds") == 0 && i + 1 < argc) {
            g_rounds = (unsigned int)strtoul(argv[++i], NULL, 10);
        } else if (strcmp(argv[i], "--seed") == 0 && i + 1 < argc) {
            g_seed = strtoull(argv[++i], NULL, 10);
        } else {
            failf("unknown or incomplete argument: %s", argv[i]);
        }
    }
    if (g_rounds == 0) {
        failf("rounds must be > 0");
    }
    if (g_workers == 0 || g_workers > MAX_WORKERS) {
        failf("workers must be in 1..%u", MAX_WORKERS);
    }
}

int main(int argc, char **argv)
{
    parse_args(argc, argv);

    if (strcmp(g_case_name, "multi_file_write_verify") == 0) {
        case_multi_file_write_verify();
    } else if (strcmp(g_case_name, "multi_file_read_write") == 0) {
        case_multi_file_read_write();
    } else if (strcmp(g_case_name, "create_unlink_churn") == 0) {
        case_create_unlink_churn();
    } else if (strcmp(g_case_name, "rename_churn") == 0) {
        case_rename_churn();
    } else if (strcmp(g_case_name, "write_truncate_fsync") == 0) {
        case_write_truncate_fsync();
    } else if (strcmp(g_case_name, "unlink_while_open") == 0) {
        case_unlink_while_open();
    } else if (strcmp(g_case_name, "allocator_churn") == 0) {
        case_allocator_churn();
    } else {
        failf("unknown case");
    }

    printf("EXT4_PHASE2_CASE_PASS case=%s seed=%" PRIu64 " workers=%u rounds=%u\n",
           g_case_name, g_seed, g_workers, g_rounds);
    return 0;
}
