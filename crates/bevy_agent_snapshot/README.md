# bevy_agent_snapshot

Gameplay snapshots and checked restoration for agent-controlled Bevy simulations.

Part of [bevy-agent](https://github.com/briansunter/bevy-agent). Requires **Bevy 0.18.1** and **Rust 1.91+**. Version **0.0.1** is experimental; pin all companion crates to the same exact version.

```toml
[dependencies]
bevy_agent_snapshot = "=0.0.1"
```

## Register authoritative state

Implement `SnapshotType` with a stable wire ID and a positive schema version for each serializable gameplay type. Register components and resources through `SnapshotAppExt`; mark required resources with `register_required_snapshot_resource`. Gameplay entities need both `SnapshotEntity` and `StableEntityId`.

Only registered state is captured. Include hidden state that affects future gameplay, such as cooldowns, RNG state, and pending input. Keep windows, GPU state, sockets, audio handles, and asset-server internals outside the snapshot registry.

`SnapshotStore` provides retention, pins, and validated imports. Snapshot format version 3 and per-type schema versions are separate from the Cargo package version. Older artifact formats are rejected; there is no migration layer in this release.

Use the coordinated `AgentApp::snapshot` and `restore` APIs in `bevy_agent_runner` when combining snapshots with replay and branches.

## Next steps

- [Snapshot integration](https://github.com/briansunter/bevy-agent/blob/master/docs/controllable-game.md#stable-snapshot-types)
- [Architecture and invariants](https://github.com/briansunter/bevy-agent/blob/master/docs/architecture.md)
- [API reference](https://docs.rs/bevy_agent_snapshot)

Licensed under **MIT OR Apache-2.0**, at your option.
