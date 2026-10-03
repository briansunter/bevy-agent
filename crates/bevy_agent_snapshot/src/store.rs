use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use bevy::prelude::*;
use bevy_agent_core::{AgentControlState, SnapshotId};

use crate::{
    Snapshot, SnapshotCreateResult, SnapshotPolicy, SnapshotRole, SnapshotStore, capture_snapshot,
    validate_snapshot_full,
};
///
/// Explicitly retain a snapshot across history replacement until unpinned.
pub fn pin_snapshot(world: &mut World, snapshot_id: SnapshotId) -> Result<()> {
    let mut store = world
        .get_resource_mut::<SnapshotStore>()
        .ok_or_else(|| anyhow!("SnapshotStore is not installed"))?;
    if !store.snapshots.contains_key(&snapshot_id) {
        return Err(anyhow!("snapshot {snapshot_id:?} not found"));
    }
    store.pinned.insert(snapshot_id);
    Ok(())
}

/// Remove a pin previously added with [`pin_snapshot`].
pub fn unpin_snapshot(world: &mut World, snapshot_id: SnapshotId) -> Result<()> {
    let mut store = world
        .get_resource_mut::<SnapshotStore>()
        .ok_or_else(|| anyhow!("SnapshotStore is not installed"))?;
    if !store.snapshots.contains_key(&snapshot_id) {
        return Err(anyhow!("snapshot {snapshot_id:?} not found"));
    }
    store.pinned.remove(&snapshot_id);
    Ok(())
}

/// Atomically merge validated payloads into the live store.
///
/// Every payload is checked against the destination registry and action catalog
/// before any owner state changes. Duplicate batch IDs and conflicting existing
/// IDs are rejected; an identical previously installed payload is idempotent.
/// Imported checkpoints are indexed by tick and ID, independent of input order.
/// Labels and automatic protection are maintained here alongside byte charges.
pub fn install_snapshots(world: &mut World, mut snapshots: Vec<Snapshot>) -> Result<()> {
    let store = world
        .get_resource::<SnapshotStore>()
        .ok_or_else(|| anyhow!("SnapshotStore is not installed"))?;
    let mut ids = BTreeSet::new();
    for snapshot in &snapshots {
        let id = snapshot.manifest.snapshot_id;
        if !ids.insert(id) {
            return Err(anyhow!("duplicate imported snapshot {id:?}"));
        }
        validate_snapshot_full(world, snapshot)?;
        if let Some(existing) = store.get(id)
            && serde_json::to_value(existing)? != serde_json::to_value(snapshot)?
        {
            return Err(anyhow!(
                "imported snapshot {id:?} conflicts with existing payload"
            ));
        }
    }
    snapshots.retain(|snapshot| !store.snapshots.contains_key(&snapshot.manifest.snapshot_id));
    if snapshots.is_empty() {
        return Ok(());
    }
    snapshots
        .sort_unstable_by_key(|snapshot| (snapshot.manifest.tick, snapshot.manifest.snapshot_id));
    let charges = snapshots
        .iter()
        .map(snapshot_charge)
        .collect::<Result<Vec<_>>>()?;
    let total = charges.iter().try_fold(0usize, |total, charge| {
        total
            .checked_add(*charge)
            .ok_or_else(|| anyhow!("snapshot byte counter overflow"))
    })?;
    let victims = store.admission_victims(total, snapshot_budget(world))?;
    let mut store = world.resource_mut::<SnapshotStore>();
    store.remove_payloads(&victims);
    for (snapshot, charge) in snapshots.into_iter().zip(charges) {
        let id = snapshot.manifest.snapshot_id;
        if store.snapshots.contains_key(&id) {
            continue;
        }
        let first = store.snapshots.is_empty();
        if let Some(label) = &snapshot.manifest.label {
            store.labels.insert(label.clone(), id);
        }
        if needs_temporary_protection(snapshot.manifest.role, first) {
            store.protected.insert(id);
        }
        store.checkpoints.push(id);
        store.charges.insert(id, charge);
        store.retained_bytes += charge;
        store.snapshots.insert(id, Arc::new(snapshot));
    }
    let mut checkpoints = std::mem::take(&mut store.checkpoints);
    checkpoints.sort_unstable_by_key(|id| (store.snapshots[id].manifest.tick, *id));
    store.checkpoints = checkpoints;
    Ok(())
}

/// Set the episode id used to tag subsequently created checkpoints.
pub fn set_snapshot_episode(world: &mut World, episode_id: u64) {
    if let Some(mut store) = world.get_resource_mut::<SnapshotStore>() {
        store.episode_id = episode_id;
        store.protected.clear();
    }
}

