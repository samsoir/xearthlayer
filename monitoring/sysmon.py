#!/usr/bin/env python3
"""
sysmon — Flight session monitor for X-Plane on Linux.

Collects system-wide performance data at high resolution and correlates
it with X-Plane application events.  Designed for diagnosing micro-stutters,
memory pressure, IO contention, and GPU bottlenecks during flight sessions
with ortho scenery streaming (XEarthLayer, AutoOrtho).

Sampling rates
  200 ms   CPU usage, disk IO, memory (captures burst detail)
  1 s      GPU, interrupts, vmstat, PSI, CPU frequency, per-process

CSV output
  cpu.csv      Per-CPU usage breakdown (user/sys/iowait/irq/idle)
  mem.csv      RAM, swap, dirty pages, writeback
  io.csv       Per-device throughput, latency, utilization
  vram.csv     GPU memory, utilization, clocks, power, PCIe, throttling
  irq.csv      Interrupt rates per CPU
  proc.csv     Per-process CPU%, RSS, IO, threads
  vmstat.csv   Page reclaim, allocation stalls, TLB shootdowns, swap IO
  psi.csv      Pressure Stall Information (CPU, memory, IO)
  freq.csv     Per-CPU clock frequency

Optional: X-Plane in-sim telemetry (--xplane flag)
  xplane_telemetry.csv  FPS, CPU/GPU frame time, position, speed (5 Hz default)
  Spawns xplane_telemetry.py as subprocess (UDP RREF on port 49000).
  Requires: X-Plane Settings > Network > Accept incoming connections.

Post-run correlation
  Matches IO spikes and allocation stalls against X-Plane Log.txt events
  (DSF loads, airport loading, weather changes).  Output: xplane_events.csv

Companion scripts (same directory)
  xplane_telemetry.py   X-Plane FPS/CPU/GPU telemetry via UDP RREF (auto-started with --xplane)
  sysmon_trace.sh       bpftrace sidecar — Direct Reclaim, slow IO, DMA fences
  post_crash.sh         Post-crash GPU / kernel diagnostics
  cgwatcher.py          Dynamic CPU priority for competing workloads

Requirements
  Python 3.9+, Linux kernel 4.20+ (PSI support)
  Optional: nvidia-ml-py (pip install nvidia-ml-py) for direct NVML GPU access
"""

__version__ = "3.0"

import argparse
import time, os, subprocess, statistics, sys, re, csv, threading, signal
from collections import defaultdict
from pathlib import Path
from datetime import datetime

# ─── GPU backend (deferred init — call init_gpu() from main) ─────────
_USE_NVML = False
_NVML_HANDLE = None
_NVML_LIB = None  # pynvml module reference (local import, stored globally)
_GPU_BACKEND = "none"  # "nvml", "nvidia-smi", "none"
_NVML_FAIL_COUNT = 0
_NVML_FAIL_MAX = 3  # Switch to nvidia-smi after this many consecutive failures


def init_gpu(disable=False):
    """Initialize GPU monitoring.  Returns backend name string."""
    global _USE_NVML, _NVML_HANDLE, _NVML_LIB, _GPU_BACKEND

    if disable:
        _GPU_BACKEND = "disabled"
        return _GPU_BACKEND

    # Try NVML direct bindings first (fastest, no subprocess overhead)
    try:
        import pynvml
        pynvml.nvmlInit()
        _NVML_HANDLE = pynvml.nvmlDeviceGetHandleByIndex(0)
        _NVML_LIB = pynvml
        _USE_NVML = True
        name = pynvml.nvmlDeviceGetName(_NVML_HANDLE)
        if isinstance(name, bytes):
            name = name.decode()
        _GPU_BACKEND = f"NVML ({name})"
        return _GPU_BACKEND
    except Exception:
        pass

    # Fallback: nvidia-smi subprocess
    try:
        out = subprocess.check_output(
            ["nvidia-smi", "--query-gpu=name", "--format=csv,noheader"],
            stderr=subprocess.DEVNULL, timeout=3
        ).decode().strip()
        _GPU_BACKEND = f"nvidia-smi ({out})"
        return _GPU_BACKEND
    except Exception:
        pass

    # Detect AMD (informational — no GPU sampling yet)
    try:
        with open("/sys/class/drm/card0/device/vendor") as f:
            vendor = f.read().strip()
        if vendor == "0x1002":
            _GPU_BACKEND = "AMD detected (GPU sampling not yet supported)"
            return _GPU_BACKEND
    except FileNotFoundError:
        pass

    _GPU_BACKEND = "none (no NVIDIA GPU found)"
    return _GPU_BACKEND


# ─── Configuration defaults (overridden by CLI args in main) ──────────
DURATION = int(os.environ.get("SYSMON_DURATION", 1200))
INTERVAL = float(os.environ.get("SYSMON_INTERVAL", 0.2))
OUTDIR = Path(os.environ.get("SYSMON_OUTDIR", "/tmp/sysmon_out"))

PROC_PATTERNS = [p.strip() for p in
                 os.environ.get("SYSMON_PROCS",
                                "X-Plane,autoortho,qemu-system,"
                                "xearthlayer").split(",")]

NUM_CPUS = os.cpu_count()
CLK_TCK = os.sysconf("SC_CLK_TCK")

# ─── helpers ───────────────────────────────────────────────────────────

def read_file(path):
    with open(path) as f:
        return f.read()

def sum_prefix(d, prefix):
    """Sum all values in dict whose keys start with prefix."""
    return sum(v for k, v in d.items() if k.startswith(prefix))

def fmt3(lst):
    """Format avg/max/p95 for a list of values."""
    if not lst or len(lst) < 2:
        return "—"
    avg = statistics.mean(lst)
    mx = max(lst)
    p95 = sorted(lst)[int(len(lst) * 0.95)] if len(lst) > 20 else mx
    return f"{avg:5.1f}/{mx:5.1f}/{p95:5.1f}"

# ─── CPU ───────────────────────────────────────────────────────────────

def parse_cpu_stat():
    """Parse /proc/stat for per-CPU times and context switch count."""
    lines = read_file("/proc/stat").splitlines()
    cpus = {}
    ctxt = 0
    for line in lines:
        if line.startswith("cpu"):
            parts = line.split()
            name = parts[0]
            vals = list(map(int, parts[1:]))
            if name == "cpu":
                cpus["all"] = vals
            else:
                cpus[int(name.replace("cpu", ""))] = vals
        elif line.startswith("ctxt "):
            ctxt = int(line.split()[1])
    return cpus, ctxt

def cpu_delta(prev, curr):
    result = {}
    for key in curr:
        if key not in prev:
            continue
        p, c = prev[key], curr[key]
        d = [c[i] - p[i] for i in range(len(c))]
        total = sum(d)
        if total == 0:
            result[key] = {"user": 0, "sys": 0, "iowait": 0, "irq": 0,
                           "softirq": 0, "idle": 0, "steal": 0, "guest": 0}
        else:
            result[key] = {
                "user": (d[0] + d[1]) / total * 100,
                "sys": d[2] / total * 100,
                "idle": d[3] / total * 100,
                "iowait": d[4] / total * 100,
                "irq": d[5] / total * 100,
                "softirq": d[6] / total * 100,
                "steal": d[7] / total * 100,
                "guest": (d[8] + d[9]) / total * 100 if len(d) > 9
                         else d[8] / total * 100,
            }
    return result

# ─── CPU frequency ─────────────────────────────────────────────────────

def parse_cpu_freq():
    """Read scaling_cur_freq for each CPU. Returns list of MHz values."""
    freqs = []
    for i in range(NUM_CPUS):
        try:
            khz = int(read_file(
                f"/sys/devices/system/cpu/cpu{i}/cpufreq/scaling_cur_freq"
            ).strip())
            freqs.append(khz / 1000)
        except (FileNotFoundError, PermissionError, ValueError):
            freqs.append(0)
    return freqs

# ─── interrupts ────────────────────────────────────────────────────────

def parse_interrupts():
    lines = read_file("/proc/interrupts").splitlines()
    result = {}
    for line in lines[1:]:
        parts = line.split()
        if len(parts) < NUM_CPUS + 1:
            continue
        irq_name = parts[0].rstrip(":")
        counts = []
        for i in range(1, NUM_CPUS + 1):
            try:
                counts.append(int(parts[i]))
            except (ValueError, IndexError):
                counts.append(0)
        desc = " ".join(parts[NUM_CPUS + 1:])
        result[irq_name] = {"counts": counts, "desc": desc}
    return result

