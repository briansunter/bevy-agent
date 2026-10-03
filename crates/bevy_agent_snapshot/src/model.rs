use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::sync::Arc;

use bevy::prelude::*;
use bevy_agent_core::{
    AgentAction, ScheduledAction, SimClock, SnapshotChecksum, SnapshotId, StableEntityId,
    TimelineId,
};
use serde::{Deserialize, Serialize};

use crate::can_evict;
#[derive(Resource, Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct SnapshotPolicy {
    /// Maximum charged payload bytes, including JSON allocations and entry overhead.
    pub max_snapshot_bytes: usize,
    pub checkpoint_every_ticks: u64,
    pub keep_last_n_checkpoints: usize,
    pub checkpoint_on_terminal: bool,
    pub checkpoint_on_branch: bool,
}

impl Default for SnapshotPolicy {
    fn default() -> Self {
        Self {
            max_snapshot_bytes: 64 * 1024 * 1024,
            checkpoint_every_ticks: 120,
            keep_last_n_checkpoints: 100,
            checkpoint_on_terminal: true,
            checkpoint_on_branch: true,
        }
    }
}

/// Semantic role of a snapshot checkpoint.
///
/// Automatic roles are temporarily protected until the owner publishes its
/// active-history references. Protection expires when that history is replaced;
/// explicit manual pins are independent and persist until unpinned.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotRole {
    /// First snapshot of an episode; protected while its history is active.
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

/// Live payload owner and its checkpoint, label, and retention indices.
///
/// Payloads are immutable through this API. Use the checked world-level
/// creation, import, pin, and deletion functions to maintain every index.
#[derive(Resource, Clone, Debug, Default)]
pub struct SnapshotStore {
    pub(crate) snapshots: HashMap<SnapshotId, Arc<Snapshot>>,
    pub(crate) charges: HashMap<SnapshotId, usize>,
    pub(crate) retained_bytes: usize,
    pub(crate) protected: BTreeSet<SnapshotId>,
    pub(crate) labels: HashMap<String, SnapshotId>,
    pub(crate) checkpoints: Vec<SnapshotId>,
    /// Snapshots that must never be evicted by [`crate::prune_checkpoints_with_refs`].
    ///
    /// The initial snapshot and fork/branch snapshots are pinned on creation;
    /// callers may additionally pin any snapshot referenced by a `ReplayLog`.
    pub(crate) pinned: BTreeSet<SnapshotId>,
    /// Current episode identifier used to tag new checkpoints.
    ///
    /// Set via [`crate::set_snapshot_episode`]; [`crate::create_snapshot`] copies this into
    /// [`SnapshotManifest::episode_id`] so checkpoints can be correlated with
    /// the episode that produced them.
    pub(crate) episode_id: u64,
}

impl SnapshotStore {
    pub fn get(&self, id: SnapshotId) -> Option<&Snapshot> {
        self.snapshots.get(&id).map(Arc::as_ref)
    }
    /// All stored payloads; iteration order is unspecified. Use
    /// [`Self::checkpoints`] for the ordered retention index.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&SnapshotId, &Snapshot)> {
        self.snapshots
            .iter()
            .map(|(id, snapshot)| (id, snapshot.as_ref()))
    }
    pub fn len(&self) -> usize {
        self.snapshots.len()
    }
    pub fn is_empty(&self) -> bool {
        self.snapshots.is_empty()
    }
    pub fn checkpoints(&self) -> &[SnapshotId] {
        &self.checkpoints
    }
    pub fn pinned(&self) -> &BTreeSet<SnapshotId> {
        &self.pinned
    }
    /// Automatic protection currently owned by active history, separate from manual pins.
    pub fn protected(&self) -> &BTreeSet<SnapshotId> {
        &self.protected
    }
    pub fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }
    pub fn shared(&self, id: SnapshotId) -> Option<Arc<Snapshot>> {
        self.snapshots.get(&id).cloned()
    }
    pub fn episode_id(&self) -> u64 {
        self.episode_id
    }
    pub fn lookup_label(&self, label: &str) -> Option<SnapshotId> {
        self.labels.get(label).copied()
    }

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
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub manifest: SnapshotManifest,
    pub clock: SimClock,
    pub resources: Vec<ResourceSnapshot>,
    /// Stable type IDs of registered resources that were absent at capture time.
    ///
    /// Restoring a snapshot removes these resources when present, so a world
    /// that gained an optional resource after the snapshot is returned to the
    /// exact registered-resource surface.
    pub absent_resources: Vec<String>,
    pub entities: Vec<EntitySnapshot>,
    pub action_queue: Vec<ScheduledAction<AgentAction>>,
    pub replay_state: SnapshotReplayState,
    pub checksum: SnapshotChecksum,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotManifest {
    /// Required snapshot wire-contract version.
    pub schema_version: u32,
    pub snapshot_id: SnapshotId,
    pub tick: u64,
    pub label: Option<String>,
    pub game_id: String,
    pub game_version: String,
    pub agent_control_version: String,
    pub schema_hash: String,
    pub created_from_timeline: TimelineId,
    /// Semantic checkpoint role driving history protection and retention.
    ///
    pub role: SnapshotRole,
    /// Episode that produced this checkpoint (copied from
    /// [`SnapshotStore::episode_id`] at creation).
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
#[serde(deny_unknown_fields)]
pub struct SnapshotReplayState {
    pub replay_cursor_tick: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntitySnapshot {
    pub stable_id: StableEntityId,
    pub archetype_hint: Option<String>,
    pub components: Vec<ComponentSnapshot>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentSnapshot {
    pub type_id: String,
    pub schema_version: u32,
    pub value: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceSnapshot {
    pub type_id: String,
    pub schema_version: u32,
    pub value: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotCreateResult {
    pub snapshot_id: SnapshotId,
    pub tick: u64,
    pub checksum: SnapshotChecksum,
}