fn needs_temporary_protection(role: SnapshotRole, is_first: bool) -> bool {
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
/// Automatic roles receive temporary protection. The owner indexes the returned
/// ID before publishing live references through [`crate::enforce_retention`].
/// Byte-budget admission evicts only unpinned, unprotected payloads and rejects
/// without changing the store if protected history prevents admission.
pub fn create_snapshot_with_role(
    world: &mut World,
    label: Option<String>,
    role: SnapshotRole,
) -> Result<SnapshotCreateResult> {
    // Resolve the store before capture: a Result-returning API reports a
    // missing plugin instead of panicking through World::resource.
    let episode_id = world
        .get_resource::<SnapshotStore>()
        .ok_or_else(|| anyhow!("SnapshotStore is not installed"))?
        .episode_id;
    let mut snapshot = capture_snapshot(world, label.clone())?;
    snapshot.manifest.role = role;
    // Episode tagging: checkpoints record the store's current episode.
    // (Checksum covers the gameplay payload; role/episode travel in the
    // manifest alongside the checksum, not inside it.)

    snapshot.manifest.episode_id = episode_id;
    let charge = snapshot_charge(&snapshot)?;
    let victims = world
        .resource::<SnapshotStore>()
        .admission_victims(charge, snapshot_budget(world))?;
    let result = SnapshotCreateResult {
        snapshot_id: snapshot.manifest.snapshot_id,
        tick: snapshot.manifest.tick,
        checksum: snapshot.checksum.clone(),
    };

    {
        let mut store = world.resource_mut::<SnapshotStore>();
        store.remove_payloads(&victims);
        let is_first = store.checkpoints.is_empty() && store.snapshots.is_empty();
        if let Some(label) = &label {
            store.labels.insert(label.clone(), result.snapshot_id);
        }
        store.checkpoints.push(result.snapshot_id);
        store.charges.insert(result.snapshot_id, charge);
        store.retained_bytes += charge;
        store
            .snapshots
            .insert(result.snapshot_id, Arc::new(snapshot));
        if needs_temporary_protection(role, is_first) {
            store.protected.insert(result.snapshot_id);
        }
    }

    if let Some(mut control) = world.get_resource_mut::<AgentControlState>() {
        control.last_snapshot_created = Some(result.snapshot_id);
    }

    Ok(result)
}

pub fn lookup_snapshot_by_label(world: &World, label: &str) -> Option<SnapshotId> {
    world
        .get_resource::<SnapshotStore>()?
        .labels
        .get(label)
        .copied()
}
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
    store.get(snapshot_id).is_some()
        && !store.pinned.contains(&snapshot_id)
        && !store.protected.contains(&snapshot_id)
        && !referenced.contains(&snapshot_id)
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
    let store = world
        .get_resource::<SnapshotStore>()
        .ok_or_else(|| anyhow!("SnapshotStore is not installed"))?;
    if store.pinned.contains(&id) {
        return Err(anyhow!("snapshot {id:?} is pinned and cannot be deleted"));
    }
    if referenced.contains(&id) || store.protected.contains(&id) {
        return Err(anyhow!(
            "snapshot {id:?} is referenced and cannot be deleted"
        ));
    }
    if !store.snapshots.contains_key(&id) {
        return Err(anyhow!("snapshot {id:?} not found"));
    }
    let mut store = world.resource_mut::<SnapshotStore>();
    store.remove_payloads(&BTreeSet::from([id]));
    Ok(())
}

/// Canonical retention over EVICTABLE checkpoints only.
///
/// Manually pinned, active-history-protected, and caller-referenced snapshots
/// are excluded from the `keep_last_n` count: enforcement counts only
/// evictable checkpoints (see [`SnapshotStore::evictable_candidates`] and
/// [`can_evict`]) and evicts the oldest evictable checkpoint other
/// than the most recently created one, so the newly created id always
/// survives. When no evictable checkpoint other than the newest exists, the
/// limit is exceeded rather than deleting the new snapshot.
///
/// Callers that own a `ReplayLog` must pass
/// `collect_replay_references(log)` from `bevy_agent_replay` as `referenced`
/// (initial + all checkpoint values + topology `fork_snapshot`s).
pub fn prune_checkpoints_with_refs(
    world: &mut World,
    keep_last_n: usize,
    referenced: &BTreeSet<SnapshotId>,
) {
    // Newest checkpoint is the just-created id; never evict it here.
    let Some(mut store) = world.get_resource_mut::<SnapshotStore>() else {
        return;
    };
    // New history owns only its current references. A snapshot-only integration
    // retains its latest current-episode initial checkpoint automatically.
    store.protected = referenced.clone();
    if referenced.is_empty()
        && let Some(id) = store
            .checkpoints
            .iter()
            .rev()
            .find(|id| {
                let snapshot = &store.snapshots[id];
                snapshot.manifest.episode_id == store.episode_id
                    && snapshot.manifest.role == SnapshotRole::Initial
            })
            .copied()
    {
        store.protected.insert(id);
    }
    let abandoned = store
        .checkpoints
        .iter()
        .filter(|id| {
            let role = store.snapshots[id].manifest.role;
            matches!(
                role,
                SnapshotRole::Initial | SnapshotRole::BranchFork | SnapshotRole::RecordingBaseline
            ) && !store.pinned.contains(id)
                && !store.protected.contains(id)
        })
        .copied()
        .collect();
    store.remove_payloads(&abandoned);
    let newest = store.checkpoints.last().copied();
    let evictable = store.evictable_candidates(referenced);
    let victims = evictable
        .iter()
        .copied()
        .filter(|id| Some(*id) != newest)
        .take(evictable.len().saturating_sub(keep_last_n))
        .collect::<BTreeSet<_>>();
    if victims.is_empty() {
        return;
    }
    store.remove_payloads(&victims);
}

