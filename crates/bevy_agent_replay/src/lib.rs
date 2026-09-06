//! Replay logs and timeline branches for deterministic agent-controlled games.

use std::collections::{BTreeMap, HashMap};

use bevy::prelude::*;
use bevy_agent_core::{
    ActionSource, AgentAction, AgentControlState, AgentSet, BranchId, CurrentInputFrame,
    EnvironmentMetadata, SnapshotChecksum, SnapshotId, TimelineId,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplayManifest {
    pub replay_id: Uuid,
    pub game_id: String,
    pub game_version: String,
}

impl Default for ReplayManifest {
    fn default() -> Self {
        Self {
            replay_id: Uuid::new_v4(),
            game_id: "unknown-game".to_string(),
            game_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ActionRecord {
    pub tick: u64,
    #[serde(default)]
    pub branch_id: BranchId,
    pub source: ActionSource,
    pub action: AgentAction,
}

/// Branch-tagged checkpoint entry. The legacy `ReplayLog::checkpoints`
/// (`BTreeMap<u64, SnapshotId>`) cannot hold two checkpoints at the same tick
/// on different branches; this vector preserves parent/child same-tick
/// isolation while the legacy map is kept for back-compat serialization.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BranchCheckpoint {
    pub tick: u64,
    #[serde(default)]
    pub branch_id: BranchId,
    pub snapshot_id: SnapshotId,
}

/// Execution context for the simulation. `Reconstructing` is installed for the
/// duration of history replay (`restore_tick`/`branch` interval replay) so
/// policy systems and snapshot bookkeeping can stay quiet while recorded
/// ticks are rebuilt. See `ReplayLog::truncate_future` for the post-restore
/// stepping policy.
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionContext {
    #[default]
    Live,
    Reconstructing,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ReplayLog {
    pub manifest: ReplayManifest,
    pub initial_snapshot: Option<SnapshotId>,
    pub records: Vec<ActionRecord>,
    pub checkpoints: BTreeMap<u64, SnapshotId>,
    #[serde(default, alias = "checksums")]
    pub snapshot_checksums: BTreeMap<u64, SnapshotChecksum>,
    /// Branch-tagged checkpoints. Allows the same tick to hold distinct
    /// checkpoints per branch (parent/child isolation).
    #[serde(default)]
    pub branch_checkpoints: Vec<BranchCheckpoint>,
}

impl ReplayLog {
    #[must_use]
    pub fn actions_between(&self, start_exclusive: u64, end_inclusive: u64) -> Vec<ActionRecord> {
        self.records
            .iter()
            .filter(|record| record.tick > start_exclusive && record.tick <= end_inclusive)
            .cloned()
            .collect()
    }

    /// Actions visible on `branch`, i.e. records whose `branch_id` is the
    /// branch itself or one of its ancestors in `timeline`, restricted to
    /// `(start_exclusive, end_inclusive]`. Records predating a fork stay
    /// visible on the child; records on unrelated branches do not. Records
    /// tagged with a branch unknown to `timeline` (e.g. logs recorded before
    /// branch tagging existed) are treated as universally visible for
    /// back-compat replay.
    #[must_use]
    pub fn actions_for_branch(
        &self,
        timeline: &Timeline,
        branch: BranchId,
        start_exclusive: u64,
        end_inclusive: u64,
    ) -> Vec<ActionRecord> {
        self.records
            .iter()
            .filter(|record| {
                record.tick > start_exclusive
                    && record.tick <= end_inclusive
                    && (lineage_contains(timeline, record.branch_id, branch)
                        || !timeline.branches.contains_key(&record.branch_id))
            })
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn nearest_checkpoint_at_or_before(&self, tick: u64) -> Option<(u64, SnapshotId)> {
        self.checkpoints
            .range(..=tick)
            .next_back()
            .map(|(tick, snapshot_id)| (*tick, *snapshot_id))
    }

    /// Branch-aware checkpoint selection: the nearest checkpoint at-or-before
    /// `tick` whose branch is the requested branch or an ancestor of it. A
    /// checkpoint recorded on an ancestor *after* the fork point is not
    /// visible to the child (parent future beyond the fork is excluded).
    /// Falls back to the legacy tick map for logs without branch-tagged data.
    #[must_use]
    pub fn nearest_checkpoint_for_branch(
        &self,
        timeline: &Timeline,
        branch: BranchId,
        tick: u64,
    ) -> Option<(u64, SnapshotId)> {
        let mut best: Option<(u64, BranchId, SnapshotId)> = None;
        let mut consider = |candidate_tick: u64, candidate_branch: BranchId, id: SnapshotId| {
            if candidate_tick > tick {
                return;
            }
            if !lineage_contains(timeline, candidate_branch, branch) {
                return;
            }
            // Ancestor checkpoints beyond the fork are parent-future state the
            // child never shared; exclude them.
            if candidate_branch != branch
                && let Some(fork_tick) =
                    branch_fork_from_ancestor(timeline, candidate_branch, branch)
                && candidate_tick > fork_tick
            {
                return;
            }
            // Nearest tick wins; ties prefer the deepest branch (self over
            // ancestor) so same-tick parent/child checkpoints stay isolated.
            let replace = match best {
                None => true,
                Some((best_tick, best_branch, _)) => {
                    candidate_tick > best_tick
                        || (candidate_tick == best_tick
                            && best_branch != candidate_branch
                            && lineage_contains(timeline, best_branch, candidate_branch))
                }
            };
            if replace {
                best = Some((candidate_tick, candidate_branch, id));
            }
        };
        for checkpoint in &self.branch_checkpoints {
            consider(
                checkpoint.tick,
                checkpoint.branch_id,
                checkpoint.snapshot_id,
            );
        }
        // Legacy map entries carry no branch tag. Only consult them when no
        // branch-tagged data exists (old logs); treat them as visible to any
        // branch at-or-before the tick to preserve back-compat behavior.
        if self.branch_checkpoints.is_empty() {
            for (candidate_tick, id) in &self.checkpoints {
                if *candidate_tick <= tick {
                    let replace = match best {
                        None => true,
                        Some((best_tick, _, _)) => *candidate_tick > best_tick,
                    };
                    if replace {
                        best = Some((*candidate_tick, branch, *id));
                    }
                }
            }
        }
        // Initial snapshot covers tick 0 when nothing else matches.
        if best.is_none()
            && let Some(initial) = self.initial_snapshot
        {
            best = Some((0, branch, initial));
        }
        best.map(|(tick, _, id)| (tick, id))
    }

    /// Post-restore stepping policy: diverging from a restored tick truncates
    /// the recorded future on the same branch. Call this before appending new
    /// steps after `restore_tick` so stale future records can never be
    /// replayed or confused with the diverged history. Records on other
    /// branches are preserved.
    pub fn truncate_future(&mut self, branch: BranchId, tick: u64) {
        self.records
            .retain(|record| !(record.branch_id == branch && record.tick > tick));
        self.branch_checkpoints
            .retain(|checkpoint| !(checkpoint.branch_id == branch && checkpoint.tick > tick));
        // Keep the legacy map consistent for entries that belong to this
        // branch lineage only when branch-tagged data is absent; otherwise the
        // legacy map may alias another branch's same-tick checkpoint.
        if self.branch_checkpoints.is_empty() {
            self.checkpoints
                .retain(|checkpoint_tick, _| *checkpoint_tick <= tick);
        }
    }

    /// Records a checkpoint on a branch, keeping both the legacy tick map and
    /// the branch-tagged vector consistent.
    pub fn push_checkpoint(&mut self, branch: BranchId, tick: u64, snapshot_id: SnapshotId) {
        self.checkpoints.insert(tick, snapshot_id);
        if let Some(existing) = self
            .branch_checkpoints
            .iter_mut()
            .find(|checkpoint| checkpoint.tick == tick && checkpoint.branch_id == branch)
        {
            existing.snapshot_id = snapshot_id;
        } else {
            self.branch_checkpoints.push(BranchCheckpoint {
                tick,
                branch_id: branch,
                snapshot_id,
            });
        }
    }

    /// Validates that every snapshot referenced by the log is present in
    /// `provided`. Used by bundle export/import to guarantee retention
    /// integrity after `prune_checkpoints` runs in the snapshot crate.
    #[must_use]
    pub fn missing_snapshot_reference(
        &self,
        provided: &std::collections::BTreeSet<SnapshotId>,
    ) -> Option<SnapshotId> {
        let mut referenced = std::collections::BTreeSet::new();
        if let Some(initial) = self.initial_snapshot {
            referenced.insert(initial);
        }
        referenced.extend(self.checkpoints.values().copied());
        referenced.extend(
            self.branch_checkpoints
                .iter()
                .map(|checkpoint| checkpoint.snapshot_id),
        );
        referenced.difference(provided).next().copied()
    }
}

#[derive(Resource, Clone, Debug, Serialize, Deserialize)]
pub struct ReplayRecorder {
    pub recording: bool,
    pub log: ReplayLog,
}

impl Default for ReplayRecorder {
    fn default() -> Self {
        Self {
            recording: true,
            log: ReplayLog::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TimelineBranch {
    pub branch_id: BranchId,
    pub parent_branch: Option<BranchId>,
    pub fork_tick: u64,
    pub fork_snapshot: Option<SnapshotId>,
    pub label: Option<String>,
    pub actions: Vec<ActionRecord>,
}

#[derive(Resource, Clone, Debug, Serialize, Deserialize)]
pub struct Timeline {
    pub timeline_id: TimelineId,
    pub current_branch: BranchId,
    pub branches: HashMap<BranchId, TimelineBranch>,
}

impl Default for Timeline {
    fn default() -> Self {
        let timeline_id = TimelineId::new();
        let branch_id = BranchId::new();
        let mut branches = HashMap::new();
        branches.insert(
            branch_id,
            TimelineBranch {
                branch_id,
                parent_branch: None,
                fork_tick: 0,
                fork_snapshot: None,
                label: Some("root".to_string()),
                actions: Vec::new(),
            },
        );
        Self {
            timeline_id,
            current_branch: branch_id,
            branches,
        }
    }
}

impl Timeline {
    pub fn create_branch(
        &mut self,
        fork_tick: u64,
        fork_snapshot: Option<SnapshotId>,
        label: Option<String>,
    ) -> BranchId {
        let parent = self.current_branch;
        let branch_id = BranchId::new();
        self.branches.insert(
            branch_id,
            TimelineBranch {
                branch_id,
                parent_branch: Some(parent),
                fork_tick,
                fork_snapshot,
                label,
                actions: Vec::new(),
            },
        );
        self.current_branch = branch_id;
        branch_id
    }

    /// Ancestry chain from `branch` up to the root (branch first, root last).
    #[must_use]
    pub fn lineage(&self, branch: BranchId) -> Vec<BranchId> {
        let mut chain = Vec::new();
        let mut current = Some(branch);
        while let Some(id) = current {
            chain.push(id);
            current = self
                .branches
                .get(&id)
                .and_then(|branch| branch.parent_branch);
        }
        chain
    }

    /// Truncates future actions on `branch` beyond `tick` (post-restore
    /// diverge policy, timeline side; see `ReplayLog::truncate_future`).
    pub fn truncate_future(&mut self, branch: BranchId, tick: u64) {
        if let Some(branch_state) = self.branches.get_mut(&branch) {
            branch_state.actions.retain(|record| record.tick <= tick);
        }
    }
}

/// Returns true when `ancestor` equals `descendant` or appears in the
/// descendant's parent chain.
#[must_use]
pub fn lineage_contains(timeline: &Timeline, ancestor: BranchId, descendant: BranchId) -> bool {
    let mut current = Some(descendant);
    while let Some(id) = current {
        if id == ancestor {
            return true;
        }
        current = timeline
            .branches
            .get(&id)
            .and_then(|branch| branch.parent_branch);
    }
    false
}

/// Fork tick at which the path from `descendant` leaves `ancestor`
/// (the `fork_tick` of the child directly under `ancestor`).
/// Returns `None` when both are equal or `ancestor` is not an ancestor.
#[must_use]
pub fn branch_fork_from_ancestor(
    timeline: &Timeline,
    ancestor: BranchId,
    descendant: BranchId,
) -> Option<u64> {
    if ancestor == descendant {
        return None;
    }
    let mut current = descendant;
    loop {
        let branch = timeline.branches.get(&current)?;
        match branch.parent_branch {
            Some(parent) if parent == ancestor => return Some(branch.fork_tick),
            Some(parent) => current = parent,
            None => return None,
        }
    }
}

pub struct AgentReplayPlugin;

impl Plugin for AgentReplayPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ReplayRecorder>()
            .init_resource::<Timeline>()
            .init_resource::<ExecutionContext>()
            .add_systems(
                bevy_agent_core::AgentTick,
                record_replay_step.in_set(AgentSet::ReplayRecord),
            );
    }
}

pub fn record_replay_step(
    input: Res<CurrentInputFrame>,
    mut recorder: ResMut<ReplayRecorder>,
    mut timeline: ResMut<Timeline>,
    control: Res<AgentControlState>,
    context: Res<ExecutionContext>,
) {
    if !recorder.recording {
        return;
    }
    // No recording append while reconstructing history.
    if *context == ExecutionContext::Reconstructing {
        return;
    }

    let branch_id = if timeline.branches.contains_key(&control.branch_id) {
        control.branch_id
    } else {
        timeline.current_branch
    };
    let mut records = Vec::new();
    for (index, action) in input.actions.iter().cloned().enumerate() {
        records.push(ActionRecord {
            tick: input.tick,
            branch_id,
            source: input
                .sources
                .get(index)
                .cloned()
                .unwrap_or(ActionSource::Agent),
            action,
        });
    }

    recorder.log.records.extend(records.iter().cloned());
    if let Some(branch) = timeline.branches.get_mut(&branch_id) {
        branch.actions.extend(records);
    }
}

pub fn start_recording(world: &mut World, initial_snapshot: Option<SnapshotId>) {
    let metadata = world
        .get_resource::<EnvironmentMetadata>()
        .cloned()
        .unwrap_or_default();
    // Reset the timeline to a fresh root so a new recording never inherits
    // stale branches; callers that need a baseline snapshot pass it in (the
    // runner captures the current tick when `None` is given and snapshot
    // support is installed).
    let (timeline_id, branch_id) = {
        let timeline = world.get_resource_mut::<Timeline>();
        if let Some(mut timeline) = timeline {
            *timeline = Timeline::default();
            (timeline.timeline_id, timeline.current_branch)
        } else {
            let timeline = Timeline::default();
            let ids = (timeline.timeline_id, timeline.current_branch);
            world.insert_resource(timeline);
            ids
        }
    };
    if let Some(mut control) = world.get_resource_mut::<AgentControlState>() {
        control.timeline_id = timeline_id;
        control.branch_id = branch_id;
    }
    if world.get_resource::<ReplayRecorder>().is_none() {
        world.insert_resource(ReplayRecorder::default());
    }
    let mut recorder = world.resource_mut::<ReplayRecorder>();
    recorder.recording = true;
    recorder.log = ReplayLog::default();
    recorder.log.manifest.game_id = metadata.name;
    recorder.log.manifest.game_version = metadata.version;
    recorder.log.initial_snapshot = initial_snapshot;
}

pub fn stop_recording(world: &mut World) -> ReplayLog {
    let mut recorder = world.resource_mut::<ReplayRecorder>();
    recorder.recording = false;
    recorder.log.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(tick: u64, action: AgentAction) -> ActionRecord {
        ActionRecord {
            tick,
            branch_id: BranchId::default(),
            source: ActionSource::Agent,
            action,
        }
    }

    fn record_on(branch_id: BranchId, tick: u64, action: AgentAction) -> ActionRecord {
        ActionRecord {
            tick,
            branch_id,
            source: ActionSource::Agent,
            action,
        }
    }

    #[test]
    fn replay_log_filters_actions_between_ticks() {
        let log = ReplayLog {
            records: vec![
                record(1, AgentAction::Noop),
                record(2, AgentAction::Jump),
                record(3, AgentAction::Interact),
            ],
            ..Default::default()
        };

        let actions = log.actions_between(1, 3);

        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0].tick, 2);
        assert_eq!(actions[1].tick, 3);
    }

    #[test]
    fn replay_log_finds_nearest_checkpoint_at_or_before_tick() {
        let first = SnapshotId::new();
        let second = SnapshotId::new();
        let log = ReplayLog {
            checkpoints: [(10, first), (20, second)].into_iter().collect(),
            ..Default::default()
        };

        assert_eq!(log.nearest_checkpoint_at_or_before(9), None);
        assert_eq!(log.nearest_checkpoint_at_or_before(10), Some((10, first)));
        assert_eq!(log.nearest_checkpoint_at_or_before(25), Some((20, second)));
    }

    #[test]
    fn timeline_create_branch_tracks_parent_and_switches_current_branch() {
        let mut timeline = Timeline::default();
        let parent = timeline.current_branch;
        let snapshot = SnapshotId::new();

        let child = timeline.create_branch(42, Some(snapshot), Some("try-alt".to_string()));

        assert_ne!(child, parent);
        assert_eq!(timeline.current_branch, child);
        let branch = timeline.branches.get(&child).unwrap();
        assert_eq!(branch.parent_branch, Some(parent));
        assert_eq!(branch.fork_tick, 42);
        assert_eq!(branch.fork_snapshot, Some(snapshot));
        assert_eq!(branch.label.as_deref(), Some("try-alt"));
    }

    #[test]
    fn start_and_stop_recording_resets_and_returns_log() {
        let mut world = World::new();
        world.insert_resource(ReplayRecorder::default());
        let snapshot = SnapshotId::new();

        start_recording(&mut world, Some(snapshot));
        {
            let mut recorder = world.resource_mut::<ReplayRecorder>();
            recorder.log.records.push(record(1, AgentAction::Jump));
        }
        let log = stop_recording(&mut world);

        assert!(!world.resource::<ReplayRecorder>().recording);
        assert_eq!(log.initial_snapshot, Some(snapshot));
        assert_eq!(log.records.len(), 1);
    }

    #[test]
    fn actions_for_branch_includes_ancestors_but_not_siblings() {
        let mut timeline = Timeline::default();
        let root = timeline.current_branch;
        let child = timeline.create_branch(5, None, None);
        // Sibling forked off root while keeping `child` addressable.
        timeline.current_branch = root;
        let sibling = timeline.create_branch(5, None, None);
        timeline.current_branch = child;

        let log = ReplayLog {
            records: vec![
                record_on(root, 3, AgentAction::Noop),
                record_on(child, 7, AgentAction::Jump),
                record_on(sibling, 7, AgentAction::Interact),
            ],
            ..Default::default()
        };

        let child_actions = log.actions_for_branch(&timeline, child, 0, 10);
        assert_eq!(child_actions.len(), 2);
        assert!(child_actions.iter().any(|r| r.tick == 3));
        assert!(child_actions.iter().any(|r| r.action == AgentAction::Jump));

        let sibling_actions = log.actions_for_branch(&timeline, sibling, 0, 10);
        assert_eq!(sibling_actions.len(), 2);
        assert!(
            sibling_actions
                .iter()
                .any(|r| r.action == AgentAction::Interact)
        );
    }

    #[test]
    fn checkpoint_selection_excludes_parent_future_beyond_fork() {
        let mut timeline = Timeline::default();
        let root = timeline.current_branch;
        let child = timeline.create_branch(5, None, None);

        let parent_early = SnapshotId::new();
        let parent_late = SnapshotId::new();
        let mut log = ReplayLog::default();
        log.push_checkpoint(root, 3, parent_early);
        log.push_checkpoint(root, 8, parent_late);

        // Child restoring at tick 6 must see the tick-3 ancestor checkpoint,
        // not the tick-8 parent-future checkpoint recorded after the fork.
        assert_eq!(
            log.nearest_checkpoint_for_branch(&timeline, child, 6),
            Some((3, parent_early))
        );
        // Root itself still sees its own tick-8 checkpoint.
        assert_eq!(
            log.nearest_checkpoint_for_branch(&timeline, root, 9),
            Some((8, parent_late))
        );
    }

    #[test]
    fn same_tick_checkpoints_are_isolated_per_branch() {
        let mut timeline = Timeline::default();
        let root = timeline.current_branch;
        let child = timeline.create_branch(5, None, None);

        let parent_snap = SnapshotId::new();
        let child_snap = SnapshotId::new();
        let mut log = ReplayLog::default();
        log.push_checkpoint(root, 5, parent_snap);
        log.push_checkpoint(child, 5, child_snap);

        assert_eq!(
            log.nearest_checkpoint_for_branch(&timeline, child, 5),
            Some((5, child_snap))
        );
        assert_eq!(
            log.nearest_checkpoint_for_branch(&timeline, root, 5),
            Some((5, parent_snap))
        );
    }

    #[test]
    fn truncate_future_diverges_same_branch_only() {
        let mut timeline = Timeline::default();
        let root = timeline.current_branch;
        let child = timeline.create_branch(5, None, None);

        let mut log = ReplayLog {
            records: vec![
                record_on(root, 6, AgentAction::Noop),
                record_on(root, 9, AgentAction::Jump),
                record_on(child, 9, AgentAction::Interact),
            ],
            ..Default::default()
        };
        let doomed = SnapshotId::new();
        let kept = SnapshotId::new();
        log.push_checkpoint(root, 9, doomed);
        log.push_checkpoint(child, 9, kept);

        log.truncate_future(root, 6);
        timeline.truncate_future(root, 6);

        assert!(
            log.records
                .iter()
                .all(|r| r.branch_id != root || r.tick <= 6)
        );
        assert!(log.records.iter().any(|r| r.branch_id == child));
        assert!(
            log.branch_checkpoints
                .iter()
                .all(|c| c.branch_id != root || c.tick <= 6)
        );
        assert!(log.branch_checkpoints.iter().any(|c| c.snapshot_id == kept));
        assert!(
            !log.branch_checkpoints
                .iter()
                .any(|c| c.snapshot_id == doomed)
        );
    }
}
