//! Compact observations and canonical checksums of authoritative gameplay state.

use std::hash::Hash;

use bevy::prelude::*;
use bevy_agent_core::{
    EntityObservation, EnvironmentChecksum, EpisodeState, ObjectiveObservation, Observation,
    ObservationMode, PlayerObservation, RewardState, SimClock, StableEntityId, StableHasher,
    SymbolicObservation,
};

use crate::model::*;

/// Both supported modes have the same symbolic body. The schema describes the
/// complete wire observation, including the domain-specific debug payload.
pub(crate) fn platformer_observation_schema() -> serde_json::Value {
    let symbolic = serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["tick", "player", "visible_entities", "inventory", "objectives"],
        "properties": {
            "tick": { "type": "integer", "minimum": 0 },
            "player": {
                "type": "object",
                "additionalProperties": false,
                "required": ["stable_id", "position", "velocity", "health", "score", "on_ground"],
                "properties": {
                    "stable_id": { "$ref": "#/$defs/stable_id" },
                    "position": { "$ref": "#/$defs/vec3" },
                    "velocity": { "type": "array", "items": { "type": "number" }, "minItems": 2, "maxItems": 2 },
                    "health": { "type": "number" },
                    "score": { "type": "integer" },
                    "on_ground": { "type": "boolean" }
                }
            },
            "visible_entities": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["stable_id", "kind", "position", "extra"],
                    "properties": {
                        "stable_id": { "$ref": "#/$defs/stable_id" },
                        "kind": { "enum": ["platform", "coin", "goal"] },
                        "position": { "$ref": "#/$defs/vec3" },
                        "extra": { "type": "object" }
                    }
                }
            },
            "inventory": { "type": "array", "maxItems": 0 },
            "objectives": {
                "type": "array", "minItems": 1, "maxItems": 1,
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["id", "complete", "progress"],
                    "properties": {
                        "id": { "const": "reach_goal" },
                        "complete": { "type": "boolean" },
                        "progress": { "type": "number", "minimum": 0, "maximum": 1 }
                    }
                }
            }
        }
    });
    let mut symbolic_envelope = symbolic.clone();
    symbolic_envelope["properties"]["kind"] = serde_json::json!({ "const": "Symbolic" });
    symbolic_envelope["required"]
        .as_array_mut()
        .expect("symbolic required fields are an array")
        .push(serde_json::json!("kind"));
    serde_json::json!({
        "title": "PlatformerObservation",
        "$defs": {
            "stable_id": { "type": ["integer", "null"], "minimum": 0 },
            "vec3": { "type": "array", "items": { "type": "number" }, "minItems": 3, "maxItems": 3 },
            "symbolic": symbolic
        },
        "oneOf": [symbolic_envelope, {
            "type": "object",
            "additionalProperties": false,
            "required": ["kind", "symbolic", "pixels", "debug"],
            "properties": {
                "kind": { "const": "Hybrid" },
                "symbolic": { "$ref": "#/$defs/symbolic" },
                "pixels": { "type": "null" },
                "debug": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["score", "coins_collected", "done", "truncated", "reason"],
                    "properties": {
                        "score": { "type": "integer" },
                        "coins_collected": { "type": "integer", "minimum": 0 },
                        "done": { "type": "boolean" },
                        "truncated": { "type": "boolean" },
                        "reason": { "type": ["string", "null"] }
                    }
                }
            }
        }]
    })
}

/// Observation modes: only Symbolic and Hybrid are implemented. `Hybrid`
/// returns symbolic plus a debug payload; `PlayerKnowledge` maps to plain
/// `Symbolic`. The domain catalog rejects all other modes before extraction.
/// Pixels are always `None`; software capture is a separate API.
pub(crate) fn platformer_observation(world: &mut World, mode: ObservationMode) -> Observation {
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
        if let Ok((stable_id, transform, player, velocity, on_ground)) = player_query.single(world)
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
        ObservationMode::Hybrid => Observation::Hybrid {
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
        ObservationMode::PlayerKnowledge => Observation::Symbolic(symbolic),
        _ => unreachable!("unsupported modes must be rejected by the domain catalog"),
    }
}

/// Every optional component retains its presence in the checksum: an absent
/// coin or velocity must not alias a present component with a zero value.
#[derive(Hash)]
struct EntityChecksumRow {
    stable_id: u128,
    transform: Option<([u32; 3], [u32; 4], [u32; 3])>,
    velocity: Option<[u32; 2]>,
    collider: Option<[u32; 2]>,
    health: Option<u32>,
    on_ground: Option<bool>,
    platform: bool,
    goal: bool,
    coin: Option<i32>,
}

pub(crate) fn platformer_checksum(world: &mut World) -> EnvironmentChecksum {
    let clock = world.resource::<SimClock>().clone();
    let score = world.resource::<GameScore>().clone();
    let state = world.resource::<PlatformerState>().clone();
    let episode = world.resource::<EpisodeState>().clone();
    let config = world.resource::<PlatformerConfig>();
    let direction = world.resource::<LastMoveDirection>();
    let reward = world.resource::<RewardState>();
    let mut hasher = StableHasher::new();

    // Domain-local version: changing the covered gameplay state changes this
    // checksum contract without changing the core hasher's wire-format version.
    hasher.write_string("sample_platformer/state/v2");
    clock.tick.hash(&mut hasher);
    clock.dt_seconds.to_bits().hash(&mut hasher);
    clock.elapsed_seconds.to_bits().hash(&mut hasher);
    score.value.hash(&mut hasher);
    state.won.hash(&mut hasher);
    state.coins_collected.hash(&mut hasher);
    episode.done.hash(&mut hasher);
    episode.truncated.hash(&mut hasher);
    episode.reason.hash(&mut hasher);
    config.max_ticks.hash(&mut hasher);
    config.death_y.to_bits().hash(&mut hasher);
    direction.0.to_bits().hash(&mut hasher);
    reward.current_reward.to_bits().hash(&mut hasher);
    reward.cumulative_reward.to_bits().hash(&mut hasher);

    let mut entity_rows = Vec::new();
    let mut query = world.query::<(
        Option<&StableEntityId>,
        Option<&Transform>,
        Option<&Velocity>,
        Option<&Collider>,
        Option<&Player>,
        Option<&OnGround>,
        Option<&Platform>,
        Option<&Goal>,
        Option<&Coin>,
    )>();
    for (stable_id, transform, velocity, collider, player, on_ground, platform, goal, coin) in
        query.iter(world)
    {
        let Some(stable_id) = stable_id.copied() else {
            continue;
        };
        entity_rows.push(EntityChecksumRow {
            stable_id: stable_id.0,
            transform: transform.map(|value| {
                (
                    value.translation.to_array().map(f32::to_bits),
                    value.rotation.to_array().map(f32::to_bits),
                    value.scale.to_array().map(f32::to_bits),
                )
            }),
            velocity: velocity.map(|value| value.linvel.to_array().map(f32::to_bits)),
            collider: collider.map(|value| value.half_extents.to_array().map(f32::to_bits)),
            health: player.map(|value| value.health.to_bits()),
            on_ground: on_ground.map(|value| value.0),
            platform: platform.is_some(),
            goal: goal.is_some(),
            coin: coin.map(|value| value.value),
        });
    }
    entity_rows.sort_by_key(|row| row.stable_id);
    entity_rows.hash(&mut hasher);

    EnvironmentChecksum {
        tick: clock.tick,
        hash: hasher.finish_hash(),
    }
}
