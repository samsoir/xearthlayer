#!/usr/bin/env python3
"""
xplane_telemetry.py — X-Plane 12 telemetry recorder via UDP RREF protocol.

Subscribes to datarefs by name (IDs assigned per session), X-Plane pushes
all values in a single UDP packet at the requested frequency.

Requires: X-Plane 12, Settings > Network > Accept incoming connections.
No plugin needed.

Usage:
    python3 xplane_telemetry.py [-r 5] [-o output_dir] [-d 7200]

    -r, --rate     Poll rate in Hz (default: 5)
    -o, --output   Output directory (default: current dir)
    -d, --duration Max duration in seconds (default: 7200 = 2h)
"""

import argparse
import csv
import os
import signal
import socket
import struct
import sys
import time
from pathlib import Path

# Datarefs to subscribe — name, our column alias, index (assigned by us)
DATAREFS = [
    (0,  "sim/operation/misc/frame_rate_period",         "frame_time_s"),
    (1,  "sim/time/gpu_time_per_frame_sec_approx",       "gpu_time_s"),
    (2,  "sim/time/framerate_period",                    "framerate_period_s"),
    (3,  "sim/flightmodel/position/latitude",            "lat"),
    (4,  "sim/flightmodel/position/longitude",           "lon"),
    (5,  "sim/flightmodel/position/elevation",           "elev_m"),
    (6,  "sim/flightmodel/position/y_agl",               "agl_m"),
    (7,  "sim/flightmodel/position/groundspeed",         "gs_ms"),
    (8,  "sim/flightmodel/position/indicated_airspeed",  "ias_ms"),
    (9,  "sim/flightmodel/position/true_airspeed",       "tas_ms"),
    (10, "sim/time/total_running_time_sec",              "sim_time_s"),
    (11, "sim/time/paused",                              "paused"),
    (12, "sim/time/sim_speed",                           "sim_speed"),
]

CSV_COLUMNS = [
    "timestamp", "rel_s", "fps", "frame_time_ms", "cpu_time_ms", "gpu_time_ms",
    "lat", "lon", "elev_m", "agl_m", "gs_kts", "ias_kts",
    "sim_time_s", "paused",
]

XP_HOST = "127.0.0.1"
XP_PORT = 49000


class XPlaneUDP:
    def __init__(self, host=XP_HOST, port=XP_PORT):
        self.target = (host, port)
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.settimeout(3.0)
        self.values = {}       # index → float value
        self.idx_to_col = {}   # index → column name
        self.connected = False
        self._freq = 5         # stored for re-subscribe

        for idx, name, col in DATAREFS:
            self.idx_to_col[idx] = col

    def _send_subscriptions(self, freq):
        """Send RREF subscribe packets for all datarefs."""
        for idx, name, col in DATAREFS:
            msg = struct.pack("<4sxii400s", b"RREF", freq, idx,
                              name.encode("utf-8"))
            self.sock.sendto(msg, self.target)
            time.sleep(0.02)

    def subscribe(self, freq, wait=True):
        """Subscribe to all datarefs.  Retries until X-Plane responds.

        With wait=True (default), blocks until connected.
        With wait=False, tries once and returns immediately.
        """
        self._freq = freq
        print(f"Subscribing to {len(DATAREFS)} datarefs at {freq} Hz...")
        for idx, name, col in DATAREFS:
            print(f"  [{idx:2d}] {col:25s} = {name}")

        attempt = 0
        while True:
            attempt += 1
            self._send_subscriptions(freq)

            try:
                self._recv_one()
                self.connected = True
                print(f"Connected — receiving data from X-Plane"
                      + (f" (attempt {attempt})" if attempt > 1 else ""))
                return True
            except socket.timeout:
                if not wait:
                    return False
                backoff = min(5, 1 + attempt * 0.5)
                print(f"  No response (attempt {attempt}), "
                      f"retrying in {backoff:.0f}s...",
                      flush=True)
                time.sleep(backoff)

    def resubscribe(self):
        """Re-subscribe after X-Plane restart or prolonged timeout."""
        print("Re-subscribing (X-Plane may have restarted)...", flush=True)
        self.values.clear()
        self.connected = False
        return self.subscribe(self._freq, wait=True)

    def unsubscribe_all(self):
        """Unsubscribe from all datarefs."""
        for idx, name, col in DATAREFS:
            msg = struct.pack("<4sxii400s", b"RREF", 0, idx,
                              name.encode("utf-8"))
            self.sock.sendto(msg, self.target)
            time.sleep(0.01)

    def _recv_one(self):
        """Receive one RREF packet and update values dict."""
        data, _ = self.sock.recvfrom(4096)
        if len(data) < 5 or data[:4] != b"RREF":
            return
        payload = data[5:]
        n = len(payload) // 8
        for i in range(n):
            idx, val = struct.unpack("<if", payload[i*8:(i+1)*8])
            if val != val:  # NaN check
                val = 0.0
            self.values[idx] = val

    def drain(self):
        """Drain all pending packets (non-blocking)."""
        self.sock.setblocking(False)
        try:
            while True:
                self._recv_one()
        except BlockingIOError:
            pass
        finally:
            self.sock.settimeout(3.0)

    def recv_update(self):
        """Block until next RREF packet arrives, update values."""
        self._recv_one()

    def sample(self):
        """Return formatted dict from current values."""
        frame_time = self.values.get(0, 0)
        gpu_time = self.values.get(1, 0)
        fps = (1.0 / frame_time) if frame_time > 0.001 else 0
        cpu_time = max(0, frame_time - gpu_time)
        gs_kts = self.values.get(7, 0) * 1.94384
        ias_kts = self.values.get(8, 0) * 1.94384

        return {
            "fps": f"{fps:.1f}",
            "frame_time_ms": f"{frame_time * 1000:.2f}",
            "cpu_time_ms": f"{cpu_time * 1000:.2f}",
            "gpu_time_ms": f"{gpu_time * 1000:.2f}",
            "lat": f"{self.values.get(3, 0):.6f}",
            "lon": f"{self.values.get(4, 0):.6f}",
            "elev_m": f"{self.values.get(5, 0):.1f}",
            "agl_m": f"{self.values.get(6, 0):.1f}",
            "gs_kts": f"{gs_kts:.1f}",
            "ias_kts": f"{ias_kts:.1f}",
            "sim_time_s": f"{self.values.get(10, 0):.1f}",
            "paused": str(int(self.values.get(11, 0))),
        }

    def close(self):
        self.unsubscribe_all()
        self.sock.close()


