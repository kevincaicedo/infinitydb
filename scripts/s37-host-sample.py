#!/usr/bin/env python3
"""Record reference-host thermals, CPU activity, topology and storage counters."""
import json
from pathlib import Path
import sys
import time


def readings(pattern):
    result = {}
    for path in Path("/sys").glob(pattern):
        try:
            result[str(path)] = path.read_text().strip()
        except OSError as error:
            result[str(path)] = {"error": str(error)}
    return result


def snapshot():
    return {
        "unix_time": time.time(),
        "monotonic": time.monotonic(),
        "temperature": readings("class/hwmon/hwmon*/temp*_input"),
        "throttle": readings("devices/system/cpu/cpu*/thermal_throttle/*throttle_count"),
        "governor": readings("devices/system/cpu/cpu*/cpufreq/scaling_governor"),
        "epp": readings("devices/system/cpu/cpu*/cpufreq/energy_performance_preference"),
        "frequency": readings("devices/system/cpu/cpu*/cpufreq/scaling_cur_freq"),
        "turbo": readings("devices/system/cpu/intel_pstate/no_turbo"),
        "siblings": readings("devices/system/cpu/cpu*/topology/thread_siblings_list"),
        "cpu_stat": Path("/proc/stat").read_text(),
        "load": Path("/proc/loadavg").read_text(),
        "disk_stat": readings("block/nvme*n*/stat"),
        "disk_model": readings("class/nvme/nvme*/model"),
        "memory": Path("/proc/meminfo").read_text(),
    }


if __name__ == "__main__":
    with Path(sys.argv[1]).open("x") as output:
        while True:
            output.write(json.dumps(snapshot(), sort_keys=True) + "\n")
            output.flush()
            if len(sys.argv) > 2 and sys.argv[2] == "--once":
                break
            time.sleep(5)
