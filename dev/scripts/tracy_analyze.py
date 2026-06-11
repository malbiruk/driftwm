#!/usr/bin/env python3
"""Analyze a driftwm Tracy capture: frame cadence and per-zone timing.

Exports CSVs from a .tracy file via `tracy-csvexport`, then reports:
  - exec-time stats for each Tracy zone (span)
  - frame-to-frame interval cadence during *active* rendering (idle gaps
    filtered out — driftwm parks the render loop when nothing moves, so raw
    intervals are dominated by multi-second idle pauses)
  - optional: frame times bucketed by a Tracy plot value (e.g. the chunked
    tile-bg's `bg_chunks.target_lod`), for per-LOD / per-state breakdowns

Usage:
    dev/scripts/tracy_analyze.py CAPTURE.tracy
    dev/scripts/tracy_analyze.py CAPTURE.tracy --bucket-by bg_chunks.target_lod
    dev/scripts/tracy_analyze.py CAPTURE.tracy --bucket-by frame.commits
    dev/scripts/tracy_analyze.py CAPTURE.tracy --frame-zone winit::frame
    dev/scripts/tracy_analyze.py CAPTURE.tracy --gpu

The csvexport binary is found via $TRACY_CSVEXPORT, else `tracy-csvexport` on
PATH, else ~/tracy/csvexport/build/tracy-csvexport (the from-source build that
dev/docs/PROFILING.md describes).
"""

import argparse
import bisect
import csv
import os
import shutil
import subprocess
import sys

# An interval longer than this means the render loop was parked (no damage),
# not a slow frame — exclude from cadence stats.
IDLE_GAP_NS = 100_000_000  # 100 ms
VBLANK_60_NS = 16_666_667  # one 60 Hz vblank


def find_csvexport() -> str:
    env = os.environ.get("TRACY_CSVEXPORT")
    if env and os.path.exists(env):
        return env
    on_path = shutil.which("tracy-csvexport")
    if on_path:
        return on_path
    fallback = os.path.expanduser("~/tracy/csvexport/build/tracy-csvexport")
    if os.path.exists(fallback):
        return fallback
    sys.exit(
        "tracy-csvexport not found. Set $TRACY_CSVEXPORT, put it on PATH, or "
        "build it (see dev/docs/PROFILING.md)."
    )


def export(csvexport: str, trace: str, extra_args: list[str]) -> str:
    proc = subprocess.run(
        [csvexport, *extra_args, trace], capture_output=True, text=True
    )
    if proc.returncode != 0:
        sys.exit(f"tracy-csvexport failed: {proc.stderr.strip()}")
    return proc.stdout


def fmt_ms(ns: float) -> float:
    return ns / 1e6


def stats(label: str, vals_ns: list[int]) -> None:
    if not vals_ns:
        print(f"{label:<28}(no samples)")
        return
    v = sorted(vals_ns)
    n = len(v)

    def pct(p: float) -> int:
        return v[min(n - 1, int(n * p))]

    over = sum(1 for x in v if x > VBLANK_60_NS) * 100.0 / n
    print(
        f"{label:<28}N={n:<6}mean={fmt_ms(sum(v) / n):<7.2f}"
        f"p50={fmt_ms(pct(0.5)):<7.2f}p90={fmt_ms(pct(0.9)):<7.2f}"
        f"p99={fmt_ms(pct(0.99)):<7.2f}max={fmt_ms(v[-1]):<8.2f}>16.6ms={over:.1f}%"
    )


def parse_zones(unwrapped_csv: str) -> dict[str, list[tuple[int, int]]]:
    """name -> [(start_ns, exec_ns), ...]"""
    zones: dict[str, list[tuple[int, int]]] = {}
    for row in csv.DictReader(unwrapped_csv.splitlines()):
        try:
            t = int(row["ns_since_start"])
            dt = int(row["exec_time_ns"])
        except (ValueError, KeyError):
            continue
        zones.setdefault(row["name"], []).append((t, dt))
    for v in zones.values():
        v.sort()
    return zones


def parse_plot(plots_csv: str, plot_name: str) -> list[tuple[int, float]]:
    # `-u -p` emits the zone rows first, then appends plot rows under the same
    # header — so this stream is zones-then-plots, not plots only. Filtering by
    # name skips the zone rows; only `name`/`ns_since_start`/`value` are read,
    # all of which the plot rows populate correctly.
    out: list[tuple[int, float]] = []
    for row in csv.DictReader(plots_csv.splitlines()):
        if row.get("name") != plot_name:
            continue
        try:
            out.append((int(row["ns_since_start"]), float(row["value"])))
        except (ValueError, TypeError, KeyError):
            continue
    out.sort()
    return out


def report_zone_times(zones: dict[str, list[tuple[int, int]]]) -> None:
    print("=== zone exec times ===")
    by_total = sorted(
        zones.items(), key=lambda kv: sum(d for _, d in kv[1]), reverse=True
    )
    for name, samples in by_total:
        stats(name, [d for _, d in samples])


def report_cadence(frame_starts: list[int]) -> None:
    intervals = [
        frame_starts[i + 1] - frame_starts[i] for i in range(len(frame_starts) - 1)
    ]
    active = [iv for iv in intervals if iv < IDLE_GAP_NS]
    print("\n=== frame cadence (idle gaps >100ms removed) ===")
    stats("active interval", active)
    if not active:
        return
    n = len(active)
    one = sum(1 for v in active if v < 25e6)
    two = sum(1 for v in active if 25e6 <= v < 42e6)
    more = sum(1 for v in active if v >= 42e6)
    print(f"\nactive frames: {n}   effective fps: {1000 / (sum(active) / n / 1e6):.1f}")
    print(f"  1 vblank  (<25 ms, ~60fps):    {one:>5} ({one * 100.0 / n:.1f}%)")
    print(f"  2 vblanks (25-42 ms, ~30fps):  {two:>5} ({two * 100.0 / n:.1f}%)")
    print(f"  3+ vblanks (>42 ms, <=20fps):  {more:>5} ({more * 100.0 / n:.1f}%)")


