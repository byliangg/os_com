// SPDX-License-Identifier: MPL-2.0
#define _GNU_SOURCE

#include <stdint.h>
#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static int do_sync_op(const char *op, int fd) {
    if (strcmp(op, "fsync") == 0) {
        return fsync(fd);
    }
    if (strcmp(op, "fdatasync") == 0) {
        return fdatasync(fd);
    }
    if (strcmp(op, "syncfs") == 0) {
        return syncfs(fd);
    }

    errno = EINVAL;
    return -1;
}

static int do_pwrite_op(int fd, const char *offset_str, const char *length_str, const char *fill_str) {
    char *end = NULL;
    unsigned long long offset;
    unsigned long length;
    unsigned long fill;
    unsigned char *buf;
    size_t written = 0;

    errno = 0;
    offset = strtoull(offset_str, &end, 0);
    if (errno != 0 || end == offset_str || *end != '\0') {
        errno = EINVAL;
        return -1;
    }

    errno = 0;
    length = strtoul(length_str, &end, 0);
    if (errno != 0 || end == length_str || *end != '\0') {
        errno = EINVAL;
        return -1;
    }

    errno = 0;
    fill = strtoul(fill_str, &end, 0);
    if (errno != 0 || end == fill_str || *end != '\0' || fill > 0xffUL) {
        errno = EINVAL;
        return -1;
    }

    if (length == 0) {
        return 0;
    }

    buf = malloc(length);
    if (buf == NULL) {
        errno = ENOMEM;
        return -1;
    }
    memset(buf, (int)fill, length);

    while (written < length) {
        ssize_t rc = pwrite(fd, buf + written, length - written, (off_t)offset + (off_t)written);
        if (rc < 0) {
            if (errno == EINTR) {
                continue;
            }
            free(buf);
            return -1;
        }
        if (rc == 0) {
            errno = EIO;
            free(buf);
            return -1;
        }
        written += (size_t)rc;
    }

    free(buf);
    return 0;
}

int main(int argc, char *argv[]) {
    int fd;
    int rc;
    const char *op;
    const char *path;

    if (argc != 3 && argc != 6) {
        fprintf(stderr, "usage: %s <fsync|fdatasync|syncfs> <path>\n", argv[0]);
        fprintf(stderr, "   or: %s pwrite <path> <offset> <length> <fill_byte>\n", argv[0]);
        return 2;
    }

    op = argv[1];
    path = argv[2];

    if (strcmp(op, "pwrite") == 0) {
        fd = open(path, O_CREAT | O_RDWR | O_CLOEXEC, 0666);
    } else {
        fd = open(path, O_RDONLY | O_CLOEXEC);
        if (fd < 0) {
            fd = open(path, O_RDWR | O_CLOEXEC);
        }
    }
    if (fd < 0) {
        fprintf(stderr, "%s: open(%s) failed: %s\n", argv[0], path, strerror(errno));
        return 1;
    }

    if (strcmp(op, "pwrite") == 0) {
        if (argc != 6) {
            fprintf(stderr, "%s: pwrite requires <path> <offset> <length> <fill_byte>\n", argv[0]);
            close(fd);
            return 2;
        }
        rc = do_pwrite_op(fd, argv[3], argv[4], argv[5]);
    } else {
        rc = do_sync_op(op, fd);
    }
    if (rc < 0) {
        fprintf(stderr, "%s: %s(%s) failed: %s\n", argv[0], op, path, strerror(errno));
        close(fd);
        return 1;
    }

    if (close(fd) < 0) {
        fprintf(stderr, "%s: close(%s) failed: %s\n", argv[0], path, strerror(errno));
        return 1;
    }

    return 0;
}
