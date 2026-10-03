//! Reference platformer for deterministic `bevy_agent_control` integration.
//!
//! The plugin wires together serializable gameplay state (`model`), deterministic
//! tick systems (`simulation`), read-only observations/checksums (`observation`),
//! and presentation (`capture`). Headless and visual builders share the same rules.
//!
//! Snapshots require the current registry schema, including every required game
//! resource. Partial or incompatible state is rejected before world mutation.

mod capture;
mod model;
mod observation;
mod simulation;

#[cfg(feature = "visual")]
pub use capture::PlatformerVisualPlugin;
pub use model::{
    Coin, Collider, DODGE_DASH_SPEED, GRAVITY, GameScore, Goal, JUMP_SPEED, LastMoveDirection,
    MAX_FALL_SPEED, MAX_HORIZONTAL_SPEED, OnGround, PLAYER_SPEED, Platform, PlatformerConfig,
    PlatformerState, Player, Velocity,
};
pub use simulation::reset_level;

use bevy::prelude::*;
use bevy_agent_core::{
    AgentActionKind, AgentControlAppExt, AgentReset, AgentResetSet, AgentSet, ObservationMode,
    StableEntityId,
};
use bevy_agent_runner::{AgentControlPlugins, VisualCaptureAppExt};
use bevy_agent_snapshot::{SnapshotAppExt, register_snapshot_components};

use capture::platformer_visual_capture;
use observation::{platformer_checksum, platformer_observation, platformer_observation_schema};
use simulation::{
    apply_player_actions, check_terminal_state, collect_coins, physics_step, reset_tick_reward,
};

#[cfg(feature = "visual")]
use bevy::window::WindowResolution;

pub struct PlatformerPlugin;

impl Plugin for PlatformerPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PlatformerConfig>()
            .init_resource::<GameScore>()
            .init_resource::<PlatformerState>()
            .init_resource::<LastMoveDirection>()
            .set_snapshot_metadata("sample_platformer", env!("CARGO_PKG_VERSION"))
            .set_supported_actions([
                AgentActionKind::Noop,
                AgentActionKind::Move,
                AgentActionKind::Jump,
                AgentActionKind::Dodge,
            ])
            .set_supported_observation_modes([
                ObservationMode::PlayerKnowledge,
                ObservationMode::Hybrid,
            ])
            .set_observation_schema(platformer_observation_schema())
            .expect("sample observation schema is valid");
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
        )
        .expect("sample snapshot component IDs and versions are valid");
        app.register_required_snapshot_resource::<GameScore>()
            .expect("score snapshot registration is valid")
            .register_required_snapshot_resource::<PlatformerState>()
            .expect("state snapshot registration is valid")
            .register_required_snapshot_resource::<PlatformerConfig>()
            .expect("config snapshot registration is valid")
            .register_required_snapshot_resource::<LastMoveDirection>()
            .expect("facing snapshot registration is valid")
            .insert_observation_extractor(platformer_observation)
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
        .add_plugins(AgentControlPlugins::default())
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
    .add_plugins(AgentControlPlugins::default())
    .add_plugins(PlatformerPlugin)
    .add_plugins(PlatformerVisualPlugin);
    app
}

#[cfg(test)]
mod tests;