def interrupt_delta(prev, curr):
    result = {}
    for irq in curr:
        if irq in prev:
            delta = [curr[irq]["counts"][i] - prev[irq]["counts"][i]
                     for i in range(NUM_CPUS)]
            if sum(delta) > 0:
                result[irq] = {"delta": delta, "desc": curr[irq]["desc"]}
    return result

# ─── memory ────────────────────────────────────────────────────────────

def parse_meminfo():
    result = {}
    for line in read_file("/proc/meminfo").splitlines():
        parts = line.split()
        result[parts[0].rstrip(":")] = int(parts[1])
    return result

# ─── disk IO ──────────────────────────────────────────────────────────

def parse_diskstats():
    result = {}
    for line in read_file("/proc/diskstats").splitlines():
        parts = line.split()
        if len(parts) < 14:
            continue
        dev = parts[2]
        if (any(dev.startswith(p) for p in ["nvme", "sd"])
                and (not dev[-1].isdigit() or dev.endswith("n1")
                     or (dev.startswith("sd") and len(dev) == 3))):
            result[dev] = {
                "reads": int(parts[3]),
                "sectors_read": int(parts[5]),
                "ms_reading": int(parts[6]),
                "writes": int(parts[7]),
                "sectors_written": int(parts[9]),
                "ms_writing": int(parts[10]),
                "ios_in_progress": int(parts[11]),
                "weighted_ms_io": int(parts[13]),
            }
    return result

def diskstat_delta(prev, curr, dt):
    result = {}
    for dev in curr:
        if dev not in prev:
            continue
        p, c = prev[dev], curr[dev]
        dr = c["reads"] - p["reads"]
        dw = c["writes"] - p["writes"]
        dsr = c["sectors_read"] - p["sectors_read"]
        dsw = c["sectors_written"] - p["sectors_written"]
        dmr = c["ms_reading"] - p["ms_reading"]
        dmw = c["ms_writing"] - p["ms_writing"]
        dwio = c["weighted_ms_io"] - p["weighted_ms_io"]
        result[dev] = {
            "r_per_s": dr / dt,
            "w_per_s": dw / dt,
            "rMB_per_s": dsr * 512 / 1024 / 1024 / dt,
            "wMB_per_s": dsw * 512 / 1024 / 1024 / dt,
            "avg_r_lat_ms": dmr / dr if dr > 0 else 0,
            "avg_w_lat_ms": dmw / dw if dw > 0 else 0,
            "io_util_pct": min(dwio / (dt * 1000) * 100, 100),
            "ios_in_progress": c["ios_in_progress"],
        }
    return result

# ─── GPU / VRAM ───────────────────────────────────────────────────────

def get_vram():
    """Get NVIDIA GPU stats. Uses NVML direct bindings when available,
    falls back to nvidia-smi subprocess.
    Returns CSV string: mem_used,mem_total,mem_free,temp,gpu_util,
                        mem_util,gpu_clock,mem_clock,power,
                        pcie_tx_kbs,pcie_rx_kbs,throttle_reasons,perf_state"""
    global _USE_NVML, _NVML_FAIL_COUNT, _GPU_BACKEND
    if _GPU_BACKEND == "disabled":
        return ""
    if _USE_NVML:
        nv = _NVML_LIB
        try:
            mem = nv.nvmlDeviceGetMemoryInfo(_NVML_HANDLE)
            util = nv.nvmlDeviceGetUtilizationRates(_NVML_HANDLE)
            temp = nv.nvmlDeviceGetTemperature(
                _NVML_HANDLE, nv.NVML_TEMPERATURE_GPU)
            clk_gpu = nv.nvmlDeviceGetClockInfo(
                _NVML_HANDLE, nv.NVML_CLOCK_GRAPHICS)
            clk_mem = nv.nvmlDeviceGetClockInfo(
                _NVML_HANDLE, nv.NVML_CLOCK_MEM)
            power = nv.nvmlDeviceGetPowerUsage(_NVML_HANDLE)
            # Extended GPU diagnostics
            try:
                pcie_tx = nv.nvmlDeviceGetPcieThroughput(
                    _NVML_HANDLE, nv.NVML_PCIE_UTIL_TX_BYTES) // 1024
                pcie_rx = nv.nvmlDeviceGetPcieThroughput(
                    _NVML_HANDLE, nv.NVML_PCIE_UTIL_RX_BYTES) // 1024
            except Exception:
                pcie_tx = pcie_rx = 0
            try:
                throttle = nv.nvmlDeviceGetCurrentClocksThrottleReasons(
                    _NVML_HANDLE)
            except Exception:
                throttle = 0
            try:
                pstate = nv.nvmlDeviceGetPerformanceState(_NVML_HANDLE)
            except Exception:
                pstate = 0
            _NVML_FAIL_COUNT = 0  # Reset on success
            return (f"{mem.used // 1048576}, {mem.total // 1048576}, "
                    f"{mem.free // 1048576}, {temp}, {util.gpu}, "
                    f"{util.memory}, {clk_gpu}, {clk_mem}, "
                    f"{power / 1000:.2f}, "
                    f"{pcie_tx}, {pcie_rx}, {throttle}, {pstate}")
        except Exception as e:
            _NVML_FAIL_COUNT += 1
            if _NVML_FAIL_COUNT == 1:
                print(f"  NVML query failed: {e}")
            if _NVML_FAIL_COUNT >= _NVML_FAIL_MAX:
                print(f"  NVML failed {_NVML_FAIL_COUNT}x, switching to nvidia-smi")
                _USE_NVML = False
                _GPU_BACKEND = _GPU_BACKEND.replace("NVML", "nvidia-smi (NVML fallback)")
            # Fall through to nvidia-smi on this call too
    # Fallback: nvidia-smi subprocess
    try:
        out = subprocess.check_output(
            ["nvidia-smi",
             "--query-gpu=memory.used,memory.total,memory.free,"
             "temperature.gpu,utilization.gpu,utilization.memory,"
             "clocks.current.graphics,clocks.current.memory,power.draw,"
             "pstate",
             "--format=csv,noheader,nounits"],
            stderr=subprocess.DEVNULL, timeout=2
        ).decode().strip()
        if not out:
            return ""
        # nvidia-smi returns 10 fields, add pcie_tx, pcie_rx, throttle
        return out + ", 0, 0, 0"
    except Exception as e:
        print(f"  [ERROR] nvidia-smi failed: {e}", flush=True)
        return ""

# ─── vmstat ───────────────────────────────────────────────────────────

def parse_vmstat():
    result = {}
    for line in read_file("/proc/vmstat").splitlines():
        parts = line.split()
        if len(parts) == 2:
            try:
                result[parts[0]] = int(parts[1])
            except ValueError:
                pass
    return result

# ─── PSI ──────────────────────────────────────────────────────────────

def parse_psi():
    """Parse /proc/pressure/{cpu,memory,io}."""
    result = {}
    empty = {"some_avg10": 0, "some_avg60": 0,
             "full_avg10": 0, "full_avg60": 0}
    for resource in ["cpu", "memory", "io"]:
        try:
            data = dict(empty)
            for line in read_file(f"/proc/pressure/{resource}").splitlines():
                parts = line.split()
                if not parts:
                    continue
                kind = parts[0]  # "some" or "full"
                for part in parts[1:]:
                    if "=" in part:
                        k, v = part.split("=")
                        key = f"{kind}_{k}"
                        if key in data:
                            data[key] = float(v)
            result[resource] = data
        except (FileNotFoundError, PermissionError):
            result[resource] = dict(empty)
    return result

# ─── X-Plane Log.txt parsing ─────────────────────────────────────────

XPLANE_LOG = os.environ.get("SYSMON_XPLANE_LOG", "")  # overridden by --xplane-log

# Auto-detect common X-Plane log locations
XPLANE_LOG_PATHS = [
    Path.home() / "X-Plane-12" / "Log.txt",
    Path.home() / "X-Plane-12-Native" / "Log.txt",
    Path.home() / ".local" / "share" / "X-Plane-12" / "Log.txt",
]

def find_xplane_log():
    """Find X-Plane Log.txt, prefer env var, then auto-detect."""
    if XPLANE_LOG:
        p = Path(XPLANE_LOG)
        return p if p.exists() else None
    for p in XPLANE_LOG_PATHS:
        if p.exists():
            return p
    return None

