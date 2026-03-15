#!/bin/bash
# sysmon_trace.sh — bpftrace sidecar for kernel-level diagnostics.
#
# Runs three parallel bpftrace probes that capture events invisible to
# polling-based monitors like sysmon.py:
#
#   1) Direct Reclaim   per-process reclaim latency + kswapd activity
#   2) Slow IO          block requests completing in >5 ms (NVMe outliers)
#   3) DMA Fence Waits  CPU stalling on GPU completion (>5 ms)
#
# Run alongside sysmon.py during a flight session.  Requires root
# (bpftrace attaches to kernel tracepoints).
#
# Usage:
#   sudo bash sysmon_trace.sh [options]
#
# Options:
#   -o, --outdir DIR   output directory (default: $SYSMON_OUTDIR or /tmp/sysmon_out)
#   -h, --help         show this help
#
# Stop:  Ctrl+C  (all three tracers are cleaned up automatically)
#
# Output files:
#   trace_reclaim.log  Direct Reclaim events with per-process attribution
#   trace_io_slow.log  Block IO requests exceeding 5 ms latency
#   trace_fence.log    DMA fence waits where CPU blocked on GPU >5 ms
#
# Note: Uses BPFTRACE_MAP_KEYS_MAX=65536 to avoid map overflow on long
# sessions.  The default (4096) caused 3M overflow warnings and a 697 MB
# log file in an 81-minute test run.

set -euo pipefail

# ─── Argument parsing ────────────────────────────────────────────────

OUTDIR="${SYSMON_OUTDIR:-/tmp/sysmon_out}"

while [[ $# -gt 0 ]]; do
    case "$1" in
        -o|--outdir) OUTDIR="$2"; shift 2 ;;
        -h|--help)
            sed -n '2,/^$/{ s/^# //; s/^#//; p }' "$0"
            exit 0
            ;;
        *) echo "Unknown option: $1 (use --help)"; exit 1 ;;
    esac
done

mkdir -p "$OUTDIR"

PIDS=()

cleanup() {
    echo ""
    echo "[sysmon_trace] Stopping bpftrace processes..."
    for pid in "${PIDS[@]}"; do
        kill "$pid" 2>/dev/null && echo "  Stopped PID $pid" || true
    done
    rm -f "$OUTDIR/trace_pids"
    echo "[sysmon_trace] Done."
}
trap cleanup EXIT SIGTERM SIGINT

echo "=== sysmon_trace.sh — bpftrace sidecar ==="
echo "Output: $OUTDIR"
echo ""

# Verify bpftrace is available
if ! command -v bpftrace &>/dev/null; then
    echo "[ERROR] bpftrace not found.  Install: sudo apt install bpftrace"
    exit 1
fi

# Verify running as root
if [[ $EUID -ne 0 ]]; then
    echo "[ERROR] Must run as root (bpftrace needs kernel tracepoint access)."
    echo "  Usage: sudo bash $0"
    exit 1
fi

# Increase map capacity to avoid overflow warnings on long sessions.
export BPFTRACE_MAP_KEYS_MAX=65536

# ─── Trace 1: Direct Reclaim + kswapd ────────────────────────────────
# Captures per-process reclaim duration (microseconds) and the number of
# pages reclaimed.  kswapd wake/sleep events mark background reclaim phases.

echo "[1/3] Starting direct reclaim tracer..."
bpftrace -e '
tracepoint:vmscan:mm_vmscan_direct_reclaim_begin {
    @start[tid] = nsecs;
}

tracepoint:vmscan:mm_vmscan_direct_reclaim_end {
    $dur = @start[tid];
    if ($dur > 0) {
        $us = (nsecs - $dur) / 1000;
        printf("%s pid=%d comm=%s reclaim_us=%llu nr_reclaimed=%lu\n",
               strftime("%H:%M:%S", nsecs), pid, comm, $us,
               args->nr_reclaimed);
        delete(@start[tid]);
    }
}

tracepoint:vmscan:mm_vmscan_kswapd_wake {
    printf("%s KSWAPD_WAKE nid=%d order=%d\n",
           strftime("%H:%M:%S", nsecs), args->nid, args->order);
}

tracepoint:vmscan:mm_vmscan_kswapd_sleep {
    printf("%s KSWAPD_SLEEP nid=%d\n",
           strftime("%H:%M:%S", nsecs), args->nid);
}
' > "$OUTDIR/trace_reclaim.log" 2>&1 &
PIDS+=($!)
echo "  PID $! -> trace_reclaim.log"

# ─── Trace 2: Slow IO (>5ms block request latency) ───────────────────
# Filters for completed block requests whose total latency exceeds 5 ms.
# NVMe power-state exit typically shows up as 10-11 ms clusters.

echo "[2/3] Starting slow IO tracer..."
bpftrace -e '
tracepoint:block:block_rq_issue {
    @io_start[args->sector, args->dev] = nsecs;
}

tracepoint:block:block_rq_complete {
    $start = @io_start[args->sector, args->dev];
    if ($start > 0) {
        $ms = (nsecs - $start) / 1000000;
        if ($ms > 5) {
            printf("%s dev=%d sector=%llu lat_ms=%llu nr_sector=%u\n",
                   strftime("%H:%M:%S", nsecs), args->dev,
                   (uint64)args->sector, $ms, args->nr_sector);
        }
        delete(@io_start[args->sector, args->dev]);
    }
}
' > "$OUTDIR/trace_io_slow.log" 2>&1 &
PIDS+=($!)
echo "  PID $! -> trace_io_slow.log"

# ─── Trace 3: DMA Fence Waits (CPU waiting on GPU, >5ms) ─────────────
# Fires when a CPU thread blocks waiting for a GPU fence to signal.
# Non-zero events indicate CPU-GPU synchronization bottlenecks.

echo "[3/3] Starting DMA fence tracer..."
bpftrace -e '
tracepoint:dma_fence:dma_fence_wait_start {
    @fence_start[tid] = nsecs;
}

tracepoint:dma_fence:dma_fence_wait_end {
    $start = @fence_start[tid];
    if ($start > 0) {
        $ms = (nsecs - $start) / 1000000;
        if ($ms > 5) {
            printf("%s pid=%d comm=%s fence_wait_ms=%llu\n",
                   strftime("%H:%M:%S", nsecs), pid, comm, $ms);
        }
        delete(@fence_start[tid]);
    }
}
' > "$OUTDIR/trace_fence.log" 2>&1 &
PIDS+=($!)
echo "  PID $! -> trace_fence.log"

# Save PIDs for manual cleanup if needed
printf "%s\n" "${PIDS[@]}" > "$OUTDIR/trace_pids"
echo ""
echo "[sysmon_trace] All 3 tracers running.  PIDs: ${PIDS[*]}"
echo "[sysmon_trace] Press Ctrl+C to stop."
echo ""

# Wait for any child to exit (or Ctrl+C)
wait
