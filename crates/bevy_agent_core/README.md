# bevy_agent_core

Deterministic simulation primitives for agent-controlled Bevy games.

Part of [bevy-agent](https://github.com/briansunter/bevy-agent). Requires **Bevy 0.18.1** and **Rust 1.91+**. Version **0.0.1** is experimental; pin all companion crates to the same exact version.

```toml
[dependencies]
bevy_agent_core = "=0.0.1"
```

## What this crate provides

- `AgentControlPlugin` and the reset, decision, tick, and finalization schedules.
- `AgentAction`, `AgentActionQueue`, and `CurrentInputFrame` for explicit input.
- `SimClock` and `DeterministicRng` for simulation timing and seeded randomness.
- Observation, reward, terminal-state, stable-identity, and checksum types.
- `AgentControlAppExt` for metadata, action catalogs, schemas, and extractors.

Run authoritative gameplay in `AgentTick`, consume domain actions, and read `SimClock` instead of frame time. A game must declare its supported actions and observation modes and install both observation and checksum extractors. Determinism depends on the game's state coverage and system ordering.

For a complete environment, start with `bevy_agent_runner::AgentControlPlugins` and `AgentApp`; the runner composes core, snapshots, and replay.

## Next steps

- [Getting started](https://github.com/briansunter/bevy-agent/blob/master/docs/getting-started.md)
- [Game integration guide](https://github.com/briansunter/bevy-agent/blob/master/docs/controllable-game.md)
- [API reference](https://docs.rs/bevy_agent_core)

Licensed under **MIT OR Apache-2.0**, at your option.