_TS_RE = re.compile(r"^(\d+):(\d{2}):(\d{2})\.(\d{3})\s+(.*)")
_START_RE = re.compile(r"X-Plane Started on (.+)")

def parse_xplane_log(log_path):
    """Parse X-Plane Log.txt, return (start_unix_ts, list of events).
    Each event: (unix_ts, category, message)
    Categories: DSF, AIRPORT, WEATHER, PLUGIN, ERROR, OTHER
    """
    start_ts = None
    events = []

    with open(log_path) as f:
        for line in f:
            # Find start time
            m = _START_RE.search(line)
            if m:
                try:
                    start_ts = datetime.strptime(
                        m.group(1).strip(), "%a %b %d %H:%M:%S %Y"
                    ).timestamp()
                except ValueError:
                    pass
                continue

            m = _TS_RE.match(line)
            if not m or start_ts is None:
                continue

            h, mn, s, ms = int(m.group(1)), int(m.group(2)), \
                           int(m.group(3)), int(m.group(4))
            offset = h * 3600 + mn * 60 + s + ms / 1000
            ts = start_ts + offset
            rest = m.group(5)

            # Categorize
            if "I/SCN: DSF load" in rest:
                # Extract file and load time
                dm = re.search(r"DSF load time: (\d+) for file (.+?)\.dsf", rest)
                if dm:
                    load_us = int(dm.group(1))
                    dsf_file = dm.group(2).split("/")[-1]
                    events.append((ts, "DSF",
                        f"{dsf_file}.dsf ({load_us/1000:.0f}ms)"))
            elif "I/SCN: Loading sim objects" in rest:
                events.append((ts, "AIRPORT", rest.split("I/SCN: ")[-1]))
            elif "I/WXR:" in rest:
                events.append((ts, "WEATHER", rest.split("I/WXR: ")[-1]))
            elif "E/" in rest:
                events.append((ts, "ERROR", rest[:120]))
            elif "I/SCN: Preload" in rest:
                pm = re.search(r"Preload time: (\d+)", rest)
                if pm and int(pm.group(1)) > 100000:
                    events.append((ts, "PRELOAD",
                        f"Preload {int(pm.group(1))/1000:.0f}ms"))

    return start_ts, events

def correlate_events(csv_path, events, ts_col=0, val_col=None,
                     threshold=None, window=2.0):
    """Find CSV rows exceeding threshold and match nearby X-Plane events.
    Returns list of (csv_ts, csv_val, nearby_events).
    """
    if not events or not csv_path.exists():
        return []

    hits = []
    try:
        with open(csv_path) as f:
            reader = csv.reader(f)
            header = next(reader)
            for row in reader:
                if not row or len(row) <= max(ts_col, val_col or 0):
                    continue
                try:
                    ts = float(row[ts_col])
                    val = float(row[val_col]) if val_col is not None else 0
                except (ValueError, IndexError):
                    continue
                if threshold is not None and val < threshold:
                    continue
                # Find events within window
                nearby = [(e_ts, cat, msg) for e_ts, cat, msg in events
                          if abs(e_ts - ts) <= window]
                if nearby:
                    hits.append((ts, val, nearby))
    except Exception as e:
        print(f"  [ERROR] correlate_events failed: {e}", flush=True)
    return hits

# ─── per-process ──────────────────────────────────────────────────────

def find_tracked_procs(patterns):
    """Scan /proc for processes matching name patterns.
    Returns dict pid -> display_name."""
    result = {}
    try:
        entries = os.listdir("/proc")
    except OSError as e:
        print(f"  [ERROR] Cannot list /proc: {e}", flush=True)
        return result
    for entry in entries:
        if not entry.isdigit():
            continue
        pid = int(entry)
        try:
            comm = read_file(f"/proc/{pid}/comm").strip()
            matched = False
            for pat in patterns:
                if pat.lower() in comm.lower():
                    result[pid] = comm
                    matched = True
                    break
            if not matched:
                cmdline = read_file(f"/proc/{pid}/cmdline") \
                    .replace("\0", " ").strip()
                for pat in patterns:
                    if pat.lower() in cmdline.lower():
                        result[pid] = f"{comm}[{pat}]"
                        break
        except (FileNotFoundError, PermissionError, ProcessLookupError):
            continue
    return result

def read_proc_stats(pid):
    """Read CPU times, IO counters, RSS for a single PID."""
    try:
        raw = read_file(f"/proc/{pid}/stat")
        start = raw.index("(")
        end = raw.rindex(")")
        fields = raw[end + 2:].split()
        utime = int(fields[11])
        stime = int(fields[12])
        num_threads = int(fields[17])
    except (FileNotFoundError, PermissionError, ProcessLookupError):
        return None
    except Exception as e:
        print(f"  [ERROR] read_proc_stats({pid}) stat failed: {e}", flush=True)
        return None

    read_bytes = write_bytes = 0
    try:
        for line in read_file(f"/proc/{pid}/io").splitlines():
            if line.startswith("read_bytes:"):
                read_bytes = int(line.split(":")[1])
            elif line.startswith("write_bytes:"):
                write_bytes = int(line.split(":")[1])
    except (FileNotFoundError, PermissionError):
        pass  # OK: io not readable for some processes (permission)

    rss_kb = 0
    try:
        for line in read_file(f"/proc/{pid}/status").splitlines():
            if line.startswith("VmRSS:"):
                rss_kb = int(line.split()[1])
                break
    except (FileNotFoundError, PermissionError):
        pass  # OK: process may have exited

    return {"utime": utime, "stime": stime, "num_threads": num_threads,
            "read_bytes": read_bytes, "write_bytes": write_bytes,
            "rss_kb": rss_kb}

# ═══════════════════════════════════════════════════════════════════════
#  Stats — replaces 50+ individual accumulator lists
# ═══════════════════════════════════════════════════════════════════════

class Stats:
    """Accumulates all sampled metrics for the summary report."""

    def __init__(self):
        # Per-CPU: stats.cpu[cpu_id]["user"] -> list
        self.cpu = defaultdict(lambda: defaultdict(list))
        self.freq = defaultdict(list)
        # Memory
        self.mem = defaultdict(list)
        # Disk IO: stats.io[dev]["rMB_per_s"] -> list
        self.io = defaultdict(lambda: defaultdict(list))
        # GPU
        self.gpu = defaultdict(list)
        # vmstat
        self.vm = defaultdict(list)
        # PSI
        self.psi = defaultdict(list)
        # Per-process: stats.proc[name]["cpu"] -> list
        self.proc = defaultdict(lambda: defaultdict(list))
        # Interrupts
        self.irq_totals = defaultdict(lambda: [0] * NUM_CPUS)
        self.irq_descs = {}

# ═══════════════════════════════════════════════════════════════════════
#  CSVWriters — manages all 9 CSV output files
# ═══════════════════════════════════════════════════════════════════════

