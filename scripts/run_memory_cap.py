#!/usr/bin/env python3
import argparse
import json
import os
import pathlib
import socket
import subprocess
import sys
import time


def parse_args():
    parser = argparse.ArgumentParser(
        description="Run Sculk under enforced systemd user-cgroup memory limits"
    )
    parser.add_argument("--server-bin", default="target-static/release/sculk")
    parser.add_argument("--bench-bin", default="target/release/sculk-bench")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=25565)
    parser.add_argument("--duration", type=int, default=5)
    parser.add_argument("--workload", default="gameplay")
    parser.add_argument("--limits", nargs="+", default=["1M", "768K", "512K"])
    parser.add_argument("--trials", type=int, default=1)
    parser.add_argument("--players", type=int, default=1)
    parser.add_argument("--out-dir", default="benchmarks/runs/memory-cap")
    parser.add_argument("--startup-timeout", type=float, default=5.0)
    return parser.parse_args()


def parse_size(value):
    suffixes = {"K": 1024, "M": 1024**2, "G": 1024**3}
    normalized = value.strip().upper()
    multiplier = suffixes.get(normalized[-1:], 1)
    number = normalized[:-1] if multiplier != 1 else normalized
    size = int(number) * multiplier
    if size <= 0:
        raise ValueError(f"memory limit must be positive: {value}")
    return size


def port_open(host, port):
    try:
        with socket.create_connection((host, port), timeout=0.1):
            return True
    except OSError:
        return False


def wait_for_port(host, port, timeout):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if port_open(host, port):
            return True
        time.sleep(0.02)
    return False