def report_buckets(
    frames: list[tuple[int, int]], plot: list[tuple[int, float]], plot_name: str
) -> None:
    if not plot:
        print(f"\n(plot '{plot_name}' has no samples — nothing to bucket)")
        return
    times = [t for t, _ in plot]
    vals = [v for _, v in plot]
    buckets: dict[float, list[int]] = {}
    interval_buckets: dict[float, list[int]] = {}
    starts = [t for t, _ in frames]
    for n, (start, dt) in enumerate(frames):
        # Skip the first frame after an idle gap: a cumulative plot sample
        # there (e.g. frame.commits) covers the whole gap, not one frame.
        if n > 0 and start - starts[n - 1] >= IDLE_GAP_NS:
            continue
        i = bisect.bisect_right(times, start + dt) - 1
        if i < 0:
            continue
        # The interval n→n+1 is bucketed by the plot value at frame n. For
        # plots emitted at frame start (frame.commits), that value covers the
        # *preceding* interval — the work submitted before this render, which
        # is what contends with it.
        buckets.setdefault(vals[i], []).append(dt)
        if n + 1 < len(starts) and starts[n + 1] - start < IDLE_GAP_NS:
            interval_buckets.setdefault(vals[i], []).append(starts[n + 1] - start)
    print(f"\n=== frame exec time bucketed by {plot_name} ===")
    for key in sorted(buckets):
        stats(f"{plot_name}={key:g}", buckets[key])
    print(f"\n=== frame interval bucketed by {plot_name} (stutter = >16.6ms) ===")
    for key in sorted(interval_buckets):
        stats(f"{plot_name}={key:g}", interval_buckets[key])


def report_gpu(csvexport: str, trace: str, frames: list[tuple[int, int]] | None) -> None:
    """GPU zone stats from `tracy-csvexport -g`, split by proximity to slow
    frames. Fixed-work zones (clear, draw_solid) stretching near slow frames
    indicate whole-GPU stalls/contention rather than an expensive shader."""
    gpu: dict[str, list[tuple[float, float]]] = {}
    raw_rows = 0
    for row in csv.DictReader(export(csvexport, trace, ["-g"]).splitlines()):
        raw_rows += 1
        try:
            t = float(row["Time from start of program"])
            dt = float(row["GPU execution time"])
        except (ValueError, TypeError, KeyError):
            continue
        gpu.setdefault(row["name"], []).append((t, dt))
    if not gpu:
        if raw_rows:
            print(
                "\n(GPU rows present but columns unrecognized — "
                "tracy-csvexport version mismatch?)"
            )
        else:
            print("\n(no GPU zones in capture — udev + profile-with-tracy required)")
        return

    print("\n=== GPU zone exec times ===")
    for name, spans in sorted(gpu.items(), key=lambda kv: -sum(d for _, d in kv[1])):
        stats(name, [int(d) for _, d in spans])

    if not frames:
        return
    starts = [t for t, _ in frames]
    slow = sorted(
        starts[i]
        for i in range(len(starts) - 1)
        if 25e6 <= starts[i + 1] - starts[i] < IDLE_GAP_NS
    )
    if not slow:
        print("\n(no slow frames — skipping near-slow GPU split)")
        return

    def near_slow(t: float) -> bool:
        j = bisect.bisect_left(slow, t)
        return any(
            0 <= k < len(slow) and abs(slow[k] - t) < 0.5e9 for k in (j - 1, j)
        )

    print("\n=== GPU zones near slow frames (±0.5s) vs calm ===")
    for name, spans in sorted(gpu.items(), key=lambda kv: -sum(d for _, d in kv[1])):
        stats(f"{name} (near slow)", [int(d) for t, d in spans if near_slow(t)])
        stats(f"{name} (calm)", [int(d) for t, d in spans if not near_slow(t)])


def main() -> None:
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    ap.add_argument("trace", help="path to a .tracy capture")
    ap.add_argument(
        "--frame-zone",
        default="udev::render_frame",
        help="zone whose starts define frame boundaries "
        "(default: udev::render_frame; use winit::frame for the winit backend)",
    )
    ap.add_argument(
        "--bucket-by",
        metavar="PLOT",
        help="bucket frame exec times by this Tracy plot (e.g. bg_chunks.target_lod)",
    )
    ap.add_argument(
        "--gpu",
        action="store_true",
        help="also report GPU zone stats, split by proximity to slow frames",
    )
    args = ap.parse_args()

    if not os.path.exists(args.trace):
        sys.exit(f"no such file: {args.trace}")

    csvexport = find_csvexport()
    zones = parse_zones(export(csvexport, args.trace, ["-u"]))

    report_zone_times(zones)

    frames = zones.get(args.frame_zone)
    if not frames:
        print(
            f"\n(no '{args.frame_zone}' zone found — wrong backend? "
            f"try --frame-zone winit::frame)"
        )
        if args.gpu:
            report_gpu(csvexport, args.trace, None)
        return
    report_cadence([t for t, _ in frames])

    if args.bucket_by:
        plot = parse_plot(export(csvexport, args.trace, ["-u", "-p"]), args.bucket_by)
        report_buckets(frames, plot, args.bucket_by)

    if args.gpu:
        report_gpu(csvexport, args.trace, frames)


if __name__ == "__main__":
    main()
