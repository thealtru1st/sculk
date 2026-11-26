# Sculk

Sculk is a tiny, fast Minecraft Java Edition server written in Rust. It runs a generated survival-style world, with terrain, biomes, trees, caves, ores, block breaking and placing, inventories, crafting, and multiplayer chat. Its target is the least CPU and memory per player, so it runs comfortably on weak Linux hardware.

It targets Minecraft **26.1.2** (protocol 775).

<p align="center">
  <img src="docs/images/terrain-map.png" alt="Top-down map of a Sculk world, seed 0" width="512">
  <br>
  <em>Seed 0, 1024×1024 blocks, rendered with <code>sculk-map</code></em>
</p>

## Highlights

- **One thread, no async runtime.** A `mio` event loop and a 20 Hz tick handle everything, with vectored writes and no locks.
- **Encode once, send to everyone.** Broadcast packets and chunks are encoded a single time and shared between recipients. Entity movement for a whole tick goes into one buffer that every player reads from.
- **Fast terrain.** Chunks are generated column by column, written directly into palettes, and cached after compression.
- **Vanilla data, generated.** Block states, items, recipes, loot drops, registries, tags and packet ids are generated from Mojang's official data-generator reports, not hand-maintained.
- **Persistent worlds.** With `--world`, block edits and inventories are saved to one compact file.
- **Hardened input handling.** Frame and decompression limits, per-connection error isolation, chat rate limits, reach checks, and slow-client eviction.

## Performance

These were measured with synthetic clients from `sculk-bench`. In the mixed workload each client moves 20 times a second and chats every half second. Each figure is the median of 3 runs of 10 seconds, with compression on.

| Players | CPU time per 10 s | Packets delivered | Peak private memory | Tick p99 |
|--------:|------------------:|------------------:|--------------------:|---------:|
| 1       | 0.02 s            | 101               | 0.96 MB             | –        |
| 10      | 0.09 s            | 39 k              | 1.0 MB              | 1.4 ms   |
| 50      | 0.33 s            | 1.04 M            | 1.25 MB             | 2.1 ms   |
| 100     | 0.67 s            | 4.18 M            | 1.6 MB              | 5.8 ms   |
| 500     | 3.17 s            | 92.1 M            | 8.7 MB              | 23 ms    |

**Under a hard memory limit:** the statically linked build keeps working inside a cgroup capped at **512 KB** with one player, and at 896 KB with ten players (swap disabled, `./bench memory`).

At 50 players:
- **Chunk streaming:** 50,750 chunks in 10 s for 0.55 s of CPU, about 11 µs per chunk delivered.
- **Chat flood:** 123,115 messages delivered for 0.25 s of CPU.
- **Fresh chunks:** generating and encoding a new chunk takes about 80 µs with no heap allocations. Compression brings it to about 1.2 KB.
- **Binary and startup:** the release binary is 660 KB and starts in about 20 ms.

"Private memory" is the server's own pages (USS); shared system libraries are excluded.

Hardware: Intel i7-11370H, WSL2 (Linux 6.18), rustc 1.88. Reproduce with `./bench sculk` or `./bench scale`.

## Build and run

You need stable Rust and the zlib development headers. On Debian/Ubuntu:

```bash
sudo apt-get install build-essential pkg-config zlib1g-dev
cargo build --release
target/release/sculk --world world.sculk
```

```text
--host <HOST>          Bind address (default 0.0.0.0)
--port <PORT>          Bind port (default 25565)
--max-players <N>      Player limit (default 1000)
--seed <SEED>          World seed, number or text (default 0)
--world <FILE>         Save block edits and inventories to FILE
--motd <TEXT>          Server list description
--no-compression       Disable protocol compression
```

Then, in Minecraft 26.1.2, go to Multiplayer → Add Server and enter `localhost` (or the machine's address).

Sculk runs in offline mode: it does not authenticate players with Mojang, and the client is trusted for inventory contents and movement. Run it for friends on a private network or behind an authenticating proxy, not as a public server.

## Benchmarks and checks

`sculk-bench` drives any number of synthetic clients through deterministic workloads: `idle`, `walk`, `explore`, `chunk-heavy`, `block-heavy`, `chat-heavy`, `mixed`, `gameplay`. The `bench` wrapper builds the server, runs it, samples CPU and memory from `/proc`, and writes JSON results:

```bash
./bench sculk          # benchmark (PLAYERS, DURATION, WORKLOAD, TRIALS)
./bench gameplay       # self-check that every gameplay feature works
./bench scale          # 1 to 1000 clients, repeated trials, medians and variance
./bench terrain        # chunk generation and encoding throughput
./bench memory         # hard cgroup memory-limit sweep (Linux + systemd)
cargo run --release -p sculk-bench --bin sculk-map -- --seed 0 --chunks 64 --scale 2
```

CI runs formatting, clippy, the tests, the gameplay self-check and a 50-player benchmark on every push.

## Regenerating vanilla data

```bash
java -DbundlerMainClass=net.minecraft.data.Main -jar server.jar --reports --server --output generated
python3 scripts/gen_vanilla_data.py generated
python3 scripts/gen_vanilla_items.py generated
```

## Scope

Sculk is intentionally small. It does not include:
- mobs, combat or health
- redstone
- other dimensions
- commands or plugins
- online-mode authentication
- support for multiple protocol versions

See [FEATURES.md](FEATURES.md) for the full list.

## License

MIT. See [LICENSE](LICENSE).

Sculk is not an official Minecraft product and is not approved by or associated with Mojang or Microsoft.
