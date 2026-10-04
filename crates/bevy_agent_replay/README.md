# bevy_agent_replay

Action logs, checkpoint indexes, and branching timelines for Bevy agent simulations.

Part of [bevy-agent](https://github.com/briansunter/bevy-agent). Requires **Bevy 0.18.1** and **Rust 1.91+**. Version **0.0.1** is experimental; pin all companion crates to the same exact version.

```toml
[dependencies]
bevy_agent_replay = "=0.0.1"
```

## Replay ownership

`AgentReplayPlugin` records completed controlled ticks. `ReplayRecorder` owns logs; `Timeline` owns branch topology. Their checked APIs enforce history bounds and reference consistency.

This crate owns recording and topology. `bevy_agent_runner` coordinates world restoration, replay reconstruction, branches, and portable `ReplayBundle` export/import with snapshots. Use the runner's `restore_tick`, `branch`, and `load_replay_bundle` APIs for complete environment operations.

Replay correctness depends on deterministic gameplay, a complete snapshot registry, and a checksum extractor that includes all authoritative state. Current replay manifests use format version 3; older artifacts are rejected.

## Next steps

- [Getting started](https://github.com/briansunter/bevy-agent/blob/master/docs/getting-started.md)
- [Replay portability](https://github.com/briansunter/bevy-agent/blob/master/docs/controllable-game.md#replay-portability)
- [API reference](https://docs.rs/bevy_agent_replay)

Licensed under **MIT OR Apache-2.0**, at your option.
