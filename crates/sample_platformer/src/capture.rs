//! Read-only software capture and optional Bevy presentation for the sample.

use anyhow::Result;
use bevy::prelude::*;
use bevy_agent_core::{AgentControlState, SimClock, StableEntityId};
use bevy_agent_runner::{VisualCaptureOptions, VisualCaptureResult, visual_capture_path};
use image::{Rgba, RgbaImage};

use crate::model::*;

#[cfg(feature = "visual")]
use bevy::camera::ScalingMode;
#[cfg(feature = "visual")]
use bevy_agent_core::SnapshotEntity;

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

pub(crate) fn platformer_visual_capture(
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

    let mut rectangles = Vec::new();
    let mut query = world.query::<(
        Option<&StableEntityId>,
        &Transform,
        Option<&Collider>,
        Option<&Player>,
        Option<&Platform>,
        Option<&Coin>,
        Option<&Goal>,
    )>();
    for (stable_id, transform, collider, player, platform, coin, goal) in query.iter(world) {
        let half_extents = collider
            .map(|collider| collider.half_extents)
            .unwrap_or(Vec2::splat(0.25));
        let (layer, half_extents, color) = if platform.is_some() {
            (0, half_extents, Rgba([112, 124, 145, 255]))
        } else if coin.is_some() {
            (1, Vec2::splat(0.18), Rgba([245, 196, 52, 255]))
        } else if goal.is_some() {
            (2, half_extents, Rgba([47, 191, 103, 255]))
        } else if player.is_some() {
            (3, half_extents, Rgba([58, 144, 235, 255]))
        } else {
            continue;
        };
        rectangles.push((
            layer,
            stable_id.map(|id| id.0),
            transform.translation,
            half_extents,
            color,
        ));
    }
    // ECS allocation and archetype iteration order change during restore.
    // Explicit layers keep the player visible and stable IDs order each layer.
    rectangles.sort_by_key(|(layer, stable_id, center, half_extents, _)| {
        (
            *layer,
            *stable_id,
            center.to_array().map(f32::to_bits),
            half_extents.to_array().map(f32::to_bits),
        )
    });
    for (_, _, center, half_extents, color) in rectangles {
        draw_world_rect(&mut image, center, half_extents, color);
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

    // Clamp only intersecting rectangles. A fully offscreen entity should not
    // collapse into a colored line at the edge of the capture.
    if center.x + half_extents.x < WORLD_LEFT
        || center.x - half_extents.x > WORLD_RIGHT
        || center.y + half_extents.y < WORLD_BOTTOM
        || center.y - half_extents.y > WORLD_TOP
    {
        return;
    }

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
