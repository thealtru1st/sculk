#!/usr/bin/env python3
import argparse
import json
import os
import pathlib
import platform
import shlex
import signal
import socket
import statistics
import subprocess
import sys
import time


CLK_TCK = os.sysconf(os.sysconf_names["SC_CLK_TCK"])
PAGE_SIZE = os.sysconf(os.sysconf_names["SC_PAGE_SIZE"])


def parse_args():
    parser = argparse.ArgumentParser(description="Run a managed server benchmark with synthetic clients")
    parser.add_argument("--name", required=True)
    parser.add_argument("--server-cmd", required=True)
    parser.add_argument("--bench-bin", default="target/release/sculk-bench")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=25565)
    parser.add_argument("--players", nargs="+", type=int, default=[1, 10, 100])
    parser.add_argument("--duration", type=int, default=15)
    parser.add_argument("--workload", default="mixed")
    parser.add_argument("--out-dir", required=True)
    parser.add_argument("--idle-sample", type=float, default=1.0)
    parser.add_argument("--sample-interval", type=float, default=0.1)
    parser.add_argument("--startup-timeout", type=float, default=10.0)
    parser.add_argument("--trials", type=int, default=1)
    return parser.parse_args()


def proc_times(pid):
    with open(f"/proc/{pid}/stat", "r", encoding="utf-8") as f:
        stat = f.read()
    fields = stat.rsplit(") ", 1)[1].split()
    user = int(fields[11]) / CLK_TCK
    system = int(fields[12]) / CLK_TCK
    rss_kb = int(fields[21]) * PAGE_SIZE / 1024
    return user + system, int(rss_kb)


def proc_sample(pid):
    cpu, stat_rss_kb = proc_times(pid)
    values = {}
    try:
        with open(f"/proc/{pid}/smaps_rollup", "r", encoding="utf-8") as f:
            for line in f:
                fields = line.split()
                if len(fields) >= 2 and fields[0].endswith(":"):
                    values[fields[0][:-1]] = int(fields[1])
    except OSError:
        pass
    rss_kb = values.get("Rss", stat_rss_kb)
    pss_kb = values.get("Pss", rss_kb)
    uss_kb = values.get("Private_Clean", 0) + values.get("Private_Dirty", 0)
    return cpu, rss_kb, pss_kb, uss_kb


def binary_size(cmd):
    exe = pathlib.Path(shlex.split(cmd)[0])
    if not exe.exists():
        return None
    return exe.stat().st_size


def name_prefix(name, players):
    cleaned = "".join(c for c in f"{name}{players}" if c.isalnum() or c == "_")
    return (cleaned or "bench")[:12]


def port_open(host, port):
    try:
        with socket.create_connection((host, port), timeout=0.1):
            return True
    except OSError:
        return False


def log_tail(path, limit=4096):
    try:
        data = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return ""
    return data[-limit:].strip()


def wait_for_port(host, port, deadline, proc):
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            return False
        if port_open(host, port):
            return True
        time.sleep(0.02)
    return False


def terminate(proc):
    if proc.poll() is not None:
        return
    try:
        os.killpg(proc.pid, signal.SIGTERM)
        proc.wait(timeout=2)
    except Exception:
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except Exception:
            pass
        proc.wait(timeout=2)


def cpu_percent(before_cpu, after_cpu, wall_secs):
    if wall_secs <= 0:
        return 0.0
    return 100.0 * (after_cpu - before_cpu) / wall_secs


def command_version(command):
    try:
        result = subprocess.run(
            command,
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=5,
        )
        return result.stdout.splitlines()[0].strip()
    except (OSError, subprocess.SubprocessError, IndexError):
        return None


def host_metadata():
    cpu_model = None
    try:
        for line in pathlib.Path("/proc/cpuinfo").read_text(encoding="utf-8").splitlines():
            if line.lower().startswith("model name"):
                cpu_model = line.split(":", 1)[1].strip()
                break
    except (OSError, IndexError):
        pass
    return {
        "platform": platform.platform(),
        "kernel": platform.release(),
        "architecture": platform.machine(),
        "cpu_model": cpu_model,
        "logical_cpus": os.cpu_count(),
        "python": platform.python_version(),
        "rustc": command_version(["rustc", "--version"]),
        "cargo": command_version(["cargo", "--version"]),
        "zlib": command_version(["pkg-config", "--modversion", "zlib"]),
    }


