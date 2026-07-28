# bevy_agent_control

A Bevy 0.18.1 workspace for driving a game as a deterministic simulation that an AI agent can step, inspect, snapshot, restore, replay, and branch.

The workspace is organized around small crates with one responsibility each:

- `bevy_agent_core`: schedules, `SimClock`, domain actions, input frames, observations, rewards, terminal state, checksums.
- `bevy_agent_runner`: owns `App`, provides `AgentControlPlugins`, and exposes `reset`, `step`, `step_many`, `fast_forward`, `snapshot`, `restore`, `restore_tick`, `branch`, and visual capture hooks.
- `bevy_agent_snapshot`: tier-1 gameplay snapshots with registered resources/components and `StableEntityId`.
- `bevy_agent_replay`: action logs, checkpoint indexes, and timeline branches.
- `bevy_agent_remote`: local JSON-RPC bridge with capability and session-token checks.
- `sample_platformer`: a headless Bevy platformer wired into `AgentTick`.

## Requirements

- Rust 1.91 or newer
- Cargo
- Python 3.10 or newer only for the optional stdlib client in `python/`

Run the commands below from the repository root. The HTTP server examples are long-running: leave the server running in one terminal and use `agentctl`, `curl`, or Python from another. `agentctl` defaults to `http://127.0.0.1:4000/rpc`; use `--url` when the server is elsewhere.

## Repository Layout

- `crates/`: reusable runtime crates, the `agentctl` CLI, and the sample platformer.
- `python/`: an optional stdlib-only HTTP client.
- `docs/`: user, integration, and publishing guides; start with [`docs/README.md`](docs/README.md).
- `skills/`: Codex skills and their protocol/integration references.
- `Cargo.toml` and `Cargo.lock`: workspace metadata and the locked dependency graph.

## Documentation

- [`docs/README.md`](docs/README.md): documentation index.
- [`docs/codex-interaction.md`](docs/codex-interaction.md): step, capture, snapshot, restore, branch, replay, and security commands.
- [`docs/controllable-game.md`](docs/controllable-game.md): integration checklist for a Bevy game.
- [`docs/publishing.md`](docs/publishing.md): package and publish checks.
- [`skills/control-bevy-agent-game/SKILL.md`](skills/control-bevy-agent-game/SKILL.md): agent-facing control workflow.
- [`skills/integrate-bevy-agent-control/SKILL.md`](skills/integrate-bevy-agent-control/SKILL.md): agent-facing integration workflow.

## Quick Start

```rust
use bevy_agent_core::AgentAction;
use bevy_agent_runner::{AgentApp, AgentEnvironment, ResetOptions};

let mut env = AgentApp::new(sample_platformer::build_headless_app);
let obs0 = env.reset(ResetOptions::default())?;
let step1 = env.step(AgentAction::Move { x: 1.0, y: 0.0 })?;
let snapshot = env.snapshot()?;
env.step(AgentAction::Jump)?;
env.restore(snapshot.snapshot_id)?;
let alternate = env.step(AgentAction::Noop)?;
# anyhow::Ok(())
```

Install the standard Bevy agent stack with a plugin group:

```rust
use bevy_agent_runner::AgentControlPlugins;

app.add_plugins(AgentControlPlugins::deterministic())
    .add_plugins(GamePlugin);
```

Run the sample agent:

```sh
cargo run -p sample_platformer --example agent_play
```

Start a local HTTP/WebSocket remote server:

```sh
cargo run -p sample_platformer --example remote_http -- 127.0.0.1:4000
```

The server exposes `POST /rpc`, `GET /health`, and `GET /ws`. For a command-by-command interaction guide, see [`docs/codex-interaction.md`](docs/codex-interaction.md).

Drive it with the CLI:

```sh
cargo run -p agentctl -- info
cargo run -p agentctl -- reset --seed 42
cargo run -p agentctl -- step '{"type":"Move","x":1.0,"y":0.0}'
cargo run -p agentctl -- capture --out-dir screenshots --label after_step
cargo run -p agentctl -- snapshot
cargo run -p agentctl -- replay-export replay.json
```

