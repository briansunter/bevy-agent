use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, anyhow};
use bevy::prelude::*;
use bevy_agent_core::{
    AgentAction, AgentActionCatalog, AgentActionQueue, ScheduledAction, SimClock, SnapshotChecksum,
    SnapshotEntity, SnapshotId, StableEntityId, validate_future_action,
};

use crate::registry::{
    ComponentRestoreFn, ComponentValidateFn, ResourceRemoveFn, ResourceRestoreFn,
    ResourceValidateFn,
};
use crate::{
    FaultState, RollbackFailed, SNAPSHOT_SCHEMA_VERSION, Snapshot, SnapshotMetadata,
    SnapshotRegistry, SnapshotStore, SnapshotType, StableIdRemapHook, capture_snapshot,
    checksum_snapshot, checksum_snapshot_with_remap_exclusions,
};
pub fn restore_snapshot(world: &mut World, snapshot_id: SnapshotId) -> Result<SnapshotChecksum> {
    let snapshot = world
        .get_resource::<SnapshotStore>()
        .ok_or_else(|| anyhow!("SnapshotStore is not installed"))?
        .snapshots
        .get(&snapshot_id)
        .cloned()
        .ok_or_else(|| anyhow!("snapshot {snapshot_id:?} not found"))?;

    restore_snapshot_value(world, &snapshot)
}

/// Restore with an explicit stable-id remapping hook (see [`restore_snapshot_value_with_remap`]).
pub fn restore_snapshot_with_remap(
    world: &mut World,
    snapshot_id: SnapshotId,
    remap: Option<&StableIdRemapHook>,
) -> Result<SnapshotChecksum> {
    let snapshot = world
        .get_resource::<SnapshotStore>()
        .ok_or_else(|| anyhow!("SnapshotStore is not installed"))?
        .snapshots
        .get(&snapshot_id)
        .cloned()
        .ok_or_else(|| anyhow!("snapshot {snapshot_id:?} not found"))?;

    restore_snapshot_value_with_remap(world, &snapshot, remap)
}

/// Two-phase restore with atomicity guarantees.
///
/// # Entity references
///
/// Bevy [`Entity`](bevy::prelude::Entity) ids are reallocated on every restore; only
/// [`StableEntityId`](bevy_agent_core::StableEntityId) is preserved. Prefer stable-id-first components that
/// reference other entities by [`StableEntityId`](bevy_agent_core::StableEntityId) (for example
/// `Attack { target }`), which need no remapping because stable ids are
/// restored verbatim.
///
/// Restore allocates one entity per snapshot entry with
/// `(SnapshotEntity, StableEntityId)`, inserts ALL restored components, and
/// only then runs the optional `remap` hook as a fix-up pass over the live
/// components. The hook receives the `StableEntityId -> Entity` map and
/// should rewrite raw `Entity` fields in place (query live components and
/// patch them); it must not assume components are absent.
///
/// Resolve-phase integration: timeline/branch resolve flows that need
/// cross-timeline entity identity should run their id-resolution through
/// this same hook after components are installed, so fix-ups always observe
/// the final restored component values.
///
/// # Atomicity
///
/// Phase 1 (prepare) performs every fallible check *without touching the
/// world*: clock validation, schema hash, game/version metadata, duplicate
/// stable ids, registration resolution, typed `serde_json::from_value`
/// validation of all resources/components, and checksum preconditions.
/// Phase 2 (apply) clears entities and replaces resources/entities only
/// after prepare succeeds. If apply fails late (including the post-restore
/// checksum verification), the world is rolled back to a backup captured
/// before mutation, leaving the original semantic state unchanged. If the
/// rollback itself fails, a [`RollbackFailed`] error (carrying both the
/// original and rollback errors) is returned and the world is marked with a
/// [`FaultState`] resource instead of claiming success.
pub fn restore_snapshot_value(world: &mut World, snapshot: &Snapshot) -> Result<SnapshotChecksum> {
    restore_snapshot_value_with_remap(world, snapshot, None)
}

