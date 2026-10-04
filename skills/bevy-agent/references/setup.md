# Setup and first environment

## Select a path

- **New game:** copy the bundled counter starter, verify it, then replace its model and systems incrementally.
- **Existing game:** inspect its Bevy version and features first. Use the integration reference; do not replace its manifest with the starter.
- **Existing server:** install or locate `agentctl`, then use the runtime reference. No Bevy build is needed just to control it.

## Run the standalone starter

Copy `assets/counter/Cargo.toml` and `assets/counter/src/main.rs` from this skill into a new destination, preserving the `src/` layout. Check the destination before copying so existing work is not overwritten. Run from the new project:

```sh
cargo run
cargo test
```

Expected application output:

```text
Snapshot restore and replay matched; counter is back at tick 1.
```

Expected tests: **2 passed; 0 failed**. The first run creates a lockfile. Keep it for repeatable application builds and use `--locked` afterward. The starter has its own `[workspace]`; remove that section and add it as a member only when intentionally integrating with an existing workspace.

The starter needs no GPU, server, API key, or model. It demonstrates:

| Piece | Responsibility |
| --- | --- |
| `Counter` and its `SnapshotType` implementation | Serializable state with a stable wire identity |
| `build_app` | Plugins, catalogs, metadata, schema, state registration, schedules |
| `reset_counter` | Episode initialization |
| `increment_counter` | Gameplay in a controlled tick |
| `checksum` | Core checksum plus the game's authoritative counter |
| `main` and `tests` | Reset, step, snapshot, restore, and history assertions |

`Noop` still advances a tick. The counter increments because of its simulation system, not because `Noop` has built-in increment semantics.

## Choose dependencies

| Need | Direct dependency |
| --- | --- |
| Actions, schedules, clock, metadata, observations | `bevy_agent_core = "=0.0.4"` |
| Snapshot registration and type identities | `bevy_agent_snapshot = "=0.0.4"` |
| Composed plugins and `AgentApp` | `bevy_agent_runner = "=0.0.4"` |
| Replay types used directly by your code | `bevy_agent_replay = "=0.0.4"` |
| HTTP, WebSocket, or stdio server | `bevy_agent_remote = "=0.0.4"` |

The starter manifest includes Bevy and serialization dependencies. For checkout/path dependencies, keep the same version of all companion crates. Do not patch one crate to a different release while leaving the others in the registry.

## Run the repository sample

If a checkout already exists, reuse it. Otherwise:

```sh
git clone https://github.com/briansunter/bevy-agent.git
cd bevy-agent
```

Inspect the checked-out version before running these commands; the repository may be newer than this skill's baseline. For exactly this baseline, use the `v0.0.4` tag in a fresh clone or separate checkout, preserving existing local work.

```sh
cargo run -p bevy_agent_runner --example counter --locked
cargo test -p bevy_agent_runner --example counter --locked
cargo run -p sample_platformer --example agent_play --locked
```

The platformer supplies real movement, collisions, rewards, and terminal behavior. Its `agent_play` example prints initial state and a terminal response if the script ends the episode.

## Install the remote client

```sh
cargo install bevy_agent_cli --version 0.0.4 --locked
agentctl --version
```

Expected: `agentctl 0.0.4`. The package named `agentctl` on crates.io is unrelated. An existing compatible CLI may be reused; inspect `--help` and discovery responses if versions differ.

To work from a checkout without installing:

```sh
cargo run -p bevy_agent_cli --bin agentctl --locked -- info
```

This still needs a running server. Continue with [Runtime](runtime.md) for the server/client workflow or [Integration](integration.md) for your own game.
