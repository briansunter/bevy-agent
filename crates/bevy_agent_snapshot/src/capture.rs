use std::collections::HashSet;

use anyhow::{Result, anyhow};
use bevy::prelude::*;
use bevy_agent_core::{
    AgentActionQueue, AgentControlState, SimClock, SnapshotChecksum, SnapshotEntity, SnapshotId,
    StableEntityId,
};

use crate::{
    EntitySnapshot, SNAPSHOT_SCHEMA_VERSION, Snapshot, SnapshotManifest, SnapshotMetadata,
    SnapshotRegistry, SnapshotReplayState, SnapshotRole, SnapshotStore, checksum_snapshot,
};
pub fn capture_snapshot(world: &mut World, label: Option<String>) -> Result<Snapshot> {
    let (schema_hash, mut resource_regs, mut component_regs) = {
        let registry = world
            .get_resource::<SnapshotRegistry>()
            .ok_or_else(|| anyhow!("SnapshotRegistry is not installed"))?;
        (
            registry.schema_hash(),
            registry
                .resource_serializers
                .values()
                .cloned()
                .collect::<Vec<_>>(),
            registry
                .component_serializers
                .values()
                .cloned()
                .collect::<Vec<_>>(),
        )
    };

    let clock = world
        .get_resource::<SimClock>()
        .cloned()
        .ok_or_else(|| anyhow!("SimClock is not installed"))?;
    clock
        .validate()
        .map_err(|error| anyhow!("invalid snapshot clock: {error}"))?;
    let metadata = world
        .get_resource::<SnapshotMetadata>()
        .cloned()
        .unwrap_or_default();
    let control = world
        .get_resource::<AgentControlState>()
        .cloned()
        .unwrap_or_default();

    let mut resources = Vec::new();
    let mut absent_resources = Vec::new();
    resource_regs.sort_by_key(|registration| registration.type_id);
    for registration in resource_regs {
        if let Some(snapshot) = (registration.capture)(world)? {
            resources.push(snapshot);
        } else {
            if registration.required {
                return Err(anyhow!(
                    "required snapshot resource {} is missing",
                    registration.type_id
                ));
            }
            absent_resources.push(registration.type_id.to_string());
        }
    }
    resources.sort_by(|a, b| a.type_id.cmp(&b.type_id));
    absent_resources.sort();

    component_regs.sort_by_key(|registration| registration.type_id);

    let mut entities = Vec::new();
    let entity_ids = {
        let mut query = world.query_filtered::<Entity, With<SnapshotEntity>>();
        query.iter(world).collect::<Vec<_>>()
    };

    let mut stable_ids = HashSet::with_capacity(entity_ids.len());
    for entity_id in entity_ids {
        let entity = world.entity(entity_id);
        if !entity.contains::<SnapshotEntity>() {
            continue;
        }

        let stable_id = entity.get::<StableEntityId>().copied().ok_or_else(|| {
            anyhow!(
                "snapshot entity {:?} is missing StableEntityId",
                entity.id()
            )
        })?;

        if !stable_ids.insert(stable_id) {
            return Err(anyhow!(
                "snapshot entities contain duplicate StableEntityId {}",
                stable_id.0
            ));
        }

        let mut components = Vec::new();
        for registration in &component_regs {
            if let Some(snapshot) = (registration.capture)(&entity)? {
                components.push(snapshot);
            }
        }
        components.sort_by(|a, b| a.type_id.cmp(&b.type_id));

        entities.push(EntitySnapshot {
            stable_id,
            archetype_hint: None,
            components,
        });
    }
    entities.sort_by_key(|entity| entity.stable_id.0);

    let action_queue = world
        .get_resource::<AgentActionQueue>()
        .map(|queue| queue.iter().cloned().collect())
        .unwrap_or_default();

    let snapshot_id = SnapshotId::new();
    let episode_id = world
        .get_resource::<SnapshotStore>()
        .map(|store| store.episode_id)
        .unwrap_or_default();
    let manifest = SnapshotManifest {
        schema_version: SNAPSHOT_SCHEMA_VERSION,
        snapshot_id,
        tick: clock.tick,
        label,
        game_id: metadata.game_id,
        game_version: metadata.game_version,
        agent_control_version: metadata.agent_control_version,
        schema_hash,
        created_from_timeline: control.timeline_id,
        role: SnapshotRole::Manual,
        episode_id,
    };

    let clock_tick = clock.tick;
    let mut snapshot = Snapshot {
        manifest,
        clock,
        resources,
        absent_resources,
        entities,
        action_queue,
        replay_state: SnapshotReplayState {
            replay_cursor_tick: clock_tick,
        },
        checksum: SnapshotChecksum { tick: 0, hash: 0 },
    };
    snapshot.checksum = checksum_snapshot(&snapshot)?;
    crate::validate_snapshot_full(world, &snapshot)?;
    Ok(snapshot)
}
