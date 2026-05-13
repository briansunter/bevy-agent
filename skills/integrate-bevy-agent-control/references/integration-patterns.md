# bevy_agent_control Integration Patterns

## Imports

```rust
use bevy::prelude::*;
use bevy_agent_core::{
    AgentAction, AgentControlAppExt, AgentControlPlugin, AgentReset, AgentResetSet, AgentSet,
    AgentTick, CurrentInputFrame, EpisodeState, Observation, ObservationMode, RewardState,
    SimClock, SnapshotEntity, StableEntityId, StableIdAllocator, StateChecksum,
};
use bevy_agent_replay::AgentReplayPlugin;
use bevy_agent_runner::{
    VisualCaptureAppExt, VisualCaptureOptions, VisualCaptureResult, visual_capture_path,
};
use bevy_agent_snapshot::{AgentSnapshotPlugin, SnapshotAppExt, clear_snapshot_entities};
```

## Plugin Setup

```rust
impl Plugin for GamePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<GameScore>()
            .set_snapshot_metadata("my_game", env!("CARGO_PKG_VERSION"))
            .register_snapshot_component::<StableEntityId>()
            .register_snapshot_component::<Transform>()
            .register_snapshot_component::<Velocity>()
            .register_snapshot_component::<Player>()
            .register_snapshot_resource::<GameScore>()
            .insert_observation_extractor(game_observation)
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

Keep a small deterministic builder for tests and agents, then layer visual plugins separately:

```rust
pub fn build_headless_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(AgentControlPlugin::deterministic())
        .add_plugins(AgentSnapshotPlugin)
        .add_plugins(AgentReplayPlugin)
        .add_plugins(GamePlugin);
    app
}

pub fn build_visual_app() -> App {
    let mut app = build_headless_app();
    app.add_plugins(DefaultPlugins)
        .add_systems(Update, (camera_follow, render_debug_overlay));
    app
}
```

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

Prefer a game-specific serializable action enum:

```rust
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(tag = "type")]
pub enum GameAction {
    Noop,
    Move { x: f32, y: f32 },
    Jump,
    Interact,
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
            _ => {}
        }
    }
}
```

For game-specific action spaces, define a serializable enum and keep the same pipeline shape: queue scheduled domain actions, drain into `CurrentInputFrame<MyAction>`, and consume that frame in simulation systems.

Human input should enqueue `GameAction` values and should not mutate gameplay components directly.

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
    use std::hash::{Hash, Hasher};

    let clock = world.resource::<SimClock>();
    let score = world.resource::<GameScore>();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    clock.tick.hash(&mut hasher);
    score.value.hash(&mut hasher);

    StateChecksum {
        tick: clock.tick,
        hash: hasher.finish(),
    }
}
```

Include stable IDs, gameplay transforms/velocities, RNG state, score, episode state, and any resources that affect future simulation.

Sort query output by stable ID before hashing so iteration order cannot affect checksums.

## Remote

For local HTTP testing, expose the game with a localhost bind and optional token. Do not bind privileged mutation APIs to public interfaces by default.

Use the repository's remote examples as the baseline:

```sh
cargo run -p sample_platformer --example remote_http -- 127.0.0.1:4000
AGENT_TOKEN=secret cargo run -p sample_platformer --example remote_http -- 127.0.0.1:4000
```

Remote mutation methods should require the same capability checks as restore/replay/branch methods.

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
