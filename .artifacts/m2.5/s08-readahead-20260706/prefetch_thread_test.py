import os, time, ctypes, threading, queue
libc = ctypes.CDLL("libc.so.6", use_errno=True)
DONTNEED = 4
path = os.path.expanduser("~/.cache/willneed-test.bin")
CHUNK = 1 << 20
N = 512

def evict(fd):
    os.fsync(fd); libc.posix_fadvise(fd, 0, 0, DONTNEED)

def burn(ms):
    t = time.perf_counter() + ms/1000.0
    while time.perf_counter() < t: pass

# prefetch thread: stays one chunk ahead, reads into page cache
def run_prefetch():
    fd = os.open(path, os.O_RDONLY)
    evict(fd); time.sleep(0.3)
    stop = False
    cursor = {"want": 1}
    cv = threading.Condition()
    def worker():
        pfd = os.open(path, os.O_RDONLY)
        done = 0
        while True:
            with cv:
                while cursor["want"] <= done and not stop:
                    cv.wait(0.01)
                if stop: break
                target = cursor["want"]
            while done < target and done < N:
                os.pread(pfd, CHUNK, done*CHUNK)  # populate page cache
                done += 1
        os.close(pfd)
    t = threading.Thread(target=worker); t.start()
    t0 = time.perf_counter(); read_t = 0.0
    for i in range(N):
        with cv:
            cursor["want"] = i + 2  # keep 1-2 chunks ahead
            cv.notify()
        r0 = time.perf_counter()
        os.pread(fd, CHUNK, i*CHUNK)
        read_t += time.perf_counter() - r0
        burn(0.9)
    stop = True
    with cv: cv.notify()
    total = time.perf_counter() - t0
    print(f"prefetch-thread: {N/1024/total:.2f} GiB/s eff, pread {read_t/N*1e3:.2f} ms/chunk")

run_prefetch()