def parse_runtime_metrics(path):
    values = {}
    try:
        lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError:
        return values
    for line in reversed(lines):
        if not line.startswith("[METRICS]: "):
            continue
        for field in line.removeprefix("[METRICS]: ").split():
            if "=" not in field:
                continue
            key, raw = field.split("=", 1)
            if raw in ("true", "false"):
                values[key] = raw == "true"
                continue
            try:
                values[key] = int(raw)
            except ValueError:
                try:
                    values[key] = float(raw)
                except ValueError:
                    values[key] = raw
        break
    return values


def wait_for_runtime_metrics(path, timeout):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        values = parse_runtime_metrics(path)
        if values:
            return values
        time.sleep(0.05)
    return parse_runtime_metrics(path)


def run_one(args, players, out_dir, trial):
    suffix = "" if args.trials == 1 else f"-trial-{trial}"
    log_path = out_dir / f"{players}-player{suffix}.server.log"
    if port_open(args.host, args.port):
        raise RuntimeError(
            f"refusing to benchmark {args.name}: {args.host}:{args.port} is already in use"
        )
    with log_path.open("w", encoding="utf-8") as log:
        start = time.monotonic()
        server = subprocess.Popen(
            shlex.split(args.server_cmd),
            stdout=log,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        try:
            if not wait_for_port(args.host, args.port, start + args.startup_timeout, server):
                reason = (
                    f"server exited with status {server.returncode}"
                    if server.poll() is not None
                    else f"server did not listen on {args.host}:{args.port}"
                )
                tail = log_tail(log_path)
                if tail:
                    reason = f"{reason}; log tail: {tail}"
                raise RuntimeError(reason)
            if server.poll() is not None:
                tail = log_tail(log_path)
                reason = f"server exited with status {server.returncode} after opening port"
                if tail:
                    reason = f"{reason}; log tail: {tail}"
                raise RuntimeError(reason)
            startup_ms = (time.monotonic() - start) * 1000.0

            idle_start_cpu, idle_start_rss, idle_start_pss, idle_start_uss = proc_sample(
                server.pid
            )
            idle_start = time.monotonic()
            max_rss_kb = idle_start_rss
            max_pss_kb = idle_start_pss
            max_uss_kb = idle_start_uss
            while time.monotonic() - idle_start < args.idle_sample:
                _, rss, pss, uss = proc_sample(server.pid)
                max_rss_kb = max(max_rss_kb, rss)
                max_pss_kb = max(max_pss_kb, pss)
                max_uss_kb = max(max_uss_kb, uss)
                time.sleep(args.sample_interval)
            idle_end_cpu, idle_rss_kb, idle_pss_kb, idle_uss_kb = proc_sample(server.pid)
            idle_wall = time.monotonic() - idle_start

            bench_cmd = [
                args.bench_bin,
                "--host",
                args.host,
                "--port",
                str(args.port),
                "--players",
                str(players),
                "--duration",
                str(args.duration),
                "--workload",
                args.workload,
                "--name-prefix",
                name_prefix(args.name, players),
                "--json",
            ]
            before_cpu, before_rss, before_pss, before_uss = proc_sample(server.pid)
            max_rss_kb = max(max_rss_kb, before_rss)
            max_pss_kb = max(max_pss_kb, before_pss)
            max_uss_kb = max(max_uss_kb, before_uss)
            bench_start = time.monotonic()
            client = subprocess.Popen(
                bench_cmd,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            while client.poll() is None:
                _, rss, pss, uss = proc_sample(server.pid)
                max_rss_kb = max(max_rss_kb, rss)
                max_pss_kb = max(max_pss_kb, pss)
                max_uss_kb = max(max_uss_kb, uss)
                time.sleep(args.sample_interval)
            stdout, stderr = client.communicate()
            bench_wall = time.monotonic() - bench_start
            after_cpu, after_rss, after_pss, after_uss = proc_sample(server.pid)
            max_rss_kb = max(max_rss_kb, after_rss)
            max_pss_kb = max(max_pss_kb, after_pss)
            max_uss_kb = max(max_uss_kb, after_uss)

            if client.returncode != 0:
                raise RuntimeError(f"sculk-bench failed: {stderr.strip()}")
            bot = json.loads(stdout)
            bot_packets = bot.get("packets_sent", 0) + bot.get("packets_recv", 0)
            runtime_metrics = wait_for_runtime_metrics(log_path, 1.0)
            result = {
                **bot,
                **runtime_metrics,
                "server": args.name,
                "server_cmd": args.server_cmd,
                "server_pid": server.pid,
                "server_log": str(log_path),
                "startup_ms": startup_ms,
                "binary_size_bytes": binary_size(args.server_cmd),
                "idle_rss_kb": idle_rss_kb,
                "idle_pss_kb": idle_pss_kb,
                "idle_uss_kb": idle_uss_kb,
                "idle_cpu_percent": cpu_percent(idle_start_cpu, idle_end_cpu, idle_wall),
                "server_cpu_percent": cpu_percent(before_cpu, after_cpu, bench_wall),
                "server_cpu_seconds": after_cpu - before_cpu,
                "server_rss_start_kb": before_rss,
                "server_rss_end_kb": after_rss,
                "server_rss_max_kb": max_rss_kb,
                "memory_per_player_kb": max(0.0, (max_rss_kb - idle_rss_kb) / max(players, 1)),
                "server_pss_start_kb": before_pss,
                "server_pss_end_kb": after_pss,
                "server_pss_max_kb": max_pss_kb,
                "pss_per_player_kb": max(0.0, (max_pss_kb - idle_pss_kb) / max(players, 1)),
                "server_uss_start_kb": before_uss,
                "server_uss_end_kb": after_uss,
                "server_uss_max_kb": max_uss_kb,
                "uss_per_player_kb": max(0.0, (max_uss_kb - idle_uss_kb) / max(players, 1)),
                "chunks_per_sec": bot.get("chunks_recv", 0) / max(bench_wall, 0.001),
                "login_throughput_per_sec": bot.get("login_ok", 0) / max(bench_wall, 0.001),
                "total_packets_per_sec": bot_packets / max(bench_wall, 0.001),
                "bench_wall_secs": bench_wall,
                "trial": trial,
            }
            return result
        finally:
            terminate(server)


def summarize_trials(results, metadata):
    if len(results) == 1:
        summary = dict(results[0])
        summary["trials"] = 1
        summary["host"] = metadata
        return summary

    summary = dict(results[0])
    numeric_keys = sorted(
        key
        for key in set().union(*(result.keys() for result in results))
        if all(
            isinstance(result.get(key), (int, float))
            and not isinstance(result.get(key), bool)
            for result in results
        )
        and key not in {"server_pid", "trial"}
    )
    variation = {}
    for key in numeric_keys:
        values = [float(result[key]) for result in results]
        summary[key] = statistics.median(values)
        variation[key] = {
            "min": min(values),
            "max": max(values),
            "mean": statistics.fmean(values),
            "median": statistics.median(values),
            "population_stddev": statistics.pstdev(values),
        }
    summary.pop("server_pid", None)
    summary.pop("trial", None)
    summary["trials"] = len(results)
    summary["variation"] = variation
    summary["trial_results"] = [
        pathlib.Path(result["result_file"]).name for result in results
    ]
    summary["host"] = metadata
    return summary


def main():
    args = parse_args()
    out_dir = pathlib.Path(args.out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    if not pathlib.Path(args.bench_bin).exists():
        print(f"benchmark binary not found: {args.bench_bin}", file=sys.stderr)
        return 2
    if args.trials < 1:
        print("--trials must be at least 1", file=sys.stderr)
        return 2

    metadata = host_metadata()

    for players in args.players:
        results = []
        for trial in range(1, args.trials + 1):
            result = run_one(args, players, out_dir, trial)
            suffix = "" if args.trials == 1 else f"-trial-{trial}"
            trial_path = out_dir / f"{players}-player{suffix}.json"
            result["result_file"] = str(trial_path)
            result["host"] = metadata
            trial_path.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
            results.append(result)
        result = summarize_trials(results, metadata)
        out_path = out_dir / f"{players}-player.json"
        out_path.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
        print(json.dumps(result, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