/// Options controlling post-restore checksum verification.
///
/// The default (`RestoreOptions::default()`) performs a full checksum
/// comparison: the re-captured world must equal the stored snapshot bit for
/// bit (modulo canonical ordering).
///
/// # Remap / checksum interaction
///
/// Raw Bevy [`Entity`](bevy::prelude::Entity) ids are reallocated on every restore and are NOT
/// stable across capture/restore. Components must reference other entities
/// by [`StableEntityId`](bevy_agent_core::StableEntityId) (stable-id-first, e.g. `Attack { target:
/// StableEntityId }`), which restores verbatim and verifies cleanly.
///
/// When a `remap` hook rewrites raw-`Entity` fields in place after restore,
/// the rewritten values legitimately differ from the captured bytes, so a
/// full checksum comparison would spuriously fail. Pass those component type
/// names in `excluded_components`
/// so verification recomputes both sides with
/// [`checksum_snapshot_with_remap_exclusions`], excluding the remapped
/// fix-up from the comparison. The prepare-phase checksum precondition still
/// uses the full checksum; only post-restore verification honors exclusions.
#[derive(Clone, Debug, Default)]
pub struct RestoreOptions {
    /// Component stable type IDs (as in [`crate::ComponentSnapshot::type_id`]) to
    /// exclude from post-restore checksum verification.
    pub excluded_components: Vec<String>,
}

impl RestoreOptions {
    /// Verification that ignores raw-`Entity` fix-ups in the given
    /// components (stable-payload comparison on both sides).
    #[must_use]
    pub fn without_entity_ids(excluded_components: &[&str]) -> Self {
        Self {
            excluded_components: excluded_components
                .iter()
                .map(|name| (*name).to_string())
                .collect(),
        }
    }
}

pub fn restore_snapshot_value_with_remap(
    world: &mut World,
    snapshot: &Snapshot,
    remap: Option<&StableIdRemapHook>,
) -> Result<SnapshotChecksum> {
    restore_snapshot_value_with_options(world, snapshot, remap, &RestoreOptions::default())
}

/// Restore with explicit checksum exclusions for remapped raw-`Entity`
/// components (see [`crate::RestoreOptions`]).
///
/// Equivalent to [`restore_snapshot_value_with_options`] with
/// `RestoreOptions::without_entity_ids(excluded_components)`.
pub fn restore_snapshot_value_with_remap_and_exclusions(
    world: &mut World,
    snapshot: &Snapshot,
    remap: Option<&StableIdRemapHook>,
    excluded_components: &[&str],
) -> Result<SnapshotChecksum> {
    restore_snapshot_value_with_options(
        world,
        snapshot,
        remap,
        &RestoreOptions::without_entity_ids(excluded_components),
    )
}

/// Canonical restore: two-phase apply plus configurable verification.
///
/// Behaves exactly like [`restore_snapshot_value_with_remap`] when `options`
/// is default; when `excluded_components` is non-empty, post-restore
/// verification compares [`checksum_snapshot_with_remap_exclusions`] on both
/// the stored and re-captured snapshots instead of the full checksum.
pub fn restore_snapshot_value_with_options(
    world: &mut World,
    snapshot: &Snapshot,
    remap: Option<&StableIdRemapHook>,
    options: &RestoreOptions,
) -> Result<SnapshotChecksum> {
    // ---- Phase 1: Prepare (no world mutation) ----
    let plan = prepare_restore_plan(world, snapshot)?;

    // Backup current semantic state before mutating, so late failures can
    // roll back. Capture itself is fallible but mutation-free.
    let backup = capture_snapshot(world, None)?;

    // ---- Phase 2: Apply ----
    // Rollback-failure paths install `FaultState`; early rejects above never do.
    if let Err(error) = apply_restore_plan(world, &plan, remap) {
        return Err(rollback_after_failure(
            world, &backup, remap, error, "apply",
        ));
    }

    // Post-restore verification: re-capture and compare checksums.
    // With explicit exclusions, both sides are recomputed with
    // `checksum_snapshot_with_remap_exclusions` so raw-`Entity` fix-ups
    // applied by the remap hook are excluded from the comparison.
    let verification = (|| -> Result<SnapshotChecksum> {
        let recaptured = capture_snapshot(world, snapshot.manifest.label.clone())?;
        let excluded: Vec<&str> = options
            .excluded_components
            .iter()
            .map(String::as_str)
            .collect();
        if !excluded.is_empty() {
            let actual = checksum_snapshot_with_remap_exclusions(&recaptured, &excluded)?;
            let expected = checksum_snapshot_with_remap_exclusions(snapshot, &excluded)?;
            if actual.hash != expected.hash || actual.tick != expected.tick {
                return Err(anyhow!(
                    "restored checksum mismatch (excluding {:?}): expected {:?}, got {:?}",
                    excluded,
                    expected,
                    actual
                ));
            }
            return Ok(actual);
        }
        let checksum = checksum_snapshot(&recaptured)?;
        if checksum.hash != snapshot.checksum.hash || checksum.tick != snapshot.checksum.tick {
            return Err(anyhow!(
                "restored checksum mismatch: expected {:?}, got {:?}",
                snapshot.checksum,
                checksum
            ));
        }
        Ok(checksum)
    })();
    match verification {
        Ok(checksum) => Ok(checksum),
        Err(error) => Err(rollback_after_failure(
            world,
            &backup,
            remap,
            error,
            "verification",
        )),
    }
}

