//! Checked mutation boundary for live recording state.

use bevy_agent_core::{
    BranchId, EnvironmentMetadata, SnapshotChecksum, SnapshotId, TimelineId, validate_clock_tick,
};

use crate::{ActionRecord, REPLAY_SCHEMA_VERSION, ReplayLog, ReplayRecorder, Timeline};

impl ReplayRecorder {
    pub fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }
    pub fn max_history_bytes(&self) -> usize {
        self.max_history_bytes
    }
    /// Fresh owner with the same admission limit; no action history is copied.
    pub fn empty_replacement(&self) -> Self {
        let mut replacement = Self {
            recording: self.recording,
            max_history_bytes: self.max_history_bytes,
            ..Self::default()
        };
        replacement.log.manifest.episode_id = self.log.manifest.episode_id;
        replacement
    }
    pub fn set_max_history_bytes(&mut self, bytes: usize) -> Result<(), String> {
        let charged = history_charge(&self.log)?;
        if bytes == 0 || charged > bytes {
            return Err("history byte budget cannot be smaller than admitted history".into());
        }
        self.max_history_bytes = bytes;
        self.retained_bytes = charged;
        Ok(())
    }
    #[must_use]
    pub fn is_recording(&self) -> bool {
        self.recording
    }

    #[must_use]
    pub fn log(&self) -> &ReplayLog {
        &self.log
    }

    /// Replace live history only after validating its standalone invariants.
    /// Snapshot payloads and action catalogs are checked by the runner first.
    pub fn replace_log(&mut self, log: ReplayLog) -> Result<(), String> {
        log.validate_history()?;
        let charged = history_charge(&log)?;
        if charged > self.max_history_bytes {
            return Err("replay history byte budget exhausted".into());
        }
        self.log = log;
        self.retained_bytes = charged;
        self.rebuild_record_index();
        Ok(())
    }

    /// Reset episode history without changing whether recording is enabled.
    pub fn reset_episode(
        &mut self,
        metadata: &EnvironmentMetadata,
        timeline: &Timeline,
        tick: u64,
    ) -> Result<(), String> {
        timeline.validate()?;
        validate_clock_tick(tick).map_err(|error| error.to_string())?;
        let episode_id = self
            .log
            .manifest
            .episode_id
            .checked_add(1)
            .ok_or_else(|| "replay episode counter exhausted".to_string())?;
        let mut log = ReplayLog {
            initial_tick: tick,
            cursor_tick: tick,
            end_tick: tick,
            ..ReplayLog::default()
        };
        log.manifest.game_id.clone_from(&metadata.name);
        log.manifest.game_version.clone_from(&metadata.version);
        log.manifest.episode_id = episode_id;
        log.sync_topology(timeline, tick);
        self.replace_log(log)
    }

    pub fn set_baseline(
        &mut self,
        snapshot_id: SnapshotId,
        tick: u64,
        timeline: &Timeline,
    ) -> Result<(), String> {
        if tick != self.log.initial_tick {
            return Err(format!(
                "baseline tick {tick} differs from recording start {}",
                self.log.initial_tick
            ));
        }
        self.validate_cursor(timeline, tick)?;
        self.sync_topology(timeline, tick)?;
        self.log.initial_snapshot = Some(snapshot_id);
        Ok(())
    }

    /// Synchronize topology after an explicit branch without granting raw log access.
    pub fn sync_topology(&mut self, timeline: &Timeline, cursor_tick: u64) -> Result<(), String> {
        self.validate_cursor(timeline, cursor_tick)?;
        let extra = self.topology_charge(timeline)?;
        self.reserve_history(extra)?;
        self.log.sync_topology(timeline, cursor_tick);
        Ok(())
    }

    pub fn index_checkpoint(
        &mut self,
        timeline: &Timeline,
        branch: BranchId,
        tick: u64,
        snapshot_id: SnapshotId,
        checksum: SnapshotChecksum,
    ) -> Result<(), String> {
        if checksum.tick != tick {
            return Err("checkpoint checksum tick disagrees".into());
        }
        let bytes = self.checkpoint_charge(timeline, branch, tick)?;
        self.reserve_history(bytes)?;
        self.log.timeline_topology = timeline.branches().values().cloned().collect();
        self.log
            .timeline_topology
            .sort_by_key(|branch| (branch.fork_tick, branch.branch_id));
        self.log.push_checkpoint(branch, tick, snapshot_id);
        self.log.insert_branch_checksum(branch, tick, checksum);
        self.log.end_tick = self.log.end_tick.max(tick);
        Ok(())
    }

    /// Pure admission check used before a runner creates a manual payload.
    pub fn validate_checkpoint_capacity(
        &self,
        timeline: &Timeline,
        branch: BranchId,
        tick: u64,
    ) -> Result<(), String> {
        self.check_history_capacity(self.checkpoint_charge(timeline, branch, tick)?)?;
        Ok(())
    }
    fn checkpoint_charge(
        &self,
        timeline: &Timeline,
        branch: BranchId,
        tick: u64,
    ) -> Result<usize, String> {
        self.validate_topology_extension(timeline)?;
        let info = timeline
            .branches()
            .get(&branch)
            .ok_or_else(|| format!("checkpoint references unknown branch {branch:?}"))?;
        validate_clock_tick(tick).map_err(|error| error.to_string())?;
        if tick < self.log.initial_tick || tick < info.fork_tick {
            return Err("checkpoint tick disagrees with recording bounds or checksum".to_string());
        }
        let checkpoint_bytes = if self
            .log
            .branch_checkpoints
            .iter()
            .any(|checkpoint| checkpoint.branch_id == branch && checkpoint.tick == tick)
        {
            0
        } else {
            1024
        };
        Ok(self.topology_charge(timeline)? + checkpoint_bytes)
    }

    /// Truncate only the active branch, preserving all prefixes inherited by children.
    pub fn truncate_future(
        &mut self,
        timeline: &Timeline,
        branch: BranchId,
        tick: u64,
    ) -> Result<(), String> {
        self.validate_truncation(timeline, branch, tick)?;
        if !self.recording {
            return Ok(());
        }
        let has_future = self
            .record_index
            .get(&branch)
            .and_then(|ticks| ticks.last_key_value())
            .is_some_and(|(last, _)| *last > tick)
            || self
                .log
                .completed_ticks
                .get(&branch)
                .and_then(|ticks| ticks.last())
                .is_some_and(|last| *last > tick)
            || self
                .log
                .branch_checksums
                .get(&branch)
                .and_then(|ticks| ticks.last_key_value())
                .is_some_and(|(last, _)| *last > tick)
            || self
                .log
                .branch_checkpoints
                .iter()
                .any(|checkpoint| checkpoint.branch_id == branch && checkpoint.tick > tick);
        if !has_future {
            return Ok(());
        }
        self.log.truncate_future(branch, tick);
        self.rebuild_record_index();
        self.retained_bytes = history_charge(&self.log)?;
        Ok(())
    }

    pub fn validate_truncation(
        &self,
        timeline: &Timeline,
        branch: BranchId,
        tick: u64,
    ) -> Result<(), String> {
        if !self.recording {
            return Ok(());
        }
        self.validate_cursor(timeline, tick)?;
        if timeline.current_branch() != branch {
            return Err("cannot truncate an inactive branch".to_string());
        }
        if let Some(child) = timeline
            .branches()
            .values()
            .find(|child| child.parent_branch == Some(branch) && child.fork_tick > tick)
        {
            return Err(format!(
                "cannot truncate shared history before descendant fork {}; create a branch at tick {tick} first",
                child.fork_tick
            ));
        }
        Ok(())
    }

    /// Copy only visible frames in the selected interval, preserving their
    /// original order even when a portable log interleaves branches.
    #[must_use]
    pub fn actions_for_branch(
        &self,
        timeline: &Timeline,
        branch: BranchId,
        start_exclusive: u64,
        end_inclusive: u64,
    ) -> Vec<ActionRecord> {
        let visibility = crate::timeline::BranchVisibility::new(timeline, branch);
        let mut positions = Vec::new();
        for (branch, ticks) in &self.record_index {
            let Some((start, end)) = visibility.range(*branch, start_exclusive, end_inclusive)
            else {
                continue;
            };
            for (_, indices) in ticks.range((
                std::ops::Bound::Excluded(start),
                std::ops::Bound::Included(end),
            )) {
                positions.extend_from_slice(indices);
            }
        }
        positions.sort_unstable();
        positions
            .into_iter()
            .map(|index| self.log.records[index].clone())
            .collect()
    }

    #[must_use]
    pub fn recorded_range(&self, timeline: &Timeline, branch: BranchId) -> (u64, u64) {
        let initial = self.log.initial_tick;
        let visibility = crate::timeline::BranchVisibility::new(timeline, branch);
        let mut end = initial;
        for (branch, ticks) in &self.record_index {
            if let Some((start, limit)) = visibility.range(*branch, initial, u64::MAX)
                && let Some((tick, _)) = ticks
                    .range((
                        std::ops::Bound::Excluded(start),
                        std::ops::Bound::Included(limit),
                    ))
                    .next_back()
            {
                end = end.max(*tick);
            }
        }
        for checkpoint in &self.log.branch_checkpoints {
            if visibility.contains(checkpoint.branch_id, checkpoint.tick) {
                end = end.max(checkpoint.tick);
            }
        }
        for (branch, checksums) in &self.log.branch_checksums {
            if let Some((start, limit)) = visibility.range(*branch, initial, u64::MAX)
                && let Some((tick, _)) = checksums
                    .range((
                        std::ops::Bound::Excluded(start),
                        std::ops::Bound::Included(limit),
                    ))
                    .next_back()
            {
                end = end.max(*tick);
            }
        }
        for (branch, ticks) in &self.log.completed_ticks {
            if let Some((start, limit)) = visibility.range(*branch, initial, u64::MAX)
                && let Some(tick) = ticks
                    .range((
                        std::ops::Bound::Excluded(start),
                        std::ops::Bound::Included(limit),
                    ))
                    .next_back()
            {
                end = end.max(*tick);
            }
        }
        (initial, end)
    }

    pub(crate) fn append_records(&mut self, records: impl IntoIterator<Item = ActionRecord>) {
        for record in records {
            self.record_index
                .entry(record.branch_id)
                .or_default()
                .entry(record.tick)
                .or_default()
                .push(self.log.records.len());
            self.log.records.push(record);
        }
    }
    pub(crate) fn record_frame(
        &mut self,
        branch: BranchId,
        tick: u64,
        records: Vec<ActionRecord>,
    ) -> Result<(), String> {
        let bytes = records.iter().try_fold(128usize, |total, record| {
            let size = serde_json::to_vec(record)
                .map_err(|error| error.to_string())?
                .len();
            let size = size
                .checked_add(action_heap(record)?)
                .ok_or_else(|| "history byte counter overflow".to_string())?;
            total
                .checked_add(size + 256)
                .ok_or_else(|| "history byte counter overflow".to_string())
        })?;
        self.reserve_history(bytes)?;
        self.log.record_tick(branch, tick);
        self.append_records(records);
        Ok(())
    }
    fn reserve_history(&mut self, additional: usize) -> Result<(), String> {
        self.retained_bytes = self.check_history_capacity(additional)?;
        Ok(())
    }
    fn check_history_capacity(&self, additional: usize) -> Result<usize, String> {
        let next = self
            .retained_bytes
            .checked_add(additional)
            .ok_or_else(|| "history byte counter overflow".to_string())?;
        if next > self.max_history_bytes {
            return Err(
                "replay history byte budget exhausted; start a new recording or reset".into(),
            );
        }
        Ok(next)
    }
    fn topology_charge(&self, timeline: &Timeline) -> Result<usize, String> {
        let previous = serde_json::to_vec(&self.log.timeline_topology)
            .map_err(|e| e.to_string())?
            .len();
        let next: Vec<_> = timeline.branches().values().collect();
        let encoded = serde_json::to_vec(&next).map_err(|e| e.to_string())?.len();
        Ok(encoded.saturating_sub(previous)
            + next.len().saturating_sub(self.log.timeline_topology.len()) * 512)
    }

    /// Save only metadata touched by a fork. Reconstruction never appends actions.
    pub fn branch_savepoint(&self) -> BranchSavepoint {
        BranchSavepoint {
            topology: self.log.timeline_topology.clone(),
            active: self.log.active_branch,
            cursor: self.log.cursor_tick,
            end: self.log.end_tick,
            checkpoints: self.log.branch_checkpoints.len(),
            bytes: self.retained_bytes,
        }
    }
    pub fn rollback_branch(&mut self, backup: BranchSavepoint) {
        let branches: std::collections::BTreeSet<_> =
            backup.topology.iter().map(|b| b.branch_id).collect();
        self.log
            .branch_checksums
            .retain(|id, _| branches.contains(id));
        self.log
            .completed_ticks
            .retain(|id, _| branches.contains(id));
        self.log.branch_checkpoints.truncate(backup.checkpoints);
        self.log.timeline_topology = backup.topology;
        self.log.active_branch = backup.active;
        self.log.cursor_tick = backup.cursor;
        self.log.end_tick = backup.end;
        self.retained_bytes = backup.bytes;
    }

    pub(crate) fn rebuild_record_index(&mut self) {
        self.record_index.clear();
        for (index, record) in self.log.records.iter().enumerate() {
            self.record_index
                .entry(record.branch_id)
                .or_default()
                .entry(record.tick)
                .or_default()
                .push(index);
        }
    }

    fn validate_cursor(&self, timeline: &Timeline, tick: u64) -> Result<(), String> {
        self.validate_topology_extension(timeline)?;
        validate_clock_tick(tick).map_err(|error| error.to_string())?;
        let info = &timeline.branches()[&timeline.current_branch()];
        if tick < info.fork_tick {
            return Err(format!(
                "cannot step before active branch fork {}; create a branch at tick {tick} first",
                info.fork_tick
            ));
        }
        if tick < self.log.initial_tick {
            return Err("cursor precedes recording baseline".to_string());
        }
        Ok(())
    }

    /// Incremental synchronization may add branches, but must preserve every
    /// admitted branch's identity and visibility intervals. Replacing history
    /// with a different tree requires checked `replace_log` admission.
    fn validate_topology_extension(&self, timeline: &Timeline) -> Result<(), String> {
        timeline.validate()?;
        for previous in &self.log.timeline_topology {
            let current = timeline
                .branches()
                .get(&previous.branch_id)
                .ok_or_else(|| "topology removes an admitted history branch".to_string())?;
            if current.parent_branch != previous.parent_branch
                || current.fork_tick != previous.fork_tick
                || current.fork_snapshot != previous.fork_snapshot
            {
                return Err("topology changes an admitted branch's history visibility".to_string());
            }
        }
        Ok(())
    }
}

