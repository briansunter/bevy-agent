//! Serializable authoritative gameplay components, resources, and tuning constants.

use bevy::prelude::*;
use bevy_agent_snapshot::SnapshotType;
use serde::{Deserialize, Serialize};

pub const PLAYER_SPEED: f32 = 6.0;
pub const JUMP_SPEED: f32 = 9.5;
pub const GRAVITY: f32 = -24.0;
pub const MAX_FALL_SPEED: f32 = -18.0;
/// Horizontal push added per `Dodge` on top of the summed `Move` base.
/// Standalone `Dodge` therefore dashes at this speed in the facing/last-move
/// direction (`+X` when idle); `Move + Dodge` on the same tick composes by
/// summation instead of overwriting.
pub const DODGE_DASH_SPEED: f32 = 6.0;
/// Clamp for the composed horizontal velocity so stacked dodges stay sane.
pub const MAX_HORIZONTAL_SPEED: f32 = 15.0;

/// Last nonzero horizontal move direction, used as the facing for a
/// standalone `Dodge` dash. Defaults to `+X` when the player has never moved.
/// This is authoritative state: reset, snapshot, and checksum paths include it.
#[derive(Resource, Clone, Copy, Debug, Serialize, Deserialize)]
pub struct LastMoveDirection(pub f32);

impl Default for LastMoveDirection {
    fn default() -> Self {
        Self(1.0)
    }
}

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

/// Episode limits retained across resets and captured by snapshots.
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

macro_rules! snapshot_types {
    ($($type:ty => $id:literal),+ $(,)?) => {
        $(
            impl SnapshotType for $type {
                const TYPE_ID: &'static str = $id;
                const SCHEMA_VERSION: u32 = 1;
            }
        )+
    };
}

// Stable wire IDs belong to the game contract and survive Rust module moves.
snapshot_types! {
    Player => "sample_platformer.player",
    Velocity => "sample_platformer.velocity",
    Collider => "sample_platformer.collider",
    OnGround => "sample_platformer.on_ground",
    Platform => "sample_platformer.platform",
    Goal => "sample_platformer.goal",
    Coin => "sample_platformer.coin",
    PlatformerConfig => "sample_platformer.config",
    GameScore => "sample_platformer.score",
    PlatformerState => "sample_platformer.state",
    LastMoveDirection => "sample_platformer.last_move_direction",
}
