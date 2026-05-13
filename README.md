# bevy_agent_control

A Bevy 0.18.1 workspace for driving a game as a deterministic simulation that an AI agent can step, inspect, snapshot, restore, replay, and branch.

The implementation is split into small crates:

- `bevy_agent_core`: schedules, `SimClock`, domain actions, input frames, observations, rewards, terminal state, checksums.
- `bevy_agent_runner`: owns `App`, provides `AgentControlPlugins`, and exposes `reset`, `step`, `step_many`, `fast_forward`, `snapshot`, `restore`, `restore_tick`, `branch`, and visual capture hooks.
- `bevy_agent_snapshot`: tier-1 gameplay snapshots with registered resources/components and `StableEntityId`.
- `bevy_agent_replay`: action logs, checkpoint indexes, and timeline branches.
- `bevy_agent_remote`: local JSON-RPC bridge with capability and session-token checks.
- `sample_platformer`: a headless Bevy platformer wired into `AgentTick`.

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
obs = env.reset(seed=42)
step = env.step({"type": "Move", "x": 1.0, "y": 0.0})
snapshot = env.snapshot()
env.restore(snapshot["snapshot_id"])
capture = env.capture(output_dir="screenshots", label="after_restore")
```

Run the tests:

```sh
cargo test --workspace
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

`agent.visual.capture` returns a PNG path plus tick/frame/size metadata. Games can register a software capture renderer for headless runs, as the sample platformer does, or use Bevy primary-window screenshots in `visual` builds.

Start the sample with visual/render features when you want render plugins and primary-window screenshot support compiled in:

```sh
cargo run -p sample_platformer --features visual --example remote_http_visual -- 127.0.0.1:4000
```

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

The bridge currently provides an in-process/stdio JSON-RPC handler. It is deliberately transport-light so it can be embedded into tests, CLIs, or a Bevy Remote Protocol transport without changing the simulation API.

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

- run in `AgentTick`, not ordinary frame `Update`;
- read `SimClock`, not wall-clock `Time`;
- consume `CurrentInputFrame<AgentAction>`, not keyboard/mouse state directly;
- use stable IDs for semantic entity identity;
- register all gameplay state with `SnapshotAppExt` or the snapshot registration macros;
- use `StableHasher` or an equivalent deterministic checksum path for replay validation;
- keep rendering/UI systems read-only with respect to authoritative simulation state.

The sample platformer follows this contract: movement, gravity, collision, coin pickup, reward, terminal checks, observation extraction, snapshots, replay, and branches all run headlessly under explicit agent control.
