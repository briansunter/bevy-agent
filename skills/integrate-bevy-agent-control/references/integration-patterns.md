# bevy_agent_control Integration Patterns

## Imports

```rust
use std::hash::Hash;

use bevy::prelude::*;
use bevy_agent_core::{
    ActionSource, AgentAction, AgentActionCatalog, AgentActionKind, AgentActionQueue,
    AgentControlAppExt, AgentControlState, AgentDecision, AgentReset, AgentResetSet,
    AgentSet, AgentTick, CurrentInputFrame, EpisodeState, ExecutionContext, Observation,
    ObservationMode, RewardState, SimClock, SnapshotEntity, StableEntityId,
    StableHasher, StableIdAllocator, EnvironmentChecksum,
};
use bevy_agent_runner::{
    AgentControlPlugins, VisualCaptureAppExt, VisualCaptureOptions, VisualCaptureResult,
    visual_capture_path,
};
use bevy_agent_snapshot::{
    SnapshotAppExt, SnapshotType, clear_snapshot_entities, register_snapshot_components,
};
```

## Plugin Setup

```rust
impl Plugin for GamePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<GameScore>()
            .init_resource::<GameplayRng>()
            .set_snapshot_metadata("my_game", env!("CARGO_PKG_VERSION"))
            .set_supported_actions([
                AgentActionKind::Noop,
                AgentActionKind::Move,
                AgentActionKind::Jump,
                AgentActionKind::Custom,
            ])
            .set_supported_observation_modes([
                ObservationMode::PlayerKnowledge,
                ObservationMode::Hybrid,
            ])
            .set_observation_schema(game_observation_schema())
            .expect("game observation schema is valid")
            .register_custom_action_schema(
                "game_action",
                serde_json::json!({
                    "oneOf": [
                        {
                            "type": "object",
                            "required": ["type"],
                            "properties": { "type": { "const": "dash" } }
                        },
                        {
                            "type": "object",
                            "required": ["type", "x", "y"],
                            "properties": {
                                "type": { "const": "interact_at" },
                                "x": { "type": "number", "minimum": -100000, "maximum": 100000 },
                                "y": { "type": "number", "minimum": -100000, "maximum": 100000 }
                            }
                        }
                    ]
                }),
            )
            .expect("game custom action schema is valid");

        register_snapshot_components!(app, StableEntityId, Transform, Velocity, Player,)
            .expect("game snapshot component IDs and versions are valid");
        app.register_required_snapshot_resource::<GameScore>()
            .expect("score snapshot identity is valid")
            .register_required_snapshot_resource::<GameplayRng>()
            .expect("RNG snapshot identity is valid");

        app.insert_observation_extractor(game_observation)
            .insert_checksum_extractor(game_checksum)
            .insert_visual_capture_renderer(game_visual_capture)
            .add_systems(AgentDecision, agent_policy)
            .add_systems(AgentReset, reset_level.in_set(AgentResetSet::Game))
            .add_systems(
                AgentTick,
                (apply_actions, physics_step, scoring)
                    .chain()
                    .in_set(AgentSet::Simulation),
            )
            .add_systems(AgentTick, terminal_check.in_set(AgentSet::TerminalCheck));
    }
}
```

Both extractors are required. `AgentApp` construction validates the integration
and returns an error if metadata, catalogs, resources, schedules, or extractors
are missing. The default mode is `Hybrid`; if the game only supports another
mode, set `ObservationConfig.mode` to that supported mode before construction.

`AgentPostTick` finishes before `AgentFinalize` collects observations, records
replay state, and captures automatic snapshots. Presentation systems may read
gameplay state in `Update`; they must not advance the authoritative clock or
physics.

## Stable Snapshot Identity

Implement `SnapshotType` for each registered game-owned type. For example:

```rust
impl SnapshotType for Player {
    const TYPE_ID: &'static str = "my_game.player";
    const SCHEMA_VERSION: u32 = 1;
}

impl SnapshotType for Velocity {
    const TYPE_ID: &'static str = "my_game.velocity";
    const SCHEMA_VERSION: u32 = 1;
}

impl SnapshotType for GameScore {
    const TYPE_ID: &'static str = "my_game.score";
    const SCHEMA_VERSION: u32 = 1;
}

impl SnapshotType for GameplayRng {
    const TYPE_ID: &'static str = "my_game.rng";
    const SCHEMA_VERSION: u32 = 1;
}
```

