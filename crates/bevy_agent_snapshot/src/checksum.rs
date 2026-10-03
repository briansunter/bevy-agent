use std::collections::HashSet;
use std::hash::Hasher;

use anyhow::Result;
use bevy_agent_core::{CHECKSUM_VERSION, SnapshotChecksum, StableHasher};

use crate::{SNAPSHOT_SCHEMA_VERSION, Snapshot};
pub fn checksum_snapshot(snapshot: &Snapshot) -> Result<SnapshotChecksum> {
    checksum_snapshot_with_remap_exclusions(snapshot, &[])
}

/// Stable-payload checksum excluding remapped raw-`Entity` components.
///
/// Full [`checksum_snapshot`] hashes every captured component value,
/// including raw Bevy [`Entity`](bevy::prelude::Entity) ids when a component stores them directly.
/// Raw `Entity` ids are reallocated on every restore, so a `remap` hook that
/// rewrites them in place would always fail full verification. This variant
/// skips the components named in `excluded_components` (matched against
/// [`crate::ComponentSnapshot::type_id`]); pass the stable type IDs of components whose
/// raw-`Entity` fields the remap hook fixes up.
///
/// Prefer stable-id-first components (references by [`StableEntityId`](bevy_agent_core::StableEntityId))
/// which need no exclusion; use this only for the raw-`Entity` fix-up set
/// passed to verification via [`crate::RestoreOptions`]. An empty exclusion list is
/// exactly [`checksum_snapshot`].
pub fn checksum_snapshot_with_remap_exclusions(
    snapshot: &Snapshot,
    excluded_components: &[&str],
) -> Result<SnapshotChecksum> {
    // Versioned canonical serialization: CHECKSUM_VERSION first, fixed-width
    // LE ints, explicit f32/f64 bits, sorted resources/entities/components
    // (capture already sorts; re-sort defensively for hand-built snapshots),
    // and sorted JSON object keys (via StableHasher::write_json).
    let excluded: HashSet<&str> = excluded_components.iter().copied().collect();
    let mut hasher = StableHasher::new();
    hasher.write_string("bevy_agent_snapshot");
    hasher.write_u32(SNAPSHOT_SCHEMA_VERSION);
    hasher.write_u32(CHECKSUM_VERSION);
    hasher.write_u64(snapshot.clock.tick);
    hasher.write_u32(snapshot.clock.dt_seconds.to_bits());
    hasher.write_u64(snapshot.clock.elapsed_seconds.to_bits());

    let mut resources = snapshot.resources.iter().collect::<Vec<_>>();
    resources.sort_by(|a, b| a.type_id.cmp(&b.type_id));
    hasher.write_u64(resources.len() as u64);
    for resource in resources {
        hasher.write_string(&resource.type_id);
        hasher.write_u32(resource.schema_version);
        hasher.write_json(&resource.value);
    }
    let mut absent = snapshot.absent_resources.iter().collect::<Vec<_>>();
    absent.sort();
    hasher.write_u64(absent.len() as u64);
    for name in absent {
        hasher.write_string(name);
    }
    let mut entities = snapshot.entities.iter().collect::<Vec<_>>();
    entities.sort_by_key(|entity| entity.stable_id.0);
    hasher.write_u64(entities.len() as u64);
    for entity in entities {
        hasher.write_u128(entity.stable_id.0);
        let mut components = entity.components.iter().collect::<Vec<_>>();
        components.sort_by(|a, b| a.type_id.cmp(&b.type_id));
        components.retain(|component| !excluded.contains(component.type_id.as_str()));
        hasher.write_u64(components.len() as u64);
        for component in components {
            hasher.write_string(&component.type_id);
            hasher.write_u32(component.schema_version);
            hasher.write_json(&component.value);
        }
    }
    hasher.write_u64(snapshot.action_queue.len() as u64);
    for action in &snapshot.action_queue {
        hasher.write_json(&serde_json::to_value(action)?);
    }
    hasher.write_u64(snapshot.replay_state.replay_cursor_tick);

    Ok(SnapshotChecksum {
        tick: snapshot.clock.tick,
        hash: hasher.finish_hash(),
    })
}