struct RestorePlan {
    clock: SimClock,
    resource_restores: Vec<(ResourceRestoreFn, String, serde_json::Value)>,
    absent_removers: Vec<(ResourceRemoveFn, String)>,
    entity_restores: Vec<(StableEntityId, Vec<(ComponentRestoreFn, serde_json::Value)>)>,
    action_queue: Vec<ScheduledAction<AgentAction>>,
}

fn prepare_restore_plan(world: &World, snapshot: &Snapshot) -> Result<RestorePlan> {
    if snapshot.manifest.schema_version != SNAPSHOT_SCHEMA_VERSION {
        return Err(anyhow!(
            "snapshot schema version mismatch: expected {}, got {}",
            SNAPSHOT_SCHEMA_VERSION,
            snapshot.manifest.schema_version
        ));
    }
    let registry = world
        .get_resource::<SnapshotRegistry>()
        .ok_or_else(|| anyhow!("SnapshotRegistry is not installed"))?;
    // Clock validation: typed deserialization already produced `SimClock`;
    // run its semantic check to reject tick/dt anomalies (zero/NaN/infinite
    // dt, negative/non-finite elapsed, absurd tick) before touching the world.
    snapshot
        .clock
        .validate()
        .map_err(|error| anyhow!("invalid snapshot clock: {error}"))?;
    validate_structure(registry, snapshot)?;

    // Schema hash.
    {
        let registry = registry.schema_hash();
        if snapshot.manifest.schema_hash != registry {
            return Err(anyhow!(
                "snapshot schema hash mismatch: expected {registry}, got {}",
                snapshot.manifest.schema_hash
            ));
        }
    }

    // Game/version metadata: enforced whenever the snapshot names a real game.
    {
        let current = world
            .get_resource::<SnapshotMetadata>()
            .cloned()
            .unwrap_or_default();
        if current.agent_control_version != snapshot.manifest.agent_control_version {
            return Err(anyhow!(
                "snapshot agent control version mismatch: expected {}, got {}",
                current.agent_control_version,
                snapshot.manifest.agent_control_version
            ));
        }
        if snapshot.manifest.game_id != "unknown-game" && !snapshot.manifest.game_id.is_empty() {
            if current.game_id != snapshot.manifest.game_id {
                return Err(anyhow!(
                    "snapshot game mismatch: expected {}, got {}",
                    snapshot.manifest.game_id,
                    current.game_id
                ));
            }
            if current.game_version != snapshot.manifest.game_version {
                return Err(anyhow!(
                    "snapshot game version mismatch: expected {}, got {}",
                    snapshot.manifest.game_version,
                    current.game_version
                ));
            }
        }
    }

    // Duplicate stable ids.
    {
        let mut stable_ids = HashSet::with_capacity(snapshot.entities.len());
        for entity in &snapshot.entities {
            if !stable_ids.insert(entity.stable_id) {
                return Err(anyhow!(
                    "snapshot contains duplicate StableEntityId {}",
                    entity.stable_id.0
                ));
            }
        }
    }

    // Checksum precondition: the stored checksum must match the snapshot
    // payload, catching corruption/tampering before touching the world.
    {
        let recomputed = checksum_snapshot(snapshot)?;
        if recomputed.hash != snapshot.checksum.hash || recomputed.tick != snapshot.checksum.tick {
            return Err(anyhow!(
                "snapshot checksum precondition failed: stored {:?}, recomputed {:?}",
                snapshot.checksum,
                recomputed
            ));
        }
    }

    // Clone the needed registry fns up front (decouples borrows) and decode
    // every value to owned `serde_json::Value`s first.
    let (resource_fns, component_fns) = {
        let registry = world
            .get_resource::<SnapshotRegistry>()
            .ok_or_else(|| anyhow!("SnapshotRegistry is not installed"))?;
        let resource_fns: HashMap<
            String,
            (ResourceRestoreFn, ResourceValidateFn, ResourceRemoveFn),
        > = registry
            .resource_serializers
            .iter()
            .map(|(name, reg)| ((*name).to_string(), (reg.restore, reg.validate, reg.remove)))
            .collect();
        let component_fns: HashMap<String, (ComponentRestoreFn, ComponentValidateFn)> = registry
            .component_serializers
            .iter()
            .map(|(name, reg)| ((*name).to_string(), (reg.restore, reg.validate)))
            .collect();
        (resource_fns, component_fns)
    };

    // Resources: resolve + typed validation without touching the world.
    let mut resource_restores = Vec::with_capacity(snapshot.resources.len());
    for resource in &snapshot.resources {
        let (restore, validate, _) = resource_fns
            .get(resource.type_id.as_str())
            .copied()
            .ok_or_else(|| anyhow!("resource {} is not registered", resource.type_id))?;
        validate(&resource.value).with_context(|| {
            format!(
                "validating resource {} (late failure guarded)",
                resource.type_id
            )
        })?;
        resource_restores.push((restore, resource.type_id.clone(), resource.value.clone()));
    }

    // Absent resources: must be registered so removal is well-defined.
    let mut absent_removers = Vec::with_capacity(snapshot.absent_resources.len());
    for absent in &snapshot.absent_resources {
        if registry
            .resource_serializers
            .get(absent.as_str())
            .is_some_and(|registration| registration.required)
        {
            return Err(anyhow!(
                "required snapshot resource {absent} cannot be absent"
            ));
        }
        let (_, _, remove) = resource_fns
            .get(absent.as_str())
            .copied()
            .ok_or_else(|| anyhow!("absent resource {absent} is not registered"))?;
        absent_removers.push((remove, absent.clone()));
    }

    // Entities: resolve + typed validation without touching the world.
    let mut entity_restores = Vec::with_capacity(snapshot.entities.len());
    for entity_snapshot in &snapshot.entities {
        let mut components = Vec::with_capacity(entity_snapshot.components.len());
        for component in &entity_snapshot.components {
            let (restore, validate) = component_fns
                .get(component.type_id.as_str())
                .copied()
                .ok_or_else(|| anyhow!("component {} is not registered", component.type_id))?;
            validate(&component.value).with_context(|| {
                format!(
                    "validating component {} (late failure guarded)",
                    component.type_id
                )
            })?;
            components.push((restore, component.value.clone()));
        }
        entity_restores.push((entity_snapshot.stable_id, components));
    }

    // Deserialization checks built-in fields, while accepted future intent
    // also depends on the destination catalog and restored clock. Share the
    // core queue validator with the controller and checked queue replacement.
    let catalog = world
        .get_resource::<AgentActionCatalog>()
        .ok_or_else(|| anyhow!("AgentActionCatalog is not installed"))?;
    for action in &snapshot.action_queue {
        validate_future_action(catalog, snapshot.clock.tick, action)
            .map_err(|error| anyhow!("invalid queued action: {error}"))?;
    }

    Ok(RestorePlan {
        clock: snapshot.clock.clone(),
        resource_restores,
        absent_removers,
        entity_restores,
        action_queue: snapshot.action_queue.clone(),
    })
}