class CSVWriters:
    """Opens, writes headers, and provides per-subsystem write methods."""

    def __init__(self, outdir):
        self._files = []
        self.cpu = self._open(outdir / "cpu.csv",
            "timestamp,cpu_id,user,sys,iowait,irq,softirq,"
            "idle,steal,guest")
        self.mem = self._open(outdir / "mem.csv",
            "timestamp,total_mb,used_mb,free_mb,available_mb,buffers_mb,"
            "cached_mb,swap_used_mb,swap_free_mb,dirty_mb,writeback_mb")
        self.io = self._open(outdir / "io.csv",
            "timestamp,device,r_per_s,w_per_s,rMB_per_s,wMB_per_s,"
            "avg_r_lat_ms,avg_w_lat_ms,io_util_pct,ios_in_progress")
        self.vram = self._open(outdir / "vram.csv",
            "timestamp,mem_used_mib,mem_total_mib,mem_free_mib,temp_c,"
            "gpu_util_pct,mem_util_pct,gpu_clock_mhz,mem_clock_mhz,"
            "power_w,pcie_tx_kbs,pcie_rx_kbs,throttle_reasons,perf_state")
        self.irq = self._open(outdir / "irq.csv",
            "timestamp,irq,desc,total_rate,"
            + ",".join(f"cpu{i}" for i in range(NUM_CPUS)))
        self.proc = self._open(outdir / "proc.csv",
            "timestamp,pid,name,cpu_pct,rss_mb,"
            "io_read_mbs,io_write_mbs,threads")
        self.vmstat = self._open(outdir / "vmstat.csv",
            "timestamp,ctxt_s,pgfault_s,pgmajfault_s,"
            "pgscan_kswapd_s,pgscan_direct_s,"
            "pgsteal_kswapd_s,pgsteal_direct_s,"
            "allocstall_s,compact_stall_s,"
            "tlb_shootdown_s,nr_dirty,nr_writeback,"
            "pswpin_s,pswpout_s,wset_refault_anon_s,"
            "wset_refault_file_s,thp_fault_fallback_s")
        self.psi = self._open(outdir / "psi.csv",
            "timestamp,cpu_some10,cpu_some60,"
            "mem_some10,mem_some60,mem_full10,mem_full60,"
            "io_some10,io_some60,io_full10,io_full60")
        self.freq = self._open(outdir / "freq.csv",
            "timestamp,"
            + ",".join(f"cpu{i}_mhz" for i in range(NUM_CPUS)))

    def _open(self, path, header):
        f = open(path, "w", buffering=1)  # line-buffered: flush after every \n
        f.write(header + "\n")
        self._files.append(f)
        return f

    def close(self):
        for f in self._files:
            f.close()

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()

    def write_cpu(self, ts, cpu_id, v):
        self.cpu.write(
            f"{ts:.3f},{cpu_id},{v['user']:.1f},{v['sys']:.1f},"
            f"{v['iowait']:.1f},{v['irq']:.1f},{v['softirq']:.1f},"
            f"{v['idle']:.1f},{v['steal']:.1f},{v['guest']:.1f}\n")

    def write_io(self, ts, dev, v):
        self.io.write(
            f"{ts:.3f},{dev},{v['r_per_s']:.1f},{v['w_per_s']:.1f},"
            f"{v['rMB_per_s']:.2f},{v['wMB_per_s']:.2f},"
            f"{v['avg_r_lat_ms']:.2f},{v['avg_w_lat_ms']:.2f},"
            f"{v['io_util_pct']:.1f},{v['ios_in_progress']}\n")

    def write_mem(self, ts, total, used, free, avail, buffers, cached,
                  swap_used, swap_free, dirty, writeback):
        self.mem.write(
            f"{ts:.3f},{total:.0f},{used:.0f},{free:.0f},{avail:.0f},"
            f"{buffers:.0f},{cached:.0f},{swap_used:.0f},{swap_free:.0f},"
            f"{dirty:.1f},{writeback:.1f}\n")

    def write_irq(self, ts, irq, desc, rate):
        self.irq.write(
            f"{ts:.3f},{irq},{desc},{sum(rate):.0f},"
            + ",".join(f"{r:.0f}" for r in rate) + "\n")

    def write_vram(self, ts, raw):
        self.vram.write(f"{ts:.3f},{raw}\n")

    def write_vmstat(self, ts, ctxt, pgfault, pgmajfault, pgscan_k, pgscan_d,
                     pgsteal_k, pgsteal_d, alloc, compact, tlb,
                     nr_dirty, nr_wb,
                     pswpin, pswpout, wset_anon, wset_file, thp_fallback):
        self.vmstat.write(
            f"{ts:.3f},{ctxt:.0f},{pgfault:.0f},"
            f"{pgmajfault:.0f},{pgscan_k:.0f},{pgscan_d:.0f},"
            f"{pgsteal_k:.0f},{pgsteal_d:.0f},"
            f"{alloc:.1f},{compact:.1f},{tlb:.0f},"
            f"{nr_dirty},{nr_wb},"
            f"{pswpin:.0f},{pswpout:.0f},{wset_anon:.0f},"
            f"{wset_file:.0f},{thp_fallback:.0f}\n")

    def write_psi(self, ts, psi):
        self.psi.write(
            f"{ts:.3f},"
            f"{psi['cpu']['some_avg10']:.2f},"
            f"{psi['cpu']['some_avg60']:.2f},"
            f"{psi['memory']['some_avg10']:.2f},"
            f"{psi['memory']['some_avg60']:.2f},"
            f"{psi['memory']['full_avg10']:.2f},"
            f"{psi['memory']['full_avg60']:.2f},"
            f"{psi['io']['some_avg10']:.2f},"
            f"{psi['io']['some_avg60']:.2f},"
            f"{psi['io']['full_avg10']:.2f},"
            f"{psi['io']['full_avg60']:.2f}\n")

    def write_freq(self, ts, freqs):
        self.freq.write(f"{ts:.3f},"
                        + ",".join(f"{f:.0f}" for f in freqs) + "\n")

    def write_proc(self, ts, pid, name, cpu_pct, rss_mb, io_r, io_w, threads):
        self.proc.write(
            f"{ts:.3f},{pid},{name},{cpu_pct:.1f},"
            f"{rss_mb:.0f},{io_r:.2f},{io_w:.2f},{threads}\n")

# ═══════════════════════════════════════════════════════════════════════
#  collect — main sampling loop
# ═══════════════════════════════════════════════════════════════════════

