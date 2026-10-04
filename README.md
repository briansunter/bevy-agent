# bevy-agent

**Deterministic control for Bevy games and simulations.** Give an AI agent, test harness, or Rust client an explicit loop: reset the world, apply an action, advance a tick, and inspect the result. Save snapshots, replay inputs, and branch from earlier states to compare decisions.

Built for **Bevy 0.18.1** and **Rust 1.91+**, with headless defaults and optional rendering. HTTP, WebSocket, stdio, CLI, and Python clients share the same JSON-RPC control surface.

> **Version 0.0.1** is the initial experimental release. Cargo version numbers, snapshot/replay format versions, and game schema versions are separate contracts. Pin companion crates to the same exact release; breaking changes may occur before 1.0.

## Try it

Clone the repository and run a complete environment:

```sh
git clone https://github.com/briansunter/bevy-agent.git
cd bevy-agent
cargo run -p bevy_agent_runner --example counter
cargo run -p sample_platformer --example agent_play
```

The [counter example](crates/bevy_agent_runner/examples/counter.rs) is a small, complete integration using only runtime crates. The [platformer](crates/sample_platformer) adds movement, collisions, coins, rewards, terminal conditions, and screenshots.

## Add it to your game

Add the runtime crates you need:

```toml
[dependencies]
bevy = { version = "0.18.1", default-features = false, features = ["std"] }
bevy_agent_core = "=0.0.1"
bevy_agent_runner = "=0.0.1"
bevy_agent_snapshot = "=0.0.1"
# Optional JSON-RPC server:
bevy_agent_remote = "=0.0.1"
```

For local development, use these crates as path dependencies or run the repository examples. The integration guide explains the additional dependencies needed for serializable game state.

Install `AgentControlPlugins::default()`, register authoritative gameplay state, declare the action/observation contract, and supply observation and checksum extractors. Then control the integrated app:

```rust
use bevy_agent_core::AgentAction;
use bevy_agent_runner::{AgentApp, AgentEnvironment, ResetOptions};

// In this repository, the sample supplies a complete app builder.
let mut env = AgentApp::new(sample_platformer::build_headless_app)?;
env.reset(ResetOptions::default())?;
let saved = env.snapshot()?;
let moved = env.step(AgentAction::Move { x: 1.0, y: 0.0 })?;
env.restore(saved.snapshot_id)?;
let alternate = env.step(AgentAction::Jump)?;
```

See [Getting started](docs/getting-started.md) for the complete setup and [Making a game controllable](docs/controllable-game.md) for integration details. The sample game stays in the repository and is not a crates.io dependency.

## Choose your crates

