//! Sample-specific guarantees beyond the reusable runtime integration tests.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use bevy::prelude::*;
use bevy_agent_core::{
    AgentAction, EpisodeState, LastStepResponse, Observation, RewardState, StableEntityId,
    collect_observation, run_agent_tick,
};
use bevy_agent_runner::{
    AgentApp, AgentEnvironment, CaptureSource, ResetOptions, VisualCaptureOptions,
};
use bevy_agent_snapshot::{
    SnapshotType, capture_snapshot, checksum_snapshot, restore_snapshot_value,
};
use sample_platformer::{
    Coin, Collider, DODGE_DASH_SPEED, GameScore, LastMoveDirection, PlatformerConfig, Player,
    Velocity, build_headless_app,
};

fn environment() -> AgentApp {
    let mut env = AgentApp::new(build_headless_app).unwrap();
    env.reset(ResetOptions::default()).unwrap();
    env
}

fn player(world: &mut World) -> Entity {
    world
        .query_filtered::<Entity, With<Player>>()
        .single(world)
        .unwrap()
}

fn symbolic_player(observation: &Observation) -> &bevy_agent_core::PlayerObservation {
    match observation {
        Observation::Symbolic(symbolic) | Observation::Hybrid { symbolic, .. } => &symbolic.player,
        other => panic!("unexpected observation: {other:?}"),
    }
}

fn checksum(world: &mut World) -> u64 {
    collect_observation(world).unwrap();
    world
        .resource::<LastStepResponse>()
        .0
        .as_ref()
        .unwrap()
        .checksum
        .as_ref()
        .unwrap()
        .hash
}

#[test]
fn reset_restores_initial_facing_and_checksum() {
    let mut env = environment();
    let initial_checksum = checksum(env.world_mut());
    env.step(AgentAction::Move { x: -1.0, y: 0.0 }).unwrap();
    assert_eq!(env.world().resource::<LastMoveDirection>().0, -1.0);

    env.reset(ResetOptions::default()).unwrap();
    assert_eq!(checksum(env.world_mut()), initial_checksum);
    let dash = env.step(AgentAction::Dodge).unwrap();
    assert_eq!(
        symbolic_player(&dash.observation).velocity[0],
        DODGE_DASH_SPEED
    );
}

#[test]
fn snapshot_restore_restores_facing_for_future_dodge() {
    let mut env = environment();
    env.step(AgentAction::Move { x: -1.0, y: 0.0 }).unwrap();
    let snapshot = env.snapshot().unwrap();
    let expected = env.step(AgentAction::Dodge).unwrap();
    env.step(AgentAction::Move { x: 1.0, y: 0.0 }).unwrap();

    env.restore(snapshot.snapshot_id).unwrap();
    let restored = env.step(AgentAction::Dodge).unwrap();
    assert_eq!(restored.checksum, expected.checksum);
    assert_eq!(restored.reward, expected.reward);
    assert_eq!(
        symbolic_player(&restored.observation).velocity[0],
        -DODGE_DASH_SPEED
    );
}

#[test]
fn portable_replay_restores_facing_in_a_fresh_environment() {
    let mut source = environment();
    source.step(AgentAction::Move { x: -1.0, y: 0.0 }).unwrap();
    source.snapshot().unwrap();
    let expected = source.step(AgentAction::Dodge).unwrap();
    let bundle = source.export_replay_bundle().unwrap();

    let mut restored = environment();
    restored.load_replay_bundle(bundle).unwrap();
    restored.restore_tick(1).unwrap();
    let actual = restored.step(AgentAction::Dodge).unwrap();
    assert_eq!(actual.checksum, expected.checksum);
    assert_eq!(
        symbolic_player(&actual.observation).velocity[0],
        -DODGE_DASH_SPEED
    );
}