def collect(writers, stats):
    """Run the collection loop. Returns (elapsed, sample_count, irq_sample_count)."""

    prev_cpu, prev_ctxt = parse_cpu_stat()
    prev_disk = parse_diskstats()
    prev_irq = parse_interrupts()
    prev_vmstat = parse_vmstat()
    prev_time = time.monotonic()

    tracked_pids = {}
    prev_proc = {}
    last_proc_scan = 0
    PROC_SCAN_INTERVAL = 5.0

    start = time.time()
    sample_count = 0
    irq_sample_count = 0
    last_slow = time.monotonic()

    try:
        while time.time() - start < DURATION:
            time.sleep(INTERVAL)
            now_mono = time.monotonic()
            ts = time.time()
            dt = now_mono - prev_time
            if dt < 0.05:
                continue

            # CPU (every sample)
            try:
                curr_cpu, curr_ctxt = parse_cpu_stat()
                cd = cpu_delta(prev_cpu, curr_cpu)
                for cpu_id, v in cd.items():
                    writers.write_cpu(ts, cpu_id, v)
                    stats.cpu[cpu_id]["user"].append(v["user"])
                    stats.cpu[cpu_id]["sys"].append(v["sys"])
                    stats.cpu[cpu_id]["iowait"].append(v["iowait"])
                    stats.cpu[cpu_id]["irq"].append(v["irq"] + v["softirq"])
                    stats.cpu[cpu_id]["guest"].append(v["guest"])
                    stats.cpu[cpu_id]["idle"].append(v["idle"])
                prev_cpu = curr_cpu
            except Exception as e:
                print(f"  [ERROR] CPU sample failed: {e}", flush=True)

            # Disk IO (every sample)
            try:
                curr_disk = parse_diskstats()
                dd = diskstat_delta(prev_disk, curr_disk, dt)
                for dev, v in dd.items():
                    writers.write_io(ts, dev, v)
                    stats.io[dev]["rMB_per_s"].append(v["rMB_per_s"])
                    stats.io[dev]["wMB_per_s"].append(v["wMB_per_s"])
                    stats.io[dev]["r_lat"].append(v["avg_r_lat_ms"])
                    stats.io[dev]["w_lat"].append(v["avg_w_lat_ms"])
                    stats.io[dev]["util"].append(v["io_util_pct"])
                prev_disk = curr_disk
            except Exception as e:
                print(f"  [ERROR] IO sample failed: {e}", flush=True)

            # Memory (every sample)
            try:
                mi = parse_meminfo()
                total = mi["MemTotal"] / 1024
                free = mi["MemFree"] / 1024
                avail = mi["MemAvailable"] / 1024
                buffers = mi["Buffers"] / 1024
                cached = mi["Cached"] / 1024
                used = total - free - buffers - cached
                swap_total = mi["SwapTotal"] / 1024
                swap_free = mi["SwapFree"] / 1024
                swap_used = swap_total - swap_free
                dirty = mi.get("Dirty", 0) / 1024
                writeback = mi.get("Writeback", 0) / 1024
                writers.write_mem(ts, total, used, free, avail, buffers, cached,
                                  swap_used, swap_free, dirty, writeback)
                stats.mem["used"].append(used)
                stats.mem["avail"].append(avail)
                stats.mem["swap_used"].append(swap_used)
                stats.mem["dirty"].append(dirty)
                stats.mem["writeback"].append(writeback)
            except Exception as e:
                print(f"  [ERROR] Memory sample failed: {e}", flush=True)

            # ── 1-second probes ──
            slow_dt = now_mono - last_slow
            if slow_dt >= 1.0:

                # Interrupts
                try:
                    curr_irq = parse_interrupts()
                    irq_d = interrupt_delta(prev_irq, curr_irq)
                    for irq, iv in irq_d.items():
                        rate = [x / slow_dt for x in iv["delta"]]
                        writers.write_irq(ts, irq, iv["desc"], rate)
                        for i in range(NUM_CPUS):
                            stats.irq_totals[irq][i] += iv["delta"][i]
                        stats.irq_descs[irq] = iv["desc"]
                    prev_irq = curr_irq
                    irq_sample_count += 1
                except Exception as e:
                    print(f"  [ERROR] IRQ sample failed: {e}", flush=True)

                # VRAM / GPU
                try:
                    vram = get_vram()
                    if vram:
                        writers.write_vram(ts, vram)
                        parts = [p.strip() for p in vram.split(",")]
                        stats.gpu["used"].append(float(parts[0]))
                        stats.gpu["util"].append(float(parts[4]))
                        if len(parts) >= 9:
                            stats.gpu["clock"].append(float(parts[6]))
                            stats.gpu["power"].append(float(parts[8]))
                except Exception as e:
                    print(f"  [ERROR] VRAM sample failed: {e}", flush=True)

                # vmstat
                try:
                    curr_vmstat = parse_vmstat()
                    v_ctxt = (curr_ctxt - prev_ctxt) / slow_dt
                    v_pgfault = (curr_vmstat.get("pgfault", 0)
                                 - prev_vmstat.get("pgfault", 0)) / slow_dt
                    v_pgmajfault = (curr_vmstat.get("pgmajfault", 0)
                                    - prev_vmstat.get("pgmajfault", 0)) / slow_dt
                    v_pgscan_k = (sum_prefix(curr_vmstat, "pgscan_kswapd")
                                  - sum_prefix(prev_vmstat, "pgscan_kswapd")
                                  ) / slow_dt
                    v_pgscan_d = (sum_prefix(curr_vmstat, "pgscan_direct")
                                  - sum_prefix(prev_vmstat, "pgscan_direct")
                                  ) / slow_dt
                    v_pgsteal_k = (sum_prefix(curr_vmstat, "pgsteal_kswapd")
                                   - sum_prefix(prev_vmstat, "pgsteal_kswapd")
                                   ) / slow_dt
                    v_pgsteal_d = (sum_prefix(curr_vmstat, "pgsteal_direct")
                                   - sum_prefix(prev_vmstat, "pgsteal_direct")
                                   ) / slow_dt
                    v_alloc = (sum_prefix(curr_vmstat, "allocstall")
                               - sum_prefix(prev_vmstat, "allocstall")) / slow_dt
                    v_compact = (curr_vmstat.get("compact_stall", 0)
                                 - prev_vmstat.get("compact_stall", 0)) / slow_dt
                    tlb_curr = (curr_vmstat.get("nr_tlb_remote_flush", 0)
                                + curr_vmstat.get("nr_tlb_remote_flush_received", 0))
                    tlb_prev = (prev_vmstat.get("nr_tlb_remote_flush", 0)
                                + prev_vmstat.get("nr_tlb_remote_flush_received", 0))
                    v_tlb = (tlb_curr - tlb_prev) / slow_dt
                    nr_dirty = curr_vmstat.get("nr_dirty", 0)
                    nr_wb = curr_vmstat.get("nr_writeback", 0)
                    v_pswpin = (curr_vmstat.get("pswpin", 0)
                                - prev_vmstat.get("pswpin", 0)) / slow_dt
                    v_pswpout = (curr_vmstat.get("pswpout", 0)
                                 - prev_vmstat.get("pswpout", 0)) / slow_dt
                    v_wset_anon = (curr_vmstat.get("workingset_refault_anon", 0)
                                   - prev_vmstat.get("workingset_refault_anon", 0)
                                   ) / slow_dt
                    v_wset_file = (curr_vmstat.get("workingset_refault_file", 0)
                                   - prev_vmstat.get("workingset_refault_file", 0)
                                   ) / slow_dt
                    v_thp_fallback = (curr_vmstat.get("thp_fault_fallback", 0)
                                      - prev_vmstat.get("thp_fault_fallback", 0)
                                      ) / slow_dt

                    writers.write_vmstat(ts, v_ctxt, v_pgfault, v_pgmajfault,
                                         v_pgscan_k, v_pgscan_d,
                                         v_pgsteal_k, v_pgsteal_d,
                                         v_alloc, v_compact, v_tlb,
                                         nr_dirty, nr_wb,
                                         v_pswpin, v_pswpout,
                                         v_wset_anon, v_wset_file,
                                         v_thp_fallback)

                    stats.vm["ctxt"].append(v_ctxt)
                    stats.vm["pgfault"].append(v_pgfault)
                    stats.vm["pgmajfault"].append(v_pgmajfault)
                    stats.vm["pgscan_kswapd"].append(v_pgscan_k)
                    stats.vm["pgscan_direct"].append(v_pgscan_d)
                    stats.vm["pgsteal_kswapd"].append(v_pgsteal_k)
                    stats.vm["pgsteal_direct"].append(v_pgsteal_d)
                    stats.vm["allocstall"].append(v_alloc)
                    stats.vm["compact_stall"].append(v_compact)
                    stats.vm["tlb"].append(v_tlb)
                    stats.vm["pswpin"].append(v_pswpin)
                    stats.vm["pswpout"].append(v_pswpout)
                    stats.vm["wset_refault_anon"].append(v_wset_anon)
                    stats.vm["wset_refault_file"].append(v_wset_file)
                    stats.vm["thp_fault_fallback"].append(v_thp_fallback)

                    prev_vmstat = curr_vmstat
                    prev_ctxt = curr_ctxt
                except Exception as e:
                    print(f"  [ERROR] vmstat sample failed: {e}", flush=True)

                # PSI
                try:
                    psi = parse_psi()
                    writers.write_psi(ts, psi)
                    stats.psi["cpu_some10"].append(psi["cpu"]["some_avg10"])
                    stats.psi["mem_some10"].append(psi["memory"]["some_avg10"])
                    stats.psi["mem_full10"].append(psi["memory"]["full_avg10"])
                    stats.psi["io_some10"].append(psi["io"]["some_avg10"])
                    stats.psi["io_full10"].append(psi["io"]["full_avg10"])
                except Exception as e:
                    print(f"  [ERROR] PSI sample failed: {e}", flush=True)

                # CPU frequency
                try:
                    freqs = parse_cpu_freq()
                    writers.write_freq(ts, freqs)
                    for i, fq in enumerate(freqs):
                        if fq > 0:
                            stats.freq[i].append(fq)
                except Exception as e:
                    print(f"  [ERROR] CPU freq sample failed: {e}", flush=True)

                # Per-process tracking
                try:
                    if now_mono - last_proc_scan >= PROC_SCAN_INTERVAL:
                        tracked_pids = find_tracked_procs(PROC_PATTERNS)
                        last_proc_scan = now_mono

                    for pid, name in list(tracked_pids.items()):
                        pstats = read_proc_stats(pid)
                        if pstats is None:
                            tracked_pids.pop(pid, None)
                            prev_proc.pop(pid, None)
                            continue

                        rss_mb = pstats["rss_kb"] / 1024

                        if pid in prev_proc:
                            pp = prev_proc[pid]
                            d_u = pstats["utime"] - pp["utime"]
                            d_s = pstats["stime"] - pp["stime"]
                            cpu_pct = (d_u + d_s) / CLK_TCK / slow_dt * 100
                            d_rb = pstats["read_bytes"] - pp["read_bytes"]
                            d_wb = pstats["write_bytes"] - pp["write_bytes"]
                            io_r = d_rb / 1024 / 1024 / slow_dt
                            io_w = d_wb / 1024 / 1024 / slow_dt

                            writers.write_proc(ts, pid, name, cpu_pct, rss_mb,
                                               io_r, io_w, pstats["num_threads"])

                            stats.proc[name]["cpu"].append(cpu_pct)
                            stats.proc[name]["rss"].append(rss_mb)
                            stats.proc[name]["io_r"].append(io_r)
                            stats.proc[name]["io_w"].append(io_w)
                            stats.proc[name]["threads"].append(pstats["num_threads"])

                        prev_proc[pid] = pstats
                except Exception as e:
                    print(f"  [ERROR] Process tracking failed: {e}", flush=True)

                last_slow = now_mono

            prev_time = now_mono
            sample_count += 1

            if sample_count % 300 == 0:
                elapsed = time.time() - start
                print(f"  {elapsed:.0f}s / {DURATION}s  "
                      f"({sample_count} samples)", flush=True)

    except KeyboardInterrupt:
        print("\nInterrupted.")

    elapsed = time.time() - start
    return start, elapsed, sample_count, irq_sample_count