def main():
    parser = argparse.ArgumentParser(description="X-Plane 12 telemetry recorder (UDP RREF)")
    parser.add_argument("-r", "--rate", type=int, default=5, help="Poll rate Hz (default: 5)")
    parser.add_argument("-o", "--output", default=".", help="Output directory")
    parser.add_argument("-d", "--duration", type=int, default=7200, help="Max duration sec")
    parser.add_argument("--host", default=XP_HOST)
    parser.add_argument("--port", type=int, default=XP_PORT)
    args = parser.parse_args()

    outdir = Path(args.output)
    outdir.mkdir(parents=True, exist_ok=True)
    outfile = outdir / "xplane_telemetry.csv"

    xp = XPlaneUDP(args.host, args.port)

    # Block until X-Plane is reachable (retries with backoff)
    xp.subscribe(args.rate, wait=True)

    # Graceful shutdown
    running = True
    def handle_signal(sig, frame):
        nonlocal running
        running = False
    signal.signal(signal.SIGINT, handle_signal)
    signal.signal(signal.SIGTERM, handle_signal)

    start_time = time.time()
    samples = 0
    last_print = 0
    consecutive_timeouts = 0
    RESUBSCRIBE_AFTER = 3       # re-subscribe after N consecutive timeouts

    print(f"\nRecording at {args.rate} Hz to {outfile}")
    print(f"Max duration: {args.duration}s. Press Ctrl+C to stop.\n")

    with open(outfile, "w", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=CSV_COLUMNS, extrasaction="ignore")
        writer.writeheader()

        while running:
            elapsed = time.time() - start_time
            if elapsed >= args.duration:
                print(f"\nDuration limit ({args.duration}s) reached.")
                break

            try:
                xp.recv_update()
                consecutive_timeouts = 0
                row = xp.sample()
                now = time.time()
                row["timestamp"] = f"{now:.3f}"
                row["rel_s"] = f"{now - start_time:.2f}"
                writer.writerow(row)
                samples += 1

                # Flush and print status every 10 seconds
                if now - last_print >= 10:
                    f.flush()
                    last_print = now
                    print(f"[{elapsed:7.1f}s] FPS={row['fps']}  "
                          f"CPU={row['cpu_time_ms']}ms  GPU={row['gpu_time_ms']}ms  "
                          f"Lat={row['lat']}  GS={row['gs_kts']}kts  "
                          f"({samples} samples)")

            except socket.timeout:
                consecutive_timeouts += 1
                if consecutive_timeouts >= RESUBSCRIBE_AFTER:
                    print(f"  {consecutive_timeouts} consecutive timeouts",
                          flush=True)
                    f.flush()
                    xp.resubscribe()
                    consecutive_timeouts = 0
                continue
            except Exception as e:
                print(f"Error: {e}", file=sys.stderr)
                time.sleep(0.5)

    xp.close()
    elapsed = time.time() - start_time
    rate = samples / elapsed if elapsed > 0 else 0
    print(f"\nDone. {samples} samples in {elapsed:.1f}s ({rate:.1f} Hz) → {outfile}")


if __name__ == "__main__":
    main()
