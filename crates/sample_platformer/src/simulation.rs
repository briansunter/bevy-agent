//! Reset and deterministic gameplay systems scheduled by the sample plugin.

use bevy::prelude::*;
use bevy_agent_core::{
    AgentAction, AgentActionCatalog, CurrentInputFrame, EpisodeState, RewardState, SimClock,
    SnapshotEntity, StableEntityId, StableIdAllocator, validate_action_against_catalog,
    validate_clock_tick,
};
use bevy_agent_snapshot::clear_snapshot_entities;

use crate::model::*;

/// Recreates episode-local game state while retaining [`PlatformerConfig`].
///
/// Installed in `AgentResetSet::Game`; run the full `AgentReset` schedule when
/// resetting the clock, stable ID allocator, input queue, and game together.
pub fn reset_level(world: &mut World) {
    clear_snapshot_entities(world);
    *world.resource_mut::<GameScore>() = GameScore::default();
    *world.resource_mut::<PlatformerState>() = PlatformerState::default();
    *world.resource_mut::<LastMoveDirection>() = LastMoveDirection::default();

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

pub(crate) fn reset_tick_reward(mut reward: ResMut<RewardState>, episode: Res<EpisodeState>) {
    // Absorbing terminal semantics: once done/truncated, ticks must not
    // accumulate shaping rewards (post-terminal ticks are no-ops).
    if !episode.should_accumulate_reward() {
        reward.current_reward = 0.0;
        return;
    }
    reward.current_reward = -0.001;
    reward.cumulative_reward += reward.current_reward;
}

/// Compound input semantics:
///
/// * Multiple `Move` actions on the same tick are summed (not overwritten:
///   the last writer does not win) into a single horizontal base.
/// * `Dodge` adds a horizontal dash push (`DODGE_DASH_SPEED`) on top of that
///   base instead of scaling whatever `Move` wrote last. A standalone
///   `Dodge` therefore dashes in the facing direction: the current tick's
///   move direction when moving, else the last nonzero move direction
///   (`LastMoveDirection`, default `+X` when idle).
/// * Actions rejected by [`validate_action_against_catalog`] (unsupported
///   kinds, non-finite or out-of-`[-1, 1]` `Move` payloads) are skipped.
/// * When the episode is terminal (absorbing), inputs are ignored so the
///   player freezes until a reset.
pub(crate) fn apply_player_actions(
    input: Res<CurrentInputFrame>,
    catalog: Res<AgentActionCatalog>,
    episode: Res<EpisodeState>,
    mut last_direction: ResMut<LastMoveDirection>,
    mut player: Query<(&mut Velocity, &OnGround), With<Player>>,
) {
    let Ok((mut velocity, on_ground)) = player.single_mut() else {
        return;
    };

    if episode.is_terminal() {
        return;
    }

    let mut move_base = 0.0_f32;
    let mut dodge_count = 0_u32;
    let mut jumped = false;
    for action in &input.actions {
        if validate_action_against_catalog(&catalog, action).is_err() {
            continue;
        }
        match action {
            AgentAction::Move { x, .. } => {
                let clamped = x.clamp(-1.0, 1.0);
                move_base += clamped * PLAYER_SPEED;
                if clamped != 0.0 {
                    last_direction.0 = clamped.signum();
                }
            }
            AgentAction::Jump if on_ground.0 && !jumped => {
                velocity.linvel.y = JUMP_SPEED;
                jumped = true;
            }
            AgentAction::Dodge => {
                dodge_count += 1;
            }
            _ => {}
        }
    }

    // Sum, don't overwrite: dodge push composes with the move base.
    let mut horizontal = move_base;
    if dodge_count > 0 {
        let facing = if move_base != 0.0 {
            move_base.signum()
        } else {
            last_direction.0
        };
        horizontal += facing * DODGE_DASH_SPEED * dodge_count as f32;
    }
    velocity.linvel.x = horizontal.clamp(-MAX_HORIZONTAL_SPEED, MAX_HORIZONTAL_SPEED);
}

#[allow(clippy::type_complexity)]
pub(crate) fn physics_step(
    clock: Res<SimClock>,
    episode: Res<EpisodeState>,
    mut player: Query<(&mut Transform, &mut Velocity, &Collider, &mut OnGround), With<Player>>,
    platforms: Query<(&Transform, &Collider), (With<Platform>, Without<Player>)>,
) {
    if episode.is_terminal() {
        return;
    }

    // Validate imported clock values (snapshot/timeline restores): a corrupt
    // tick/dt would poison integration, so freeze instead of exploding.
    if validate_clock_tick(clock.tick).is_err()
        || !clock.dt_seconds.is_finite()
        || clock.dt_seconds <= 0.0
    {
        return;
    }
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

pub(crate) fn collect_coins(
    mut commands: Commands,
    player: Query<(&Transform, &Collider), With<Player>>,
    coins: Query<(Entity, &Transform, &Collider, &Coin)>,
    mut score: ResMut<GameScore>,
    mut state: ResMut<PlatformerState>,
    mut reward: ResMut<RewardState>,
    episode: Res<EpisodeState>,
) {
    // No reward accumulation post-terminal (absorbing episodes).
    if !episode.should_accumulate_reward() {
        return;
    }
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

/// Terminal semantics are absorbing: once `done`/`truncated` is set the
/// episode stays terminal until a reset. Post-terminal ticks are no-ops —
/// inputs are ignored (`apply_player_actions`), tick/coin rewards stop
/// accumulating (`reset_tick_reward`, `collect_coins`), and stepping past
/// terminal without a reset must be rejected at the controller boundary
/// (`EpisodeState::ensure_not_terminal`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn check_terminal_state(
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

pub(crate) fn aabb_overlap(a_pos: Vec3, a_half: Vec2, b_pos: Vec3, b_half: Vec2) -> bool {
    (a_pos.x - b_pos.x).abs() <= a_half.x + b_half.x
        && (a_pos.y - b_pos.y).abs() <= a_half.y + b_half.y
}
