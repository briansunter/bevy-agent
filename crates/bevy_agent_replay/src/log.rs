use std::collections::BTreeSet;

use bevy_agent_core::{BranchId, SnapshotChecksum, SnapshotId};

use crate::timeline::BranchVisibility;

use crate::{ActionRecord, BranchCheckpoint, ReplayLog, Timeline, lineage_contains};

/// Maximum ticks allowed in a single reconstruction interval.
pub const MAX_RECONSTRUCTION_TICKS: u64 = 100_000;

/// Every checkpoint needed by a portable recording or live retention policy.
#[must_use]
pub fn collect_replay_references(log: &ReplayLog) -> BTreeSet<SnapshotId> {
    let mut referenced = BTreeSet::new();
    referenced.extend(log.initial_snapshot);
    referenced.extend(
        log.branch_checkpoints
            .iter()
            .map(|checkpoint| checkpoint.snapshot_id),
    );
    referenced.extend(
        log.timeline_topology
            .iter()
            .filter_map(|branch| branch.fork_snapshot),
    );
    referenced
}

impl ReplayLog {
    /// Record completed simulation work, including frames without actions.
    pub fn record_tick(&mut self, branch: BranchId, tick: u64) {
        self.cursor_tick = tick;
        self.end_tick = self.end_tick.max(tick);
        self.active_branch = Some(branch);
        self.completed_ticks.entry(branch).or_default().insert(tick);
    }

    #[must_use]
    pub fn actions_between(&self, start_exclusive: u64, end_inclusive: u64) -> Vec<ActionRecord> {
        self.records
            .iter()
            .filter(|record| record.tick > start_exclusive && record.tick <= end_inclusive)
            .cloned()
            .collect()
    }

    /// Global history end for export metadata. Navigation uses branch bounds.
    #[must_use]
    pub fn log_end_tick(&self) -> u64 {
        self.end_tick.max(self.log_end_tick_without_bounds())
    }

    /// Last recorded tick visible on a branch, with ancestor contributions
    /// bounded by the forks on the path to that branch.
    #[must_use]
    pub fn branch_end_tick(&self, timeline: &Timeline, branch: BranchId) -> u64 {
        let mut end = self.initial_tick;
        if !timeline.branches.contains_key(&branch) {
            return end;
        }
        let visibility = BranchVisibility::new(timeline, branch);
        let visible = |record_branch, tick| visibility.contains(record_branch, tick);
        for record in &self.records {
            if visible(record.branch_id, record.tick) {
                end = end.max(record.tick);
            }
        }
        for checkpoint in &self.branch_checkpoints {
            if visible(checkpoint.branch_id, checkpoint.tick) {
                end = end.max(checkpoint.tick);
            }
        }
        for (id, checksums) in &self.branch_checksums {
            for tick in checksums.keys() {
                if visible(*id, *tick) {
                    end = end.max(*tick);
                }
            }
        }
        for (id, ticks) in &self.completed_ticks {
            for tick in ticks {
                if visible(*id, *tick) {
                    end = end.max(*tick);
                }
            }
        }
        end
    }

    #[must_use]
    pub fn recorded_range(&self, timeline: &Timeline, branch: BranchId) -> (u64, u64) {
        (self.initial_tick, self.branch_end_tick(timeline, branch))
    }

    #[must_use]
    pub fn expected_checksum(&self, branch: BranchId, tick: u64) -> Option<&SnapshotChecksum> {
        self.branch_checksums
            .get(&branch)
            .and_then(|checksums| checksums.get(&tick))
    }

    pub fn insert_branch_checksum(
        &mut self,
        branch: BranchId,
        tick: u64,
        checksum: SnapshotChecksum,
    ) {
        self.branch_checksums
            .entry(branch)
            .or_default()
            .insert(tick, checksum);
    }

    /// Capture current topology for export without moving the recording
    /// baseline to the first action or checkpoint.
    pub fn sync_topology(&mut self, timeline: &Timeline, cursor_tick: u64) {
        self.timeline_topology = timeline.branches.values().cloned().collect();
        self.timeline_topology
            .sort_by_key(|branch| (branch.fork_tick, branch.branch_id.0));
        self.active_branch = Some(timeline.current_branch);
        self.cursor_tick = cursor_tick;
        self.end_tick = self.log_end_tick();
    }

    #[must_use]
    pub fn actions_for_branch(
        &self,
        timeline: &Timeline,
        branch: BranchId,
        start_exclusive: u64,
        end_inclusive: u64,
    ) -> Vec<ActionRecord> {
        let visibility = BranchVisibility::new(timeline, branch);
        self.records
            .iter()
            .filter(|record| {
                record.tick > start_exclusive
                    && record.tick <= end_inclusive
                    && visibility.contains(record.branch_id, record.tick)
            })
            .cloned()
            .collect()
    }

