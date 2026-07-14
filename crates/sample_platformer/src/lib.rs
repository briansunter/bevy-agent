//! Headless sample platformer demonstrating `bevy_agent_control` integration.

use std::hash::Hash;

use anyhow::Result;
use bevy::prelude::*;
use bevy_agent_core::{
    AgentAction, AgentControlAppExt, AgentControlState, AgentReset, AgentResetSet, AgentSet,
    CurrentInputFrame, EntityObservation, EpisodeState, ObjectiveObservation, Observation,
    ObservationMode, PlayerObservation, RewardState, SimClock, SnapshotEntity, StableEntityId,
    StableHasher, StableIdAllocator, StateChecksum, SymbolicObservation,
};
use bevy_agent_runner::{
    AgentControlPlugins, VisualCaptureAppExt, VisualCaptureOptions, VisualCaptureResult,
    visual_capture_path,
};
use bevy_agent_snapshot::{
    SnapshotAppExt, clear_snapshot_entities, register_snapshot_components,
    register_snapshot_resources,
};
use image::{Rgba, RgbaImage};
use serde::{Deserialize, Serialize};

#[cfg(feature = "visual")]
use bevy::{camera::ScalingMode, window::WindowResolution};

pub const PLAYER_SPEED: f32 = 6.0;
pub const JUMP_SPEED: f32 = 9.5;
pub const GRAVITY: f32 = -24.0;
pub const MAX_FALL_SPEED: f32 = -18.0;

#[derive(Component, Clone, Debug, Serialize, Deserialize)]
pub struct Player {
    pub health: f32,
}

#[derive(Component, Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Velocity {
    pub linvel: Vec2,
}

#[derive(Component, Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Collider {
    pub half_extents: Vec2,
}

#[derive(Component, Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct OnGround(pub bool);

#[derive(Component, Clone, Debug, Serialize, Deserialize)]
pub struct Platform;

#[derive(Component, Clone, Debug, Serialize, Deserialize)]
pub struct Goal;

#[derive(Component, Clone, Debug, Serialize, Deserialize)]
pub struct Coin {
    pub value: i32,
}

#[derive(Resource, Clone, Debug, Serialize, Deserialize)]
pub struct PlatformerConfig {
    pub max_ticks: u64,
    pub death_y: f32,
}

impl Default for PlatformerConfig {
    fn default() -> Self {
        Self {
            max_ticks: 900,
            death_y: -8.0,
        }
    }
}

#[derive(Resource, Clone, Debug, Default, Serialize, Deserialize)]
pub struct GameScore {
    pub value: i32,
}

#[derive(Resource, Clone, Debug, Default, Serialize, Deserialize)]
pub struct PlatformerState {
    pub won: bool,
    pub coins_collected: u32,
}

pub struct PlatformerPlugin;

impl Plugin for PlatformerPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PlatformerConfig>()
            .init_resource::<GameScore>()
            .init_resource::<PlatformerState>()
            .set_snapshot_metadata("sample_platformer", env!("CARGO_PKG_VERSION"));
        register_snapshot_components!(
            app,
            StableEntityId,
            Transform,
            Player,
            Velocity,
            Collider,
            OnGround,
            Platform,
            Goal,
            Coin,
        );
        register_snapshot_resources!(app, GameScore, PlatformerState, PlatformerConfig);
        app.insert_observation_extractor(platformer_observation)
            .insert_checksum_extractor(platformer_checksum)
            .insert_visual_capture_renderer(platformer_visual_capture)
            .add_systems(AgentReset, reset_level.in_set(AgentResetSet::Game))
            .add_systems(
                bevy_agent_core::AgentTick,
                (
                    reset_tick_reward,
                    apply_player_actions,
                    physics_step,
                    collect_coins,
                )
                    .chain()
                    .in_set(AgentSet::Simulation),
            )
            .add_systems(
                bevy_agent_core::AgentTick,
                check_terminal_state.in_set(AgentSet::TerminalCheck),
            );
    }
}

pub fn build_headless_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(AgentControlPlugins::deterministic())
        .add_plugins(PlatformerPlugin);
    app
}

