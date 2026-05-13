# bevy_agent_control Integration Patterns

## Imports

```rust
use std::hash::Hash;

use bevy::prelude::*;
use bevy_agent_core::{
    AgentAction, AgentControlAppExt, AgentReset, AgentResetSet, AgentSet, AgentTick,
    CurrentInputFrame, EpisodeState, Observation, ObservationMode, RewardState, SimClock,
    SnapshotEntity, StableEntityId, StableHasher, StableIdAllocator, StateChecksum,
};
use bevy_agent_runner::{
    AgentControlPlugins, VisualCaptureAppExt, VisualCaptureOptions, VisualCaptureResult,
    visual_capture_path,
};
use bevy_agent_snapshot::{
    SnapshotAppExt, clear_snapshot_entities, register_snapshot_components,
    register_snapshot_resources,
};
```

## Plugin Setup

```rust
impl Plugin for GamePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<GameScore>()
            .init_resource::<GameplayRng>()
            .set_snapshot_metadata("my_game", env!("CARGO_PKG_VERSION"))
            .register_custom_action_schema(
                "game_action",
                serde_json::json!({
                    "type": "object",
                    "required": ["type"],
                    "properties": {
                        "type": { "enum": ["dash", "interact_at"] },
                        "x": { "type": "number" },
                        "y": { "type": "number" }
                    }
                }),
            );

        register_snapshot_components!(
            app,
            StableEntityId,
            Transform,
            Velocity,
            Player,
        );
        register_snapshot_resources!(app, GameScore, GameplayRng);

        app.insert_observation_extractor(game_observation)
            .insert_checksum_extractor(game_checksum)
            .insert_visual_capture_renderer(game_visual_capture)
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

## Headless and Visual Builders

Keep a small deterministic builder for tests and agents, then build visual apps separately so render plugins never enter headless test runs:

```rust
pub fn build_headless_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(AgentControlPlugins::deterministic())
        .add_plugins(GamePlugin);
    app
}

#[cfg(feature = "visual")]
pub fn build_visual_app() -> App {
    let mut app = App::new();
    app.add_plugins(DefaultPlugins)
        .add_plugins(AgentControlPlugins::visual_debug())
        .add_plugins(GamePlugin)
        .add_plugins(GameVisualPlugin);
    app
}
```

Use `AgentControlPlugins::remote()` for long-running apps that are intended to be controlled through a remote loop, and use `with_snapshot_policy`, `without_snapshots`, or `without_replay` only when the default control/snapshot/replay stack is intentionally too broad.

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

## Actions

Use built-in `AgentAction` variants when they describe the control surface. For game-specific controls, carry a stable JSON payload in `AgentAction::Custom` and register the payload schema during plugin setup.

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
        ObservationMode::FullDebugState | ObservationMode::Hybrid => Observation::Hybrid {
            symbolic,
            pixels: None,
            debug: Some(serde_json::json!({ "score": score })),
        },
        _ => Observation::Symbolic(symbolic),
    }
}
```

## Checksum

```rust
fn game_checksum(world: &mut World) -> StateChecksum {
    let clock = world.resource::<SimClock>();
    let score = world.resource::<GameScore>();
    let mut hasher = StableHasher::new();
    clock.tick.hash(&mut hasher);
    clock.dt_seconds.to_bits().hash(&mut hasher);
    score.value.hash(&mut hasher);

    StateChecksum {
        tick: clock.tick,
        hash: hasher.finish_hash(),
    }
}
```

Include stable IDs, gameplay transforms/velocities, RNG state, score, episode state, and any resources that affect future simulation.

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

Remote schemas should advertise built-in actions, registered custom action schemas, and all observation variants. Remote mutation methods should require the same capability checks as restore/replay/branch methods.

## Tests

Prefer tests that operate through `AgentApp`:

```rust
use bevy_agent_core::AgentAction;
use bevy_agent_runner::{AgentApp, AgentEnvironment, ResetOptions};

#[test]
fn step_advances_one_tick() {
    let mut env = AgentApp::new(build_headless_app);
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
    let mut env = AgentApp::new(build_headless_app);
    let first = env.reset(ResetOptions { seed: Some(42), ..Default::default() }).unwrap();
    let snapshot = env.snapshot().unwrap();

    let actions = [
        AgentAction::Move { x: 1.0, y: 0.0 },
        AgentAction::Jump,
        AgentAction::Noop,
    ];
    let a = env.step_many(actions.clone()).unwrap().checksum;

    env.restore(snapshot.snapshot_id).unwrap();
    let b = env.step_many(actions).unwrap().checksum;

    assert_eq!(first.tick, 0);
    assert_eq!(a, b);
}
```
