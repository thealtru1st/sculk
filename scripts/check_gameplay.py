#!/usr/bin/env python3
"""Assert that a `gameplay` workload run exercised every feature cleanly.

usage: check_gameplay.py RESULT_DIR
"""
import json
import pathlib
import sys

VIEW_CHUNKS = 25  # (2 * view distance + 1) ** 2


def check(result):
    players = int(result["players"])
    failures = []

    def expect(name, ok, detail):
        if not ok:
            failures.append(f"{players} players: {name}: {detail}")

    expect("logins", result["login_ok"] == players, f"{result['login_ok']} of {players}")
    for key in ("errors", "chunk_parse_errors", "component_parse_errors"):
        expect(key, result[key] == 0, result[key])
    expect(
        "chunks",
        result["chunks_recv"] >= VIEW_CHUNKS * players,
        f"{result['chunks_recv']} < {VIEW_CHUNKS * players}",
    )
    for key in (
        "block_ack_recv",
        "block_update_recv",
        "inventory_content_recv",
        "container_slot_recv",
        "system_chat_recv",
        "set_time_recv",
    ):
        expect(key, result[key] > 0, "none received")
    if players > 1:
        expect("entity_sync_recv", result["entity_sync_recv"] > 0, "no movement replicated")
    return failures


def main():
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    files = sorted(pathlib.Path(sys.argv[1]).glob("*-player.json"))
    if not files:
        raise SystemExit("gameplay check: no result files found")
    failures = []
    for file in files:
        failures += check(json.loads(file.read_text(encoding="utf-8")))
    for failure in failures:
        print(f"gameplay check failed: {failure}", file=sys.stderr)
    if failures:
        raise SystemExit(1)
    print(f"gameplay check passed ({len(files)} result files)")


if __name__ == "__main__":
    main()