Types must also implement `Clone`, `Serialize`, `Deserialize`, and their Bevy
component/resource trait. `Transform` and core snapshot types already implement
`SnapshotType`. Explicit nonempty IDs survive module moves; Rust type names are
diagnostic only. IDs must be unique and versions positive. Registration rejects
conflicts atomically, and batch macros return a result. Change the per-type
version when its serialized representation changes.

Required resources participate in the registry schema and cannot be omitted at
capture or restore. Use `register_snapshot_resource` or
`register_snapshot_resources!` only when absence is valid. Registry, store,
recorder, and timeline collections are private; inspect getters and use checked
operations. Prefer runner snapshot/restore/branch/import methods for changes
that must coordinate world state with history. Portable DTOs remain inspectable
and every import is preflight-validated.

Snapshot manifests, replay manifests, and replay bundles use format 3. Older
artifacts must be regenerated; there are no migration or compatibility paths.
Per-type versions and the checksum encoding version remain separate contracts.

## Headless and Visual Builders

Keep a small deterministic builder for tests and agents, then build visual apps separately so render plugins never enter headless test runs:

```rust
pub fn build_headless_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(AgentControlPlugins::default())
        .add_plugins(GamePlugin);
    app
}

#[cfg(feature = "visual")]
pub fn build_visual_app() -> App {
    let mut app = App::new();
    app.add_plugins(DefaultPlugins)
        .add_plugins(AgentControlPlugins::default())
        .add_plugins(GamePlugin)
        .add_plugins(GameVisualPlugin);
    app
}
```

Use `AgentControlPlugins::default()` for the shared simulation stack. Rendering and networking are separate integrations. Use `with_snapshot_policy`, `without_snapshots`, or `without_replay` when the application needs a smaller stack.

## Reset

```rust
fn reset_level(world: &mut World) {
    clear_snapshot_entities(world);
    *world.resource_mut::<GameScore>() = GameScore::default();

    let id = world.resource_mut::<StableIdAllocator>().allocate();
    world.spawn((
        SnapshotEntity,
        id,
        Player { health: 100.0 },
        Velocity::default(),
        Transform::default(),
    ));
}
```

Give every authoritative resource an explicit reset policy. Reset
episode-local state, including hidden input memory such as facing direction;
retain intended configuration. Reseed a game-owned RNG from the episode seed
when resetting it. Register and checksum all state that can affect a future
tick, whether reset or retained.

## Actions

Use built-in `AgentAction` variants when they describe the control surface. For game-specific controls, carry a stable JSON payload in `AgentAction::Custom` and register the payload schema during plugin setup.

Core compiles and caches draft 2020-12 JSON Schema validators at registration.
Custom payloads must match at least one registered schema before scheduling,
controller execution, or acceptance from replay/snapshot input. Registration
rejects malformed schemas and remote references without replacing the prior
contract. Remote discovery exposes the enforced schema. The game decodes valid
commands and owns their effects; add schema bounds for domain numeric limits.

```rust
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GameAction {
    Dash,
    InteractAt { x: f32, y: f32 },
}
```

```rust
fn apply_actions(
    input: Res<CurrentInputFrame>,
    mut player: Query<&mut Velocity, With<Player>>,
) {
    let Ok(mut velocity) = player.single_mut() else {
        return;
    };

    velocity.0.x = 0.0;
    for action in &input.actions {
        match action {
            AgentAction::Move { x, .. } => velocity.0.x = x.clamp(-1.0, 1.0) * 6.0,
            AgentAction::Jump => velocity.0.y = 9.5,
            AgentAction::Custom { value } => {
                if let Ok(action) = serde_json::from_value::<GameAction>(value.clone()) {
                    apply_game_action(&mut velocity, action);
                }
            }
            _ => {}
        }
    }
}
```

Human input should enqueue `AgentAction` values and should not mutate gameplay components directly.

Use `ControlMode::Hybrid` for human and agent input together. A rejected source,
invalid action, or non-future tick returns an error before queue mutation:

