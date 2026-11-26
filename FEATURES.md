# Features

Sculk implements a focused offline-mode feature set for Minecraft Java Edition `26.1.2` (protocol `775`).

## Protocol and networking

- Server-list status with a configurable MOTD, and ping.
- Offline login, configuration (registries, tags, known packs, feature flags) and play.
- zlib compression with a 2,048-byte threshold, in both directions. One compressor is reused for every packet.
- Packet ids, registries and tags come from the vanilla data generator.
- A single-threaded `mio` event loop with vectored writes and partial-write queues. Every packet is encoded once and shared by all its recipients.
- Inbound limits: a 2 MiB frame cap, checked decompressed lengths, and string and slot bounds. A malformed packet disconnects only the client that sent it.
- Clients that stop reading are dropped once 4 MiB of output is queued for them. Connections that never finish logging in time out after 30 seconds.
- Options: bind address, port, player limit, seed, world file, MOTD, compression toggle.

## World

- A seeded procedural overworld (`--seed`, numeric or text).
- Biomes chosen from temperature and moisture:
  - plains, forest, birch forest, taiga, snowy plains, snowy taiga, desert
  - beaches, rivers (including frozen rivers), and oceans (including frozen and warm)
  - stony peaks and snowy slopes
- Continents, hills, mountains and river valleys. Sea level is 62, and cold water freezes over.
- Winding caves and ravines, lava pools at y 9 and below, and depth-banded ores with deepslate variants.
- Oak, birch and spruce trees that line up across chunk borders. Grass, ferns, flower meadows, cactus and dead bushes.
- Chunk sections use the smallest palette that fits: a single value or 4–8 bits per block. Each chunk is generated and encoded in about 80 µs with no heap allocations.
- A bounded LRU cache of compressed chunks. Edited chunks are re-encoded with the edits baked in.
- Chunks within view distance stream nearest-first, up to 10 per player per tick.
- Persistence: `--world FILE` saves block edits and player inventories every 10 seconds, writing the save atomically.

## Players and gameplay

- Join and leave messages, tab list, player entities, skin layers.
- Movement, look and head rotation, broadcast once per tick from one shared buffer. Moves larger than 100 blocks are rejected.
- Arm swing, sneaking pose and held-slot selection.
- Chat with a per-player rate limit and a character filter.
- A day cycle, plus keepalives every 15 seconds (a client that misses one is disconnected).
- Block breaking:
  - drops follow vanilla loot tables, tool kinds and tool tiers
  - bedrock and fluids can't be broken
  - a reach check applies
- Block placement:
  - for every placeable item, using its default block state
  - checks reach and that the target can be replaced
  - won't place a block inside a player
- A 46-slot player inventory with vanilla stack sizes. Inventories are kept by player name.
- 2×2 inventory crafting and 3×3 crafting tables, covering all 1,013 vanilla shaped and shapeless recipes, including mirrored patterns and tag ingredients.

## Tooling

- `sculk-bench` runs synthetic clients through deterministic workloads:
  - `idle`, `walk`, `explore`
  - `chunk-heavy`, `block-heavy`, `chat-heavy`
  - `mixed`, `gameplay`

  Every chunk, chat component and time packet the clients receive is checked.
- `./bench` builds and runs the server and records results as JSON:
  - CPU, RSS, PSS and USS
  - tick and chunk latency percentiles
  - optional allocation counters
  - delivered work
  - host and toolchain details
- `./bench gameplay` is a self-check that every gameplay feature delivers work with zero errors. CI runs it.
- `./bench scale` runs repeated 1–1,000-client sweeps. `./bench terrain` measures generator throughput. `./bench memory` sweeps hard cgroup memory limits.
- `sculk-map` renders a top-down PNG of any seed.
- Generators rebuild every vanilla table from Mojang's data-generator reports.
- Release builds for x86_64 and ARM64 Linux, with SHA-256 checksums.

## Not included

- Online-mode authentication and encryption.
- A server-authoritative inventory: the client is trusted for inventory contents and movement.
- Mobs, combat, health and hunger, redstone, item entities, and other dimensions.
- Commands, permissions, plugins, and support for multiple protocol versions.
- Bedrock Edition.