#[cfg(feature = "visual")]
pub fn build_visual_app() -> App {
    let mut app = App::new();
    app.add_plugins(
        DefaultPlugins
            .set(WindowPlugin {
                primary_window: Some(Window {
                    title: "bevy_agent_control sample platformer".to_string(),
                    resolution: WindowResolution::new(960, 540),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .set(ImagePlugin::default_nearest()),
    )
    .insert_resource(ClearColor(Color::srgb(0.08, 0.11, 0.16)))
    .add_plugins(AgentControlPlugins::visual_debug())
    .add_plugins(PlatformerPlugin)
    .add_plugins(PlatformerVisualPlugin);
    app
}

#[cfg(feature = "visual")]
pub struct PlatformerVisualPlugin;

#[cfg(feature = "visual")]
impl Plugin for PlatformerVisualPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, setup_visual_camera)
            .add_systems(Update, ensure_visual_sprites);
    }
}

#[cfg(feature = "visual")]
fn setup_visual_camera(mut commands: Commands) {
    commands.spawn((
        Camera2d,
        Projection::Orthographic(OrthographicProjection {
            scaling_mode: ScalingMode::FixedVertical {
                viewport_height: 10.0,
            },
            ..OrthographicProjection::default_2d()
        }),
        Transform::from_xyz(0.5, 2.4, 100.0),
    ));
}

#[cfg(feature = "visual")]
#[allow(clippy::type_complexity)]
fn ensure_visual_sprites(
    mut commands: Commands,
    entities: Query<
        (
            Entity,
            Option<&Player>,
            Option<&Platform>,
            Option<&Coin>,
            Option<&Goal>,
            Option<&Collider>,
        ),
        (With<SnapshotEntity>, Without<Sprite>),
    >,
) {
    for (entity, player, platform, coin, goal, collider) in &entities {
        let size = collider
            .map(|collider| collider.half_extents * 2.0)
            .unwrap_or(Vec2::splat(0.5));
        let sprite = if player.is_some() {
            Sprite::from_color(Color::srgb(0.18, 0.56, 0.95), size)
        } else if platform.is_some() {
            Sprite::from_color(Color::srgb(0.50, 0.56, 0.66), size)
        } else if coin.is_some() {
            Sprite::from_color(Color::srgb(0.96, 0.77, 0.20), Vec2::splat(0.45))
        } else if goal.is_some() {
            Sprite::from_color(Color::srgb(0.18, 0.78, 0.41), size)
        } else {
            continue;
        };
        commands.entity(entity).insert(sprite);
    }
}

pub fn reset_level(world: &mut World) {
    clear_snapshot_entities(world);
    *world.resource_mut::<GameScore>() = GameScore::default();
    *world.resource_mut::<PlatformerState>() = PlatformerState::default();

    spawn_player(world, Vec3::new(-6.0, 1.2, 0.0));
    spawn_platform(world, Vec3::new(0.0, -0.5, 0.0), Vec2::new(18.0, 1.0));
    spawn_platform(world, Vec3::new(-1.0, 2.0, 0.0), Vec2::new(3.0, 0.6));
    spawn_platform(world, Vec3::new(4.0, 4.0, 0.0), Vec2::new(3.0, 0.6));
    spawn_coin(world, Vec3::new(-1.0, 3.0, 0.0), 5);
    spawn_coin(world, Vec3::new(4.0, 5.0, 0.0), 10);
    spawn_goal(world, Vec3::new(7.5, 0.6, 0.0));
}

fn next_id(world: &mut World) -> StableEntityId {
    world.resource_mut::<StableIdAllocator>().allocate()
}

fn spawn_player(world: &mut World, translation: Vec3) {
    let id = next_id(world);
    world.spawn((
        SnapshotEntity,
        id,
        Player { health: 100.0 },
        Velocity::default(),
        Collider {
            half_extents: Vec2::new(0.35, 0.65),
        },
        OnGround(false),
        Transform::from_translation(translation),
    ));
}

fn spawn_platform(world: &mut World, translation: Vec3, size: Vec2) {
    let id = next_id(world);
    world.spawn((
        SnapshotEntity,
        id,
        Platform,
        Collider {
            half_extents: size / 2.0,
        },
        Transform::from_translation(translation),
    ));
}

fn spawn_coin(world: &mut World, translation: Vec3, value: i32) {
    let id = next_id(world);
    world.spawn((
        SnapshotEntity,
        id,
        Coin { value },
        Collider {
            half_extents: Vec2::splat(0.25),
        },
        Transform::from_translation(translation),
    ));
}

fn spawn_goal(world: &mut World, translation: Vec3) {
    let id = next_id(world);
    world.spawn((
        SnapshotEntity,
        id,
        Goal,
        Collider {
            half_extents: Vec2::new(0.45, 0.7),
        },
        Transform::from_translation(translation),
    ));
}

fn reset_tick_reward(mut reward: ResMut<RewardState>) {
    reward.current_reward = -0.001;
    reward.cumulative_reward += reward.current_reward;
}

fn apply_player_actions(
    input: Res<CurrentInputFrame>,
    mut player: Query<(&mut Velocity, &OnGround), With<Player>>,
) {
    let Ok((mut velocity, on_ground)) = player.single_mut() else {
        return;
    };

    velocity.linvel.x = 0.0;
    for action in &input.actions {
        match action {
            AgentAction::Move { x, .. } => {
                velocity.linvel.x = x.clamp(-1.0, 1.0) * PLAYER_SPEED;
            }
            AgentAction::Jump if on_ground.0 => {
                velocity.linvel.y = JUMP_SPEED;
            }
            AgentAction::Dodge => {
                velocity.linvel.x *= 1.75;
            }
            _ => {}
        }
    }
}

#[allow(clippy::type_complexity)]
fn physics_step(
    clock: Res<SimClock>,
    mut player: Query<(&mut Transform, &mut Velocity, &Collider, &mut OnGround), With<Player>>,
    platforms: Query<(&Transform, &Collider), (With<Platform>, Without<Player>)>,
) {
    let Ok((mut transform, mut velocity, collider, mut on_ground)) = player.single_mut() else {
        return;
    };

    let dt = clock.dt_seconds;
    let previous = transform.translation;
    velocity.linvel.y = (velocity.linvel.y + GRAVITY * dt).max(MAX_FALL_SPEED);
    transform.translation.x += velocity.linvel.x * dt;
    transform.translation.y += velocity.linvel.y * dt;
    on_ground.0 = false;

    for (platform_transform, platform_collider) in &platforms {
        let player_bottom = transform.translation.y - collider.half_extents.y;
        let previous_bottom = previous.y - collider.half_extents.y;
        let platform_top = platform_transform.translation.y + platform_collider.half_extents.y;

        let overlaps_x = (transform.translation.x - platform_transform.translation.x).abs()
            <= collider.half_extents.x + platform_collider.half_extents.x;
        let crossed_top = previous_bottom >= platform_top && player_bottom <= platform_top;
        if velocity.linvel.y <= 0.0 && overlaps_x && crossed_top {
            transform.translation.y = platform_top + collider.half_extents.y;
            velocity.linvel.y = 0.0;
            on_ground.0 = true;
        }
    }
}

fn collect_coins(
    mut commands: Commands,
    player: Query<(&Transform, &Collider), With<Player>>,
    coins: Query<(Entity, &Transform, &Collider, &Coin)>,
    mut score: ResMut<GameScore>,
    mut state: ResMut<PlatformerState>,
    mut reward: ResMut<RewardState>,
) {
    let Ok((player_transform, player_collider)) = player.single() else {
        return;
    };

    for (entity, coin_transform, coin_collider, coin) in &coins {
        if aabb_overlap(
            player_transform.translation,
            player_collider.half_extents,
            coin_transform.translation,
            coin_collider.half_extents,
        ) {
            commands.entity(entity).despawn();
            score.value += coin.value;
            state.coins_collected += 1;
            reward.current_reward += coin.value as f32;
            reward.cumulative_reward += coin.value as f32;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn check_terminal_state(
    clock: Res<SimClock>,
    config: Res<PlatformerConfig>,
    player: Query<(&Transform, &Collider), With<Player>>,
    goal: Query<(&Transform, &Collider), With<Goal>>,
    mut score: ResMut<GameScore>,
    mut state: ResMut<PlatformerState>,
    mut episode: ResMut<EpisodeState>,
    mut reward: ResMut<RewardState>,
) {
    if episode.done || episode.truncated {
        return;
    }

    let Ok((player_transform, player_collider)) = player.single() else {
        return;
    };

    if player_transform.translation.y < config.death_y {
        episode.done = true;
        episode.reason = Some("player_dead".to_string());
        reward.current_reward -= 25.0;
        reward.cumulative_reward -= 25.0;
        return;
    }

    for (goal_transform, goal_collider) in &goal {
        if aabb_overlap(
            player_transform.translation,
            player_collider.half_extents,
            goal_transform.translation,
            goal_collider.half_extents,
        ) {
            episode.done = true;
            episode.reason = Some("goal_reached".to_string());
            state.won = true;
            score.value += 100;
            reward.current_reward += 100.0;
            reward.cumulative_reward += 100.0;
            return;
        }
    }

    if clock.tick >= config.max_ticks {
        episode.truncated = true;
        episode.reason = Some("max_ticks".to_string());
    }
}

fn aabb_overlap(a_pos: Vec3, a_half: Vec2, b_pos: Vec3, b_half: Vec2) -> bool {
    (a_pos.x - b_pos.x).abs() <= a_half.x + b_half.x
        && (a_pos.y - b_pos.y).abs() <= a_half.y + b_half.y
}

fn platformer_visual_capture(
    world: &mut World,
    options: &VisualCaptureOptions,
) -> Result<VisualCaptureResult> {
    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 360;

    let tick = world.resource::<SimClock>().tick;
    let frame = world.resource::<AgentControlState>().frame;
    let path = visual_capture_path(options, tick, frame)?;

    let mut image = RgbaImage::from_pixel(WIDTH, HEIGHT, Rgba([18, 25, 36, 255]));
    draw_grid(&mut image, Rgba([28, 38, 52, 255]), 40);

    let mut query = world.query::<(
        &Transform,
        Option<&Collider>,
        Option<&Player>,
        Option<&Platform>,
        Option<&Coin>,
        Option<&Goal>,
    )>();
    for (transform, collider, player, platform, coin, goal) in query.iter(world) {
        let half_extents = collider
            .map(|collider| collider.half_extents)
            .unwrap_or(Vec2::splat(0.25));
        if platform.is_some() {
            draw_world_rect(
                &mut image,
                transform.translation,
                half_extents,
                Rgba([112, 124, 145, 255]),
            );
        } else if coin.is_some() {
            draw_world_rect(
                &mut image,
                transform.translation,
                Vec2::splat(0.18),
                Rgba([245, 196, 52, 255]),
            );
        } else if goal.is_some() {
            draw_world_rect(
                &mut image,
                transform.translation,
                half_extents,
                Rgba([47, 191, 103, 255]),
            );
        } else if player.is_some() {
            draw_world_rect(
                &mut image,
                transform.translation,
                half_extents,
                Rgba([58, 144, 235, 255]),
            );
        }
    }

    image.save(&path)?;
    Ok(VisualCaptureResult {
        tick,
        frame,
        path,
        width: WIDTH,
        height: HEIGHT,
        format: "png".to_string(),
    })
}

fn draw_grid(image: &mut RgbaImage, color: Rgba<u8>, spacing: u32) {
    if spacing == 0 {
        return;
    }

    for x in (0..image.width()).step_by(spacing as usize) {
        for y in 0..image.height() {
            image.put_pixel(x, y, color);
        }
    }
    for y in (0..image.height()).step_by(spacing as usize) {
        for x in 0..image.width() {
            image.put_pixel(x, y, color);
        }
    }
}

fn draw_world_rect(image: &mut RgbaImage, center: Vec3, half_extents: Vec2, color: Rgba<u8>) {
    const WORLD_LEFT: f32 = -8.5;
    const WORLD_RIGHT: f32 = 9.0;
    const WORLD_BOTTOM: f32 = -2.5;
    const WORLD_TOP: f32 = 7.5;

    let min = world_to_pixel(
        Vec2::new(center.x - half_extents.x, center.y + half_extents.y),
        image.width(),
        image.height(),
        WORLD_LEFT,
        WORLD_RIGHT,
        WORLD_BOTTOM,
        WORLD_TOP,
    );
    let max = world_to_pixel(
        Vec2::new(center.x + half_extents.x, center.y - half_extents.y),
        image.width(),
        image.height(),
        WORLD_LEFT,
        WORLD_RIGHT,
        WORLD_BOTTOM,
        WORLD_TOP,
    );
    let x_min = min.0.min(max.0);
    let x_max = min.0.max(max.0);
    let y_min = min.1.min(max.1);
    let y_max = min.1.max(max.1);

    for y in y_min..=y_max {
        for x in x_min..=x_max {
            image.put_pixel(x, y, color);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn world_to_pixel(
    point: Vec2,
    width: u32,
    height: u32,
    left: f32,
    right: f32,
    bottom: f32,
    top: f32,
) -> (u32, u32) {
    let normalized_x = ((point.x - left) / (right - left)).clamp(0.0, 1.0);
    let normalized_y = ((top - point.y) / (top - bottom)).clamp(0.0, 1.0);
    let x = (normalized_x * (width.saturating_sub(1)) as f32).round() as u32;
    let y = (normalized_y * (height.saturating_sub(1)) as f32).round() as u32;
    (x, y)
}

fn platformer_observation(world: &mut World, mode: ObservationMode) -> Observation {
    let tick = world.resource::<SimClock>().tick;
    let score_value = world.resource::<GameScore>().value;
    let state = world.resource::<PlatformerState>().clone();
    let episode = world.resource::<EpisodeState>().clone();

    let mut player_observation = PlayerObservation::default();
    let mut visible_entities = Vec::new();

    {
        let mut player_query = world.query_filtered::<(
            Option<&StableEntityId>,
            &Transform,
            &Player,
            Option<&Velocity>,
            Option<&OnGround>,
        ), With<Player>>();
        if let Some((stable_id, transform, player, velocity, on_ground)) =
            player_query.iter(world).next()
        {
            player_observation = PlayerObservation {
                stable_id: stable_id.copied(),
                position: transform.translation.to_array(),
                velocity: velocity
                    .map(|velocity| velocity.linvel.to_array())
                    .unwrap_or([0.0, 0.0]),
                health: player.health,
                score: score_value,
                on_ground: on_ground.map(|value| value.0).unwrap_or(false),
            };
        }
    }

    let mut entity_query = world.query::<(
        Option<&StableEntityId>,
        Option<&Transform>,
        Option<&Platform>,
        Option<&Coin>,
        Option<&Goal>,
    )>();
    for (stable_id, transform, platform, coin, goal) in entity_query.iter(world) {
        let stable_id = stable_id.copied();
        let position = transform
            .map(|transform| transform.translation.to_array())
            .unwrap_or([0.0, 0.0, 0.0]);
        if platform.is_some() {
            visible_entities.push(EntityObservation {
                stable_id,
                kind: "platform".to_string(),
                position,
                extra: serde_json::json!({}),
            });
        } else if let Some(coin) = coin {
            visible_entities.push(EntityObservation {
                stable_id,
                kind: "coin".to_string(),
                position,
                extra: serde_json::json!({ "value": coin.value }),
            });
        } else if goal.is_some() {
            visible_entities.push(EntityObservation {
                stable_id,
                kind: "goal".to_string(),
                position,
                extra: serde_json::json!({}),
            });
        }
    }

    visible_entities.sort_by_key(|entity| entity.stable_id.map(|id| id.0).unwrap_or_default());
    let symbolic = SymbolicObservation {
        tick,
        player: player_observation,
        visible_entities,
        inventory: Vec::new(),
        objectives: vec![ObjectiveObservation {
            id: "reach_goal".to_string(),
            complete: state.won,
            progress: if state.won { 1.0 } else { 0.0 },
        }],
    };

    match mode {
        ObservationMode::FullDebugState | ObservationMode::Hybrid => Observation::Hybrid {
            symbolic,
            pixels: None,
            debug: Some(serde_json::json!({
                "score": score_value,
                "coins_collected": state.coins_collected,
                "done": episode.done,
                "truncated": episode.truncated,
                "reason": episode.reason,
            })),
        },
        _ => Observation::Symbolic(symbolic),
    }
}

fn platformer_checksum(world: &mut World) -> StateChecksum {
    let clock = world.resource::<SimClock>().clone();
    let score = world.resource::<GameScore>().clone();
    let state = world.resource::<PlatformerState>().clone();
    let episode = world.resource::<EpisodeState>().clone();
    let mut hasher = StableHasher::new();

    clock.tick.hash(&mut hasher);
    clock.dt_seconds.to_bits().hash(&mut hasher);
    clock.elapsed_seconds.to_bits().hash(&mut hasher);
    score.value.hash(&mut hasher);
    state.won.hash(&mut hasher);
    state.coins_collected.hash(&mut hasher);
    episode.done.hash(&mut hasher);
    episode.truncated.hash(&mut hasher);
    episode.reason.hash(&mut hasher);

    let mut entity_rows = Vec::new();
    let mut query = world.query::<(
        Option<&StableEntityId>,
        Option<&Transform>,
        Option<&Velocity>,
        Option<&Player>,
        Option<&OnGround>,
        Option<&Platform>,
        Option<&Goal>,
        Option<&Coin>,
    )>();
    for (stable_id, transform, velocity, player, on_ground, platform, goal, coin) in
        query.iter(world)
    {
        let Some(stable_id) = stable_id.copied() else {
            continue;
        };
        entity_rows.push((
            stable_id.0,
            transform
                .map(|value| value.translation.x.to_bits())
                .unwrap_or(0),
            transform
                .map(|value| value.translation.y.to_bits())
                .unwrap_or(0),
            velocity.map(|value| value.linvel.x.to_bits()).unwrap_or(0),
            velocity.map(|value| value.linvel.y.to_bits()).unwrap_or(0),
            player
                .map(|value| value.health.to_bits())
                .unwrap_or_default(),
            on_ground.map(|value| value.0).unwrap_or_default(),
            platform.is_some(),
            goal.is_some(),
            coin.map(|coin| coin.value).unwrap_or_default(),
        ));
    }
    entity_rows.sort_by_key(|row| row.0);
    entity_rows.hash(&mut hasher);

    StateChecksum {
        tick: clock.tick,
        hash: hasher.finish_hash(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
