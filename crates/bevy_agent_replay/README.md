# bevy_agent_replay

Action logs, checkpoint indexes, and branching timelines for Bevy agent simulations.

Part of [bevy-agent](https://github.com/briansunter/bevy-agent). Requires **Bevy 0.18.1** and **Rust 1.91+**. Version **0.0.4** is experimental; pin all companion crates to the same exact version.

```toml
[dependencies]
bevy_agent_replay = "=0.0.4"
```

## Replay ownership

`AgentReplayPlugin` records completed controlled ticks. `ReplayRecorder` owns logs; `Timeline` owns branch topology. Their checked APIs enforce history bounds and reference consistency.

This crate owns recording and topology. `bevy_agent_runner` coordinates world restoration, replay reconstruction, branches, and portable `ReplayBundle` export/import with snapshots. Use the runner's `restore_tick`, `branch`, and `load_replay_bundle` APIs for complete environment operations.

Replay correctness depends on deterministic gameplay, a complete snapshot registry, and a checksum extractor that includes all authoritative state. Current replay manifests use format version 3; older artifacts are rejected.

## Choose the operation you need

| Goal | Runner operation |
| --- | --- |
| Return to a saved state | `snapshot` and `restore` |
| Reconstruct a recorded tick | `restore_tick` |
| Compare a different future | `branch` |
| Move recorded history to another process | Export a replay bundle, then `load_replay_bundle` |

A portable bundle must carry its referenced checkpoints; an input list alone cannot reconstruct arbitrary world state. History is bounded by retention policy. See the [worked history guide](https://briansunter.github.io/bevy-agent/guides/snapshots-replay.html) and [testing guide](https://briansunter.github.io/bevy-agent/guides/testing.html).

## Next steps

[Read the guide](https://briansunter.github.io/bevy-agent/guides/snapshots-replay.html) for an organized walkthrough, examples, and troubleshooting.

- [Getting started](https://briansunter.github.io/bevy-agent/getting-started.html)
- [Replay portability](https://briansunter.github.io/bevy-agent/controllable-game.html#replay-portability)
- [API reference](https://docs.rs/bevy_agent_replay)

Licensed under **MIT OR Apache-2.0**, at your option.