#[test]
fn checksum_covers_hidden_gameplay_state_and_component_presence() {
    type Mutation = (&'static str, fn(&mut World));
    let mutations: [Mutation; 10] = [
        ("facing", |world| {
            world.resource_mut::<LastMoveDirection>().0 = -1.0
        }),
        ("tick limit", |world| {
            world.resource_mut::<PlatformerConfig>().max_ticks += 1
        }),
        ("death limit", |world| {
            world.resource_mut::<PlatformerConfig>().death_y -= 1.0
        }),
        ("collider", |world| {
            let entity = player(world);
            world.get_mut::<Collider>(entity).unwrap().half_extents.x += 0.1;
        }),
        ("observed Z", |world| {
            let entity = player(world);
            world.get_mut::<Transform>(entity).unwrap().translation.z = 1.0;
        }),
        ("rotation", |world| {
            let entity = player(world);
            world.get_mut::<Transform>(entity).unwrap().rotation = Quat::from_rotation_z(0.5);
        }),
        ("scale", |world| {
            let entity = player(world);
            world.get_mut::<Transform>(entity).unwrap().scale.x = 2.0;
        }),
        ("immediate reward", |world| {
            world.resource_mut::<RewardState>().current_reward = 1.0
        }),
        ("cumulative reward", |world| {
            world.resource_mut::<RewardState>().cumulative_reward = 1.0
        }),
        ("zero-valued component presence", |world| {
            let entity = player(world);
            world.entity_mut(entity).insert(Coin { value: 0 });
        }),
    ];

    for (name, mutate) in mutations {
        let mut env = environment();
        let before = checksum(env.world_mut());
        mutate(env.world_mut());
        assert_ne!(checksum(env.world_mut()), before, "omitted {name}");
    }
}

#[test]
fn restore_rejects_missing_required_game_resources_before_mutation() {
    for name in [
        GameScore::TYPE_ID,
        sample_platformer::PlatformerState::TYPE_ID,
        PlatformerConfig::TYPE_ID,
        LastMoveDirection::TYPE_ID,
    ] {
        let mut env = environment();
        let before = checksum(env.world_mut());
        let mut snapshot = capture_snapshot(env.world_mut(), None).unwrap();
        snapshot
            .resources
            .retain(|resource| resource.type_id != name);
        snapshot.absent_resources.push(name.to_owned());
        snapshot.checksum = checksum_snapshot(&snapshot).unwrap();

        let error = restore_snapshot_value(env.world_mut(), &snapshot).unwrap_err();
        assert!(error.to_string().contains("required"), "{name}: {error}");
        assert_eq!(checksum(env.world_mut()), before, "{name}");
    }
}

#[test]
fn direct_terminal_tick_freezes_all_player_state_and_rewards() {
    for truncated in [false, true] {
        let mut env = environment();
        env.step(AgentAction::Move { x: -1.0, y: 0.0 }).unwrap();
        let entity = player(env.world_mut());
        let transform = *env.world().get::<Transform>(entity).unwrap();
        let velocity = env.world().get::<Velocity>(entity).unwrap().linvel;
        let direction = env.world().resource::<LastMoveDirection>().0;
        let score = env.world().resource::<GameScore>().value;
        let cumulative = env.world().resource::<RewardState>().cumulative_reward;
        {
            let mut episode = env.world_mut().resource_mut::<EpisodeState>();
            episode.done = !truncated;
            episode.truncated = truncated;
        }
        env.enqueue_action_at(2, bevy_agent_core::ActionSource::Agent, AgentAction::Dodge)
            .unwrap();

        assert!(run_agent_tick(env.world_mut()).is_err());
        // Exercise the domain systems directly after controller rejection.
        env.world_mut().run_schedule(bevy_agent_core::AgentTick);
        env.world_mut().run_schedule(bevy_agent_core::AgentFinalize);

        assert_eq!(*env.world().get::<Transform>(entity).unwrap(), transform);
        assert_eq!(
            env.world().get::<Velocity>(entity).unwrap().linvel,
            velocity
        );
        assert_eq!(env.world().resource::<LastMoveDirection>().0, direction);
        assert_eq!(env.world().resource::<GameScore>().value, score);
        assert_eq!(env.world().resource::<RewardState>().current_reward, 0.0);
        assert_eq!(
            env.world().resource::<RewardState>().cumulative_reward,
            cumulative
        );
    }
}

#[derive(Component)]
struct PresentationMarker;

struct CaptureDirectory(PathBuf);

impl CaptureDirectory {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        Self(std::env::temp_dir().join(format!(
            "sample-platformer-gameplay-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
        )))
    }

    fn capture(&self, env: &mut AgentApp, label: &str) -> image::RgbaImage {
        let captured = env
            .capture_visual(VisualCaptureOptions {
                output_dir: self.0.clone(),
                label: Some(label.to_owned()),
                source: CaptureSource::Software,
                ..Default::default()
            })
            .unwrap();
        image::open(captured.path).unwrap().into_rgba8()
    }
}

impl Drop for CaptureDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn software_capture_preserves_layers_across_archetype_changes_and_restore() {
    let mut env = environment();
    let entity = player(env.world_mut());
    env.world_mut()
        .get_mut::<Transform>(entity)
        .unwrap()
        .translation = Vec3::new(0.0, 0.2, 0.0);
    let snapshot = env.snapshot().unwrap();
    let expected_checksum = checksum(env.world_mut());
    let captures = CaptureDirectory::new();
    let before = captures.capture(&mut env, "before");

    env.world_mut()
        .entity_mut(entity)
        .insert(PresentationMarker);
    let reordered = captures.capture(&mut env, "reordered");
    assert_eq!(before, reordered);
    assert_eq!(checksum(env.world_mut()), expected_checksum);

    env.restore(snapshot.snapshot_id).unwrap();
    assert_eq!(before, captures.capture(&mut env, "restored"));
    assert_eq!(checksum(env.world_mut()), expected_checksum);
}

#[test]
fn fully_offscreen_entities_do_not_leave_capture_border_artifacts() {
    let mut env = environment();
    let captures = CaptureDirectory::new();
    let before = captures.capture(&mut env, "before");
    env.world_mut().spawn((
        StableEntityId::from_u64(500),
        Coin { value: 0 },
        Collider {
            half_extents: Vec2::splat(0.25),
        },
        Transform::from_xyz(100.0, 3.0, 0.0),
    ));
    assert_eq!(before, captures.capture(&mut env, "offscreen"));
}
