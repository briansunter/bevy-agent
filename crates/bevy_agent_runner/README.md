# bevy_agent_runner

Step, inspect, snapshot, restore, and branch a Bevy game through a Rust environment API.

Part of [bevy-agent](https://github.com/briansunter/bevy-agent). Requires **Bevy 0.18.1** and **Rust 1.91+**. Version **0.0.1** is experimental; pin all companion crates to the same exact version.

```toml
[dependencies]
bevy_agent_core = "=0.0.1"
bevy_agent_runner = "=0.0.1"
bevy_agent_snapshot = "=0.0.1"
```

## Control an integrated game

The app builder must install `AgentControlPlugins`, register gameplay state, and configure metadata, supported actions/modes, observations, and checksums. Construction validates this contract.

```rust
use bevy::prelude::App;
use bevy_agent_core::AgentAction;
use bevy_agent_runner::{AgentApp, AgentEnvironment, ResetOptions};

fn drive_game(build_game: impl FnOnce() -> App) -> anyhow::Result<()> {
    let mut env = AgentApp::new(build_game)?;
    env.reset(ResetOptions::default())?;
    let snapshot = env.snapshot()?;
    let first = env.step(AgentAction::Noop)?;
    env.restore(snapshot.snapshot_id)?;
    let repeated = env.step(AgentAction::Noop)?;
    assert_eq!(first.checksum, repeated.checksum);
    Ok(())
}
```

The packaged `counter` example contains a complete, headless app and exercises reset, stepping, snapshots, and replay:

```sh
cargo run -p bevy_agent_runner --example counter
```

Run that command from the repository or this crate's unpacked source. [Read the example](https://github.com/briansunter/bevy-agent/blob/master/crates/bevy_agent_runner/examples/counter.rs) to build your own environment.

## Features and guarantees

- Default features are headless; `visual` enables Bevy render/window capture support.
- `AgentControlPlugins::default()` installs core, snapshots, and replay. Configure retention through `with_snapshot_policy`.
- `step_many` and fast-forward stop at terminal state.
- Mutation failures report the committed tick and recovery state. A faulted environment requires a successful reset.
- Snapshot and replay owners each default to a configurable 64 MiB retention budget.
- Checksums detect consistency errors; they do not authenticate artifacts or guarantee identical behavior across platforms.

## Next steps

- [Getting started](https://github.com/briansunter/bevy-agent/blob/master/docs/getting-started.md)
- [Game integration guide](https://github.com/briansunter/bevy-agent/blob/master/docs/controllable-game.md)
- [API reference](https://docs.rs/bevy_agent_runner)

Licensed under **MIT OR Apache-2.0**, at your option.
