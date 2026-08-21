# fuaprobe — ADATA LEGEND 700 (DRAM-less Gen3, ext4, kernel 7.0.0-30), 2026-08-20, dev-tier
# queue/fua=1, write_cache=write back. 256 MiB pre-written files (written extents), sequential 4 KiB.. writes.
# mode: buf-fdatasync = today's production log path (buffered write + linked IORING_FSYNC DATASYNC)
#       direct-dsync  = O_DIRECT|O_DSYNC (FUA write, no flush)   direct-rwfdsync = pwritev2 RWF_DSYNC per write
#       direct-dsync-unwritten = same on fallocate'd-but-unwritten extents (the trap)
idle, 1 writer
buf-fdatasync            bytes=4096     thr=1  p50=    915 us  p90=    993 us  p99=   1272 us  max=    4551 us  barriers/s=    1056
direct-fdatasync         bytes=4096     thr=1  p50=    993 us  p90=   1000 us  p99=   1590 us  max=    4143 us  barriers/s=    1006
direct-dsync             bytes=4096     thr=1  p50=    294 us  p90=    303 us  p99=    648 us  max=    2056 us  barriers/s=    3120
direct-rwfdsync          bytes=4096     thr=1  p50=    295 us  p90=    378 us  p99=    682 us  max=    3588 us  barriers/s=    2934
direct-dsync-unwritten   bytes=4096     thr=1  p50=   1391 us  p90=   1421 us  p99=   2250 us  max=   12346 us  barriers/s=     709
buf-fdatasync            bytes=65536    thr=1  p50=    924 us  p90=   1122 us  p99=   1849 us  max=    4723 us  barriers/s=    1022
direct-dsync             bytes=65536    thr=1  p50=    372 us  p90=    405 us  p99=    677 us  max=    1941 us  barriers/s=    2606
direct-dsync             bytes=16384    thr=1  p50=    375 us  p90=    381 us  p99=    712 us  max=    1757 us  barriers/s=    2598
buf-fdatasync            bytes=1048576  thr=1  p50=   1602 us  p90=   1916 us  p99=   3412 us  max=   13031 us  barriers/s=     578
direct-dsync             bytes=1048576  thr=1  p50=   1008 us  p90=  21465 us  p99=  25334 us  max=   56130 us  barriers/s=     176
concurrent writers (one file each = N cells sharing the device), steady state
direct-dsync             bytes=4096     thr=2  p50=    575 us  p90=    736 us  p99=   1277 us  max=    3420 us  barriers/s=    3094
buf-fdatasync            bytes=4096     thr=2  p50=   1652 us  p90=   1735 us  p99=   2189 us  max=    4809 us  barriers/s=    1177
direct-dsync             bytes=4096     thr=4  p50=    336 us  p90=    649 us  p99=   1262 us  max=    4167 us  barriers/s=    9918
buf-fdatasync            bytes=4096     thr=4  p50=   4027 us  p90=   4474 us  p99=   5106 us  max=    7542 us  barriers/s=    1041
direct-fdatasync         bytes=4096     thr=4  p50=   3078 us  p90=   3487 us  p99=   4157 us  max=    9021 us  barriers/s=    1388
direct-dsync             bytes=16384    thr=4  p50=    296 us  p90=    582 us  p99=    836 us  max=   14231 us  barriers/s=   10983
direct-dsync             bytes=65536    thr=4  p50=    436 us  p90=    727 us  p99=   1446 us  max=   13931 us  barriers/s=    8009
buf-fdatasync            bytes=65536    thr=4  p50=   3161 us  p90=   4069 us  p99=  28307 us  max=   30352 us  barriers/s=     835
direct-fdatasync         bytes=65536    thr=4  p50=   3140 us  p90=   3490 us  p99=   4312 us  max=   23582 us  barriers/s=    1356
direct-dsync             bytes=262144   thr=4  p50=    902 us  p90=   1215 us  p99=   2536 us  max=   20521 us  barriers/s=    3903
buf-fdatasync            bytes=262144   thr=4  p50=   3791 us  p90=   4305 us  p99=   5753 us  max=   25310 us  barriers/s=    1062
direct-fdatasync         bytes=262144   thr=4  p50=   3839 us  p90=   4382 us  p99=   5346 us  max=   28830 us  barriers/s=    1044
direct-dsync             bytes=1048576  thr=4  p50=   3058 us  p90=  25019 us  p99=  45311 us  max=   72484 us  barriers/s=     471
buf-fdatasync            bytes=1048576  thr=4  p50=   5852 us  p90=   6976 us  p99=   8523 us  max=   19414 us  barriers/s=     672
direct-fdatasync         bytes=1048576  thr=4  p50=   5821 us  p90=   6823 us  p99=   8448 us  max=   24612 us  barriers/s=     686
with a 100 MB/s background buffered sequential writer (checkpoint / tier-flush analog), 1 writer 4 KiB
buf-fdatasync            bytes=4096     thr=1 bg=100MB/s  p50=   1259 us  p90=   1290 us  p99=   2523 us  max=   37548 us  barriers/s=     736
direct-dsync             bytes=4096     thr=1 bg=100MB/s  p50=    372 us  p90=    381 us  p99=    670 us  max=   15499 us  barriers/s=    2529

# F2 discriminator (same binary infinityd e5b2c48-era target/release build of 2026-08-19, native, no docker; 200k × 1 KB load, 128k ops, 32 conns, zipfian, 100% write, FSYNC always)
# INFO persistence is cell-scoped (the connection's cell).
4 cells (taskset 0,2,4,6): throughput 6,283  p50 5.013 ms  p99 8.383  max 16.07   fsync_latency_p50_us 2751  fsync_group_p50 7   → p50 ÷ fsync = 1.82
1 cell  (taskset 0):       throughput 11,926 p50 2.539 ms  p99 4.084  max 71.05   fsync_latency_p50_us 1535  fsync_group_p50 16  → p50 ÷ fsync = 1.65
