# Making a Game Controllable

Use this checklist when adapting a Bevy game to `bevy_agent_control`.

1. Add `AgentControlPlugins::deterministic()` for headless control, or `AgentControlPlugins::visual_debug()` for visual control.
2. Move authoritative gameplay into `AgentTick`.
3. Register environment metadata, supported actions, and any domain observation schema.
4. Convert keyboard/gamepad/network input into domain `AgentAction` values.
5. Read `CurrentInputFrame` in simulation systems.
6. Read `SimClock` for deterministic timing.
7. Add `SnapshotEntity` and `StableEntityId` to gameplay entities.
8. Register gameplay components/resources with `SnapshotAppExt` or `register_snapshot_components!` / `register_snapshot_resources!`.
9. Add an observation extractor that returns compact symbolic or domain state.
10. Add a checksum extractor over gameplay state using `StableHasher` or another deterministic hash path.
11. Optionally register a visual capture renderer for agent-readable PNG screenshots.
12. Keep rendering, UI, audio, and debug overlays out of authoritative simulation state.

The sample platformer is the reference implementation. It registers its player, platforms, coins, goal, score, episode state, observations, and checksum in `PlatformerPlugin`.

## Minimal Integration Shape

```rust
app.add_plugins(AgentControlPlugins::deterministic())
    .add_plugins(GamePlugin);

app.add_systems(
    AgentTick,
    (player_actions, physics, scoring)
        .chain()
        .in_set(AgentSet::Simulation),
);

app.add_systems(AgentDecision, agent_policy);
```

`AgentDecision` is the per-tick policy hook: it runs exactly once immediately
before `AgentPreTick` and should enqueue actions for `clock.tick + 1`.

Use `AgentControlAppExt::set_environment_metadata`,
`set_supported_actions`, and `set_observation_schema` to make discovery
game-specific. Games that tunnel domain commands through
`AgentAction::Custom` can register JSON schemas with
`register_custom_action_schema`.

## Visual Capture

Use `VisualCaptureAppExt::insert_visual_capture_renderer` when a game can cheaply draw a debugging view from gameplay state. This works in headless runs and powers `agent.visual.capture`.

Visual Bevy apps can enable the `visual` features and install
`BevyRemoteControlPlugin` in the normal app before calling `app.run()`.
Select `CaptureSource::PrimaryWindow` to bypass a registered software renderer.
Visual capture should not change simulation state or checksums.

## Replay Portability

`agent.replay.export` writes a `ReplayBundle`, not only action UUID references.
The bundle embeds every initial/checkpoint snapshot needed for a fresh process
to load it and call `restore_tick`.

## Snapshot Rule

Only register gameplay state you can safely destroy and recreate. Do not snapshot windows, GPU state, audio handles, sockets, or asset-server internals.
