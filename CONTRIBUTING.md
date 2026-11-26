# Contributing

Sculk keeps a narrow feature set on purpose. Changes should improve correctness, measured performance, portability or maintainability, without turning the project into a general server platform.

## Workflow

1. Branch from `main`.
2. Make one focused change, and update the docs it affects in the same commit.
3. Run the checks below.
4. Check `git status` for stray generated or scratch files before committing.

Keep performance changes and feature changes in separate commits, so each can be measured on its own.

## Checks

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p sculk --all-targets --features allocation-metrics -- -D warnings
cargo test --workspace --release
cargo test -p sculk --release --features allocation-metrics
PYTHONDONTWRITEBYTECODE=1 python3 -m py_compile scripts/*.py
bash -n bench
./bench gameplay
```

CI runs all of these on every push.

## Performance changes

- Record the command, hardware, workload, and before/after results. `BASELINE=... CANDIDATE=... ./bench compare` prints both side by side.
- Compare work-normalized figures, such as CPU per delivered packet or chunk, not CPU percentage alone.
- Keep a change only if the improvement is repeatable (`TRIALS=3`), or if it is needed for correctness.
- Run `./bench terrain` before and after touching terrain or chunk encoding.

## Generated code

`crates/sculk/src/vanilla.rs` and `crates/sculk/src/vanilla_items.rs` are generated from Mojang's data-generator reports by the scripts in `scripts/`. Don't edit them by hand. Change the generator and regenerate instead (see the README).

## Scope

Don't add plugin APIs, mobs, redstone or other large vanilla systems without first discussing a change to the project scope in [FEATURES.md](FEATURES.md).