fn history_charge(log: &ReplayLog) -> Result<usize, String> {
    let encoded = serde_json::to_vec(log)
        .map_err(|error| error.to_string())?
        .len();
    let ticks = log
        .completed_ticks
        .values()
        .map(std::collections::BTreeSet::len)
        .sum::<usize>();
    let checksums = log
        .branch_checksums
        .values()
        .map(std::collections::BTreeMap::len)
        .sum::<usize>();
    let parts = [
        (log.records.len(), 256usize),
        (ticks, 128),
        (checksums, 128),
        (log.branch_checkpoints.len(), 384),
        (log.timeline_topology.len(), 512),
    ];
    let mut bytes = parts
        .into_iter()
        .try_fold(encoded, |bytes, (count, charge)| {
            count
                .checked_mul(charge)
                .and_then(|n| bytes.checked_add(n))
                .ok_or_else(|| "history byte counter overflow".to_string())
        })?;
    for record in &log.records {
        bytes = bytes
            .checked_add(action_heap(record)?)
            .ok_or_else(|| "history byte counter overflow".to_string())?;
    }
    Ok(bytes)
}

fn action_heap(record: &ActionRecord) -> Result<usize, String> {
    match &record.action {
        bevy_agent_core::AgentAction::Custom { value, .. } => {
            bevy_agent_core::json_heap_bytes(value)
                .ok_or_else(|| "history byte counter overflow".into())
        }
        _ => Ok(0),
    }
}

