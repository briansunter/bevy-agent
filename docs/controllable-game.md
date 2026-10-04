# Making a Game Controllable

Use this checklist when adapting a Bevy game to `bevy-agent`.

Start from the [complete counter](./getting-started.md), then add one piece of your game at a time. Aim for a headless app that can reset, accept one action, and return a meaningful observation before adding rendering or networking.

## 1. Decide what belongs to the simulation

Write down every field that can affect a future action. For a platformer that includes position, velocity, collision state, coins, cooldowns, RNG state, and the last movement direction. Treat reset, snapshot registration, and checksums as three views of that same inventory.

| State | Reset policy | Snapshot and checksum? |
| --- | --- | --- |
| Player position, score, cooldown | Initialize for the episode | Yes |
| Seeded gameplay RNG | Initialize from the reset seed | Yes |
| Difficulty or level configuration | Retain or replace deliberately | Yes, if it affects outcomes |
| Window, audio, socket, debug overlay | Manage outside the simulation | No |

The sample platformer's `PlatformerPlugin` connects its serializable model, simulation, observation/checksum contract, and presentation. Use the [example directory](./examples.md) to run it.

## 2. Build a headless app first

Your builder returns a fresh Bevy `App` with control plugins and your game plugin. Register required resources before constructing `AgentApp`. The builder should not call `app.run()`; the runner controls the simulation directly.

Use `MinimalPlugins` for a small headless integration. Choose Bevy rendering plugins separately when you add a window. An existing `Update` system may continue to animate UI, but it must not change authoritative gameplay behind the controller's back.

## 3. Move gameplay into controlled schedules

```rust
app.add_plugins(AgentControlPlugins::default())
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

`AgentPostTick` hooks finish before `AgentFinalize` extracts observations,
records replay input, and captures periodic snapshots. Register simulation
systems in `AgentTick` and keep finalization ordering intact so responses and
snapshots describe the completed state.

## 4. Declare inputs and outputs

Use `AgentControlAppExt::set_environment_metadata`,
`set_supported_actions`, and `set_supported_observation_modes` to declare the
integration contract. Install both extractors before constructing `AgentApp`.
The configured default observation mode must be supported. For example:

```rust
app.set_environment_metadata("my_game", env!("CARGO_PKG_VERSION"), None)
    .set_supported_actions([AgentActionKind::Noop, AgentActionKind::Move])
    .set_supported_observation_modes([
        ObservationMode::PlayerKnowledge,
        ObservationMode::Hybrid,
    ])
    .insert_observation_extractor(game_observation)
    .insert_checksum_extractor(game_checksum);

app.set_observation_schema(game_observation_schema())
    .expect("game observation schema is valid");
```

`set_observation_schema` describes the **full serialized `Observation`**,
including its `kind` tag and nested fields. For `Observation::Domain`, describe
`kind`, `tick`, and `value`, not just `value`. An optional schema is compiled
once during registration and validated against every collected observation.

For `AgentAction::Custom`, declare `AgentActionKind::Custom` and register at
least one matching schema with `register_custom_action_schema`. Core compiles
and caches draft 2020-12 JSON Schema validators. A custom payload must match a
registered schema before it can be scheduled, applied through the controller,
or accepted from replay/snapshot input. Registration rejects malformed schemas
and remote references; discovery exposes the same contract. The game still
owns the meaning of a valid command and its gameplay effects.

## Fallible boundaries

`AgentApp::new`, `from_app`, and `from_running_app` return `Result` after checking
resources, schedules, metadata, catalogs, and extractors. Use `?` in application
startup and `expect` for static plugin configuration errors:

```rust
let mut env = AgentApp::new(build_headless_app)?;
env.reset(ResetOptions::default())?;
env.enqueue_action_at(5, ActionSource::Agent, AgentAction::Noop)?;
```

Direct integrations use `schedule_action(world, tick, source, action)` or
`AgentActionQueue::schedule` with the catalog, clock, control state, and
execution context. Both return a result and validate the action, future tick,
and accepted source before changing the queue. Choose `ControlMode::Hybrid`
when both human and agent sources should be accepted. `run_agent_tick` and
observation collection also return results; propagate failures instead of
reusing an earlier response.

## Stable snapshot types

Register the inventory you identified above. A Rust type name is not a stable file-format identity: choose a game-owned ID and keep it stable across module moves.

Wire identity must survive Rust module moves. Implement `SnapshotType` for each
game type; built-in core types and Bevy `Transform` already have implementations:

```rust
impl SnapshotType for GameScore {
    const TYPE_ID: &'static str = "my_game.score";
    const SCHEMA_VERSION: u32 = 1;
}