fn apply_restore_plan(
    world: &mut World,
    plan: &RestorePlan,
    remap: Option<&StableIdRemapHook>,
) -> Result<()> {
    clear_snapshot_entities(world);
    world.insert_resource(plan.clock.clone());

    for (restore, _name, value) in &plan.resource_restores {
        restore(world, value)?;
    }
    for (remove, _name) in &plan.absent_removers {
        remove(world);
    }

    // Allocate entities, insert restored components first, then run the
    // remap hook as a fix-up pass over live components. The hook observes
    // installed component values via the StableEntityId -> Entity map, so
    // raw-`Entity` fix-ups (and resolve-phase id resolution) always see the
    // final restored state.
    let mut id_map: HashMap<StableEntityId, Entity> =
        HashMap::with_capacity(plan.entity_restores.len());
    for (stable_id, _) in &plan.entity_restores {
        let entity = world.spawn((SnapshotEntity, *stable_id)).id();
        id_map.insert(*stable_id, entity);
    }
    for (stable_id, components) in &plan.entity_restores {
        let entity_id = id_map
            .get(stable_id)
            .copied()
            .ok_or_else(|| anyhow!("missing entity for StableEntityId {}", stable_id.0))?;
        let mut entity = world
            .get_entity_mut(entity_id)
            .map_err(|_| anyhow!("restored entity {:?} vanished", entity_id))?;
        for (restore, value) in components {
            restore(&mut entity, value)?;
        }
    }
    if let Some(remap) = remap {
        remap(world, &id_map);
    }

    if world.contains_resource::<AgentActionQueue>() {
        world.resource_scope(|world, mut queue: Mut<AgentActionQueue>| {
            let catalog = world
                .get_resource::<AgentActionCatalog>()
                .ok_or_else(|| anyhow!("AgentActionCatalog is not installed"))?;
            queue
                .replace_pending(catalog, plan.clock.tick, plan.action_queue.clone())
                .map_err(|error| anyhow!("invalid restored queue: {error}"))
        })?;
    }
    Ok(())
}

