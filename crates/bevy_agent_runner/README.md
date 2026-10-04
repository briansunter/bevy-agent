# bevy_agent_runner

Step, inspect, snapshot, restore, and branch a Bevy game through a Rust environment API.

Part of [bevy-agent](https://github.com/briansunter/bevy-agent). Requires **Bevy 0.18.1** and **Rust 1.91+**. Version **0.0.4** is experimental; pin all companion crates to the same exact version.

```toml
[dependencies]
bevy_agent_core = "=0.0.4"
bevy_agent_runner = "=0.0.4"
bevy_agent_snapshot = "=0.0.4"
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

## Verify your environment

The counter also includes two runnable tests:

```sh
cargo test -p bevy_agent_runner --example counter --locked
```

Run this from the repository or unpacked crate. It checks gameplay assertions, repeated seeded resets, snapshot restoration, and replay reconstruction. The [testing guide](https://briansunter.github.io/bevy-agent/guides/testing.html) explains how to adapt these checks to your game.

After each step, inspect `done` and `truncated` before continuing. Read `observation` to choose the next action, and use `checksum` to compare equivalent transitions. Snapshots contain registered authoritative state; they do not clone rendering or transport resources.

## Next steps

- [Getting started](https://briansunter.github.io/bevy-agent/getting-started.html)
- [Game integration guide](https://briansunter.github.io/bevy-agent/controllable-game.html)
- [API reference](https://docs.rs/bevy_agent_runner)

Licensed under **MIT OR Apache-2.0**, at your option.