```rust
fn agent_policy(
    catalog: Res<AgentActionCatalog>,
    clock: Res<SimClock>,
    control: Res<AgentControlState>,
    context: Res<ExecutionContext>,
    mut queue: ResMut<AgentActionQueue>,
) {
    let Some(next_tick) = clock.tick.checked_add(1) else {
        error!("cannot schedule input beyond the tick limit");
        return;
    };
    if let Err(error) = queue.schedule(
        &catalog, &clock, &control, &context,
        next_tick, ActionSource::Agent, AgentAction::Noop,
    ) {
        error!("agent policy input rejected: {error}");
    }
}
```

With exclusive access to a world, `bevy_agent_core::schedule_action` applies the
same checks. `AgentApp::enqueue_action_at` is also fallible. `run_agent_tick`
returns collection/integration errors rather than a stale cached response.

## Deterministic Time

```rust
fn physics_step(clock: Res<SimClock>, mut query: Query<(&mut Transform, &mut Velocity)>) {
    let dt = clock.dt_seconds;
    for (mut transform, mut velocity) in &mut query {
        transform.translation.x += velocity.0.x * dt;
        transform.translation.y += velocity.0.y * dt;
    }
}
```

Do not read `Res<Time>` for deterministic gameplay.

Use tick counters for deterministic gameplay timers:

```rust
#[derive(Component, Clone, serde::Deserialize, serde::Serialize)]
struct CooldownTicks(u32);

impl SnapshotType for CooldownTicks {
    const TYPE_ID: &'static str = "my_game.cooldown_ticks";
    const SCHEMA_VERSION: u32 = 1;
}

fn cooldowns(mut query: Query<&mut CooldownTicks>) {
    for mut cooldown in &mut query {
        cooldown.0 = cooldown.0.saturating_sub(1);
    }
}
```

## Visual Capture

Register a capture renderer when agents need screenshots during headless play. The renderer should read gameplay state, write a PNG, and return path metadata.

```rust
fn game_visual_capture(
    world: &mut World,
    options: &VisualCaptureOptions,
) -> anyhow::Result<VisualCaptureResult> {
    let tick = world.resource::<SimClock>().tick;
    let frame = world.resource::<bevy_agent_core::AgentControlState>().frame;
    let path = visual_capture_path(options, tick, frame)?;

    // Draw a small PNG from gameplay state here.
    // Keep this read-only with respect to authoritative simulation state.

    Ok(VisualCaptureResult {
        tick,
        frame,
        path,
        width: 640,
        height: 360,
        format: "png".to_string(),
    })
}
```

Visual apps can also enable `bevy_agent_runner/visual` and use Bevy primary-window screenshots. Keep this out of checksums and snapshots.

## Observation

```rust
fn game_observation(world: &mut World, mode: ObservationMode) -> Observation {
    let tick = world.resource::<SimClock>().tick;
    let score = world.resource::<GameScore>().value;

    let symbolic = bevy_agent_core::SymbolicObservation {
        tick,
        player: bevy_agent_core::PlayerObservation {
            score,
            ..Default::default()
        },
        visible_entities: Vec::new(),
        inventory: Vec::new(),
        objectives: Vec::new(),
    };

    match mode {
        ObservationMode::Hybrid => Observation::Hybrid {
            symbolic,
            pixels: None,
            debug: Some(serde_json::json!({ "score": score })),
        },
        ObservationMode::PlayerKnowledge => Observation::Symbolic(symbolic),
        _ => unreachable!("unsupported modes are rejected by the catalog"),
    }
}
```

Declare exactly these two modes in plugin setup. Unsupported modes fail before
calling the extractor; collection does not fall back to another mode.

Observation schemas describe the full serialized `Observation`, including
`kind`, rather than only `Domain.value` or `Hybrid.debug`. A compact envelope
schema for the extractor above is:

```rust
fn game_observation_schema() -> serde_json::Value {
    serde_json::json!({
        "oneOf": [
            {
                "type": "object",
                "required": ["kind", "tick", "player", "visible_entities", "inventory", "objectives"],
                "properties": {
                    "kind": { "const": "Symbolic" },
                    "tick": { "type": "integer", "minimum": 0 },
                    "player": { "type": "object" },
                    "visible_entities": { "type": "array" },
                    "inventory": { "type": "array" },
                    "objectives": { "type": "array" }
                }
            },
            {
                "type": "object",
                "required": ["kind", "symbolic", "pixels", "debug"],
                "properties": {
                    "kind": { "const": "Hybrid" },
                    "symbolic": { "type": "object" },
                    "pixels": { "type": "null" },
                    "debug": {
                        "type": "object", "required": ["score"],
                        "properties": { "score": { "type": "integer" } }
                    }
                }
            }
        ]
    })
}
```