/// Restore a known-good transaction backup without capturing current state.
///
/// Use this only to roll back an outer transaction: current gameplay state may
/// be unserializable after that transaction fails. The backup is fully
/// validated before apply and the result is verified against its checksum.
/// This function creates no second backup; an apply or verification failure
/// may leave partial state, so the transaction owner must install [`FaultState`]
/// and reject further execution. Normal restoration uses
/// [`restore_snapshot_value`] with its automatic rollback instead.
pub fn restore_snapshot_backup(world: &mut World, backup: &Snapshot) -> Result<SnapshotChecksum> {
    let plan = prepare_restore_plan(world, backup)?;
    apply_restore_plan(world, &plan, None)?;
    let restored = capture_snapshot(world, None)?;
    if restored.checksum != backup.checksum {
        return Err(anyhow!(
            "transaction backup verification failed: expected {:?}, got {:?}",
            backup.checksum,
            restored.checksum
        ));
    }
    Ok(restored.checksum)
}

fn rollback_snapshot_with_remap(
    world: &mut World,
    backup: &Snapshot,
    remap: Option<&StableIdRemapHook>,
) -> Result<()> {
    let plan = prepare_restore_plan(world, backup)?;
    apply_restore_plan(world, &plan, remap)
}

/// Full offline validation of a snapshot payload (complete prepare without
/// world mutation).
///
/// Runs the entire `prepare_restore_plan` validation -- [`SimClock::validate`],
/// schema hash, game/version metadata, duplicate stable ids, checksum
/// recompute, typed `serde_json::from_value` decode of every
/// resource/component via the [`SnapshotRegistry`], and action-queue
/// round-trip -- without touching the world and without installing
/// [`FaultState`] on failure. The runner calls this per referenced snapshot
/// before transactional install; early rejects return `Err` with no
/// world mutation and no fault marker.
pub fn validate_snapshot_full(world: &World, snapshot: &Snapshot) -> Result<()> {
    prepare_restore_plan(world, snapshot).map(|_| ())
}
pub fn clear_snapshot_entities(world: &mut World) {
    let entities: Vec<Entity> = {
        let mut query = world.query_filtered::<Entity, With<SnapshotEntity>>();
        query.iter(world).collect()
    };

    for entity in entities {
        if let Ok(entity_mut) = world.get_entity_mut(entity) {
            entity_mut.despawn();
        }
    }
}

