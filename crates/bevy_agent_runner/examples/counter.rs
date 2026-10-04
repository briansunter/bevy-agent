//! A complete headless environment using only published runtime crates.

use bevy::prelude::*;
use bevy_agent_core::{
    AgentAction, AgentActionKind, AgentControlAppExt, AgentReset, AgentResetSet, AgentSet,
    AgentTick, EnvironmentChecksum, Observation, ObservationMode, SimClock, StableHasher,
    default_checksum,
};
use bevy_agent_runner::{AgentApp, AgentControlPlugins, AgentEnvironment, ResetOptions};
use bevy_agent_snapshot::{SnapshotAppExt, SnapshotType};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Resource, Clone, Default, Serialize, Deserialize)]
struct Counter(u64);

impl SnapshotType for Counter {
    const TYPE_ID: &'static str = "counter.value";
    const SCHEMA_VERSION: u32 = 1;
}

fn build_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(AgentControlPlugins::default())
        .init_resource::<Counter>()
        .set_snapshot_metadata("counter", env!("CARGO_PKG_VERSION"))
        .set_supported_actions([AgentActionKind::Noop])
        .set_supported_observation_modes([ObservationMode::Hybrid])
        .insert_observation_extractor(|world, _mode| Observation::Domain {
            tick: world.resource::<SimClock>().tick,
            value: json!({ "count": world.resource::<Counter>().0 }),
        })
        .insert_checksum_extractor(checksum)
        .add_systems(AgentReset, reset_counter.in_set(AgentResetSet::Game))
        .add_systems(AgentTick, increment_counter.in_set(AgentSet::Simulation));
    app.register_required_snapshot_resource::<Counter>()
        .expect("counter snapshot identity is valid");
    app.set_observation_schema(json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["kind", "tick", "value"],
        "properties": {
            "kind": { "const": "Domain" },
            "tick": { "type": "integer", "minimum": 0 },
            "value": {
                "type": "object",
                "additionalProperties": false,
                "required": ["count"],
                "properties": { "count": { "type": "integer", "minimum": 0 } }
            }
        }
    }))
    .expect("counter observation schema is valid");
    app
}

fn reset_counter(mut counter: ResMut<Counter>) {
    counter.0 = 0;
}

fn increment_counter(mut counter: ResMut<Counter>) {
    counter.0 += 1;
}

fn checksum(world: &mut World) -> EnvironmentChecksum {
    let core = default_checksum(world);
    let mut hasher = StableHasher::new();
    hasher.write_json(&json!({
        "core": core.hash,
        "count": world.resource::<Counter>().0,
    }));
    EnvironmentChecksum {
        tick: core.tick,
        hash: hasher.finish_hash(),
    }
}

fn main() -> anyhow::Result<()> {
    let mut env = AgentApp::new(build_app)?;
    env.reset(ResetOptions::default())?;
    env.step(AgentAction::Noop)?;
    let snapshot = env.snapshot()?;
    let first = env.step(AgentAction::Noop)?;
    env.restore(snapshot.snapshot_id)?;
    let repeated = env.step(AgentAction::Noop)?;
    assert_eq!(first.checksum, repeated.checksum);
    assert_eq!(env.world().resource::<Counter>().0, 2);
    env.restore_tick(1)?;
    assert_eq!(env.world().resource::<Counter>().0, 1);
    println!("Snapshot restore and replay matched; counter is back at tick 1.");
    Ok(())
}
