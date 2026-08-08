import os, time, ctypes
libc = ctypes.CDLL("libc.so.6", use_errno=True)
DONTNEED, WILLNEED, SEQUENTIAL = 4, 3, 2
path = os.path.expanduser("~/.cache/willneed-test.bin")
CHUNK = 1 << 20
N = 512

def evict(fd):
    os.fsync(fd)
    libc.posix_fadvise(fd, 0, 0, DONTNEED)

def burn(ms):
    t = time.perf_counter() + ms/1000.0
    while time.perf_counter() < t: pass

def run(name, seq=False, ahead=0, batch=1):
    fd = os.open(path, os.O_RDONLY)
    evict(fd); time.sleep(0.3)
    if seq: libc.posix_fadvise(fd, 0, 0, SEQUENTIAL)
    t0 = time.perf_counter(); read_t = 0.0
    for i in range(N):
        if ahead and i % batch == 0:
            libc.posix_fadvise(fd, (i+1)*CHUNK, ahead*CHUNK, WILLNEED)
        r0 = time.perf_counter()
        buf = os.pread(fd, CHUNK, i*CHUNK)
        read_t += time.perf_counter() - r0
        burn(0.9)
    total = time.perf_counter() - t0
    print(f"{name:28s}: {N/1024/total:.2f} GiB/s eff, pread {read_t/N*1e3:.2f} ms/chunk")
    os.close(fd)

run("baseline")
run("sequential", seq=True)
run("willneed+1 every chunk", ahead=1)
run("willneed+4 every 4", ahead=4, batch=4)
run("willneed+8 every 8", ahead=8, batch=8)
run("seq + willneed+8/8", seq=True, ahead=8, batch=8)
