use super::*;
use crate::simulation::aabb_overlap;
use bevy_agent_core::{
    AgentAction, EpisodeState, Observation, ObservationMode, RewardState, SimClock,
};

#[test]
fn platformer_config_default_sets_episode_limits() {
    let config = PlatformerConfig::default();

    assert_eq!(config.max_ticks, 900);
    assert_eq!(config.death_y, -8.0);
}

#[test]
fn aabb_overlap_detects_touching_and_separated_boxes() {
    assert!(aabb_overlap(
        Vec3::ZERO,
        Vec2::splat(1.0),
        Vec3::new(2.0, 0.0, 0.0),
        Vec2::splat(1.0),
    ));
    assert!(!aabb_overlap(
        Vec3::ZERO,
        Vec2::splat(1.0),
        Vec3::new(2.1, 0.0, 0.0),
        Vec2::splat(1.0),
    ));
}

#[test]
fn headless_app_contains_agent_resources_after_startup() {
    let mut app = build_headless_app();
    app.finish();
    app.cleanup();

    assert!(app.world().contains_resource::<SimClock>());
    assert!(app.world().contains_resource::<GameScore>());
    assert!(app.world().contains_resource::<PlatformerState>());
}

#[test]
fn platformer_checksum_includes_observed_player_state() {
    let mut app = build_headless_app();
    app.finish();
    app.cleanup();
    let player = app
        .world_mut()
        .spawn((
            StableEntityId::from_u64(1),
            Transform::default(),
            Velocity::default(),
            Player { health: 100.0 },
            OnGround(false),
        ))
        .id();

    let baseline = platformer_checksum(app.world_mut()).hash;
    app.world_mut().get_mut::<Player>(player).unwrap().health = 50.0;
    let damaged = platformer_checksum(app.world_mut()).hash;
    assert_ne!(damaged, baseline);

    app.world_mut().get_mut::<OnGround>(player).unwrap().0 = true;
    let grounded = platformer_checksum(app.world_mut()).hash;
    assert_ne!(grounded, damaged);
}

fn reset_app() -> App {
    let mut app = build_headless_app();
    app.finish();
    app.cleanup();
    app.world_mut().run_schedule(AgentReset);
    app
}

fn player_velocity(app: &mut App) -> Vec2 {
    app.world_mut()
        .query_filtered::<&Velocity, With<Player>>()
        .single(app.world())
        .map(|velocity| velocity.linvel)
        .unwrap()
}

fn step_with(app: &mut App, actions: Vec<AgentAction>) {
    use bevy_agent_core::{ActionSource, run_agent_tick, schedule_action};
    let next_tick = app.world().resource::<SimClock>().tick + 1;
    for action in actions {
        schedule_action(app.world_mut(), next_tick, ActionSource::Agent, action).unwrap();
    }
    run_agent_tick(app.world_mut()).unwrap();
}

#[test]
fn standalone_dodge_dashes_in_default_plus_x() {
    let mut app = reset_app();
    step_with(&mut app, vec![AgentAction::Dodge]);
    assert_eq!(player_velocity(&mut app).x, DODGE_DASH_SPEED);
}

#[test]
fn move_and_dodge_on_same_tick_sum() {
    let mut app = reset_app();
    step_with(
        &mut app,
        vec![AgentAction::Move { x: 1.0, y: 0.0 }, AgentAction::Dodge],
    );
    assert_eq!(player_velocity(&mut app).x, PLAYER_SPEED + DODGE_DASH_SPEED);
}

#[test]
fn multiple_moves_sum_instead_of_overwriting() {
    let mut app = reset_app();
    step_with(
        &mut app,
        vec![
            AgentAction::Move { x: 0.5, y: 0.0 },
            AgentAction::Move { x: 0.5, y: 0.0 },
        ],
    );
    assert!((player_velocity(&mut app).x - PLAYER_SPEED).abs() < 1e-5);
}

#[test]
fn invalid_move_payload_is_rejected() {
    let mut app = reset_app();
    assert!(
        bevy_agent_core::schedule_action(
            app.world_mut(),
            1,
            bevy_agent_core::ActionSource::Agent,
            AgentAction::Move { x: 5.0, y: 0.0 },
        )
        .is_err()
    );
    assert_eq!(player_velocity(&mut app).x, 0.0);
}

#[test]
fn terminal_episode_freezes_player_and_stops_rewards() {
    let mut app = reset_app();
    app.world_mut().resource_mut::<EpisodeState>().done = true;
    app.world_mut().resource_mut::<EpisodeState>().reason = Some("goal_reached".to_string());
    let cumulative = app.world().resource::<RewardState>().cumulative_reward;
    bevy_agent_core::schedule_action(
        app.world_mut(),
        1,
        bevy_agent_core::ActionSource::Agent,
        AgentAction::Dodge,
    )
    .unwrap();
    assert!(bevy_agent_core::run_agent_tick(app.world_mut()).is_err());
    // Even when an integration invokes the gameplay schedule directly, the
    // domain's absorbing systems must preserve terminal gameplay state.
    app.world_mut().run_schedule(bevy_agent_core::AgentTick);
    app.world_mut().run_schedule(bevy_agent_core::AgentFinalize);
    assert_eq!(player_velocity(&mut app).x, 0.0);
    let reward = app.world().resource::<RewardState>().clone();
    assert_eq!(reward.current_reward, 0.0);
    assert_eq!(reward.cumulative_reward, cumulative);
}

#[test]
fn unsupported_observation_modes_are_rejected_before_extraction() {
    let mut app = reset_app();
    for mode in [
        ObservationMode::PixelFrame,
        ObservationMode::FullDebugState,
        ObservationMode::DiffSinceLastTick,
    ] {
        assert!(bevy_agent_core::collect_observation_with_mode(app.world_mut(), mode).is_err());
        assert_eq!(app.world().resource::<SimClock>().tick, 0);
    }
    let hybrid = platformer_observation(app.world_mut(), ObservationMode::Hybrid);
    assert!(matches!(hybrid, Observation::Hybrid { .. }));
}
