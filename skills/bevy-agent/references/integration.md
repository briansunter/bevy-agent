# Integrate a game

## Inventory before editing

Find the app builder, plugins, input adapters, gameplay schedules, reset/spawn logic, randomness, and current tests. Preserve the game's existing rules and renderer. Establish one concrete acceptance case, such as “moving right advances exactly one tick and increases player x.”

For each authoritative component/resource, record its reset policy, snapshot registration, checksum coverage, and whether it belongs in a player observation. Include hidden cooldowns, pending input, RNG state, and last movement direction when they affect future actions. Do not use an ECS dump as the default observation.

## Build the first vertical slice

Use the bundled counter as a complete compiling shape. The snippets below are adaptation points, not standalone programs: `GamePlugin`, systems, and gameplay types refer to the user's game.

```rust
let mut app = App::new();
app.add_plugins(MinimalPlugins)
    .add_plugins(AgentControlPlugins::default())
    .add_plugins(GamePlugin);
app.add_systems(AgentReset, reset_game.in_set(AgentResetSet::Game));
app.add_systems(
    AgentTick,
    (apply_actions, physics, scoring)
        .chain()
        .in_set(AgentSet::Simulation),
);
```

Import Bevy types from `bevy::prelude`, schedule types from `bevy_agent_core`, and `AgentControlPlugins` from `bevy_agent_runner`. The headless builder returns `App`; it does not call `app.run()`.

Consume `CurrentInputFrame<AgentAction>` in authoritative systems. Input adapters enqueue checked actions instead of changing position directly. Use `ControlMode::Hybrid` only when both human and agent sources should be accepted. Autonomous policies belong in `AgentDecision`, which runs before the controlled tick, rather than render-frame `Update`.

Use `SimClock` for simulation timing. `AgentPostTick` finishes before `AgentFinalize` collects the response and history. Preserve that ordering. Presentation systems may read authoritative state in `Update`, but must not advance gameplay.

## Declare the contract

The counter shows the necessary extension traits and imports. Set:

- environment/snapshot metadata;
- supported `AgentActionKind` values;
- supported `ObservationMode` values and a supported default (`Hybrid` is the baseline default);
- an observation extractor and checksum extractor;
- a full observation schema when the game defines one.

For `Observation::Domain`, the schema describes the full serialized object containing `kind`, `tick`, and `value`, not only the inner value. Custom actions require declaring `AgentActionKind::Custom` and registering at least one matching payload schema using `register_custom_action_schema`. Registration compiles validators; malformed schemas and remote schema references are rejected.

Check the actual custom-action envelope in `agent.schema` before generating client JSON. A label like “dash” does not establish the correct envelope or permitted fields. Schemas validate shape; gameplay systems still enforce game rules.

`AgentApp::new`, `from_app`, and `from_running_app` are fallible and reject incomplete integration. Handle static registration failures with a descriptive `expect`; propagate dynamic errors. Do not add fallback observations to conceal a missing extractor.

## Register complete state

Game-owned snapshot types need their Bevy component/resource trait, `Clone`, serialization/deserialization, and `SnapshotType`:

```rust
impl SnapshotType for GameScore {
    const TYPE_ID: &'static str = "my_game.score";
    const SCHEMA_VERSION: u32 = 1;
}
app.register_required_snapshot_resource::<GameScore>()
    .expect("score snapshot identity is valid");
```

Use a stable, nonempty game-owned type ID, not a Rust module path that changes during refactoring. Increase the per-type version when its serialized contract changes. The registry rejects duplicate identities and zero versions.

Gameplay entities need `SnapshotEntity` and `StableEntityId`. Register the serializable components needed to recreate them, including stable IDs. Use checked `SnapshotAppExt` APIs/macros. Required resources must be registered as required; optional means absence after restore is valid.

Reset episode state deliberately; retain configuration only by design. Include configuration in snapshots/checksums if it affects outcomes. Keep GPU/window state, sockets, UI, audio, and asset-server internals outside registration. Rebuild presentation from restored gameplay state.

## Make checksums useful

Start with `default_checksum(world)` and combine it with all authoritative game state using `StableHasher`, as the counter does. Sort entity-derived data by stable identity before hashing if query order can vary. Do not use `DefaultHasher` as a cross-run format contract.

Assert actual game outcomes as well as equal hashes. A checksum can be consistently wrong or omit the state causing a bug. Matching checksums do not authenticate imported files or guarantee identical floating-point behavior on every platform.

## Add the server after local checks pass

Add `bevy_agent_remote = "=0.0.4"`. This minimal server uses the library's filesystem-disabled defaults:

```rust
use bevy_agent_remote::{HttpRemoteServer, JsonRpcBridge, RemoteSecurity};
use bevy_agent_runner::AgentApp;

fn serve(env: &mut AgentApp) -> anyhow::Result<()> {
    let bridge = JsonRpcBridge::new(RemoteSecurity::default())?;
    HttpRemoteServer::new("127.0.0.1:4000", bridge).serve(env)
}
```

Construct `AgentApp` from the integrated builder, then pass it to `serve`. Reuse the user's selected port. Set a session token and appropriate capabilities for an intentionally network-accessible listener; this skill does not imply a request to expose one publicly.

For rendered apps, use `BevyRemoteControlPlugin` and let `app.run()` own the Bevy main thread. Enable runner/remote `visual` features for primary-window capture, and choose the game's rendering plugins separately. Use a registered software renderer for headless screenshots. See the repository's `remote_http_visual` example before changing threading or lifecycle.

## Acceptance checks

Start with the counter's two tests, replacing its action and game assertions:

1. Same seed plus the same legal action sequence gives the same checksums at each tick.
2. A snapshot restored before an action produces the same next transition.
3. `restore_tick` reconstructs a retained point and reproduces its next transition.
4. The game-specific outcome is correct, including relevant terminal or boundary cases.

Add validation for changed schemas, future scheduling, import/export, or rendering only where the integration uses those features. Run host-appropriate Cargo checks; obey repository storage launchers. Verify transport behavior through a real server after local semantics pass.

For deeper implementation detail, use the checked-out source or [game integration guide](https://briansunter.github.io/bevy-agent/controllable-game.html). In the full repository, `skills/integrate-bevy-agent-control/references/integration-patterns.md` supplies additional patterns; this portable skill does not require that sibling directory.
