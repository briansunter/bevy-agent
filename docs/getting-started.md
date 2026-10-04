# Build your first environment

Start with a counter, not a full game. Each action advances one simulation tick. You will save a snapshot, repeat a step, and verify that replay returns to the same state.

**You need:** Rust 1.91 or newer. These examples use Bevy 0.18.1 and the published experimental release 0.0.3.

::: tip Prefer to explore first?
[Run the repository example](#run-the-repository-example) without copying any code. To control a running game from another process, follow [HTTP and the CLI](./guides/remote-control.md).
:::

## 1. Create a small Rust project

```sh
cargo new my-agent-game
cd my-agent-game
```

Replace `Cargo.toml` with:

```toml
[package]
name = "my-agent-game"
version = "0.1.0"
edition = "2024"
rust-version = "1.91"

[dependencies]
bevy = { version = "=0.18.1", default-features = false, features = ["std", "bevy_log", "bevy_state", "serialize"] }
bevy_agent_core = "=0.0.3"
bevy_agent_runner = "=0.0.3"
bevy_agent_snapshot = "=0.0.3"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
anyhow = "1"
```

There is no umbrella `bevy_agent` package. The runner composes core, snapshots, and replay; the other dependencies expose types you use directly. Keep companion crate versions pinned to the same exact release.

## 2. Add the complete environment

Replace `src/main.rs` with this example. It is the same source shipped with `bevy_agent_runner`, so the guide and runnable example stay together.

<<< ../crates/bevy_agent_runner/examples/counter.rs

The setup has four responsibilities:

- **State:** `Counter` is the authoritative gameplay resource. Its stable `SnapshotType` identity makes it serializable and restorable.
- **Input:** the environment declares `Noop` as a supported action. This simple game increments the counter on every controlled tick.
- **Output:** the observation exposes the counter value; the checksum covers both core state and the counter.
- **Schedules:** reset initializes the episode, and `AgentTick` changes gameplay. Ordinary render frames do not advance this counter.

## 3. Run it

```sh
cargo run
```

Expected output:

```text
Snapshot restore and replay matched; counter is back at tick 1.
```

The assertions prove that restoring a snapshot and repeating the same action produces the same checksum, then `restore_tick(1)` reconstructs the earlier state. A checksum is a consistency check over your declared state; it does not prove arbitrary games deterministic across platforms.

## Run the repository example

If you want the example without creating a new project:

```sh
git clone https://github.com/briansunter/bevy-agent.git
cd bevy-agent
cargo run -p bevy_agent_runner --example counter --locked
```

The repository also contains a complete platformer with movement, collisions, coins, rewards, terminal conditions, and capture:

```sh
cargo run -p sample_platformer --example agent_play --locked
```

`sample_platformer` stays in the repository. It is not a crates.io package.

## Where to go next

- [Understand actions, ticks, and observations](./concepts.md) before adapting frame-driven gameplay.
- [Integrate your own game](./controllable-game.md) to register state, define schemas, and order simulation systems.
- [Connect the CLI](./guides/remote-control.md) or [Python](./guides/python.md) to a running environment.
- [Choose your crates](./reference/crates.md) when you need remote control, rendering, or replay types.

::: info Personal Mac mini builds
On the pinned personal Mac mini, run `build-storage-check` before native builds and use `cargo-storage` from the canonical worktree, as described in the [contributing guide](./reference/contributing.md#personal-mac-mini). Other hosts use ordinary Cargo commands.
:::