def unit_properties(unit):
    result = subprocess.run(
        [
            "systemctl",
            "--user",
            "show",
            unit,
            "--property=ActiveState",
            "--property=SubState",
            "--property=Result",
            "--property=ExecMainCode",
            "--property=ExecMainStatus",
            "--property=MemoryCurrent",
            "--property=MemoryPeak",
            "--property=MemoryMax",
            "--property=MemorySwapCurrent",
            "--property=MemorySwapPeak",
            "--property=MemorySwapMax",
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
        check=False,
    )
    properties = {}
    for line in result.stdout.splitlines():
        if "=" in line:
            key, value = line.split("=", 1)
            properties[key] = value
    return properties


def integer_property(properties, name):
    try:
        return int(properties.get(name, ""))
    except ValueError:
        return None


def stop_unit(unit):
    subprocess.run(
        ["systemctl", "--user", "stop", unit],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    subprocess.run(
        ["systemctl", "--user", "reset-failed", unit],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )


def validation_passed(bot, properties, players):
    return bool(
        bot
        and bot.get("connected") == players
        and bot.get("login_ok") == players
        and bot.get("errors") == 0
        and bot.get("chunks_recv", 0) >= 25 * players
        and bot.get("chunk_parse_errors") == 0
        and bot.get("component_parse_errors") == 0
        and bot.get("block_update_recv", 0) > 0
        and bot.get("block_ack_recv", 0) > 0
        and bot.get("inventory_content_recv", 0) > 0
        and bot.get("container_slot_recv", 0) > 0
        and properties.get("ActiveState") == "active"
    )


def run_limit(args, root, out_dir, label, limit_bytes, index, trial):
    unit = f"sculk-memory-{os.getpid()}-{index}"
    log_stem = label if args.trials == 1 else f"{label}-trial-{trial}"
    log_path = out_dir / f"{log_stem}.server.log"
    log_path.unlink(missing_ok=True)
    server = str((root / args.server_bin).resolve())
    bench = str((root / args.bench_bin).resolve())
    if port_open(args.host, args.port):
        raise RuntimeError(f"{args.host}:{args.port} is already in use")

    command = [
        "systemd-run",
        "--user",
        "--quiet",
        f"--unit={unit}",
        f"--working-directory={root}",
        f"--property=MemoryMax={limit_bytes}",
        "--property=MemorySwapMax=0",
        "--property=OOMPolicy=stop",
        f"--property=StandardOutput=append:{log_path.resolve()}",
        f"--property=StandardError=append:{log_path.resolve()}",
        server,
        "--host",
        args.host,
        "--port",
        str(args.port),
        "--max-players",
        str(args.players),
    ]

    bot = None
    client_error = None
    try:
        launched = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        if launched.returncode != 0:
            client_error = launched.stderr.strip() or launched.stdout.strip()
        elif wait_for_port(args.host, args.port, args.startup_timeout):
            client = subprocess.run(
                [
                    bench,
                    "--host",
                    args.host,
                    "--port",
                    str(args.port),
                    "--players",
                    str(args.players),
                    "--duration",
                    str(args.duration),
                    "--workload",
                    args.workload,
                    "--name-prefix",
                    "MemCap",
                    "--json",
                ],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                timeout=args.duration + 30,
            )
            if client.returncode == 0:
                bot = json.loads(client.stdout)
            else:
                client_error = client.stderr.strip()
        else:
            client_error = "server did not become ready"
        time.sleep(0.1)
        properties = unit_properties(unit)
        passed = validation_passed(bot, properties, args.players)
        peak = integer_property(properties, "MemoryPeak")
        result = {
            "limit": label,
            "limit_bytes": limit_bytes,
            "trial": trial,
            "passed": passed,
            "memory_peak_bytes": peak,
            "enforced_memory_max_bytes": integer_property(properties, "MemoryMax"),
            "headroom_bytes": limit_bytes - peak if peak is not None else None,
            "memory_swap_peak_bytes": integer_property(properties, "MemorySwapPeak"),
            "enforced_memory_swap_max_bytes": integer_property(
                properties, "MemorySwapMax"
            ),
            "unit_result": properties.get("Result"),
            "unit_active_state": properties.get("ActiveState"),
            "unit_sub_state": properties.get("SubState"),
            "exec_main_code": properties.get("ExecMainCode"),
            "exec_main_status": properties.get("ExecMainStatus"),
            "client_error": client_error,
            "validation": bot,
            "server_log": str(log_path),
        }
        return result
    finally:
        stop_unit(unit)


def main():
    args = parse_args()
    root = pathlib.Path.cwd()
    out_dir = pathlib.Path(args.out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    for binary in (root / args.server_bin, root / args.bench_bin):
        if not binary.exists():
            print(f"binary not found: {binary}", file=sys.stderr)
            return 2
    if port_open(args.host, args.port):
        print(f"refusing to run: {args.host}:{args.port} is already in use", file=sys.stderr)
        return 2
    if args.trials < 1:
        print("--trials must be at least 1", file=sys.stderr)
        return 2

    results = []
    run_index = 0
    for label in args.limits:
        try:
            limit_bytes = parse_size(label)
        except ValueError as error:
            print(error, file=sys.stderr)
            return 2
        for trial in range(1, args.trials + 1):
            result = run_limit(
                args, root, out_dir, label, limit_bytes, run_index, trial
            )
            run_index += 1
            results.append(result)
            status = "PASS" if result["passed"] else "FAIL"
            peak = result["memory_peak_bytes"]
            peak_text = f"{peak / 1024:.0f} KiB" if peak is not None else "unknown"
            print(
                f"{label:>7} trial={trial}/{args.trials} {status} peak={peak_text}",
                flush=True,
            )
            write_summary(args, root, out_dir, results, complete=False)

    write_summary(args, root, out_dir, results, complete=True)
    passing = [result for result in results if result["passed"]]
    return 0 if passing else 1


def write_summary(args, root, out_dir, results, complete):
    passing = [result for result in results if result["passed"]]
    limits = []
    for label in args.limits:
        matching = [result for result in results if result["limit"] == label]
        if not matching:
            continue
        peaks = [
            result["memory_peak_bytes"]
            for result in matching
            if result["memory_peak_bytes"] is not None
        ]
        passes = sum(result["passed"] for result in matching)
        limits.append(
            {
                "limit": label,
                "limit_bytes": matching[0]["limit_bytes"],
                "completed_trials": len(matching),
                "requested_trials": args.trials,
                "passes": passes,
                "pass_rate": passes / len(matching),
                "consistently_passed": len(matching) == args.trials
                and passes == args.trials,
                "memory_peak_min_bytes": min(peaks, default=None),
                "memory_peak_max_bytes": max(peaks, default=None),
            }
        )
    consistent = [result for result in limits if result["consistently_passed"]]
    summary = {
        "server_binary": str((root / args.server_bin).resolve()),
        "server_binary_size_bytes": (root / args.server_bin).stat().st_size,
        "workload": args.workload,
        "duration_secs": args.duration,
        "trials_per_limit": args.trials,
        "swap_disabled": True,
        "complete": complete,
        "lowest_passing_limit_bytes": min(
            (result["limit_bytes"] for result in passing), default=None
        ),
        "lowest_consistently_passing_limit_bytes": min(
            (result["limit_bytes"] for result in consistent), default=None
        ),
        "limits": limits,
        "results": results,
    }
    summary_path = out_dir / "summary.json"
    summary_path.write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")
    if complete:
        print(f"summary: {summary_path}", flush=True)


if __name__ == "__main__":
    raise SystemExit(main())
