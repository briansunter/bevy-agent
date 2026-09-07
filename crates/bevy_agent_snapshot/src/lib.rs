//! Gameplay snapshot and restore support for Bevy agent simulations.
//!
//! Snapshots are intentionally gameplay-focused: only entities marked with
//! [`SnapshotEntity`](bevy_agent_core::SnapshotEntity) and registered
//! resources/components are serialized and restored.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::hash::Hasher;

use anyhow::{Context, Result, anyhow};
use bevy::ecs::world::{EntityRef, EntityWorldMut};
use bevy::prelude::*;
use bevy_agent_core::{
    AgentAction, AgentActionQueue, AgentControlState, AgentSet, CHECKSUM_VERSION,
    CurrentInputFrame, DeterministicRng, EnvironmentMetadata, EpisodeState, ObservationConfig,
    RewardState, ScheduledAction, SimClock, SnapshotChecksum, SnapshotEntity, SnapshotId,
    StableEntityId, StableHasher, StableIdAllocator, TimelineId,
};
use serde::{Deserialize, Serialize};

/// Version of the snapshot registration schema.
///
/// Included in [`SnapshotRegistry::schema_hash`]; bump when the set of
/// captured fields or their encoding changes.
pub const SNAPSHOT_SCHEMA_VERSION: u32 = 1;