# ═══════════════════════════════════════════════════════════════════════
#  print_summary — terminal report
# ═══════════════════════════════════════════════════════════════════════

def print_summary(stats, elapsed, sample_count, irq_sample_count):
    W = 110

    print(f"\n{'=' * W}")
    print(f"SYSTEM MONITORING SUMMARY  "
          f"({elapsed:.0f}s, {sample_count} samples)")
    print(f"{'=' * W}")

    _print_cpu_table(stats, W)
    _print_cpu_freq(stats, W)
    _print_procs(stats, W)
    _print_irqs(stats, W)
    _print_memory(stats, W)
    _print_vmstat(stats, W)
    _print_psi(stats, W)
    _print_gpu(stats, W)
    _print_io(stats, W)

    print(f"\n{'=' * W}")
    print(f"CSV files in: {OUTDIR}/")
    print(f"  cpu.csv  mem.csv  io.csv  vram.csv  irq.csv")
    print(f"  proc.csv  vmstat.csv  psi.csv  freq.csv  xplane_events.csv")
    print(f"{'=' * W}")


def _print_cpu_table(stats, W):
    print(f"\n{'─' * W}")
    print("PER-CPU USAGE (avg / max / p95)")
    print(f"{'─' * W}")
    print(f"{'CPU':>5} {'User%':>18} {'Sys%':>18} {'IOWait%':>18} "
          f"{'IRQ+SI%':>18} {'Guest%':>18} {'Idle%':>18}")
    print(f"{'':>5} {'avg/ max/ p95':>18} {'avg/ max/ p95':>18} "
          f"{'avg/ max/ p95':>18} {'avg/ max/ p95':>18} "
          f"{'avg/ max/ p95':>18} {'avg/ max/ p95':>18}")

    for cpu_id in sorted(k for k in stats.cpu if k != "all"):
        c = stats.cpu[cpu_id]
        print(f"{cpu_id:>5} "
              f"{fmt3(c['user']):>18} "
              f"{fmt3(c['sys']):>18} "
              f"{fmt3(c['iowait']):>18} "
              f"{fmt3(c['irq']):>18} "
              f"{fmt3(c['guest']):>18} "
              f"{fmt3(c['idle']):>18}")

    if "all" in stats.cpu:
        c = stats.cpu["all"]
        print(f"{'ALL':>5} "
              f"{fmt3(c['user']):>18} "
              f"{fmt3(c['sys']):>18} "
              f"{fmt3(c['iowait']):>18} "
              f"{fmt3(c['irq']):>18} "
              f"{fmt3(c['guest']):>18} "
              f"{fmt3(c['idle']):>18}")


def _print_cpu_freq(stats, W):
    if not any(stats.freq.values()):
        return
    print(f"\n{'─' * W}")
    print("CPU FREQUENCY MHz (avg / min / max)")
    print(f"{'─' * W}")
    for cpu_id in sorted(stats.freq.keys()):
        fl = stats.freq[cpu_id]
        if fl:
            print(f"  CPU {cpu_id:>2}:  avg={statistics.mean(fl):7.0f}   "
                  f"min={min(fl):7.0f}   max={max(fl):7.0f}")


def _print_procs(stats, W):
    if not stats.proc:
        return
    print(f"\n{'─' * W}")
    print("PER-PROCESS (avg / max / p95)")
    print(f"{'─' * W}")
    print(f"  {'Process':<30} {'CPU%':>18} {'RSS MB':>18} "
          f"{'IO Read MB/s':>18} {'IO Write MB/s':>18} {'Thr':>5}")
    for name in sorted(stats.proc.keys()):
        p = stats.proc[name]
        rss_s = fmt3(p["rss"]) if p["rss"] else "—"
        thr_s = f"{max(p['threads'])}" if p["threads"] else "—"
        print(f"  {name:<30} "
              f"{fmt3(p['cpu']):>18} "
              f"{rss_s:>18} "
              f"{fmt3(p['io_r']):>18} "
              f"{fmt3(p['io_w']):>18} "
              f"{thr_s:>5}")


def _print_irqs(stats, W):
    print(f"\n{'─' * W}")
    print("TOP INTERRUPT SOURCES (total count, per-CPU distribution)")
    print(f"{'─' * W}")
    sorted_irqs = sorted(stats.irq_totals.items(),
                         key=lambda x: sum(x[1]), reverse=True)[:30]
    print(f"{'IRQ':>8} {'Total':>12} {'Description':40} "
          f"| Per-CPU (top)")
    for irq, counts in sorted_irqs:
        total = sum(counts)
        if total < 100:
            continue
        desc = stats.irq_descs.get(irq, "")[:40]
        indexed = sorted(((i, c) for i, c in enumerate(counts) if c > 0),
                         key=lambda x: x[1], reverse=True)
        top_cpus = ", ".join(f"cpu{i}:{c}" for i, c in indexed[:6])
        print(f"{irq:>8} {total:>12,} {desc:40} | {top_cpus}")


def _print_memory(stats, W):
    print(f"\n{'─' * W}")
    print("MEMORY")
    print(f"{'─' * W}")
    m = stats.mem
    if m["used"]:
        print(f"  Used:      avg={statistics.mean(m['used']):.0f} MB  "
              f"min={min(m['used']):.0f}  max={max(m['used']):.0f}")
        print(f"  Available: avg={statistics.mean(m['avail']):.0f} MB  "
              f"min={min(m['avail']):.0f}  max={max(m['avail']):.0f}")
        print(f"  Swap Used: avg={statistics.mean(m['swap_used']):.0f} MB  "
              f"min={min(m['swap_used']):.0f}  max={max(m['swap_used']):.0f}")
        print(f"  Swap Delta: "
              f"{max(m['swap_used']) - min(m['swap_used']):.0f} MB swing")
        print(f"  Dirty:     avg={statistics.mean(m['dirty']):.1f} MB  "
              f"max={max(m['dirty']):.1f}")
        print(f"  Writeback: avg={statistics.mean(m['writeback']):.1f} MB  "
              f"max={max(m['writeback']):.1f}")


def _print_vmstat(stats, W):
    def fmt_vm(lst):
        if not lst:
            return "—"
        return (f"avg={statistics.mean(lst):8.0f}  "
                f"max={max(lst):8.0f}/s")

    print(f"\n{'─' * W}")
    print("VM PRESSURE / RECLAIM (rates per second: avg / max)")
    print(f"{'─' * W}")
    vm = stats.vm
    if vm["ctxt"]:
        print(f"  Context switches:  {fmt_vm(vm['ctxt'])}")
        print(f"  Page faults:       {fmt_vm(vm['pgfault'])}")
        print(f"  Major faults:      {fmt_vm(vm['pgmajfault'])}")
        print(f"  TLB shootdowns:    {fmt_vm(vm['tlb'])}")
        print(f"  kswapd scan:       {fmt_vm(vm['pgscan_kswapd'])}")
        print(f"  kswapd steal:      {fmt_vm(vm['pgsteal_kswapd'])}")
        print(f"  Direct scan:       {fmt_vm(vm['pgscan_direct'])}")
        print(f"  Direct steal:      {fmt_vm(vm['pgsteal_direct'])}")
        print(f"  Alloc stalls:      {fmt_vm(vm['allocstall'])}")
        print(f"  Compact stalls:    {fmt_vm(vm['compact_stall'])}")
        print(f"  Swap in:           {fmt_vm(vm['pswpin'])}")
        print(f"  Swap out:          {fmt_vm(vm['pswpout'])}")
        print(f"  WSet refault anon: {fmt_vm(vm['wset_refault_anon'])}")
        print(f"  WSet refault file: {fmt_vm(vm['wset_refault_file'])}")
        print(f"  THP fallback:      {fmt_vm(vm['thp_fault_fallback'])}")