/// Opaque rollback record for branch creation, independent of action count.
pub struct BranchSavepoint {
    topology: Vec<crate::TimelineBranch>,
    active: Option<BranchId>,
    cursor: u64,
    end: u64,
    checkpoints: usize,
    bytes: usize,
}

impl ReplayLog {
    /// Pure history validation shared by live admission and portable bundles.
    pub fn validate_history(&self) -> Result<(), String> {
        if self.manifest.schema_version != REPLAY_SCHEMA_VERSION {
            return Err(format!(
                "unsupported replay schema {}",
                self.manifest.schema_version
            ));
        }
        for tick in [self.initial_tick, self.cursor_tick, self.end_tick] {
            validate_clock_tick(tick).map_err(|error| error.to_string())?;
        }
        if self.cursor_tick < self.initial_tick
            || self.cursor_tick > self.end_tick
            || self.initial_tick > self.end_tick
        {
            return Err("replay recording bounds are inconsistent".to_string());
        }
        let timeline = self.validated_timeline(TimelineId::default())?;
        let check = |branch: BranchId, tick: u64, context: &str| -> Result<(), String> {
            validate_clock_tick(tick).map_err(|error| error.to_string())?;
            let info = timeline
                .branches()
                .get(&branch)
                .ok_or_else(|| format!("replay {context} references unknown branch {branch:?}"))?;
            if tick < info.fork_tick || tick < self.initial_tick || tick > self.end_tick {
                return Err(format!(
                    "replay {context} at tick {tick} lies outside branch/recording bounds"
                ));
            }
            Ok(())
        };
        for record in &self.records {
            check(record.branch_id, record.tick, "record")?;
        }
        let mut checkpoint_keys = std::collections::BTreeSet::new();
        for checkpoint in &self.branch_checkpoints {
            check(checkpoint.branch_id, checkpoint.tick, "checkpoint")?;
            if checkpoint.episode != self.manifest.episode_id
                || !checkpoint_keys.insert((checkpoint.branch_id, checkpoint.tick))
            {
                return Err("replay has duplicate or cross-episode checkpoint".to_string());
            }
        }
        for (branch, checksums) in &self.branch_checksums {
            if !timeline.branches().contains_key(branch) {
                return Err(format!(
                    "replay checksums reference unknown branch {branch:?}"
                ));
            }
            for (tick, checksum) in checksums {
                check(*branch, *tick, "checksum")?;
                if checksum.tick != *tick {
                    return Err("replay checksum tick differs from its key".to_string());
                }
            }
        }
        for (branch, ticks) in &self.completed_ticks {
            if !timeline.branches().contains_key(branch) {
                return Err(format!(
                    "replay completed ticks reference unknown branch {branch:?}"
                ));
            }
            for tick in ticks {
                check(*branch, *tick, "completed tick")?;
            }
        }
        Ok(())
    }

    pub fn validated_timeline(&self, id: TimelineId) -> Result<Timeline, String> {
        let active = self
            .active_branch
            .ok_or_else(|| "replay requires an active branch".to_string())?;
        Timeline::from_branches(id, active, self.timeline_topology.clone())
    }
}