type ComponentCaptureFn = fn(&EntityRef<'_>) -> Result<Option<ComponentSnapshot>>;
type ComponentRestoreFn = fn(&mut EntityWorldMut<'_>, &serde_json::Value) -> Result<()>;
type ComponentValidateFn = fn(&serde_json::Value) -> Result<()>;
/// Hook run between entity allocation passes during restore.
pub type StableIdRemapHook = dyn Fn(&mut World, &HashMap<StableEntityId, Entity>);
type ResourceCaptureFn = fn(&World) -> Result<Option<ResourceSnapshot>>;
type ResourceRestoreFn = fn(&mut World, &serde_json::Value) -> Result<()>;
type ResourceRemoveFn = fn(&mut World);
type ResourceValidateFn = fn(&serde_json::Value) -> Result<()>;

#[derive(Clone)]
pub struct ComponentRegistration {
    pub type_name: &'static str,
    pub capture: ComponentCaptureFn,
    pub restore: ComponentRestoreFn,
    pub validate: ComponentValidateFn,
}

#[derive(Clone)]
pub struct ResourceRegistration {
    pub type_name: &'static str,
    pub capture: ResourceCaptureFn,
    pub restore: ResourceRestoreFn,
    pub remove: ResourceRemoveFn,
    pub validate: ResourceValidateFn,
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

/// Semantic role of a snapshot checkpoint.
///
/// Retention and auto-pinning are driven by this enum, never by label
/// substrings: [`SnapshotRole::Initial`], [`SnapshotRole::BranchFork`], and
/// [`SnapshotRole::RecordingBaseline`] are pinned on creation and excluded
/// from the `keep_last_n` evictable count, while [`SnapshotRole::Manual`] and
/// [`SnapshotRole::Periodic`] are evictable (unless explicitly pinned or
/// referenced by a replay log).
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotRole {
    /// First snapshot of an episode/run; always pinned.
    Initial,
    /// Fork/branch point referenced by a timeline branch.
    BranchFork,
    /// Baseline snapshot referenced by a recording/replay log.
    RecordingBaseline,
    /// Explicit user-requested snapshot; evictable by default.
    #[default]
    Manual,
    /// Automatic periodic checkpoint; evictable by default.
    Periodic,
}

impl SnapshotRole {
    /// Routing helper for the reset path: the initial snapshot of an
    /// episode/run. Always auto-pinned on creation.
    ///
    /// The runner reset path must use this (not a bare
    /// [`SnapshotRole::Manual`]) so retention keeps the episode baseline.
    #[must_use]
    pub fn for_reset() -> Self {
        Self::Initial
    }

    /// Routing helper for the fork/branch path: the snapshot a new timeline
    /// branch forks from. Always auto-pinned on creation.
    ///
    /// The runner fork/branch path must use this so the fork point survives
    /// `keep_last_n` enforcement while the branch lives.
    #[must_use]
    pub fn for_fork() -> Self {
        Self::BranchFork
    }

    /// Routing helper for the recording baseline path: the snapshot a
    /// recording/replay log references as its baseline. Always auto-pinned
    /// on creation.
    ///
    /// The runner recording path must use this for the baseline snapshot so
    /// replay retention (via `collect_replay_references` in
    /// `bevy_agent_replay`) never evicts it.
    #[must_use]
    pub fn for_baseline() -> Self {
        Self::RecordingBaseline
    }
}

#[derive(Resource, Clone, Debug, Default, Serialize, Deserialize)]
pub struct SnapshotStore {
    pub snapshots: HashMap<SnapshotId, Snapshot>,
    pub labels: HashMap<String, SnapshotId>,
    pub checkpoints: Vec<SnapshotId>,
    /// Snapshots that must never be evicted by [`prune_checkpoints`].
    ///
    /// The initial snapshot and fork/branch snapshots are pinned on creation;
    /// callers may additionally pin any snapshot referenced by a [`ReplayLog`](bevy_agent_replay::ReplayLog).
    #[serde(default)]
    pub pinned: BTreeSet<SnapshotId>,
    /// Current episode identifier used to tag new checkpoints.
    ///
    /// Set via [`set_snapshot_episode`]; [`create_snapshot`] copies this into
    /// [`SnapshotManifest::episode_id`] so checkpoints can be correlated with
    /// the episode that produced them.
    #[serde(default)]
    pub episode_id: u64,
}

impl SnapshotStore {
    /// Checkpoint-ordered ids that retention may evict: members of
    /// [`SnapshotStore::checkpoints`] that are neither pinned nor present in
    /// the caller-held `referenced` set.
    ///
    /// The `referenced` set is owned by the replay/timeline layer; callers
    /// must pass `collect_replay_references(log)` from `bevy_agent_replay`
    /// (initial + all checkpoint values + topology `fork_snapshot`s) so
    /// coordinated deletes never drop a snapshot another subsystem needs.
    #[must_use]
    pub fn evictable_candidates(&self, referenced: &BTreeSet<SnapshotId>) -> Vec<SnapshotId> {
        self.checkpoints
            .iter()
            .copied()
            .filter(|id| can_evict(self, *id, referenced))
            .collect()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub manifest: SnapshotManifest,
    pub clock: SimClock,
    pub resources: Vec<ResourceSnapshot>,
    /// Type names of registered resources that were absent at capture time.
    ///
    /// Restoring a snapshot removes these resources when present, so a world
    /// that gained an optional resource after the snapshot is returned to the
    /// exact registered-resource surface.
    #[serde(default)]
    pub absent_resources: Vec<String>,
    pub entities: Vec<EntitySnapshot>,
    pub action_queue: Vec<ScheduledAction<AgentAction>>,
    pub replay_state: SnapshotReplayState,
    pub checksum: SnapshotChecksum,
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
    /// Semantic checkpoint role driving auto-pin and retention.
    ///
    /// Defaults to [`SnapshotRole::Manual`] for snapshots serialized before
    /// the role field existed (back-compat).
    #[serde(default)]
    pub role: SnapshotRole,
    /// Episode that produced this checkpoint (copied from
    /// [`SnapshotStore::episode_id`] at creation).
    #[serde(default)]
    pub episode_id: u64,
}

/// Marker inserted when a restore rollback itself fails.
///
/// A failed rollback means the world may hold a partially applied snapshot;
/// callers must treat the presence of this resource as "world state is
/// undefined until re-initialized", rather than assuming the pre-restore
/// state was recovered.
#[derive(Resource, Clone, Debug)]
pub struct FaultState {
    pub message: String,
}

/// Distinct error returned when a restore apply/verification failure is
/// followed by a rollback failure.
///
/// `original` is the apply/verification error; `rollback` is the error from
/// attempting to restore the pre-mutation backup. When this error is
/// returned the world also carries a [`FaultState`] resource describing the
/// failure.
#[derive(Debug)]
pub struct RollbackFailed {
    pub original: String,
    pub rollback: String,
}

impl fmt::Display for RollbackFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "RollbackFailed {{ original: {}, rollback: {} }}",
            self.original, self.rollback
        )
    }
}

impl std::error::Error for RollbackFailed {}

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
    pub checksum: SnapshotChecksum,
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

#[macro_export]
macro_rules! register_snapshot_components {
    ($app:expr $(, $component:ty)+ $(,)?) => {{
        use $crate::SnapshotAppExt as _;
        $(
            $app.register_snapshot_component::<$component>();
        )+
    }};
}

#[macro_export]
macro_rules! register_snapshot_resources {
    ($app:expr $(, $resource:ty)+ $(,)?) => {{
        use $crate::SnapshotAppExt as _;
        $(
            $app.register_snapshot_resource::<$resource>();
        )+
    }};
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
        self.insert_resource(EnvironmentMetadata {
            name: metadata.game_id.clone(),
            version: metadata.game_version.clone(),
            description: self
                .world()
                .get_resource::<EnvironmentMetadata>()
                .and_then(|environment| environment.description.clone()),
        });
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
                validate: validate_component::<T>,
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
                remove: remove_resource::<T>,
                validate: validate_resource::<T>,
            },
        );
    }

    #[must_use]
    pub fn schema_hash(&self) -> String {
        // Compatibility proxy, NOT a full field-layout hash: this hashes the
        // sorted registration type names plus the snapshot crate version, so
        // adding/removing a registered component or resource (or bumping the
        // crate) changes the hash and rejects cross-schema restores.
        //
        // It does NOT hash per-field layouts or game-side component versions;
        // game breaking changes must additionally bump the game component
        // version surfaced via `SnapshotMetadata::game_version`
        // (checked in `prepare_restore_plan`), which is the authoritative
        // per-game compatibility gate.
        let mut names: Vec<_> = self
            .component_serializers
            .keys()
            .chain(self.resource_serializers.keys())
            .copied()
            .collect();
        names.sort_unstable();

        // Versioned schema hash: SCHEMA_VERSION + crate version + sorted
        // registration names. When per-type field-layout schemas become
        // available they should be hashed here per type; until then the type
        // name + crate version acts as the layout proxy.
        let mut hasher = StableHasher::new();
        hasher.write_u32(SNAPSHOT_SCHEMA_VERSION);
        hasher.write_string(env!("CARGO_PKG_VERSION"));
        for name in names {
            hasher.write_string(name);
            hasher.write_string(env!("CARGO_PKG_VERSION"));
        }
        format!("{:016x}", hasher.finish_hash())
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
            .register_snapshot_resource::<StableIdAllocator>()
            .register_snapshot_resource::<DeterministicRng>()
            .register_snapshot_resource::<CurrentInputFrame>()
            .register_snapshot_resource::<ObservationConfig>()
            .register_snapshot_resource::<RewardState>()
            .register_snapshot_resource::<EpisodeState>()
            .add_systems(
                bevy_agent_core::AgentTick,
                (|world: &mut World| {
                    maybe_take_snapshot(world);
                })
                .in_set(AgentSet::Snapshot),
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

fn validate_component<T>(value: &serde_json::Value) -> Result<()>
where
    T: Component + Clone + Serialize + for<'de> Deserialize<'de> + 'static,
{
    serde_json::from_value::<T>(value.clone())
        .with_context(|| format!("deserializing component {}", std::any::type_name::<T>()))?;
    Ok(())
}

fn validate_resource<T>(value: &serde_json::Value) -> Result<()>
where
    T: Resource + Clone + Serialize + for<'de> Deserialize<'de> + 'static,
{
    serde_json::from_value::<T>(value.clone())
        .with_context(|| format!("deserializing resource {}", std::any::type_name::<T>()))?;
    Ok(())
}

fn remove_resource<T>(world: &mut World)
where
    T: Resource + 'static,
{
    world.remove_resource::<T>();
}

/// Pin a snapshot so [`prune_checkpoints`] never evicts it.
///
/// Pin the initial snapshot and any fork/branch snapshot referenced by a
/// replay log or timeline branch.
pub fn pin_snapshot(world: &mut World, snapshot_id: SnapshotId) {
    if let Some(mut store) = world.get_resource_mut::<SnapshotStore>() {
        store.pinned.insert(snapshot_id);
    }
}

/// Remove a pin previously added with [`pin_snapshot`].
pub fn unpin_snapshot(world: &mut World, snapshot_id: SnapshotId) {
    if let Some(mut store) = world.get_resource_mut::<SnapshotStore>() {
        store.pinned.remove(&snapshot_id);
    }
}

/// Set the episode id used to tag subsequently created checkpoints.
pub fn set_snapshot_episode(world: &mut World, episode_id: u64) {
    if let Some(mut store) = world.get_resource_mut::<SnapshotStore>() {
        store.episode_id = episode_id;
    }
}

fn should_auto_pin(role: SnapshotRole, is_first: bool) -> bool {
    if is_first {
        return true;
    }
    matches!(
        role,
        SnapshotRole::Initial | SnapshotRole::BranchFork | SnapshotRole::RecordingBaseline
    )
}

pub fn create_snapshot(world: &mut World, label: Option<String>) -> Result<SnapshotCreateResult> {
    create_snapshot_with_role(world, label, SnapshotRole::Manual)
}

/// Role-tagged snapshot creation (preferred).
///
/// The `role` drives auto-pin: [`SnapshotRole::Initial`],
/// [`SnapshotRole::BranchFork`], and [`SnapshotRole::RecordingBaseline`]
/// are pinned on creation (see [`SnapshotRole::for_reset`],
/// [`SnapshotRole::for_fork`], [`SnapshotRole::for_baseline`], which the
/// runner reset/fork/recording paths must use). `Manual`/`Periodic` are
/// evictable unless pinned or replay-referenced.
///
/// Retention note: creation never prunes. The owner must index the returned
/// snapshot (replay log / timeline topology) and then call
/// [`enforce_retention`] (or [`prune_checkpoints_with_refs`] directly) with
/// the live reference set (`collect_replay_references(log)` from
/// `bevy_agent_replay`). [`prune_checkpoints_with_refs`] is the single
/// retention enforcement point.
pub fn create_snapshot_with_role(
    world: &mut World,
    label: Option<String>,
    role: SnapshotRole,
) -> Result<SnapshotCreateResult> {
    let mut snapshot = capture_snapshot(world, label.clone())?;
    snapshot.manifest.role = role;
    // Episode tagging: checkpoints record the store's current episode.
    // (Checksum covers the gameplay payload; role/episode travel in the
    // manifest alongside the checksum, not inside it.)
    let episode_id = world.resource::<SnapshotStore>().episode_id;
    snapshot.manifest.episode_id = episode_id;
    let result = SnapshotCreateResult {
        snapshot_id: snapshot.manifest.snapshot_id,
        tick: snapshot.manifest.tick,
        checksum: snapshot.checksum.clone(),
    };

    {
        let mut store = world.resource_mut::<SnapshotStore>();
        let is_first = store.checkpoints.is_empty() && store.snapshots.is_empty();
        if let Some(label) = &label {
            store.labels.insert(label.clone(), result.snapshot_id);
        }
        store.checkpoints.push(result.snapshot_id);
        store.snapshots.insert(result.snapshot_id, snapshot);
        if should_auto_pin(role, is_first) {
            store.pinned.insert(result.snapshot_id);
        }
    }

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
    let mut absent_resources = Vec::new();
    resource_regs.sort_by_key(|registration| registration.type_name);
    for registration in resource_regs {
        if let Some(snapshot) = (registration.capture)(world)? {
            resources.push(snapshot);
        } else {
            absent_resources.push(registration.type_name.to_string());
        }
    }
    resources.sort_by(|a, b| a.type_name.cmp(&b.type_name));
    absent_resources.sort();

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
    let episode_id = world
        .get_resource::<SnapshotStore>()
        .map(|store| store.episode_id)
        .unwrap_or_default();
    let manifest = SnapshotManifest {
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

    let mut snapshot = Snapshot {
        manifest,
        clock,
        resources,
        absent_resources,
        entities,
        action_queue,
        replay_state: SnapshotReplayState {
            replay_cursor_tick: world.resource::<SimClock>().tick,
        },
        checksum: SnapshotChecksum { tick: 0, hash: 0 },
    };
    snapshot.checksum = checksum_snapshot(&snapshot)?;
    Ok(snapshot)
}

pub fn restore_snapshot(world: &mut World, snapshot_id: SnapshotId) -> Result<SnapshotChecksum> {
    let snapshot = world
        .resource::<SnapshotStore>()
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
        .resource::<SnapshotStore>()
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
/// Bevy [`Entity`] ids are reallocated on every restore; only
/// [`StableEntityId`] is preserved. Prefer stable-id-first components that
/// reference other entities by [`StableEntityId`] (for example
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
/// Raw Bevy [`Entity`] ids are reallocated on every restore and are NOT
/// stable across capture/restore. Components must reference other entities
/// by [`StableEntityId`] (stable-id-first, e.g. `Attack { target:
/// StableEntityId }`), which restores verbatim and verifies cleanly.
///
/// When a `remap` hook rewrites raw-`Entity` fields in place after restore,
/// the rewritten values legitimately differ from the captured bytes, so a
/// full checksum comparison would spuriously fail. Pass those component type
/// names in `excluded_components` (and/or set `verify_without_entity_ids`)
/// so verification recomputes both sides with
/// [`checksum_snapshot_with_remap_exclusions`], excluding the remapped
/// fix-up from the comparison. The prepare-phase checksum precondition still
/// uses the full checksum; only post-restore verification honors exclusions.
#[derive(Clone, Debug, Default)]
pub struct RestoreOptions {
    /// Component type names (as in [`ComponentSnapshot::type_name`]) to
    /// exclude from post-restore checksum verification.
    pub excluded_components: Vec<String>,
    /// When true, post-restore verification ignores the components listed in
    /// `excluded_components` (stable-payload comparison). When false with an
    /// empty exclusion list, verification is the full checksum.
    pub verify_without_entity_ids: bool,
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
            verify_without_entity_ids: true,
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
/// components (see [`RestoreOptions`]).
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
/// is default; when `options.verify_without_entity_ids` is set (or
/// `excluded_components` is non-empty with the flag), post-restore
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
    if let Err(apply_error) = apply_restore_plan(world, &plan, remap) {
        let original = format!("{apply_error:?}");
        match rollback_snapshot(world, &backup) {
            Ok(()) => {
                return Err(apply_error.context("restore apply failed; rolled back"));
            }
            Err(rollback_error) => {
                let rollback = format!("{rollback_error:?}");
                world.insert_resource(FaultState {
                    message: format!(
                        "restore apply failed ({original}) and rollback failed ({rollback})"
                    ),
                });
                return Err(anyhow!(RollbackFailed { original, rollback })
                    .context("restore apply failed; rollback failed"));
            }
        }
    }

    // Post-restore verification: re-capture and compare checksums.
    // With `verify_without_entity_ids`, both sides are recomputed with
    // `checksum_snapshot_with_remap_exclusions` so raw-`Entity` fix-ups
    // applied by the remap hook are excluded from the comparison.
    let verification = (|| -> Result<SnapshotChecksum> {
        let recaptured = capture_snapshot(world, snapshot.manifest.label.clone())?;
        let excluded: Vec<&str> = options
            .excluded_components
            .iter()
            .map(String::as_str)
            .collect();
        if options.verify_without_entity_ids || !excluded.is_empty() {
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
        Err(error) => {
            let original = format!("{error:?}");
            match rollback_snapshot(world, &backup) {
                Ok(()) => Err(error.context("restore verification failed; rolled back")),
                Err(rollback_error) => {
                    let rollback = format!("{rollback_error:?}");
                    world.insert_resource(FaultState {
                        message: format!(
                            "restore verification failed ({original}) and rollback failed ({rollback})"
                        ),
                    });
                    Err(anyhow!(RollbackFailed { original, rollback })
                        .context("restore verification failed; rollback failed"))
                }
            }
        }
    }
}

struct RestorePlan {
    resource_restores: Vec<(ResourceRestoreFn, String, serde_json::Value)>,
    absent_removers: Vec<(ResourceRemoveFn, String)>,
    entity_restores: Vec<(StableEntityId, Vec<(ComponentRestoreFn, serde_json::Value)>)>,
    action_queue: Vec<ScheduledAction<AgentAction>>,
}

fn prepare_restore_plan(world: &World, snapshot: &Snapshot) -> Result<RestorePlan> {
    // Clock validation: typed deserialization already produced `SimClock`;
    // run its semantic check to reject tick/dt anomalies (zero/NaN/infinite
    // dt, negative/non-finite elapsed, absurd tick) before touching the world.
    snapshot
        .clock
        .validate()
        .map_err(|error| anyhow!("invalid snapshot clock: {error}"))?;

    // Schema hash.
    {
        let registry = world.resource::<SnapshotRegistry>().schema_hash();
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
            .get(resource.type_name.as_str())
            .copied()
            .ok_or_else(|| anyhow!("resource {} is not registered", resource.type_name))?;
        validate(&resource.value).with_context(|| {
            format!(
                "validating resource {} (late failure guarded)",
                resource.type_name
            )
        })?;
        resource_restores.push((restore, resource.type_name.clone(), resource.value.clone()));
    }

    // Absent resources: must be registered so removal is well-defined.
    let mut absent_removers = Vec::with_capacity(snapshot.absent_resources.len());
    for absent in &snapshot.absent_resources {
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
                .get(component.type_name.as_str())
                .copied()
                .ok_or_else(|| anyhow!("component {} is not registered", component.type_name))?;
            validate(&component.value).with_context(|| {
                format!(
                    "validating component {} (late failure guarded)",
                    component.type_name
                )
            })?;
            components.push((restore, component.value.clone()));
        }
        entity_restores.push((entity_snapshot.stable_id, components));
    }

    // Action queue: round-trip each entry so malformed queues fail in prepare.
    for action in &snapshot.action_queue {
        let value = serde_json::to_value(action).context("validating action queue")?;
        serde_json::from_value::<ScheduledAction<AgentAction>>(value)
            .context("validating action queue")?;
    }

    Ok(RestorePlan {
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

    if let Some(mut queue) = world.get_resource_mut::<AgentActionQueue>() {
        queue.pending = plan.action_queue.iter().cloned().collect();
    }
    Ok(())
}

/// Rollback helper: re-applies a just-captured backup without
/// post-verification.
///
/// Returns `Result` so callers distinguish "rolled back cleanly" (`Ok`) from
/// "rollback itself failed" (`Err`, world possibly half-applied and requiring
/// [`FaultState`]). Used by the restore apply/verification failure paths;
/// early-reject paths (prepare failures) never reach this helper and never
/// install [`FaultState`].
pub fn rollback_snapshot(world: &mut World, backup: &Snapshot) -> Result<()> {
    let plan = prepare_restore_plan(world, backup)?;
    apply_restore_plan(world, &plan, None)
}

/// Full offline validation of a snapshot payload (complete prepare without
/// world mutation).
///
/// Runs the entire [`prepare_restore_plan`] validation -- [`SimClock::validate`],
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

pub fn lookup_snapshot_by_label(world: &World, label: &str) -> Option<SnapshotId> {
    world.resource::<SnapshotStore>().labels.get(label).copied()
}

pub fn checksum_snapshot(snapshot: &Snapshot) -> Result<SnapshotChecksum> {
    checksum_snapshot_with_remap_exclusions(snapshot, &[])
}

/// Stable-payload checksum excluding remapped raw-`Entity` components.
///
/// Full [`checksum_snapshot`] hashes every captured component value,
/// including raw Bevy [`Entity`] ids when a component stores them directly.
/// Raw `Entity` ids are reallocated on every restore, so a `remap` hook that
/// rewrites them in place would always fail full verification. This variant
/// skips the components named in `excluded_components` (matched against
/// [`ComponentSnapshot::type_name`); pass the type names of components whose
/// raw-`Entity` fields the remap hook fixes up.
///
/// Prefer stable-id-first components (references by [`StableEntityId`])
/// which need no exclusion; use this only for the raw-`Entity` fix-up set
/// passed to verification via [`RestoreOptions`]. An empty exclusion list is
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
    hasher.write_u32(CHECKSUM_VERSION);
    hasher.write_u64(snapshot.clock.tick);
    hasher.write_u32(snapshot.clock.dt_seconds.to_bits());
    hasher.write_u64(snapshot.clock.elapsed_seconds.to_bits());

    let mut resources = snapshot.resources.iter().collect::<Vec<_>>();
    resources.sort_by(|a, b| a.type_name.cmp(&b.type_name));
    for resource in resources {
        hasher.write_string(&resource.type_name);
        hasher.write_json(&resource.value);
    }
    let mut absent = snapshot.absent_resources.iter().collect::<Vec<_>>();
    absent.sort();
    for name in absent {
        hasher.write_string(name);
    }
    let mut entities = snapshot.entities.iter().collect::<Vec<_>>();
    entities.sort_by_key(|entity| entity.stable_id.0);
    for entity in entities {
        hasher.write_u128(entity.stable_id.0);
        let mut components = entity.components.iter().collect::<Vec<_>>();
        components.sort_by(|a, b| a.type_name.cmp(&b.type_name));
        for component in components {
            if excluded.contains(component.type_name.as_str()) {
                continue;
            }
            hasher.write_string(&component.type_name);
            hasher.write_json(&component.value);
        }
    }
    for action in &snapshot.action_queue {
        hasher.write_json(&serde_json::to_value(action)?);
    }
    hasher.write_u64(snapshot.replay_state.replay_cursor_tick);

    Ok(SnapshotChecksum {
        tick: snapshot.clock.tick,
        hash: hasher.finish_hash(),
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

/// Periodic auto-checkpoint.
///
/// Creates a [`SnapshotRole::Periodic`] snapshot when `tick != 0` and
/// `tick % checkpoint_every_ticks == 0`. Never enforces retention: on
/// success returns the new snapshot id so the owner can index it (replay
/// log / timeline topology) and then call [`enforce_retention`] with the
/// live reference set. Retention lives entirely in
/// [`prune_checkpoints_with_refs`]; creation paths must not prune.
pub fn maybe_take_snapshot(world: &mut World) -> Option<SnapshotId> {
    let policy = world.get_resource::<SnapshotPolicy>().cloned()?;
    if policy.checkpoint_every_ticks == 0 {
        return None;
    }

    let tick = world.resource::<SimClock>().tick;
    if tick == 0 || !tick.is_multiple_of(policy.checkpoint_every_ticks) {
        return None;
    }

    match create_snapshot_with_role(
        world,
        Some(format!("checkpoint-{tick}")),
        SnapshotRole::Periodic,
    ) {
        Ok(result) => Some(result.snapshot_id),
        Err(error) => {
            bevy::log::warn!("periodic checkpoint failed: {error:?}");
            None
        }
    }
}

/// Returns false when `snapshot_id` is pinned or appears in the caller-held
/// replay-reference set; such snapshots must never be evicted.
///
/// The `referenced` set is owned by the replay/timeline layer: pass
/// `collect_replay_references(log)` from `bevy_agent_replay` (initial + all
/// checkpoint values + topology `fork_snapshot`s). Retention enforcement
/// threads it through so coordinated deletes never drop a snapshot another
/// subsystem still needs.
pub fn can_evict(
    store: &SnapshotStore,
    snapshot_id: SnapshotId,
    referenced: &BTreeSet<SnapshotId>,
) -> bool {
    !store.pinned.contains(&snapshot_id) && !referenced.contains(&snapshot_id)
}

/// Coordinated delete: rejects snapshots that are pinned or referenced.
///
/// `referenced` must be `collect_replay_references(log)` from
/// `bevy_agent_replay` when a replay log exists (else an empty set).
/// Returns an error when `id` is pinned or present in `referenced`; otherwise
/// removes the snapshot from the store, checkpoint list, and label index.
pub fn delete_snapshot_checked(
    world: &mut World,
    id: SnapshotId,
    referenced: &BTreeSet<SnapshotId>,
) -> Result<()> {
    let store = world.resource::<SnapshotStore>();
    if store.pinned.contains(&id) {
        return Err(anyhow!("snapshot {id:?} is pinned and cannot be deleted"));
    }
    if referenced.contains(&id) {
        return Err(anyhow!(
            "snapshot {id:?} is referenced and cannot be deleted"
        ));
    }
    if !store.snapshots.contains_key(&id) {
        return Err(anyhow!("snapshot {id:?} not found"));
    }
    let mut store = world.resource_mut::<SnapshotStore>();
    store.checkpoints.retain(|candidate| *candidate != id);
    store.snapshots.remove(&id);
    store.labels.retain(|_, snapshot_id| *snapshot_id != id);
    Ok(())
}

/// Legacy retention entry point (no replay references).
///
/// Prefer [`prune_checkpoints_with_refs`] or [`enforce_retention`], the
/// canonical enforcement used with `collect_replay_references(log)` from
/// `bevy_agent_replay`. This wrapper passes an empty reference set and is
/// kept for explicit owner-driven enforcement without a replay log; no
/// creation path calls it.
pub fn prune_checkpoints(world: &mut World, keep_last_n: usize) {
    prune_checkpoints_with_refs(world, keep_last_n, &BTreeSet::new());
}

/// Canonical retention over EVICTABLE checkpoints only.
///
/// Pinned snapshots (initial/fork/baseline) and caller-referenced snapshots
/// are excluded from the `keep_last_n` count: enforcement counts only
/// evictable checkpoints (see [`SnapshotStore::evictable_candidates`] and
/// [`can_evict`]) and evicts the oldest evictable checkpoint other
/// than the most recently created one, so the newly created id always
/// survives. When no evictable checkpoint other than the newest exists, the
/// limit is exceeded rather than deleting the new snapshot.
///
/// Callers that own a [`ReplayLog`](bevy_agent_replay::ReplayLog) must pass
/// `collect_replay_references(log)` from `bevy_agent_replay` as `referenced`
/// (initial + all checkpoint values + topology `fork_snapshot`s).
pub fn prune_checkpoints_with_refs(
    world: &mut World,
    keep_last_n: usize,
    referenced: &BTreeSet<SnapshotId>,
) {
    // Newest checkpoint is the just-created id; never evict it here.
    let newest = world
        .resource::<SnapshotStore>()
        .checkpoints
        .last()
        .copied();
    loop {
        let victim = {
            let store = world.resource::<SnapshotStore>();
            let evictable_count = store
                .checkpoints
                .iter()
                .filter(|id| can_evict(store, **id, referenced))
                .count();
            if evictable_count <= keep_last_n {
                break;
            }
            store
                .checkpoints
                .iter()
                .copied()
                .find(|id| Some(*id) != newest && can_evict(store, *id, referenced))
        };
        let Some(victim) = victim else {
            // No evictable checkpoint besides the newest (or all remaining
            // are pinned/referenced): exceed the limit rather than delete.
            break;
        };
        let mut store = world.resource_mut::<SnapshotStore>();
        store.checkpoints.retain(|id| *id != victim);
        // Never delete pinned snapshots even if they somehow left checkpoints.
        if store.pinned.contains(&victim) || referenced.contains(&victim) {
            continue;
        }
        store.snapshots.remove(&victim);
        store.labels.retain(|_, snapshot_id| *snapshot_id != victim);
    }
}

/// Explicit retention enforcement (owner-called).
///
/// Reads `SnapshotPolicy::keep_last_n_checkpoints` and enforces it via
/// [`prune_checkpoints_with_refs`] with the caller-provided `referenced`
/// set. Owners must call this after indexing a newly created snapshot
/// (replay log / timeline topology) so referenced snapshots are never
/// evicted by an empty-refs prune. Creation functions
/// ([`create_snapshot`], [`create_snapshot_with_role`],
/// [`maybe_take_snapshot`]) never prune; this function plus
/// [`prune_checkpoints_with_refs`] are the only enforcement points.
pub fn enforce_retention(world: &mut World, referenced: &BTreeSet<SnapshotId>) {
    let keep = world
        .get_resource::<SnapshotPolicy>()
        .map(|policy| policy.keep_last_n_checkpoints)
        .unwrap_or(usize::MAX);
    prune_checkpoints_with_refs(world, keep, referenced);
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_agent_core::{AgentControlPlugin, AgentTick};

    #[derive(Component, Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
    struct TestComponent {
        value: i32,
    }

    #[derive(Resource, Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
    struct TestResource {
        value: String,
    }

    fn app_with_snapshot() -> App {
        let mut app = App::new();
        app.add_plugins(AgentControlPlugin::deterministic())
            .add_plugins(AgentSnapshotPlugin)
            .insert_resource(TestResource {
                value: "initial".to_string(),
            });
        register_snapshot_components!(app, TestComponent);
        register_snapshot_resources!(app, TestResource);
        app.finish();
        app.cleanup();
        app
    }

    #[test]
    fn registry_schema_hash_is_stable_regardless_of_registration_order() {
        let mut a = SnapshotRegistry::default();
        a.register_component::<StableEntityId>();
        a.register_resource::<SimClock>();

        let mut b = SnapshotRegistry::default();
        b.register_resource::<SimClock>();
        b.register_component::<StableEntityId>();

        assert_eq!(a.schema_hash(), b.schema_hash());
    }

    #[test]
    fn snapshot_registration_macros_register_types() {
        let mut app = App::new();
        app.add_plugins(AgentControlPlugin::deterministic())
            .add_plugins(AgentSnapshotPlugin);

        register_snapshot_components!(app, TestComponent);
        register_snapshot_resources!(app, TestResource);

        let registry = app.world().resource::<SnapshotRegistry>();
        assert!(
            registry
                .component_serializers
                .contains_key(std::any::type_name::<TestComponent>())
        );
        assert!(
            registry
                .resource_serializers
                .contains_key(std::any::type_name::<TestResource>())
        );
    }

    #[test]
    fn create_snapshot_stores_label_and_updates_control_state() {
        let mut app = app_with_snapshot();
        app.world_mut().spawn((
            SnapshotEntity,
            StableEntityId(10),
            TestComponent { value: 5 },
        ));

        let result = create_snapshot(app.world_mut(), Some("before".to_string())).unwrap();

        assert_eq!(result.tick, 0);
        assert_eq!(
            lookup_snapshot_by_label(app.world(), "before"),
            Some(result.snapshot_id)
        );
        assert_eq!(
            app.world()
                .resource::<AgentControlState>()
                .last_snapshot_created,
            Some(result.snapshot_id)
        );
    }

    #[test]
    fn restore_snapshot_replaces_snapshot_entities_and_resources() {
        let mut app = app_with_snapshot();
        app.world_mut().spawn((
            SnapshotEntity,
            StableEntityId(10),
            TestComponent { value: 5 },
        ));
        let snapshot = create_snapshot(app.world_mut(), Some("point".to_string())).unwrap();

        app.world_mut().resource_mut::<TestResource>().value = "changed".to_string();
        {
            let mut query = app.world_mut().query::<&mut TestComponent>();
            for mut component in query.iter_mut(app.world_mut()) {
                component.value = 99;
            }
        }
        app.world_mut().spawn((
            SnapshotEntity,
            StableEntityId(20),
            TestComponent { value: 20 },
        ));

        restore_snapshot(app.world_mut(), snapshot.snapshot_id).unwrap();

        assert_eq!(
            app.world().resource::<TestResource>().value,
            "initial".to_string()
        );
        let mut query = app.world_mut().query::<(&StableEntityId, &TestComponent)>();
        let mut rows = query
            .iter(app.world())
            .map(|(id, component)| (id.0, component.value))
            .collect::<Vec<_>>();
        rows.sort_unstable();
        assert_eq!(rows, vec![(10, 5)]);
    }

    #[test]
    fn restore_snapshot_restores_registered_core_allocator() {
        let mut app = app_with_snapshot();

        let before = app.world().resource::<StableIdAllocator>().next;
        let snapshot = create_snapshot(app.world_mut(), None).unwrap();

        app.world_mut().resource_mut::<StableIdAllocator>().next = 4242;
        restore_snapshot(app.world_mut(), snapshot.snapshot_id).unwrap();

        assert_eq!(app.world().resource::<StableIdAllocator>().next, before);
    }

    #[test]
    fn restore_snapshot_rejects_schema_mismatch_before_mutating_world() {
        let mut app = app_with_snapshot();
        let result = create_snapshot(app.world_mut(), None).unwrap();
        let snapshot = app
            .world()
            .resource::<SnapshotStore>()
            .snapshots
            .get(&result.snapshot_id)
            .cloned()
            .unwrap();
        app.world_mut().resource_mut::<TestResource>().value = "changed".to_string();

        let mut incompatible = snapshot;
        incompatible.manifest.schema_hash = "incompatible-schema".to_string();

        let error = restore_snapshot_value(app.world_mut(), &incompatible).unwrap_err();

        assert!(error.to_string().contains("schema hash mismatch"));
        assert_eq!(app.world().resource::<TestResource>().value, "changed");
    }

    #[test]
    fn snapshot_policy_prunes_old_checkpoints_and_labels() {
        let mut app = app_with_snapshot();
        app.world_mut()
            .resource_mut::<SnapshotPolicy>()
            .keep_last_n_checkpoints = 1;
        app.world_mut().spawn((
            SnapshotEntity,
            StableEntityId(10),
            TestComponent { value: 5 },
        ));

        let old = create_snapshot(app.world_mut(), Some("old".to_string())).unwrap();
        // The first snapshot is auto-pinned as the initial snapshot; unpin so
        // this test exercises the unpinned eviction path.
        unpin_snapshot(app.world_mut(), old.snapshot_id);
        app.world_mut().resource_mut::<SimClock>().tick = 1;
        let new = create_snapshot(app.world_mut(), Some("new".to_string())).unwrap();
        // Creation never prunes; the owner enforces retention after indexing.
        enforce_retention(app.world_mut(), &BTreeSet::new());

        let store = app.world().resource::<SnapshotStore>();
        assert!(!store.snapshots.contains_key(&old.snapshot_id));
        assert!(store.snapshots.contains_key(&new.snapshot_id));
        assert_eq!(store.labels.get("old"), None);
        assert_eq!(store.labels.get("new"), Some(&new.snapshot_id));
    }

    #[test]
    fn prune_skips_pinned_initial_and_branch_snapshots() {
        let mut app = app_with_snapshot();
        app.world_mut()
            .resource_mut::<SnapshotPolicy>()
            .keep_last_n_checkpoints = 1;
        app.world_mut().spawn((
            SnapshotEntity,
            StableEntityId(10),
            TestComponent { value: 5 },
        ));

        let initial =
            create_snapshot_with_role(app.world_mut(), None, SnapshotRole::Initial).unwrap();
        assert!(
            app.world()
                .resource::<SnapshotStore>()
                .pinned
                .contains(&initial.snapshot_id)
        );
        let branch =
            create_snapshot_with_role(app.world_mut(), None, SnapshotRole::BranchFork).unwrap();
        assert!(
            app.world()
                .resource::<SnapshotStore>()
                .pinned
                .contains(&branch.snapshot_id)
        );

        let empty_refs = BTreeSet::new();
        let store = app.world().resource::<SnapshotStore>();
        assert!(store.snapshots.contains_key(&initial.snapshot_id));
        assert!(store.snapshots.contains_key(&branch.snapshot_id));
        assert!(!can_evict(store, initial.snapshot_id, &empty_refs));
        assert!(!can_evict(store, branch.snapshot_id, &empty_refs));
    }

    #[test]
    fn retention_limit_one_with_pinned_initial_keeps_new_manual() {
        let mut app = app_with_snapshot();
        app.world_mut()
            .resource_mut::<SnapshotPolicy>()
            .keep_last_n_checkpoints = 1;
        app.world_mut().spawn((
            SnapshotEntity,
            StableEntityId(10),
            TestComponent { value: 5 },
        ));

        // Pinned initial is excluded from the keep_last_n evictable count.
        let initial = create_snapshot_with_role(
            app.world_mut(),
            Some("initial".to_string()),
            SnapshotRole::Initial,
        )
        .unwrap();
        let first_manual = create_snapshot(app.world_mut(), Some("m1".to_string())).unwrap();
        // Newly created id always survives: evict oldest evictable OTHER
        // than the new id.
        let second_manual = create_snapshot(app.world_mut(), Some("m2".to_string())).unwrap();
        // Creation never prunes; the owner enforces retention after indexing.
        enforce_retention(app.world_mut(), &BTreeSet::new());

        let store = app.world().resource::<SnapshotStore>();
        assert!(store.snapshots.contains_key(&initial.snapshot_id));
        assert!(
            store.snapshots.contains_key(&second_manual.snapshot_id),
            "newly created snapshot must survive enforcement"
        );
        assert!(
            !store.snapshots.contains_key(&first_manual.snapshot_id),
            "oldest evictable should be evicted once evictable count exceeds keep_last_n"
        );
        // Pinned initial does not count toward the limit: exactly one
        // evictable checkpoint remains.
        let empty_refs = BTreeSet::new();
        let evictable = store
            .checkpoints
            .iter()
            .filter(|id| can_evict(store, **id, &empty_refs))
            .count();
        assert_eq!(evictable, 1);
    }

    #[test]
    fn referenced_snapshot_is_not_evicted_and_delete_is_rejected() {
        let mut app = app_with_snapshot();
        app.world_mut()
            .resource_mut::<SnapshotPolicy>()
            .keep_last_n_checkpoints = 1;
        app.world_mut().spawn((
            SnapshotEntity,
            StableEntityId(10),
            TestComponent { value: 5 },
        ));

        let first = create_snapshot(app.world_mut(), Some("m1".to_string())).unwrap();
        // First snapshot is auto-pinned via is_first; unpin so retention is
        // driven by the referenced set in this test.
        unpin_snapshot(app.world_mut(), first.snapshot_id);
        let mut referenced = BTreeSet::new();
        referenced.insert(first.snapshot_id);
        // Creation never prunes, so no policy bump is needed: `first`
        // survives creation even though it is referenced. The owner enforces
        // retention after indexing via `enforce_retention`.
        let second = create_snapshot(app.world_mut(), Some("m2".to_string())).unwrap();
        enforce_retention(app.world_mut(), &referenced);

        let store = app.world().resource::<SnapshotStore>();
        assert!(!can_evict(store, first.snapshot_id, &referenced));
        assert!(store.snapshots.contains_key(&first.snapshot_id));
        assert!(store.snapshots.contains_key(&second.snapshot_id));

        // Coordinated delete rejects pinned and referenced snapshots.
        pin_snapshot(app.world_mut(), second.snapshot_id);
        let err =
            delete_snapshot_checked(app.world_mut(), second.snapshot_id, &referenced).unwrap_err();
        assert!(err.to_string().contains("pinned"));
        let err =
            delete_snapshot_checked(app.world_mut(), first.snapshot_id, &referenced).unwrap_err();
        assert!(err.to_string().contains("referenced"));
        // Unpinned + unreferenced delete succeeds.
        unpin_snapshot(app.world_mut(), second.snapshot_id);
        delete_snapshot_checked(app.world_mut(), second.snapshot_id, &referenced).unwrap();
        assert!(
            !app.world()
                .resource::<SnapshotStore>()
                .snapshots
                .contains_key(&second.snapshot_id)
        );
    }

    #[derive(Component, Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
    struct RefComponent {
        target: StableEntityId,
    }

    #[derive(Resource, Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
    struct OptionalResource {
        value: String,
    }

    fn app_with_refs() -> App {
        let mut app = App::new();
        app.add_plugins(AgentControlPlugin::deterministic())
            .add_plugins(AgentSnapshotPlugin)
            .insert_resource(TestResource {
                value: "initial".to_string(),
            });
        register_snapshot_components!(app, TestComponent, RefComponent);
        register_snapshot_resources!(app, TestResource, OptionalResource);
        app.finish();
        app.cleanup();
        app
    }

    #[test]
    fn stable_entity_ids_preserved_and_refs_survive_restore() {
        let mut app = app_with_refs();
        app.world_mut().spawn((
            SnapshotEntity,
            StableEntityId(1),
            TestComponent { value: 5 },
        ));
        app.world_mut().spawn((
            SnapshotEntity,
            StableEntityId(2),
            RefComponent {
                target: StableEntityId(1),
            },
        ));
        let created = create_snapshot(app.world_mut(), None).unwrap();
        let snapshot = app
            .world()
            .resource::<SnapshotStore>()
            .snapshots
            .get(&created.snapshot_id)
            .cloned()
            .unwrap();

        // Mutate: retarget the reference and change values.
        {
            let mut query = app.world_mut().query::<&mut RefComponent>();
            for mut reference in query.iter_mut(app.world_mut()) {
                reference.target = StableEntityId(999);
            }
        }

        let mut seen = std::collections::HashMap::new();
        restore_snapshot_value_with_remap(
            app.world_mut(),
            &snapshot,
            Some(&|world, map| {
                // Fix-up pass runs after components are installed, so the
                // hook observes live component values.
                assert_eq!(map.len(), 2);
                assert!(map.contains_key(&StableEntityId(1)));
                let mut query = world.query::<(&StableEntityId, Option<&RefComponent>)>();
                let found = query.iter(world).any(|(_, reference)| {
                    *reference.unwrap()
                        == RefComponent {
                            target: StableEntityId(1),
                        }
                });
                assert!(found, "remap hook must see installed components");
            }),
        )
        .unwrap();
        let mut query = app
            .world_mut()
            .query::<(&StableEntityId, Option<&RefComponent>)>();
        for (id, reference) in query.iter(app.world()) {
            seen.insert(id.0, reference.cloned());
        }
        assert_eq!(seen.len(), 2);
        assert_eq!(
            seen[&2],
            Some(RefComponent {
                target: StableEntityId(1)
            })
        );
    }

    #[test]
    fn malformed_resource_late_failure_leaves_state_unchanged() {
        let mut app = app_with_snapshot();
        app.world_mut().spawn((
            SnapshotEntity,
            StableEntityId(10),
            TestComponent { value: 5 },
        ));
        let created = create_snapshot(app.world_mut(), None).unwrap();
        let mut snapshot = app
            .world()
            .resource::<SnapshotStore>()
            .snapshots
            .get(&created.snapshot_id)
            .cloned()
            .unwrap();
        // Corrupt a resource payload so typed validation fails in prepare.
        for resource in &mut snapshot.resources {
            if resource.type_name == std::any::type_name::<TestResource>() {
                resource.value = serde_json::json!({ "value": 12345 });
            }
        }
        // Re-sign so the checksum precondition passes and the failure
        // surfaces at typed validation (the guarded late failure).
        snapshot.checksum = checksum_snapshot(&snapshot).unwrap();

        app.world_mut().resource_mut::<TestResource>().value = "live".to_string();
        let before_entities = capture_snapshot(app.world_mut(), None).unwrap();

        let error = restore_snapshot_value(app.world_mut(), &snapshot).unwrap_err();
        assert!(error.to_string().contains("validating resource"));

        // Original semantic state unchanged: resource + entities intact.
        assert_eq!(app.world().resource::<TestResource>().value, "live");
        let after = capture_snapshot(app.world_mut(), None).unwrap();
        assert_eq!(after.entities.len(), before_entities.entities.len());
        assert_eq!(
            checksum_snapshot(&after).unwrap().hash,
            checksum_snapshot(&before_entities).unwrap().hash
        );
    }

    #[test]
    fn absent_resource_restore_removes_extra_resource() {
        let mut app = app_with_refs();
        // OptionalResource absent at capture time.
        app.world_mut().spawn((SnapshotEntity, StableEntityId(1)));
        let created = create_snapshot(app.world_mut(), None).unwrap();
        let snapshot = app
            .world()
            .resource::<SnapshotStore>()
            .snapshots
            .get(&created.snapshot_id)
            .cloned()
            .unwrap();
        assert!(
            snapshot
                .absent_resources
                .contains(&std::any::type_name::<OptionalResource>().to_string())
        );

        // World gains the optional resource after the snapshot.
        app.world_mut().insert_resource(OptionalResource {
            value: "extra".to_string(),
        });
        assert!(app.world().contains_resource::<OptionalResource>());

        restore_snapshot_value(app.world_mut(), &snapshot).unwrap();
        assert!(!app.world().contains_resource::<OptionalResource>());
    }

    #[test]
    fn checksum_mismatch_leaves_state_unchanged() {
        let mut app = app_with_snapshot();
        app.world_mut().spawn((
            SnapshotEntity,
            StableEntityId(10),
            TestComponent { value: 5 },
        ));
        let created = create_snapshot(app.world_mut(), None).unwrap();
        let mut snapshot = app
            .world()
            .resource::<SnapshotStore>()
            .snapshots
            .get(&created.snapshot_id)
            .cloned()
            .unwrap();
        // Tamper the stored checksum while keeping the payload valid so the
        // failure surfaces at post-restore verification (late).
        snapshot.checksum.hash ^= 0x9e3779b97f4a7c15;
        // Recompute precondition would now fail, so patch the payload hash
        // path instead: keep precondition passing by updating the snapshot
        // through a valid re-checksum, then tamper only the stored value
        // after prepare. Simplest late-failure simulation: tamper tick hash
        // via a valid payload but wrong stored hash, and bypass the
        // precondition by re-signing... Instead exercise the verification
        // path directly: restore a snapshot whose payload was mutated after
        // signing but whose stored checksum still matches the *original*.
        let mut tampered = app
            .world()
            .resource::<SnapshotStore>()
            .snapshots
            .get(&created.snapshot_id)
            .cloned()
            .unwrap();
        tampered.entities[0].components[0].value = serde_json::json!({ "value": 777 });
        // Re-sign so the precondition passes, then corrupt the stored hash so
        // verification against live state still fails deterministically.
        let good = checksum_snapshot(&tampered).unwrap();
        tampered.checksum = good;
        tampered.checksum.hash ^= 1;
        drop(snapshot);

        app.world_mut().resource_mut::<TestResource>().value = "live".to_string();
        let before = capture_snapshot(app.world_mut(), None).unwrap();

        let error = restore_snapshot_value(app.world_mut(), &tampered).unwrap_err();
        assert!(error.to_string().contains("checksum"));

        assert_eq!(app.world().resource::<TestResource>().value, "live");
        let after = capture_snapshot(app.world_mut(), None).unwrap();
        assert_eq!(
            checksum_snapshot(&after).unwrap().hash,
            checksum_snapshot(&before).unwrap().hash
        );
    }

    #[test]
    fn checksum_detects_mutation_of_each_snapshot_field() {
        let mut app = app_with_snapshot();
        app.world_mut().spawn((
            SnapshotEntity,
            StableEntityId(10),
            TestComponent { value: 5 },
        ));
        let created = create_snapshot(app.world_mut(), None).unwrap();
        let base_snapshot = app
            .world()
            .resource::<SnapshotStore>()
            .snapshots
            .get(&created.snapshot_id)
            .cloned()
            .unwrap();
        let base = base_snapshot.checksum.hash;

        let mut mutated = base_snapshot.clone();
        mutated.clock.tick += 1;
        assert_ne!(checksum_snapshot(&mutated).unwrap().hash, base);

        let mut mutated = base_snapshot.clone();
        mutated.resources[0].value = serde_json::json!({ "value": "other" });
        assert_ne!(checksum_snapshot(&mutated).unwrap().hash, base);

        let mut mutated = base_snapshot.clone();
        mutated.entities[0].components[0].value = serde_json::json!({ "value": 6 });
        assert_ne!(checksum_snapshot(&mutated).unwrap().hash, base);

        let mut mutated = base_snapshot.clone();
        mutated.action_queue.push(bevy_agent_core::ScheduledAction {
            tick: 99,
            source: bevy_agent_core::ActionSource::Test,
            action: bevy_agent_core::AgentAction::Jump,
        });
        assert_ne!(checksum_snapshot(&mutated).unwrap().hash, base);
    }

    #[test]
    fn maybe_take_snapshot_respects_interval() {
        let mut app = app_with_snapshot();
        app.world_mut()
            .resource_mut::<SnapshotPolicy>()
            .checkpoint_every_ticks = 2;

        app.world_mut().run_schedule(AgentTick);
        assert!(
            app.world()
                .resource::<SnapshotStore>()
                .checkpoints
                .is_empty()
        );

        app.world_mut().run_schedule(AgentTick);
        assert_eq!(app.world().resource::<SnapshotStore>().checkpoints.len(), 1);
    }

    #[test]
    fn capture_snapshot_fails_for_snapshot_entity_without_stable_id() {
        let mut app = app_with_snapshot();
        app.world_mut()
            .spawn((SnapshotEntity, TestComponent { value: 5 }));

        let error = capture_snapshot(app.world_mut(), None).unwrap_err();

        assert!(error.to_string().contains("missing StableEntityId"));
    }

    #[test]
    fn restore_rejects_invalid_clock() {
        let mut app = app_with_snapshot();
        app.world_mut().spawn((
            SnapshotEntity,
            StableEntityId(10),
            TestComponent { value: 5 },
        ));
        let created = create_snapshot(app.world_mut(), None).unwrap();
        let mut snapshot = app
            .world()
            .resource::<SnapshotStore>()
            .snapshots
            .get(&created.snapshot_id)
            .cloned()
            .unwrap();
        // Poison the clock (NaN dt + absurd tick) and re-sign so the failure
        // surfaces at semantic clock validation, not the checksum gate.
        snapshot.clock.dt_seconds = f32::NAN;
        snapshot.clock.tick = u64::MAX;
        snapshot.checksum = checksum_snapshot(&snapshot).unwrap();

        let before = capture_snapshot(app.world_mut(), None).unwrap();
        let error = restore_snapshot_value(app.world_mut(), &snapshot).unwrap_err();
        assert!(
            error.to_string().contains("invalid snapshot clock"),
            "unexpected error: {error:?}"
        );
        // Prepare-phase failure: world untouched.
        let after = capture_snapshot(app.world_mut(), None).unwrap();
        assert_eq!(
            checksum_snapshot(&after).unwrap().hash,
            checksum_snapshot(&before).unwrap().hash
        );
    }

    #[test]
    fn rollback_failure_surfaces_distinctly_and_marks_fault() {
        fn failing_restore(
            _entity: &mut EntityWorldMut<'_>,
            _value: &serde_json::Value,
        ) -> Result<()> {
            Err(anyhow!("injected restore failure"))
        }

        let mut app = app_with_snapshot();
        app.world_mut().spawn((
            SnapshotEntity,
            StableEntityId(10),
            TestComponent { value: 5 },
        ));
        let created = create_snapshot(app.world_mut(), None).unwrap();
        let snapshot = app
            .world()
            .resource::<SnapshotStore>()
            .snapshots
            .get(&created.snapshot_id)
            .cloned()
            .unwrap();

        // Swap the TestComponent restore fn for one that always fails, so
        // both the initial apply and the rollback apply fail.
        {
            let mut registry = app.world_mut().resource_mut::<SnapshotRegistry>();
            let type_name = std::any::type_name::<TestComponent>();
            if let Some(registration) = registry.component_serializers.get_mut(type_name) {
                registration.restore = failing_restore;
            }
        }

        let error = restore_snapshot_value(app.world_mut(), &snapshot).unwrap_err();
        let message = format!("{error:?}");
        assert!(
            message.contains("RollbackFailed"),
            "expected distinct RollbackFailed context, got: {message}"
        );
        let fault = app
            .world()
            .get_resource::<FaultState>()
            .unwrap_or_else(|| panic!("expected FaultState after rollback failure"));
        assert!(fault.message.contains("rollback failed"));
    }
}