def _print_psi(stats, W):
    def fmt_psi(lst):
        if not lst:
            return "—"
        return (f"avg={statistics.mean(lst):5.2f}%  "
                f"max={max(lst):5.2f}%")

    print(f"\n{'─' * W}")
    print("PRESSURE STALL INFO (% of time stalled, from 10s window)")
    print(f"{'─' * W}")
    p = stats.psi
    if p["cpu_some10"]:
        print(f"  CPU  some: {fmt_psi(p['cpu_some10'])}")
        print(f"  MEM  some: {fmt_psi(p['mem_some10'])}")
        print(f"  MEM  full: {fmt_psi(p['mem_full10'])}")
        print(f"  IO   some: {fmt_psi(p['io_some10'])}")
        print(f"  IO   full: {fmt_psi(p['io_full10'])}")


def _print_gpu(stats, W):
    print(f"\n{'─' * W}")
    print("GPU / VRAM")
    print(f"{'─' * W}")
    g = stats.gpu
    if g["used"]:
        print(f"  VRAM Used: avg={statistics.mean(g['used']):.0f} MiB  "
              f"min={min(g['used']):.0f}  max={max(g['used']):.0f}")
        print(f"  GPU Util:  avg={statistics.mean(g['util']):.0f}%  "
              f"min={min(g['util']):.0f}%  max={max(g['util']):.0f}%")
        if g["clock"]:
            print(f"  GPU Clock: "
                  f"avg={statistics.mean(g['clock']):.0f} MHz  "
                  f"min={min(g['clock']):.0f}  "
                  f"max={max(g['clock']):.0f}")
        if g["power"]:
            print(f"  Power:     "
                  f"avg={statistics.mean(g['power']):.0f} W  "
                  f"min={min(g['power']):.0f}  "
                  f"max={max(g['power']):.0f}")
    else:
        print("  No VRAM data collected")


def _print_io(stats, W):
    print(f"\n{'─' * W}")
    print("DISK IO")
    print(f"{'─' * W}")
    for dev in sorted(stats.io.keys()):
        d = stats.io[dev]
        r, w = d["rMB_per_s"], d["wMB_per_s"]
        rl = [x for x in d["r_lat"] if x > 0]
        wl = [x for x in d["w_lat"] if x > 0]
        ut = d["util"]
        p95 = lambda lst: sorted(lst)[int(len(lst) * 0.95)] if len(lst) > 20 else max(lst)
        print(f"  {dev}:")
        print(f"    Read:     avg={statistics.mean(r):.2f} MB/s  "
              f"max={max(r):.1f}  p95={p95(r):.1f} MB/s")
        print(f"    Write:    avg={statistics.mean(w):.2f} MB/s  "
              f"max={max(w):.1f}  p95={p95(w):.1f} MB/s")
        if rl:
            print(f"    Read lat: avg={statistics.mean(rl):.2f} ms  "
                  f"max={max(rl):.1f}  p95={p95(rl):.1f} ms")
        if wl:
            print(f"    Wrt lat:  avg={statistics.mean(wl):.2f} ms  "
                  f"max={max(wl):.1f}  p95={p95(wl):.1f} ms")
        print(f"    IO Util:  avg={statistics.mean(ut):.1f}%  "
              f"max={max(ut):.1f}%")

    print(f"\n  IO Spikes (>100 MB/s read):")
    any_spike = False
    for dev in sorted(stats.io.keys()):
        r = stats.io[dev]["rMB_per_s"]
        spikes = sum(1 for x in r if x > 100)
        if spikes > 0:
            pct = spikes / len(r) * 100
            print(f"    {dev}: {spikes} samples >100 MB/s ({pct:.1f}%)")
            any_spike = True
    if not any_spike:
        print(f"    (none)")

# ═══════════════════════════════════════════════════════════════════════
#  correlate_xplane — X-Plane log correlation & xplane_events.csv
# ═══════════════════════════════════════════════════════════════════════

def correlate_xplane(outdir, stats, mon_start, elapsed):
    W = 110
    xp_log = find_xplane_log()
    if not xp_log:
        print(f"\n  (X-Plane Log.txt not found — set SYSMON_XPLANE_LOG "
              f"to enable correlation)")
        return

    print(f"\n{'─' * W}")
    print(f"X-PLANE LOG CORRELATION ({xp_log})")
    print(f"{'─' * W}")

    xp_start, xp_events = parse_xplane_log(xp_log)
    if not xp_start:
        print("  Could not parse X-Plane start time from log.")
        return

    mon_end = mon_start + elapsed
    xp_in_window = [(ts, cat, msg) for ts, cat, msg in xp_events
                    if mon_start - 5 <= ts <= mon_end + 5]

    dsf_in_window = [(ts, cat, msg) for ts, cat, msg in xp_in_window
                     if cat == "DSF"]
    airport_in_window = [(ts, cat, msg) for ts, cat, msg
                         in xp_in_window if cat == "AIRPORT"]
    weather_in_window = [(ts, cat, msg) for ts, cat, msg
                         in xp_in_window if cat == "WEATHER"]
    errors_in_window = [(ts, cat, msg) for ts, cat, msg
                        in xp_in_window if cat == "ERROR"]

    def ts_to_rel(ts):
        d = ts - mon_start
        return f"+{d:.1f}s"

    print(f"\n  X-Plane started: "
          f"{datetime.fromtimestamp(xp_start):%Y-%m-%d %H:%M:%S}")
    print(f"  Monitoring window: "
          f"{datetime.fromtimestamp(mon_start):%H:%M:%S} – "
          f"{datetime.fromtimestamp(mon_end):%H:%M:%S}")
    print(f"  Events in window: {len(xp_in_window)} total "
          f"({len(dsf_in_window)} DSF, {len(airport_in_window)} airport, "
          f"{len(weather_in_window)} weather, {len(errors_in_window)} errors)")

    if dsf_in_window:
        print(f"\n  DSF loads during monitoring:")
        for ts, _, msg in sorted(dsf_in_window, key=lambda x: x[0]):
            print(f"    {ts_to_rel(ts):>10}  {msg}")

    if airport_in_window:
        print(f"\n  Airport loading during monitoring:")
        for ts, _, msg in sorted(airport_in_window, key=lambda x: x[0]):
            print(f"    {ts_to_rel(ts):>10}  {msg}")

    # Correlate: IO spikes vs X-Plane events
    print(f"\n  IO Spike Correlation (>50 MB/s read):")
    io_hits = correlate_events(
        outdir / "io.csv", xp_in_window,
        ts_col=0, val_col=4, threshold=50.0, window=3.0)
    _print_correlation_hits(io_hits, ts_to_rel, val_fmt="{:6.1f} MB/s",
                            max_hits=20)

    # Correlate: Major faults vs X-Plane events
    print(f"\n  Major Fault Spike Correlation (>100/s):")
    vm_hits = correlate_events(
        outdir / "vmstat.csv", xp_in_window,
        ts_col=0, val_col=3, threshold=100.0, window=3.0)
    _print_correlation_hits(vm_hits, ts_to_rel, val_fmt="{:6.0f}/s",
                            max_hits=10)

    # Correlate: Alloc stalls vs X-Plane events
    print(f"\n  Alloc Stall Correlation (>0):")
    alloc_hits = correlate_events(
        outdir / "vmstat.csv", xp_in_window,
        ts_col=0, val_col=8, threshold=0.5, window=3.0)
    _print_correlation_hits(alloc_hits, ts_to_rel, val_fmt="{:6.1f}/s",
                            max_hits=10)

    # Save events CSV
    with open(outdir / "xplane_events.csv", "w") as f:
        f.write("timestamp,rel_offset_s,category,message\n")
        for ts, cat, msg in sorted(xp_in_window, key=lambda x: x[0]):
            msg_clean = msg.replace('"', "'")
            f.write(f'{ts:.3f},{ts - mon_start:.1f},{cat},"{msg_clean}"\n')
    print(f"\n  Saved: {outdir}/xplane_events.csv")


def _print_correlation_hits(hits, ts_to_rel, val_fmt, max_hits):
    if not hits:
        label = val_fmt.split("}")[0].split("{")[0].strip()
        print(f"    (no spikes coincided with X-Plane events)")
        return
    seen = set()
    for csv_ts, val, nearby in sorted(hits, key=lambda x: -x[1])[:max_hits]:
        key = round(csv_ts)
        if key in seen:
            continue
        seen.add(key)
        val_str = val_fmt.format(val)
        evt_str = "; ".join(f"[{cat}] {msg[:60]}"
                            for _, cat, msg in nearby[:3])
        print(f"    {ts_to_rel(csv_ts):>10}  {val_str}  ← {evt_str}")