| Package | Purpose | API |
| --- | --- | --- |
| [`bevy_agent_core`](crates/bevy_agent_core) | Schedules, actions, clock, observations, rewards, identities, checksums | [docs.rs](https://docs.rs/bevy_agent_core) |
| [`bevy_agent_snapshot`](crates/bevy_agent_snapshot) | Registered gameplay snapshots, checked restore, retention | [docs.rs](https://docs.rs/bevy_agent_snapshot) |
| [`bevy_agent_replay`](crates/bevy_agent_replay) | Input logs, checkpoint indexes, timeline topology | [docs.rs](https://docs.rs/bevy_agent_replay) |
| [`bevy_agent_runner`](crates/bevy_agent_runner) | `AgentApp`, plugin composition, stepping, restore, branches, capture | [docs.rs](https://docs.rs/bevy_agent_runner) |
| [`bevy_agent_remote`](crates/bevy_agent_remote) | JSON-RPC over HTTP, WebSocket, and stdio | [docs.rs](https://docs.rs/bevy_agent_remote) |
| [`bevy_agent_cli`](crates/agentctl) | Installs the `agentctl` command-line client | CLI |

There is no umbrella Cargo package: depend directly on the crates you use. `AgentControlPlugins` composes core, snapshots, and replay. The runner and remote `visual` features enable Bevy render/window capture support; headless software capture uses a game-supplied renderer.

## Drive it remotely

Run a local server from the repository:

```sh
cargo run -p sample_platformer --example remote_http -- 127.0.0.1:4000 --artifact-dir ./artifacts
```

Leave it running and use another terminal:

```sh
cargo run -p bevy_agent_cli --bin agentctl -- info
cargo run -p bevy_agent_cli --bin agentctl -- reset --seed 42
cargo run -p bevy_agent_cli --bin agentctl -- step '{"type":"Move","x":1.0,"y":0.0}'
cargo run -p bevy_agent_cli --bin agentctl -- capture --out-dir screenshots --label after_step
cargo run -p bevy_agent_cli --bin agentctl -- snapshot
cargo run -p bevy_agent_cli --bin agentctl -- replay-export replay.json
```

Install with `cargo install bevy_agent_cli --version 0.0.1 --locked`, then run `agentctl` directly. The default endpoint is `http://127.0.0.1:4000/rpc`; pass `--url` for another server and `--token` or `AGENT_TOKEN` for authentication.

The example grants filesystem access under `./artifacts`: captures land in `artifacts/screenshots/` and the replay in `artifacts/replay.json`. The library default disables filesystem access; replay bundles can also be transferred inline as JSON. Bind to loopback for local use and configure authentication before exposing a listener beyond it.

For a rendered window and primary-window screenshots:

```sh
cargo run -p sample_platformer --features visual --example remote_http_visual -- 127.0.0.1:4000 --artifact-dir ./artifacts
```

For Python, use the optional standard-library client from the checkout (Python 3.10+):

```sh
PYTHONPATH=python python3
```

```python
from bevy_agent_client import AgentClient

client = AgentClient("http://127.0.0.1:4000/rpc")
initial = client.reset(seed=42)
step = client.step({"type": "Move", "x": 1.0, "y": 0.0})
saved = client.snapshot()
client.restore(saved["snapshot_id"])
```

The server exposes `POST /rpc`, `GET /health`, and `GET /ws`. The [interaction guide](docs/codex-interaction.md) covers discovery, captures, retries, operation status, replay transfers, and stdio.

## Determinism and compatibility

- Run authoritative gameplay in `AgentTick`; use `AgentDecision` for a policy that runs once before each controlled tick.
- Consume `CurrentInputFrame<AgentAction>` and read `SimClock`; use seeded randomness and explicit system ordering.
- Register all authoritative state, including hidden state, with stable snapshot type IDs and schema versions. Give gameplay entities stable identities.
- Declare supported actions, observation modes, and any JSON schemas. Install both observation and checksum extractors; `AgentApp::new` validates integration.
- Keep rendering, UI, audio, and network state outside authoritative gameplay.

Determinism is an integration contract: this library does not make arbitrary frame-driven gameplay deterministic or guarantee identical floating-point results across platforms. Checksums detect consistency errors and do not authenticate imported artifacts.

Snapshot/replay artifacts currently use **format version 3**. Older artifacts are rejected and must be regenerated. Snapshot and replay owners each default to a configurable **64 MiB** retention budget. Batches stop at terminal state. A mutation failure reports its committed tick and recovery requirement; a faulted world needs a successful reset.

HTTP/WebSocket mutations support request `retry_key` deduplication and retained operation status after timeouts. Keys belong to one server instance and expire with retained results. Stdio does not have this ledger.

## Documentation and development

- [Documentation index](docs/README.md)
- [Getting started](docs/getting-started.md)
- [Game integration](docs/controllable-game.md)
- [Protocol and clients](docs/codex-interaction.md)
- [Architecture and invariants](docs/architecture.md)
- [Publishing guide](docs/publishing.md) and [changelog](CHANGELOG.md)
- [Agent control skill](skills/control-bevy-agent-game/SKILL.md) and [integration skill](skills/integrate-bevy-agent-control/SKILL.md)

```sh
cargo fmt --all -- --check
cargo test --workspace --all-targets --locked
cargo test --workspace --doc --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps --locked
python3 -m unittest discover -s python/tests
```

CI is manually dispatched and also covers all-feature tests, real transports, fuzzing, and rendered capture under Xvfb/Mesa. See the publishing guide for package validation and host-specific build storage requirements.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option. Both license texts are included in every release package.