    /// Nearest visible checkpoint, preferring deeper branches for equal
    /// ticks. The initial snapshot is located at its actual baseline tick.
    #[must_use]
    pub fn nearest_checkpoint_for_branch(
        &self,
        timeline: &Timeline,
        branch: BranchId,
        tick: u64,
    ) -> Option<(u64, SnapshotId)> {
        if tick < self.initial_tick || !timeline.branches.contains_key(&branch) {
            return None;
        }
        let visibility = BranchVisibility::new(timeline, branch);
        let mut best: Option<(u64, BranchId, SnapshotId)> = None;
        for checkpoint in &self.branch_checkpoints {
            if checkpoint.tick > tick || !visibility.contains(checkpoint.branch_id, checkpoint.tick)
            {
                continue;
            }
            let replace = match best {
                None => true,
                Some((best_tick, best_branch, _)) => {
                    checkpoint.tick > best_tick
                        || (checkpoint.tick == best_tick
                            && best_branch != checkpoint.branch_id
                            && lineage_contains(timeline, best_branch, checkpoint.branch_id))
                }
            };
            if replace {
                best = Some((
                    checkpoint.tick,
                    checkpoint.branch_id,
                    checkpoint.snapshot_id,
                ));
            }
        }
        best.map(|(tick, _, id)| (tick, id)).or_else(|| {
            self.initial_snapshot
                .map(|initial| (self.initial_tick, initial))
        })
    }

    /// Drop this branch's recorded future before stepping from a rewind.
    /// Other branches retain their checkpoints, checksums and completed ticks.
    pub fn truncate_future(&mut self, branch: BranchId, tick: u64) {
        self.records
            .retain(|record| record.branch_id != branch || record.tick <= tick);
        self.branch_checkpoints
            .retain(|checkpoint| checkpoint.branch_id != branch || checkpoint.tick <= tick);
        let key = branch;
        if let Some(checksums) = self.branch_checksums.get_mut(&key) {
            checksums.retain(|recorded_tick, _| *recorded_tick <= tick);
        }
        if self
            .branch_checksums
            .get(&key)
            .is_some_and(|checksums| checksums.is_empty())
        {
            self.branch_checksums.remove(&key);
        }
        if let Some(ticks) = self.completed_ticks.get_mut(&key) {
            ticks.retain(|recorded_tick| *recorded_tick <= tick);
        }
        if self
            .completed_ticks
            .get(&key)
            .is_some_and(|ticks| ticks.is_empty())
        {
            self.completed_ticks.remove(&key);
        }
        self.cursor_tick = self.cursor_tick.min(tick);
        self.end_tick = self.log_end_tick_without_bounds();
    }

    fn log_end_tick_without_bounds(&self) -> u64 {
        let mut end = self.initial_tick.max(self.cursor_tick);
        for record in &self.records {
            end = end.max(record.tick);
        }
        for checkpoint in &self.branch_checkpoints {
            end = end.max(checkpoint.tick);
        }
        for checksums in self.branch_checksums.values() {
            if let Some(tick) = checksums.keys().next_back() {
                end = end.max(*tick);
            }
        }
        for ticks in self.completed_ticks.values() {
            if let Some(tick) = ticks.last() {
                end = end.max(*tick);
            }
        }
        end
    }

    pub fn push_checkpoint(&mut self, branch: BranchId, tick: u64, snapshot_id: SnapshotId) {
        self.push_checkpoint_with_episode(branch, tick, snapshot_id, self.manifest.episode_id);
    }

    pub fn push_checkpoint_with_episode(
        &mut self,
        branch: BranchId,
        tick: u64,
        snapshot_id: SnapshotId,
        episode: u64,
    ) {
        if let Some(existing) = self
            .branch_checkpoints
            .iter_mut()
            .find(|checkpoint| checkpoint.tick == tick && checkpoint.branch_id == branch)
        {
            existing.snapshot_id = snapshot_id;
            existing.episode = episode;
        } else {
            self.branch_checkpoints.push(BranchCheckpoint {
                tick,
                branch_id: branch,
                snapshot_id,
                episode,
            });
        }
    }

    #[must_use]
    pub fn missing_snapshot_reference(
        &self,
        provided: &BTreeSet<SnapshotId>,
    ) -> Option<SnapshotId> {
        collect_replay_references(self)
            .difference(provided)
            .next()
            .copied()
    }
}
