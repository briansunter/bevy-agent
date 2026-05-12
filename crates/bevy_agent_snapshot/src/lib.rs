use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use anyhow::{Context, Result, anyhow};
use bevy::ecs::world::{EntityRef, EntityWorldMut};
use bevy::prelude::*;
use bevy_agent_core::{
    AgentAction, AgentActionQueue, AgentControlState, AgentSet, ScheduledAction, SimClock,
    SnapshotEntity, SnapshotId, StableEntityId, StateChecksum, TimelineId,
};
use serde::{Deserialize, Serialize};

type ComponentCaptureFn = fn(&EntityRef<'_>) -> Result<Option<ComponentSnapshot>>;
type ComponentRestoreFn = fn(&mut EntityWorldMut<'_>, &serde_json::Value) -> Result<()>;
type ResourceCaptureFn = fn(&World) -> Result<Option<ResourceSnapshot>>;
type ResourceRestoreFn = fn(&mut World, &serde_json::Value) -> Result<()>;

#[derive(Clone)]
pub struct ComponentRegistration {
    pub type_name: &'static str,
    pub capture: ComponentCaptureFn,
    pub restore: ComponentRestoreFn,
}

#[derive(Clone)]
pub struct ResourceRegistration {
    pub type_name: &'static str,
    pub capture: ResourceCaptureFn,
    pub restore: ResourceRestoreFn,
}

#[derive(Default)]
pub struct SnapshotRegistry {
    pub component_serializers: HashMap<&'static str, ComponentRegistration>,
    pub resource_serializers: HashMap<&'static str, ResourceRegistration>,
}

impl Resource for SnapshotRegistry {}

#[derive(Resource, Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotMetadata {
    pub game_id: String,
    pub game_version: String,
    pub agent_control_version: String,
}

impl Default for SnapshotMetadata {
    fn default() -> Self {
        Self {
            game_id: "unknown-game".to_string(),
            game_version: env!("CARGO_PKG_VERSION").to_string(),
            agent_control_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

#[derive(Resource, Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotPolicy {
    pub checkpoint_every_ticks: u64,
    pub keep_last_n_checkpoints: usize,
    pub checkpoint_on_terminal: bool,
    pub checkpoint_on_branch: bool,
}

impl Default for SnapshotPolicy {
    fn default() -> Self {
        Self {
            checkpoint_every_ticks: 120,
            keep_last_n_checkpoints: 100,
            checkpoint_on_terminal: true,
            checkpoint_on_branch: true,
        }
    }
}

#[derive(Resource, Clone, Debug, Default, Serialize, Deserialize)]
pub struct SnapshotStore {
    pub snapshots: HashMap<SnapshotId, Snapshot>,
    pub labels: HashMap<String, SnapshotId>,
    pub checkpoints: Vec<SnapshotId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub manifest: SnapshotManifest,
    pub clock: SimClock,
    pub resources: Vec<ResourceSnapshot>,
    pub entities: Vec<EntitySnapshot>,
    pub action_queue: Vec<ScheduledAction<AgentAction>>,
    pub replay_state: SnapshotReplayState,
    pub checksum: StateChecksum,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotManifest {
    pub snapshot_id: SnapshotId,
    pub tick: u64,
    pub label: Option<String>,
    pub game_id: String,
    pub game_version: String,
    pub agent_control_version: String,
    pub schema_hash: String,
    pub created_from_timeline: TimelineId,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SnapshotReplayState {
    pub replay_cursor_tick: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EntitySnapshot {
    pub stable_id: StableEntityId,
    pub archetype_hint: Option<String>,
    pub components: Vec<ComponentSnapshot>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ComponentSnapshot {
    pub type_name: String,
    pub value: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResourceSnapshot {
    pub type_name: String,
    pub value: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotCreateResult {
    pub snapshot_id: SnapshotId,
    pub tick: u64,
    pub checksum: StateChecksum,
}

pub trait SnapshotAppExt {
    fn register_snapshot_component<T>(&mut self) -> &mut Self
    where
        T: Component + Clone + Serialize + for<'de> Deserialize<'de> + 'static;

    fn register_snapshot_resource<T>(&mut self) -> &mut Self
    where
        T: Resource + Clone + Serialize + for<'de> Deserialize<'de> + 'static;

    fn set_snapshot_metadata(
        &mut self,
        game_id: impl Into<String>,
        game_version: impl Into<String>,
    ) -> &mut Self;
}

impl SnapshotAppExt for App {
    fn register_snapshot_component<T>(&mut self) -> &mut Self
    where
        T: Component + Clone + Serialize + for<'de> Deserialize<'de> + 'static,
    {
        if !self.world().contains_resource::<SnapshotRegistry>() {
            self.init_resource::<SnapshotRegistry>();
        }
        self.world_mut()
            .resource_mut::<SnapshotRegistry>()
            .register_component::<T>();
        self
    }

    fn register_snapshot_resource<T>(&mut self) -> &mut Self
    where
        T: Resource + Clone + Serialize + for<'de> Deserialize<'de> + 'static,
    {
        if !self.world().contains_resource::<SnapshotRegistry>() {
            self.init_resource::<SnapshotRegistry>();
        }
        self.world_mut()
            .resource_mut::<SnapshotRegistry>()
            .register_resource::<T>();
        self
    }

    fn set_snapshot_metadata(
        &mut self,
        game_id: impl Into<String>,
        game_version: impl Into<String>,
    ) -> &mut Self {
        let mut metadata = self
            .world()
            .get_resource::<SnapshotMetadata>()
            .cloned()
            .unwrap_or_default();
        metadata.game_id = game_id.into();
        metadata.game_version = game_version.into();
        self.insert_resource(metadata)
    }
}

impl SnapshotRegistry {
    pub fn register_component<T>(&mut self)
    where
        T: Component + Clone + Serialize + for<'de> Deserialize<'de> + 'static,
    {
        let type_name = std::any::type_name::<T>();
        self.component_serializers.insert(
            type_name,
            ComponentRegistration {
                type_name,
                capture: capture_component::<T>,
                restore: restore_component::<T>,
            },
        );
    }

    pub fn register_resource<T>(&mut self)
    where
        T: Resource + Clone + Serialize + for<'de> Deserialize<'de> + 'static,
    {
        let type_name = std::any::type_name::<T>();
        self.resource_serializers.insert(
            type_name,
            ResourceRegistration {
                type_name,
                capture: capture_resource::<T>,
                restore: restore_resource::<T>,
            },
        );
    }

    pub fn schema_hash(&self) -> String {
        let mut names: Vec<_> = self
            .component_serializers
            .keys()
            .chain(self.resource_serializers.keys())
            .copied()
            .collect();
        names.sort_unstable();

        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for name in names {
            name.hash(&mut hasher);
        }
        format!("{:016x}", hasher.finish())
    }
}

pub struct AgentSnapshotPlugin;

impl Plugin for AgentSnapshotPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SnapshotRegistry>()
            .init_resource::<SnapshotStore>()
            .init_resource::<SnapshotPolicy>()
            .init_resource::<SnapshotMetadata>()
            .register_snapshot_component::<StableEntityId>()
            .register_snapshot_resource::<SimClock>()
            .register_snapshot_resource::<AgentActionQueue>()
            .add_systems(
                bevy_agent_core::AgentTick,
                maybe_take_snapshot.in_set(AgentSet::Snapshot),
            );
    }
}

fn capture_component<T>(entity: &EntityRef<'_>) -> Result<Option<ComponentSnapshot>>
where
    T: Component + Clone + Serialize + for<'de> Deserialize<'de> + 'static,
{
    entity
        .get::<T>()
        .map(|component| {
            serde_json::to_value(component)
                .map(|value| ComponentSnapshot {
                    type_name: std::any::type_name::<T>().to_string(),
                    value,
                })
                .context("serializing component")
        })
        .transpose()
}

fn restore_component<T>(entity: &mut EntityWorldMut<'_>, value: &serde_json::Value) -> Result<()>
where
    T: Component + Clone + Serialize + for<'de> Deserialize<'de> + 'static,
{
    let component = serde_json::from_value::<T>(value.clone())
        .with_context(|| format!("deserializing component {}", std::any::type_name::<T>()))?;
    entity.insert(component);
    Ok(())
}

fn capture_resource<T>(world: &World) -> Result<Option<ResourceSnapshot>>
where
    T: Resource + Clone + Serialize + for<'de> Deserialize<'de> + 'static,
{
    world
        .get_resource::<T>()
        .map(|resource| {
            serde_json::to_value(resource)
                .map(|value| ResourceSnapshot {
                    type_name: std::any::type_name::<T>().to_string(),
                    value,
                })
                .context("serializing resource")
        })
        .transpose()
}

fn restore_resource<T>(world: &mut World, value: &serde_json::Value) -> Result<()>
where
    T: Resource + Clone + Serialize + for<'de> Deserialize<'de> + 'static,
{
    let resource = serde_json::from_value::<T>(value.clone())
        .with_context(|| format!("deserializing resource {}", std::any::type_name::<T>()))?;
    world.insert_resource(resource);
    Ok(())
}

pub fn create_snapshot(world: &mut World, label: Option<String>) -> Result<SnapshotCreateResult> {
    let snapshot = capture_snapshot(world, label.clone())?;
    let result = SnapshotCreateResult {
        snapshot_id: snapshot.manifest.snapshot_id,
        tick: snapshot.manifest.tick,
        checksum: snapshot.checksum.clone(),
    };

    let keep = world.resource::<SnapshotPolicy>().keep_last_n_checkpoints;
    {
        let mut store = world.resource_mut::<SnapshotStore>();
        if let Some(label) = label {
            store.labels.insert(label, result.snapshot_id);
        }
        store.checkpoints.push(result.snapshot_id);
        store.snapshots.insert(result.snapshot_id, snapshot);
    }
    prune_checkpoints(world, keep);

    if let Some(mut control) = world.get_resource_mut::<AgentControlState>() {
        control.last_snapshot_created = Some(result.snapshot_id);
    }

    Ok(result)
}

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

    let clock = world.resource::<SimClock>().clone();
    let metadata = world
        .get_resource::<SnapshotMetadata>()
        .cloned()
        .unwrap_or_default();
    let control = world
        .get_resource::<AgentControlState>()
        .cloned()
        .unwrap_or_default();

    let mut resources = Vec::new();
    resource_regs.sort_by_key(|registration| registration.type_name);
    for registration in resource_regs {
        if let Some(snapshot) = (registration.capture)(world)? {
            resources.push(snapshot);
        }
    }
    resources.sort_by(|a, b| a.type_name.cmp(&b.type_name));

    component_regs.sort_by_key(|registration| registration.type_name);

    let mut entities = Vec::new();
    let entity_ids = {
        let mut query = world.query_filtered::<Entity, With<SnapshotEntity>>();
        query.iter(world).collect::<Vec<_>>()
    };

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

        let mut components = Vec::new();
        for registration in &component_regs {
            if let Some(snapshot) = (registration.capture)(&entity)? {
                components.push(snapshot);
            }
        }
        components.sort_by(|a, b| a.type_name.cmp(&b.type_name));

        entities.push(EntitySnapshot {
            stable_id,
            archetype_hint: None,
            components,
        });
    }
    entities.sort_by_key(|entity| entity.stable_id.0);

    let action_queue = world
        .get_resource::<AgentActionQueue>()
        .map(|queue| queue.pending.iter().cloned().collect())
        .unwrap_or_default();

    let snapshot_id = SnapshotId::new();
    let manifest = SnapshotManifest {
        snapshot_id,
        tick: clock.tick,
        label,
        game_id: metadata.game_id,
        game_version: metadata.game_version,
        agent_control_version: metadata.agent_control_version,
        schema_hash,
        created_from_timeline: control.timeline_id,
    };

    let mut snapshot = Snapshot {
        manifest,
        clock,
        resources,
        entities,
        action_queue,
        replay_state: SnapshotReplayState {
            replay_cursor_tick: world.resource::<SimClock>().tick,
        },
        checksum: StateChecksum { tick: 0, hash: 0 },
    };
    snapshot.checksum = checksum_snapshot(&snapshot)?;
    Ok(snapshot)
}

pub fn restore_snapshot(world: &mut World, snapshot_id: SnapshotId) -> Result<StateChecksum> {
    let snapshot = world
        .resource::<SnapshotStore>()
        .snapshots
        .get(&snapshot_id)
        .cloned()
        .ok_or_else(|| anyhow!("snapshot {snapshot_id:?} not found"))?;

    restore_snapshot_value(world, &snapshot)
}

pub fn restore_snapshot_value(world: &mut World, snapshot: &Snapshot) -> Result<StateChecksum> {
    clear_snapshot_entities(world);

    for resource in &snapshot.resources {
        let restore = world
            .resource::<SnapshotRegistry>()
            .resource_serializers
            .get(resource.type_name.as_str())
            .map(|registration| registration.restore)
            .ok_or_else(|| anyhow!("resource {} is not registered", resource.type_name))?;
        restore(world, &resource.value)?;
    }

    for entity_snapshot in &snapshot.entities {
        let restorers = entity_snapshot
            .components
            .iter()
            .map(|component| {
                let restore = world
                    .resource::<SnapshotRegistry>()
                    .component_serializers
                    .get(component.type_name.as_str())
                    .map(|registration| registration.restore)
                    .ok_or_else(|| {
                        anyhow!("component {} is not registered", component.type_name)
                    })?;
                Ok((restore, component.value.clone()))
            })
            .collect::<Result<Vec<_>>>()?;

        let mut entity = world.spawn_empty();
        entity.insert((SnapshotEntity, entity_snapshot.stable_id));
        for (restore, value) in restorers {
            restore(&mut entity, &value)?;
        }
    }

    if let Some(mut queue) = world.get_resource_mut::<AgentActionQueue>() {
        queue.pending = snapshot.action_queue.iter().cloned().collect();
    }

    let checksum = checksum_snapshot(&capture_snapshot(world, snapshot.manifest.label.clone())?)?;
    if checksum.hash != snapshot.checksum.hash {
        return Err(anyhow!(
            "restored checksum mismatch: expected {:?}, got {:?}",
            snapshot.checksum,
            checksum
        ));
    }
    Ok(checksum)
}

pub fn lookup_snapshot_by_label(world: &World, label: &str) -> Option<SnapshotId> {
    world.resource::<SnapshotStore>().labels.get(label).copied()
}

pub fn checksum_snapshot(snapshot: &Snapshot) -> Result<StateChecksum> {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    snapshot.clock.tick.hash(&mut hasher);
    snapshot.clock.dt_seconds.to_bits().hash(&mut hasher);
    snapshot.clock.elapsed_seconds.to_bits().hash(&mut hasher);

    for resource in &snapshot.resources {
        resource.type_name.hash(&mut hasher);
        serde_json::to_string(&resource.value)?.hash(&mut hasher);
    }
    for entity in &snapshot.entities {
        entity.stable_id.hash(&mut hasher);
        for component in &entity.components {
            component.type_name.hash(&mut hasher);
            serde_json::to_string(&component.value)?.hash(&mut hasher);
        }
    }
    for action in &snapshot.action_queue {
        serde_json::to_string(action)?.hash(&mut hasher);
    }

    Ok(StateChecksum {
        tick: snapshot.clock.tick,
        hash: hasher.finish(),
    })
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

pub fn maybe_take_snapshot(world: &mut World) {
    let Some(policy) = world.get_resource::<SnapshotPolicy>().cloned() else {
        return;
    };
    if policy.checkpoint_every_ticks == 0 {
        return;
    }

    let tick = world.resource::<SimClock>().tick;
    if tick == 0 || !tick.is_multiple_of(policy.checkpoint_every_ticks) {
        return;
    }

    let _ = create_snapshot(world, Some(format!("checkpoint-{tick}")));
}

fn prune_checkpoints(world: &mut World, keep_last_n: usize) {
    if keep_last_n == 0 {
        return;
    }

    let mut store = world.resource_mut::<SnapshotStore>();
    while store.checkpoints.len() > keep_last_n {
        if let Some(oldest) = store.checkpoints.first().copied() {
            store.checkpoints.remove(0);
            store.snapshots.remove(&oldest);
            store.labels.retain(|_, snapshot_id| *snapshot_id != oldest);
        }
    }
}
