#!/usr/bin/env python3
"""Compare two benchmark result directories produced by run_bench.py.

usage: compare_bench.py BASELINE_DIR CANDIDATE_DIR

Besides raw metrics, prints work-normalized CPU cost (CPU time per delivered
packet and per delivered chunk), which stays meaningful when two servers
deliver different amounts of work for the same client workload.
"""
import json
import pathlib
import sys

LOWER_IS_BETTER = {
    "errors",
    "chunk_parse_errors",
    "component_parse_errors",
    "server_cpu_seconds",
    "server_cpu_percent",
    "cpu_us_per_packet",
    "cpu_us_per_chunk",
    "server_rss_max_kb",
    "memory_per_player_kb",
    "server_uss_max_kb",
    "uss_per_player_kb",
    "binary_size_bytes",
    "startup_ms",
    "tick_p99_us",
}

METRICS = (
    "login_ok",
    "errors",
    "packets_recv",
    "chunks_recv",
    "system_chat_recv",
    "server_cpu_seconds",
    "cpu_us_per_packet",
    "cpu_us_per_chunk",
    "server_rss_max_kb",
    "memory_per_player_kb",
    "uss_per_player_kb",
    "binary_size_bytes",
    "startup_ms",
    "tick_p99_us",
)


def load_dir(path):
    data = {}
    for file in pathlib.Path(path).glob("*-player.json"):
        item = json.loads(file.read_text(encoding="utf-8"))
        cpu = item.get("server_cpu_seconds") or 0.0
        if item.get("packets_recv"):
            item["cpu_us_per_packet"] = cpu * 1e6 / item["packets_recv"]
        if item.get("chunks_recv"):
            item["cpu_us_per_chunk"] = cpu * 1e6 / item["chunks_recv"]
        data[int(item["players"])] = item
    return data


def fmt(value):
    if value is None:
        return "-"
    if isinstance(value, float):
        return f"{value:.2f}"
    return str(value)


def verdict(metric, base, cand):
    if not isinstance(base, (int, float)) or not isinstance(cand, (int, float)) or not base:
        return ""
    ratio = cand / base
    if abs(ratio - 1.0) < 0.02:
        return f"{ratio:.2f}x same"
    better = ratio < 1.0 if metric in LOWER_IS_BETTER else ratio > 1.0
    return f"{ratio:.2f}x {'better' if better else 'worse'}"


def main():
    if len(sys.argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    base, cand = load_dir(sys.argv[1]), load_dir(sys.argv[2])
    if not base or not cand:
        print("no result files found", file=sys.stderr)
        return 1
    base_name = next(iter(base.values())).get("server", "baseline")
    cand_name = next(iter(cand.values())).get("server", "candidate")
    print(f"{'players':>7} {'metric':>22} {base_name:>14} {cand_name:>14} {'ratio':>14}")
    for players in sorted(set(base) & set(cand)):
        for metric in METRICS:
            b = base[players].get(metric)
            c = cand[players].get(metric)
            print(f"{players:>7} {metric:>22} {fmt(b):>14} {fmt(c):>14} {verdict(metric, b, c):>14}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
