# Making a Game Controllable

Use this checklist when adapting a Bevy game to `bevy_agent_control`.

1. Add `AgentControlPlugins::deterministic()` for headless control, or `AgentControlPlugins::visual_debug()` for visual control.
2. Move authoritative gameplay into `AgentTick`.
3. Convert keyboard/gamepad/network input into domain `AgentAction` values.
4. Read `CurrentInputFrame` in simulation systems.
5. Read `SimClock` for deterministic timing.
6. Add `SnapshotEntity` and `StableEntityId` to gameplay entities.
7. Register gameplay components/resources with `SnapshotAppExt` or `register_snapshot_components!` / `register_snapshot_resources!`.
8. Add an observation extractor that returns compact symbolic state.
9. Add a checksum extractor over gameplay state using `StableHasher` or another deterministic hash path.
10. Optionally register a visual capture renderer for agent-readable PNG screenshots.
11. Keep rendering, UI, audio, and debug overlays out of authoritative simulation state.

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
```

Games that tunnel domain-specific commands through `AgentAction::Custom` can register JSON schemas with `AgentControlAppExt::register_custom_action_schema`; those schemas appear in `agent.action_space` and `agent.schema`.

## Visual Capture

Use `VisualCaptureAppExt::insert_visual_capture_renderer` when a game can cheaply draw a debugging view from gameplay state. This works in headless runs and powers `agent.visual.capture`.

Visual Bevy apps can also enable `bevy_agent_runner/visual` and fall back to Bevy primary-window screenshots. Either way, visual capture should read gameplay state and write a PNG; it should not change simulation state or checksums.

## Snapshot Rule

Only register gameplay state you can safely destroy and recreate. Do not snapshot windows, GPU state, audio handles, sockets, or asset-server internals.