# ═══════════════════════════════════════════════════════════════════════
#  main — orchestration only
# ═══════════════════════════════════════════════════════════════════════

def _capture_dmesg(outdir, suffix):
    """Capture dmesg -T to a log file."""
    path = outdir / f"dmesg_{suffix}.log"
    try:
        result = subprocess.run(["dmesg", "-T"], capture_output=True,
                                timeout=5)
        path.write_bytes(result.stdout)
        print(f"  dmesg_{suffix}.log: {len(result.stdout)} bytes")
    except Exception as e:
        print(f"  [WARN] dmesg capture failed: {e}")


def _start_gpu_event_monitor(outdir):
    """Start background journalctl watching for NVRM/Xid events.
    Returns (thread, process) — process is killed on stop."""
    log_path = outdir / "gpu_events.log"
    proc = None

    def _monitor():
        nonlocal proc
        try:
            with open(log_path, "w") as f:
                proc = subprocess.Popen(
                    ["journalctl", "-k", "--grep=NVRM|Xid", "-f",
                     "--no-pager", "-o", "short-precise"],
                    stdout=f, stderr=subprocess.DEVNULL)
                proc.wait()
        except Exception as e:
            print(f"  [ERROR] GPU event monitor failed: {e}", flush=True)

    t = threading.Thread(target=_monitor, daemon=True)
    t.start()
    # Give journalctl a moment to start
    time.sleep(0.3)
    return t, proc


def parse_args():
    """Parse CLI arguments.  Env vars serve as fallback defaults."""
    p = argparse.ArgumentParser(
        prog="sysmon",
        description=(
            "Flight session monitor for X-Plane on Linux.  "
            "Collects CPU, memory, disk IO, GPU, interrupts, vmstat, "
            "and PSI data at high resolution, then correlates system "
            "events with X-Plane Log.txt entries."
        ),
        epilog=(
            "Environment variables (override defaults, overridden by CLI):\n"
            "  SYSMON_DURATION, SYSMON_INTERVAL, SYSMON_OUTDIR,\n"
            "  SYSMON_PROCS, SYSMON_XPLANE_LOG\n"
            "\n"
            "Companion scripts:\n"
            "  sysmon_trace.sh   bpftrace sidecar (sudo required)\n"
            "  post_crash.sh     post-crash GPU diagnostics\n"
            "  cgwatcher.py      dynamic CPU priority management"
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    p.add_argument("-V", "--version", action="version",
                   version=f"sysmon {__version__}")
    p.add_argument("-d", "--duration", type=int,
                   default=int(os.environ.get("SYSMON_DURATION", 1200)),
                   metavar="SEC",
                   help="recording duration in seconds (default: 1200 = 20 min)")
    p.add_argument("-i", "--interval", type=float,
                   default=float(os.environ.get("SYSMON_INTERVAL", 0.2)),
                   metavar="SEC",
                   help="base sampling interval in seconds (default: 0.2)")
    p.add_argument("-o", "--outdir", type=Path,
                   default=Path(os.environ.get("SYSMON_OUTDIR", "/tmp/sysmon_out")),
                   metavar="DIR",
                   help="output directory for CSV files (default: /tmp/sysmon_out)")
    p.add_argument("-p", "--procs", type=str,
                   default=os.environ.get("SYSMON_PROCS",
                                          "X-Plane,autoortho,qemu-system,xearthlayer"),
                   metavar="PAT",
                   help="comma-separated process name patterns to track")
    p.add_argument("-l", "--xplane-log", type=Path, default=None,
                   metavar="PATH",
                   help="path to X-Plane Log.txt (auto-detected if omitted)")
    p.add_argument("--no-gpu", action="store_true",
                   help="disable GPU monitoring (useful on headless or AMD systems)")
    p.add_argument("--no-dmesg", action="store_true",
                   help="skip dmesg capture (useful without dmesg permissions)")
    p.add_argument("--xplane", action="store_true",
                   help="start X-Plane telemetry recorder (FPS, CPU/GPU time via UDP)")
    p.add_argument("--xplane-rate", type=int, default=5, metavar="HZ",
                   help="X-Plane telemetry poll rate in Hz (default: 5)")
    return p.parse_args()


def main():
    global DURATION, INTERVAL, OUTDIR, PROC_PATTERNS, XPLANE_LOG

    args = parse_args()

    # Apply CLI args to module-level config (collect loop reads these)
    DURATION = args.duration
    INTERVAL = args.interval
    OUTDIR = args.outdir
    OUTDIR.mkdir(parents=True, exist_ok=True)
    PROC_PATTERNS = [p.strip() for p in args.procs.split(",")]
    if args.xplane_log:
        XPLANE_LOG = str(args.xplane_log)

    # Initialize GPU backend
    gpu_backend = init_gpu(disable=args.no_gpu)

    # Install signal handler for clean shutdown
    def _shutdown(signum, frame):
        print(f"\nReceived signal {signal.Signals(signum).name}, finishing...")
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, _shutdown)

    # ── Startup banner ──
    print(f"sysmon {__version__} — {DURATION}s at {INTERVAL}s base interval, "
          f"{NUM_CPUS} CPUs")
    print(f"Output:   {OUTDIR}")
    print(f"GPU:      {gpu_backend}")
    print(f"Tracking: {', '.join(PROC_PATTERNS)}")

    xp_log = find_xplane_log()
    if xp_log:
        print(f"X-Plane:  {xp_log}")
    else:
        print(f"X-Plane:  Log.txt not found (use -l to set path)")

    psi_avail = Path("/proc/pressure/cpu").exists()
    print(f"PSI:      {'available' if psi_avail else 'not available (kernel too old?)'}")

    # X-Plane telemetry subprocess (--xplane flag)
    xplane_telem_proc = None
    if args.xplane:
        telem_script = Path(__file__).parent / "xplane_telemetry.py"
        if telem_script.exists():
            telem_cmd = [
                sys.executable, str(telem_script),
                "-r", str(args.xplane_rate),
                "-d", str(DURATION),
                "-o", str(OUTDIR),
            ]
            telem_log = OUTDIR / "xplane_telemetry.log"
            with open(telem_log, "w") as tlog:
                xplane_telem_proc = subprocess.Popen(
                    telem_cmd, stdout=tlog, stderr=subprocess.STDOUT
                )
            print(f"Telemetry: xplane_telemetry.py (PID {xplane_telem_proc.pid}, "
                  f"{args.xplane_rate} Hz)")
        else:
            print(f"Telemetry: xplane_telemetry.py not found at {telem_script}",
                  file=sys.stderr)
    print()

    # Crash diagnostics: dmesg snapshot + GPU event monitor
    gpu_mon_proc = None
    if not args.no_dmesg:
        print("Crash diagnostics:")
        _capture_dmesg(OUTDIR, "pre")
        gpu_mon_thread, gpu_mon_proc = _start_gpu_event_monitor(OUTDIR)
        print(f"  gpu_events.log: journalctl monitor started")
        print()

    stats = Stats()

    with CSVWriters(OUTDIR) as writers:
        mon_start, elapsed, sample_count, irq_sample_count = \
            collect(writers, stats)

    # Stop X-Plane telemetry subprocess
    if xplane_telem_proc and xplane_telem_proc.poll() is None:
        print("Stopping X-Plane telemetry...")
        xplane_telem_proc.terminate()
        try:
            xplane_telem_proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            xplane_telem_proc.kill()
        telem_csv = OUTDIR / "xplane_telemetry.csv"
        if telem_csv.exists():
            lines = sum(1 for _ in open(telem_csv)) - 1
            print(f"  {lines} telemetry samples recorded")

    # Stop GPU event monitor and capture post-run dmesg
    if gpu_mon_proc and gpu_mon_proc.poll() is None:
        gpu_mon_proc.terminate()
        try:
            gpu_mon_proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            gpu_mon_proc.kill()
    if not args.no_dmesg:
        _capture_dmesg(OUTDIR, "post")

    print(f"\nCollected {sample_count} samples in {elapsed:.1f}s")
    print(f"1s probes: {irq_sample_count}")

    print_summary(stats, elapsed, sample_count, irq_sample_count)
    correlate_xplane(OUTDIR, stats, mon_start, elapsed)

    if _NVML_LIB:
        _NVML_LIB.nvmlShutdown()

if __name__ == "__main__":
    main()