Or drive it from Python:

```python
from bevy_agent_client import AgentClient

env = AgentClient("http://127.0.0.1:4000/rpc")
reset = env.reset(seed=42)
obs = reset["observation"]
step = env.step({"type": "Move", "x": 1.0, "y": 0.0})
snapshot = env.snapshot()
env.restore(snapshot["snapshot_id"])
capture = env.capture(output_dir="screenshots", label="after_restore")
```

Run the tests:

```sh
cargo fmt --all -- --check
cargo test --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Check optional visual/render dependencies still compile:

```sh
cargo check --workspace --all-features
```

## Low-Speed Agent Play With Screenshots

Agents can play at their own pace by alternating structured steps with on-demand captures:

```sh
cargo run -p sample_platformer --example remote_http -- 127.0.0.1:4000
cargo run -p agentctl -- reset --seed 42
cargo run -p agentctl -- step '{"type":"Move","x":1.0,"y":0.0}'
cargo run -p agentctl -- capture --out-dir screenshots --label tick_1
```

`agent.visual.capture` returns a PNG path plus tick/frame/size metadata. Set `source` to `software`, `primary_window`, or `auto`. Games can register a software capture renderer for headless runs, as the sample platformer does.

Start the sample with visual/render features when you want render plugins and primary-window screenshot support compiled in:

```sh
cargo run -p sample_platformer --features visual --example remote_http_visual -- 127.0.0.1:4000
```

The visual example installs `BevyRemoteControlPlugin` in the normal Bevy app and
then calls `app.run()`. Network I/O stays on a background thread while Bevy
state changes and primary-window capture are handled on the main thread.

## JSON-RPC Example

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "agent.step",
  "params": {
    "action": { "type": "Move", "x": 1.0, "y": 0.0 },
    "observation_mode": "Hybrid"
  }
}
```

The JSON-RPC bridge is deliberately transport-light so it can be embedded into tests, CLIs, stdio, or the HTTP/WebSocket server without changing the simulation API.

The sample also includes:

- `remote_stdio`: JSON-RPC over newline-delimited stdin/stdout.
- `remote_http`: HTTP `POST /rpc`, `GET /health`, and WebSocket JSON-RPC at `GET /ws`.
- `remote_http_visual`: visual-feature remote with render plugins and primary-window screenshot support.
- `agentctl`: a small HTTP client for common commands.
- `python/bevy_agent_client.py`: a stdlib Python wrapper.
- `docs/controllable-game.md`: integration checklist for games.
- `docs/codex-interaction.md`: command examples for agents.
- `docs/publishing.md`: crate publishing checklist.

## Determinism Contract

Gameplay systems that need replayable behavior should:

- enqueue autonomous policy decisions from `AgentDecision`, which runs exactly once before each controlled tick;
- run in `AgentTick`, not ordinary frame `Update`;
- read `SimClock`, not wall-clock `Time`;
- consume `CurrentInputFrame<AgentAction>`, not keyboard/mouse state directly;
- use stable IDs for semantic entity identity;
- register all gameplay state with `SnapshotAppExt` or the snapshot registration macros;
- use `StableHasher` or an equivalent deterministic checksum path for replay validation;
- keep rendering/UI systems read-only with respect to authoritative simulation state.

Call `set_environment_metadata`, `set_supported_actions`, and
`set_observation_schema` during integration so remote discovery describes the
game rather than the library defaults. Replay exports are portable bundles that
include all referenced initial and checkpoint snapshots.

The sample platformer follows this contract: movement, gravity, collision, coin pickup, reward, terminal checks, observation extraction, snapshots, replay, and branches all run headlessly under explicit agent control.

Commands that capture images or export replays write local runtime output such as `screenshots/` and `replay.json`. Those paths are ignored by Git; keep any intentional fixtures under an explicitly named test or example directory.