fn snapshot_charge(snapshot: &Snapshot) -> Result<usize> {
    let encoded = serde_json::to_vec(snapshot)?.len();
    let mut bytes = encoded
        .checked_mul(2)
        .and_then(|n| n.checked_add(2048))
        .ok_or_else(|| anyhow!("snapshot byte counter overflow"))?;
    for value in snapshot.resources.iter().map(|r| &r.value).chain(
        snapshot
            .entities
            .iter()
            .flat_map(|e| e.components.iter().map(|c| &c.value)),
    ) {
        bytes = bevy_agent_core::json_heap_bytes(value)
            .and_then(|heap| bytes.checked_add(heap))
            .ok_or_else(|| anyhow!("snapshot byte counter overflow"))?;
    }
    for action in &snapshot.action_queue {
        if let bevy_agent_core::AgentAction::Custom { value, .. } = &action.action {
            bytes = bevy_agent_core::json_heap_bytes(value)
                .and_then(|heap| bytes.checked_add(heap))
                .ok_or_else(|| anyhow!("snapshot byte counter overflow"))?;
        }
    }
    Ok(bytes)
}

fn snapshot_budget(world: &World) -> usize {
    world
        .get_resource::<SnapshotPolicy>()
        .map_or(64 * 1024 * 1024, |policy| policy.max_snapshot_bytes)
}
impl SnapshotStore {
    fn admission_victims(&self, incoming: usize, budget: usize) -> Result<BTreeSet<SnapshotId>> {
        let mut bytes = self
            .retained_bytes
            .checked_add(incoming)
            .ok_or_else(|| anyhow!("snapshot byte counter overflow"))?;
        let mut victims = BTreeSet::new();
        for id in &self.checkpoints {
            if bytes <= budget {
                break;
            }
            if !self.pinned.contains(id) && !self.protected.contains(id) {
                bytes -= self.charges[id];
                victims.insert(*id);
            }
        }
        if bytes > budget {
            return Err(anyhow!(
                "snapshot byte budget {budget} is exhausted; release manual pins or start a new recording"
            ));
        }
        Ok(victims)
    }
    fn remove_payloads(&mut self, ids: &BTreeSet<SnapshotId>) {
        self.checkpoints.retain(|id| !ids.contains(id));
        self.labels.retain(|_, id| !ids.contains(id));
        for id in ids {
            self.snapshots.remove(id);
            if let Some(charge) = self.charges.remove(id) {
                self.retained_bytes -= charge;
            }
            self.protected.remove(id);
        }
    }
    /// Retain explicitly pinned payloads when replacing active history.
    pub fn retained_for_replacement(&self, baseline: Option<SnapshotId>) -> Self {
        let mut retained = self.clone();
        let remove = retained
            .snapshots
            .keys()
            .filter(|id| !retained.pinned.contains(id) && Some(**id) != baseline)
            .copied()
            .collect();
        retained.remove_payloads(&remove);
        retained.protected.clear();
        retained
    }
}

/// Explicit retention enforcement (owner-called).
///
/// Reads `SnapshotPolicy::keep_last_n_checkpoints` and enforces it via
/// [`crate::prune_checkpoints_with_refs`] with the caller-provided `referenced`
/// set. Owners must call this after indexing a newly created snapshot
/// (replay log / timeline topology) so referenced snapshots are never
/// evicted by an empty-refs prune. Creation functions
/// ([`crate::create_snapshot`], [`create_snapshot_with_role`],
/// [`crate::maybe_take_snapshot`]) enforce byte-budget admission. This function
/// and [`crate::prune_checkpoints_with_refs`] enforce checkpoint-count retention.
pub fn enforce_retention(world: &mut World, referenced: &BTreeSet<SnapshotId>) {
    let keep = world
        .get_resource::<SnapshotPolicy>()
        .map(|policy| policy.keep_last_n_checkpoints)
        .unwrap_or(usize::MAX);
    prune_checkpoints_with_refs(world, keep, referenced);
}