register_snapshot_components!(app, StableEntityId, Transform, Player, Velocity)
    .expect("game component snapshot IDs and versions are valid");
app.register_required_snapshot_resource::<GameScore>()
    .expect("score snapshot ID and version are valid")
    .register_required_snapshot_resource::<GameplayRng>()
    .expect("RNG snapshot ID and version are valid");
```

IDs must be nonempty and unique, and versions must be positive. Registration
rejects identity collisions before changing the registry. Increase the type's
version when its serialized contract changes; payload versions must match the
current registry. Requiredness participates in the registry schema hash.

Define reset policy for each authoritative field: reset episode-local values
and retain intentional configuration. Capture and checksum both. Use optional
resource registration only for state whose absence is valid after restore.

## Visual capture

Use `VisualCaptureAppExt::insert_visual_capture_renderer` when a game can cheaply draw a debugging view from gameplay state. This works in headless runs and powers `agent.visual.capture`.

Visual Bevy apps can enable the `visual` features and install
`BevyRemoteControlPlugin` in the normal app before calling `app.run()`.
Select `CaptureSource::PrimaryWindow` to bypass a registered software renderer.
Visual capture should not change simulation state or checksums.

## Replay portability

`agent.replay.export` writes a `ReplayBundle`, not only action UUID references.
The bundle embeds every initial/checkpoint snapshot needed for a fresh process
to load it and call `restore_tick`.

Live `SnapshotRegistry`, `SnapshotStore`, `ReplayRecorder`, and `Timeline`
collections are private. Inspect their getters and use checked APIs for
registration, import, and topology changes. Prefer `AgentApp::snapshot`,
`restore`, `restore_tick`, `branch`, and `load_replay_bundle` for coordinated
world/history operations. Portable bundle DTOs remain inspectable, and importing
them validates the complete artifact before activation.

## Snapshot rule

Only register gameplay state you can safely destroy and recreate. Do not snapshot windows, GPU state, audio handles, sockets, or asset-server internals.

Snapshot manifests, replay manifests, and replay bundles use the current
version-3 contracts. Regenerate older artifacts; legacy logs and snapshots are
not migrated. Per-type snapshot versions and the checksum encoding version are
separate contracts. See
[`architecture.md`](architecture.md) for reset policy, state coverage, validation,
and rollback requirements.

## Verify the integration before expanding it

Use [Testing and reproducibility](./guides/testing.md) as the acceptance check:

1. Reset with a fixed seed and assert the initial gameplay state.
2. Apply one supported action and assert both the tick and its game-specific outcome.
3. Reset and replay the same inputs; compare checksums at every tick.
4. Save, step, restore, and repeat; verify that the transition matches.
5. Reconstruct an earlier tick from history and verify the next action again.

Once these pass, [add a remote server](./guides/remote-control.md#add-a-server-to-your-own-environment) or [render the game](./guides/capture.md). If construction fails, work through [Troubleshooting](./reference/troubleshooting.md) before adding more systems.

## Integration checklist

1. Add `AgentControlPlugins::default()` for control, snapshots, and replay; choose Bevy rendering plugins separately for visual builds.
2. Move authoritative gameplay into `AgentTick`.
3. Declare environment metadata, supported actions, and supported observation modes explicitly. Register JSON schemas for custom actions and the full observation envelope when the game needs a domain contract.
4. Convert keyboard/gamepad/network input into domain `AgentAction` values.
5. Read `CurrentInputFrame` in simulation systems.
6. Read `SimClock` for deterministic timing.
7. Add `SnapshotEntity` and `StableEntityId` to gameplay entities.
8. Implement `SnapshotType` with a stable wire ID and a positive per-type schema version for every registered gameplay type. Register components/resources with fallible `SnapshotAppExt` methods or registration macros. Mark resources that simulation systems require as required snapshot resources; only genuinely optional resources may be absent after restore.
9. Install an observation extractor that returns compact symbolic or domain state for each declared mode. Unsupported modes are rejected before extraction.
10. Install a checksum extractor over gameplay state using `StableHasher` or another deterministic hash path. Both extractors are required; the framework does not invent fallback state.
11. Optionally register a visual capture renderer for agent-readable PNG screenshots.
12. Keep rendering, UI, audio, and debug overlays out of authoritative simulation state.
