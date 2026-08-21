// fuaprobe — durable-write barrier latency probe: FLUSH-class (fdatasync) vs FUA-class (O_DSYNC).
// Usage: fuaprobe <dir> <mode> <write_bytes> <iters> [threads] [bg_mbps]
//   mode: buf-fdatasync | direct-fdatasync | direct-dsync | direct-rwfdsync | direct-dsync-unwritten
//   threads: independent files, one writer each (models N cells sharing one device)
//   bg_mbps: optional background buffered sequential writer with fsync every 64 MiB (models checkpoint/tier flush)
#define _GNU_SOURCE
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/uio.h>
#include <time.h>
#include <unistd.h>
#include <stdint.h>
#include <stdatomic.h>

#define FILE_BYTES (256ull << 20)
#define ALIGN 4096

static uint64_t now_ns(void) {
    struct timespec ts; clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + ts.tv_nsec;
}
static int cmp_u64(const void *a, const void *b) {
    uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b; return x < y ? -1 : x > y;
}

struct arg { const char *dir; const char *mode; size_t wb; int iters; int id; uint64_t *lat; double elapsed_s; };
static atomic_int stop_bg = 0;

static void prefill(const char *path, int unwritten) {
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) { perror("open prefill"); exit(1); }
    if (unwritten) {
        // Allocated but unwritten extents: the first O_DSYNC write must convert them (metadata) -> journal + FLUSH.
        if (posix_fallocate(fd, 0, FILE_BYTES) != 0) { perror("fallocate"); exit(1); }
    } else {
        char *z = aligned_alloc(ALIGN, 1 << 20); memset(z, 0, 1 << 20);
        for (uint64_t off = 0; off < FILE_BYTES; off += 1 << 20) {
            if (pwrite(fd, z, 1 << 20, off) != (1 << 20)) { perror("prefill pwrite"); exit(1); }
        }
        free(z);
    }
    if (fsync(fd) != 0) { perror("fsync prefill"); exit(1); }
    close(fd);
}

static void *writer(void *p) {
    struct arg *a = p;
    char path[512]; snprintf(path, sizeof path, "%s/probe-%d.bin", a->dir, a->id);
    int unwritten = strcmp(a->mode, "direct-dsync-unwritten") == 0;
    prefill(path, unwritten);
    int flags = O_RDWR;
    if (strncmp(a->mode, "direct", 6) == 0) flags |= O_DIRECT;
    if (strcmp(a->mode, "direct-dsync") == 0 || unwritten) flags |= O_DSYNC;
    int fd = open(path, flags);
    if (fd < 0) { perror("open"); exit(1); }
    char *buf = aligned_alloc(ALIGN, a->wb); memset(buf, 0xA5, a->wb);
    int use_fdatasync = strstr(a->mode, "fdatasync") != NULL;
    int use_rwf = strcmp(a->mode, "direct-rwfdsync") == 0;
    uint64_t t0 = now_ns();
    for (int i = 0; i < a->iters; i++) {
        off_t off = (off_t)((uint64_t)i * a->wb % FILE_BYTES);
        uint64_t s = now_ns();
        ssize_t n;
        if (use_rwf) { struct iovec iov = { buf, a->wb }; n = pwritev2(fd, &iov, 1, off, RWF_DSYNC); }
        else n = pwrite(fd, buf, a->wb, off);
        if (n != (ssize_t)a->wb) { perror("pwrite"); exit(1); }
        if (use_fdatasync && fdatasync(fd) != 0) { perror("fdatasync"); exit(1); }
        a->lat[i] = now_ns() - s;
    }
    a->elapsed_s = (now_ns() - t0) / 1e9;
    close(fd); free(buf);
    return NULL;
}

struct bg { const char *dir; double mbps; };
static void *background(void *p) {
    struct bg *b = p;
    char path[512]; snprintf(path, sizeof path, "%s/bg.bin", b->dir);
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    char *buf = malloc(1 << 20); memset(buf, 0x5A, 1 << 20);
    uint64_t written = 0, t0 = now_ns();
    while (!atomic_load(&stop_bg)) {
        off_t off = (off_t)(written % (4ull << 30));
        if (pwrite(fd, buf, 1 << 20, off) != (1 << 20)) { perror("bg pwrite"); break; }
        written += 1 << 20;
        if (written % (64ull << 20) == 0) fdatasync(fd);
        double target_s = (double)written / (b->mbps * 1048576.0);
        double el = (now_ns() - t0) / 1e9;
        if (target_s > el) { struct timespec ts = { 0, (long)((target_s - el) * 1e9) }; nanosleep(&ts, NULL); }
    }
    close(fd); free(buf); unlink(path);
    return NULL;
}

int main(int argc, char **argv) {
    if (argc < 5) { fprintf(stderr, "usage: %s dir mode write_bytes iters [threads] [bg_mbps]\n", argv[0]); return 2; }
    const char *dir = argv[1], *mode = argv[2];
    size_t wb = strtoull(argv[3], 0, 10); int iters = atoi(argv[4]);
    int threads = argc > 5 ? atoi(argv[5]) : 1; double bg_mbps = argc > 6 ? atof(argv[6]) : 0;
    pthread_t bgt; struct bg b = { dir, bg_mbps };
    if (bg_mbps > 0) { pthread_create(&bgt, NULL, background, &b); sleep(2); }
    pthread_t *th = calloc(threads, sizeof *th); struct arg *as = calloc(threads, sizeof *as);
    for (int t = 0; t < threads; t++) {
        as[t] = (struct arg){ dir, mode, wb, iters, t, calloc(iters, sizeof(uint64_t)), 0 };
        pthread_create(&th[t], NULL, writer, &as[t]);
    }
    uint64_t *all = calloc((size_t)iters * threads, sizeof(uint64_t)); double tot_ops = 0;
    for (int t = 0; t < threads; t++) {
        pthread_join(th[t], NULL);
        memcpy(all + (size_t)t * iters, as[t].lat, iters * sizeof(uint64_t));
        tot_ops += iters / as[t].elapsed_s;
    }
    if (bg_mbps > 0) { atomic_store(&stop_bg, 1); pthread_join(bgt, NULL); }
    size_t n = (size_t)iters * threads; qsort(all, n, sizeof(uint64_t), cmp_u64);
    printf("%-24s bytes=%-8zu thr=%d bg=%.0fMB/s  p50=%7.0f us  p90=%7.0f us  p99=%7.0f us  max=%8.0f us  barriers/s=%8.0f\n",
           mode, wb, threads, bg_mbps, all[n / 2] / 1e3, all[n * 9 / 10] / 1e3, all[n * 99 / 100] / 1e3, all[n - 1] / 1e3, tot_ops);
    for (int t = 0; t < threads; t++) { char path[512]; snprintf(path, sizeof path, "%s/probe-%d.bin", dir, t); unlink(path); }
    return 0;
}