/// Validate identities and duplicated core state before deserialization or
/// world mutation. Every registered resource must be present or explicitly
/// absent; otherwise restore would retain destination-specific state.
fn validate_structure(registry: &SnapshotRegistry, snapshot: &Snapshot) -> Result<()> {
    if snapshot.manifest.snapshot_id.0.is_nil() {
        return Err(anyhow!("snapshot ID cannot be nil"));
    }
    if snapshot.manifest.created_from_timeline.0.is_nil() {
        return Err(anyhow!("snapshot timeline ID cannot be nil"));
    }
    if snapshot.manifest.tick != snapshot.clock.tick {
        return Err(anyhow!("snapshot manifest/clock tick mismatch"));
    }
    if snapshot.replay_state.replay_cursor_tick != snapshot.clock.tick {
        return Err(anyhow!("snapshot replay cursor/clock tick mismatch"));
    }
    let mut resource_names = HashSet::with_capacity(registry.resource_serializers.len());
    for resource in &snapshot.resources {
        let registration = registry
            .resource(&resource.type_id)
            .ok_or_else(|| anyhow!("resource {} is not registered", resource.type_id))?;
        if resource.schema_version != registration.schema_version() {
            return Err(anyhow!(
                "resource {} schema version mismatch: expected {}, got {}",
                resource.type_id,
                registration.schema_version(),
                resource.schema_version
            ));
        }
        if !resource_names.insert(resource.type_id.as_str()) {
            return Err(anyhow!(
                "snapshot contains duplicate resource {}",
                resource.type_id
            ));
        }
        if resource.type_id == <SimClock as SnapshotType>::TYPE_ID {
            let captured: SimClock = serde_json::from_value(resource.value.clone())
                .context("validating captured SimClock")?;
            if captured.tick != snapshot.clock.tick
                || captured.dt_seconds.to_bits() != snapshot.clock.dt_seconds.to_bits()
                || captured.elapsed_seconds.to_bits() != snapshot.clock.elapsed_seconds.to_bits()
            {
                return Err(anyhow!(
                    "snapshot SimClock resource disagrees with clock payload"
                ));
            }
        }
        if resource.type_id == <AgentActionQueue as SnapshotType>::TYPE_ID {
            let captured: AgentActionQueue = serde_json::from_value(resource.value.clone())
                .context("validating captured AgentActionQueue")?;
            if !captured.iter().eq(snapshot.action_queue.iter()) {
                return Err(anyhow!(
                    "snapshot AgentActionQueue resource disagrees with action queue payload"
                ));
            }
        }
    }
    for absent in &snapshot.absent_resources {
        if !resource_names.insert(absent.as_str()) {
            return Err(anyhow!(
                "snapshot resource {absent} is duplicated or both present and absent"
            ));
        }
        if absent == <SimClock as SnapshotType>::TYPE_ID {
            return Err(anyhow!("snapshot SimClock cannot be absent"));
        }
        if absent == <AgentActionQueue as SnapshotType>::TYPE_ID
            && !snapshot.action_queue.is_empty()
        {
            return Err(anyhow!(
                "snapshot absent AgentActionQueue has queued actions"
            ));
        }
    }
    for registered in registry.resource_serializers.keys() {
        if !resource_names.contains(registered) {
            return Err(anyhow!(
                "snapshot omits registered resource {registered} (must be present or absent)"
            ));
        }
    }
    let stable_name = <StableEntityId as SnapshotType>::TYPE_ID;
    for entity in &snapshot.entities {
        let mut component_names = HashSet::with_capacity(entity.components.len());
        for component in &entity.components {
            let registration = registry
                .component(&component.type_id)
                .ok_or_else(|| anyhow!("component {} is not registered", component.type_id))?;
            if component.schema_version != registration.schema_version() {
                return Err(anyhow!(
                    "component {} schema version mismatch: expected {}, got {}",
                    component.type_id,
                    registration.schema_version(),
                    component.schema_version
                ));
            }
            if !component_names.insert(component.type_id.as_str()) {
                return Err(anyhow!(
                    "snapshot entity {} contains duplicate component {}",
                    entity.stable_id.0,
                    component.type_id
                ));
            }
            if component.type_id == stable_name {
                let captured: StableEntityId = serde_json::from_value(component.value.clone())
                    .context("validating captured StableEntityId")?;
                if captured != entity.stable_id {
                    return Err(anyhow!(
                        "snapshot StableEntityId component disagrees with entity {}",
                        entity.stable_id.0
                    ));
                }
            }
        }
        if registry.component_serializers.contains_key(stable_name)
            && !component_names.contains(stable_name)
        {
            return Err(anyhow!(
                "snapshot entity {} omits registered StableEntityId component",
                entity.stable_id.0
            ));
        }
    }
    Ok(())
}

fn rollback_after_failure(
    world: &mut World,
    backup: &Snapshot,
    remap: Option<&StableIdRemapHook>,
    error: anyhow::Error,
    phase: &str,
) -> anyhow::Error {
    match rollback_snapshot_with_remap(world, backup, remap) {
        Ok(()) => error.context(format!("restore {phase} failed; rolled back")),
        Err(rollback_error) => {
            let original = format!("{error:?}");
            let rollback = format!("{rollback_error:?}");
            world.insert_resource(FaultState {
                message: format!(
                    "restore {phase} failed ({original}) and rollback failed ({rollback})"
                ),
            });
            anyhow!(RollbackFailed { original, rollback })
                .context(format!("restore {phase} failed; rollback failed"))
        }
    }
}