Core caches the compiled validator and checks each collected observation.
Expand nested schemas to cover the game's public payload; the sample's
`observation` module provides a complete example. Keep schemas consistent with
actual optional fields and the information policy for each supported mode.

## Checksum

```rust
fn game_checksum(world: &mut World) -> EnvironmentChecksum {
    let clock = world.resource::<SimClock>();
    let score = world.resource::<GameScore>();
    let mut hasher = StableHasher::new();
    clock.tick.hash(&mut hasher);
    clock.dt_seconds.to_bits().hash(&mut hasher);
    score.value.hash(&mut hasher);

    EnvironmentChecksum {
        tick: clock.tick,
        hash: hasher.finish_hash(),
    }
}
```

The checksum snippet is an excerpt. Include stable IDs, complete gameplay
transforms/velocities, collider dimensions, RNG state, score, rewards, episode
state, and every resource that affects future simulation or exposed output.
Use a domain/version discriminator when changing checksum coverage.

Sort query output by stable ID before hashing so iteration order cannot affect checksums.

Do not use `DefaultHasher` for determinism checks; its algorithm is not a stable cross-version contract.

## Remote

For local HTTP testing, expose the game with a loopback bind and optional token. Non-loopback HTTP binds without a session token are rejected by `HttpRemoteServer::serve`.

Use the repository's remote examples as the baseline:

```sh
cargo run -p sample_platformer --example remote_http -- 127.0.0.1:4000
AGENT_TOKEN=secret cargo run -p sample_platformer --example remote_http -- 127.0.0.1:4000
AGENT_TOKEN=secret cargo run -p sample_platformer --example remote_http -- 0.0.0.0:4000
```

Remote schemas advertise supported built-in actions, registered custom action
schemas, and the game's explicitly supported observation modes. `JsonRpcBridge::new`
returns a result, and main-thread remote plugins must be installed before
`app.run()`. Require the appropriate capabilities for mutation, restore,
replay, branch, capture, and filesystem operations. Artifact requests use
relative paths confined to the configured artifact root.

## Tests

Prefer tests that operate through `AgentApp`:

```rust
use bevy_agent_core::AgentAction;
use bevy_agent_runner::{AgentApp, AgentEnvironment, ResetOptions};

#[test]
fn step_advances_one_tick() {
    let mut env = AgentApp::new(build_headless_app).unwrap();
    env.reset(ResetOptions::default()).unwrap();
    let response = env.step(AgentAction::Noop).unwrap();
    assert_eq!(response.tick, 1);
}
```

Add deterministic replay tests by running a seed and action list, restoring the initial snapshot, replaying the same actions, and comparing final checksums.

Snapshot restore test shape:

```rust
#[test]
fn restore_replays_to_same_checksum() {
    let mut env = AgentApp::new(build_headless_app).unwrap();
    let first = env.reset_with_response(
        ResetOptions { seed: Some(42), ..Default::default() }
    ).unwrap();
    let snapshot = env.snapshot().unwrap();

    let actions = vec![
        AgentAction::Move { x: 1.0, y: 0.0 },
        AgentAction::Jump,
        AgentAction::Noop,
    ];
    let a = env
        .step_many(actions.clone())
        .unwrap()
        .last()
        .unwrap()
        .checksum
        .clone();

    env.restore(snapshot.snapshot_id).unwrap();
    let b = env
        .step_many(actions)
        .unwrap()
        .last()
        .unwrap()
        .checksum
        .clone();

    assert_eq!(first.tick, 0);
    assert_eq!(a, b);
}
```

Use generated action/snapshot/rewind/fork/import sequences and compare against
fresh forward execution, including subsequent behavior. Check that rejected
operations preserve state, queued future inputs, cached/control metadata, and
portable exports. Corrupt exported DTOs to test rejection instead of bypassing
private live owners.
