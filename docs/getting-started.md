# Getting started

bevy-agent turns a Bevy app into a controlled environment. The game defines what an action means, what state an agent can observe, and which gameplay state belongs in a snapshot. The runtime owns controlled ticks and coordinates history.

The initial release is **0.0.1**, targeting **Bevy 0.18.1** and **Rust 1.91+**. It is experimental and has no 1.0 compatibility guarantee. The crates.io installation commands below apply after publication.

## Start with a complete example

```sh
git clone https://github.com/briansunter/bevy-agent.git
cd bevy-agent
cargo run -p bevy_agent_runner --example counter
```

The packaged [counter example](../crates/bevy_agent_runner/examples/counter.rs) creates a complete headless environment. It registers a serializable resource, defines a domain observation and its JSON schema, installs a checksum extractor, resets the counter, and increments it on controlled ticks. It verifies that restoring a snapshot and repeating an action produces the same checksum, then restores an earlier tick.

Use this example as the smallest integration. For gameplay entities, movement, collisions, rewards, terminal checks, and capture, study [`PlatformerPlugin`](../crates/sample_platformer/src/lib.rs) and run:

```sh
cargo run -p sample_platformer --example agent_play
```

## Depend on the runtime

For a headless application, use:

```toml
[dependencies]
bevy = { version = "0.18.1", default-features = false, features = ["std"] }
bevy_agent_core = "=0.0.1"
bevy_agent_runner = "=0.0.1"
bevy_agent_snapshot = "=0.0.1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
anyhow = "1"
```

Copy the counter example into `src/main.rs` to run a standalone application with this manifest. Before publication, use a `path` to each corresponding `crates/` directory. Keep all companion versions synchronized.

Add `bevy_agent_remote = "=0.0.1"` to expose JSON-RPC. Add `bevy_agent_replay = "=0.0.1"` only when your application directly uses its recording or timeline types. Enable the runner/remote `visual` feature for Bevy primary-window capture; choose your game's rendering plugins separately.

## Integrate your own game

1. Install Bevy plugins and `AgentControlPlugins::default()`.
2. Declare environment metadata, supported actions, and supported observation modes.
3. Install observation and checksum extractors; validate the complete serialized observation with a schema when needed.
4. Register gameplay components/resources with `SnapshotAppExt`. Use stable type IDs, schema versions, and entity IDs.
5. Add a reset system to `AgentReset` in `AgentResetSet::Game`.
6. Move gameplay systems into `AgentTick` with explicit ordering. Read `SimClock` and `CurrentInputFrame`.
7. Construct `AgentApp`, call `reset`, then `step` with supported actions.

Constructing `AgentApp` checks the integration before gameplay begins. Handle returned errors; unsupported input or invalid observations must not be treated as successful ticks. Check `done` and `truncated` before selecting the next action.

[Making a game controllable](controllable-game.md) covers schema registration, reset policy, snapshot coverage, ordering, visual capture, and replay portability.

## Connect a client

Start the platformer HTTP example in one terminal:

```sh
cargo run -p sample_platformer --example remote_http -- 127.0.0.1:4000 --artifact-dir ./artifacts
```

In another terminal:

```sh
cargo run -p bevy_agent_cli --bin agentctl -- info
cargo run -p bevy_agent_cli --bin agentctl -- action-space
cargo run -p bevy_agent_cli --bin agentctl -- reset --seed 42
cargo run -p bevy_agent_cli --bin agentctl -- step '{"type":"Move","x":1.0,"y":0.0}'
```

After publication, `cargo install bevy_agent_cli --version 0.0.1 --locked` installs the same executable as `agentctl`. The package name differs because the crates.io name `agentctl` belongs to another project.

The [interaction guide](codex-interaction.md) has Python, WebSocket, stdio, capture, replay, authentication, and retry examples. Discover a game's actions and observation modes before sending commands; the platformer's contract is an example, not a universal game API.
