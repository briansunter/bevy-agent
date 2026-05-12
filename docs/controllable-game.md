# Making a Game Controllable

Use this checklist when adapting a Bevy game to `bevy_agent_control`.

1. Add `AgentControlPlugin`, `AgentSnapshotPlugin`, and `AgentReplayPlugin`.
2. Move authoritative gameplay into `AgentTick`.
3. Convert keyboard/gamepad/network input into domain `AgentAction` values.
4. Read `CurrentInputFrame` in simulation systems.
5. Read `SimClock` for deterministic timing.
6. Add `SnapshotEntity` and `StableEntityId` to gameplay entities.
7. Register gameplay components/resources with `SnapshotAppExt`.
8. Add an observation extractor that returns compact symbolic state.
9. Add a checksum extractor over gameplay state.
10. Keep rendering, UI, audio, and debug overlays out of authoritative simulation state.

The sample platformer is the reference implementation. It registers its player, platforms, coins, goal, score, episode state, observations, and checksum in `PlatformerPlugin`.

## Minimal Integration Shape

```rust
app.add_plugins(AgentControlPlugin::deterministic())
    .add_plugins(AgentSnapshotPlugin)
    .add_plugins(AgentReplayPlugin)
    .add_plugins(GamePlugin);

app.add_systems(
    AgentTick,
    (player_actions, physics, scoring)
        .chain()
        .in_set(AgentSet::Simulation),
);
```

## Snapshot Rule

Only register gameplay state you can safely destroy and recreate. Do not snapshot windows, GPU state, audio handles, sockets, or asset-server internals.
