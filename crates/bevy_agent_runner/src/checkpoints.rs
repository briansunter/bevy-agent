//! Checkpoint ownership, snapshot retention, and reset / direct restore.

use super::*;

/// Computes a snapshot checksum that excludes the pending action queue.
///
/// Pending futures beyond a restore target must NOT affect tick-target state
/// verification: the queue at verify time (cleared + current frame + maybe
/// re-applied futures) differs from the queue at record time. Both the stored
/// expectation and the replay-time actual are therefore computed with the
/// queue stripped (the `action_queue` field and the serialized
/// `AgentActionQueue` resource entry), so verification compares pure
/// simulation state.
pub fn queue_excluded_checksum(snapshot: &Snapshot) -> Result<SnapshotChecksum> {
    let mut stripped = snapshot.clone();
    stripped.action_queue.clear();
    stripped
        .resources
        .retain(|resource| resource.type_id != <AgentActionQueue as SnapshotType>::TYPE_ID);
    checksum_snapshot(&stripped)
}

/// Captures the current world and checksums it excluding the queue (see
/// [`queue_excluded_checksum`]).
pub(super) fn capture_queue_excluded_checksum(world: &mut World) -> Result<SnapshotChecksum> {
    let snapshot = capture_snapshot(world, None)?;
    queue_excluded_checksum(&snapshot)
}

/// Marks an episode boundary by starting a fresh replay log and timeline root.
///
/// Called from `AgentApp::reset` after the `AgentReset` schedule and before the
/// optional initial snapshot. It replaces the `ReplayLog` with a fresh one while
/// preserving the `ReplayRecorder.recording` flag, installs a fresh `Timeline`
/// root, and synchronizes `AgentControlState.timeline_id`/`branch_id` to that
/// new root. It is a no-op for apps that do not install the replay resources, so
/// core-only and snapshot-less configurations remain supported.
fn reset_replay_and_timeline(world: &mut World) -> Result<()> {
    if !world.contains_resource::<Timeline>() {
        return Ok(());
    }
    world.insert_resource(Timeline::default());
    let (timeline_id, branch_id) = {
        let timeline = world.resource::<Timeline>();
        (timeline.timeline_id(), timeline.current_branch())
    };
    if world.contains_resource::<ReplayRecorder>() {
        world.resource_scope(|world, mut recorder: Mut<ReplayRecorder>| {
            recorder
                .reset_episode(
                    world.resource::<EnvironmentMetadata>(),
                    world.resource::<Timeline>(),
                    0,
                )
                .map_err(anyhow::Error::msg)
        })?;
        let episode = world.resource::<ReplayRecorder>().log().manifest.episode_id;
        set_snapshot_episode(world, episode);
    }
    let mut control = world.resource_mut::<AgentControlState>();
    control.timeline_id = timeline_id;
    control.branch_id = branch_id;
    world.insert_resource(ExecutionContext::Live);
    Ok(())
}

impl AgentApp {
    /// Creates a role-aware checkpoint. Callers index it in the replay log
    /// before enforcing retention so the new live reference is protected.
    pub(super) fn create_owned_checkpoint(
        &mut self,
        label: Option<String>,
        role: SnapshotRole,
    ) -> Result<SnapshotCreateResult> {
        create_snapshot_with_role(self.app.world_mut(), label, role)
    }

    pub(super) fn index_checkpoint(
        &mut self,
        branch: BranchId,
        tick: u64,
        id: SnapshotId,
        checksum: SnapshotChecksum,
    ) -> Result<()> {
        if !self.app.world().contains_resource::<ReplayRecorder>() {
            return Ok(());
        }
        let world = self.app.world_mut();
        world.resource_scope(|world, mut recorder: Mut<ReplayRecorder>| {
            recorder
                .index_checkpoint(world.resource::<Timeline>(), branch, tick, id, checksum)
                .map_err(anyhow::Error::msg)
        })?;
        Ok(())
    }

    fn live_snapshot_references(&self) -> BTreeSet<SnapshotId> {
        let mut references = self
            .replay_log()
            .map(collect_replay_references)
            .unwrap_or_default();
        if let Some(timeline) = self.app.world().get_resource::<Timeline>() {
            references.extend(
                timeline
                    .branches()
                    .values()
                    .filter_map(|branch| branch.fork_snapshot),
            );
        }
        references
    }

    /// Retention runs after indexing with both log and live graph references.
    pub(super) fn prune_with_live_refs(&mut self) {
        let Some(keep) = self
            .app
            .world()
            .get_resource::<SnapshotPolicy>()
            .map(|policy| policy.keep_last_n_checkpoints)
        else {
            return;
        };
        let references = self.live_snapshot_references();
        prune_checkpoints_with_refs(self.app.world_mut(), keep, &references);
    }

    /// Rejects deletion of pinned snapshots and live replay / graph references.
    pub fn delete_snapshot(&mut self, id: SnapshotId) -> Result<()> {
        let references = self.live_snapshot_references();
        delete_snapshot_checked(self.app.world_mut(), id, &references)
    }

    /// Explicit CheckpointCreated handling: mirrors only the checkpoint
    /// created in the current episode (via `control.last_snapshot_created`,
    /// set by `create_snapshot` inside `AgentTick`) into the replay log.
    /// No scan-all-store behavior: stale cross-episode checkpoints never
    /// contaminate the fresh log. Tagged with (episode, branch, tick).
    ///
    /// Owner-side indexing: after `maybe_take_snapshot` creates an id, it is
    /// immediately indexed into the log and retention is enforced with live
    /// refs (the snapshot crate no longer prunes with replay visibility).
    pub(super) fn sync_auto_checkpoints(&mut self) -> Result<()> {
        // Replay-disabled retention: even without a recorder, snapshot-only
        // retention still applies (empty reference set).
        if !self.app.world().contains_resource::<ReplayRecorder>() {
            if self.app.world().contains_resource::<SnapshotStore>() {
                enforce_retention(self.app.world_mut(), &BTreeSet::new());
            }
            return Ok(());
        }
        if !self.app.world().resource::<ReplayRecorder>().is_recording() {
            self.prune_with_live_refs();
            return Ok(());
        }
        let (branch, created) = match (
            self.app.world().get_resource::<AgentControlState>(),
            self.app.world().get_resource::<ReplayRecorder>(),
        ) {
            (Some(control), Some(_recorder)) => (control.branch_id, control.last_snapshot_created),
            _ => return Ok(()),
        };
        if !self.app.world().contains_resource::<SnapshotStore>() {
            return Ok(());
        }
        let Some(id) = created else {
            return Ok(());
        };
        let known = match self.app.world().get_resource::<ReplayRecorder>() {
            Some(recorder) => recorder
                .log()
                .branch_checkpoints
                .iter()
                .map(|checkpoint| checkpoint.snapshot_id)
                .collect::<BTreeSet<_>>(),
            None => return Ok(()),
        };
        if known.contains(&id) {
            return Ok(());
        }
        let (tick, checksum, episode) = match self
            .app
            .world()
            .get_resource::<SnapshotStore>()
            .and_then(|store| store.get(id))
        {
            Some(snapshot) => (
                snapshot.manifest.tick,
                queue_excluded_checksum(snapshot).unwrap_or_else(|_| snapshot.checksum.clone()),
                snapshot.manifest.episode_id,
            ),
            None => return Ok(()),
        };
        if episode
            != self
                .app
                .world()
                .resource::<ReplayRecorder>()
                .log()
                .manifest
                .episode_id
        {
            return Ok(());
        }
        self.index_checkpoint(branch, tick, id, checksum)?;
        // Owner-side retention: the snapshot creation path no longer prunes
        // with replay visibility, so enforce here with live refs immediately
        // after indexing.
        self.prune_with_live_refs();
        Ok(())
    }

    /// Honors `SnapshotPolicy::checkpoint_on_terminal`: snapshots terminal
    /// steps into the replay log on the current branch.
    pub(super) fn maybe_terminal_checkpoint(&mut self) -> Result<()> {
        let terminal = match self.app.world().get_resource::<LastStepResponse>() {
            Some(last) => last
                .0
                .as_ref()
                .is_some_and(|response| response.done || response.truncated),
            None => false,
        };
        if !terminal || !self.has_snapshot_support() {
            return Ok(());
        }
        let enabled = self
            .app
            .world()
            .get_resource::<SnapshotPolicy>()
            .map(|policy| policy.checkpoint_on_terminal)
            .unwrap_or(false);
        if !enabled {
            return Ok(());
        }
        let branch = match self.app.world().get_resource::<AgentControlState>() {
            Some(control) => control.branch_id,
            None => return Ok(()),
        };
        // Avoid double-snapshotting when the interval policy already captured
        // this tick.
        let tick = self.current_tick();
        let already = self
            .app
            .world()
            .resource::<AgentControlState>()
            .last_snapshot_created
            .and_then(|id| self.app.world().resource::<SnapshotStore>().get(id))
            .is_some_and(|snapshot| snapshot.manifest.tick == tick);
        if already {
            return Ok(());
        }
        let result =
            self.create_owned_checkpoint(Some(format!("terminal-{tick}")), SnapshotRole::Periodic)?;
        let checksum = self
            .app
            .world()
            .get_resource::<SnapshotStore>()
            .and_then(|store| store.get(result.snapshot_id))
            .and_then(|snapshot| queue_excluded_checksum(snapshot).ok())
            .unwrap_or_else(|| result.checksum.clone());
        if self
            .app
            .world()
            .get_resource::<ReplayRecorder>()
            .is_some_and(ReplayRecorder::is_recording)
        {
            self.index_checkpoint(branch, result.tick, result.snapshot_id, checksum)?;
        }
        self.prune_with_live_refs();
        Ok(())
    }

    /// Ensures the latest observation response reports a snapshot created on
    /// the same tick: automatic checkpoints run after the observation system
    /// inside `AgentFinalize`, so the stored response is patched with the current
    /// `last_snapshot_created` marker when set.
    pub(super) fn patch_snapshot_created(&mut self) {
        let snapshot = self
            .app
            .world()
            .get_resource::<AgentControlState>()
            .and_then(|control| control.last_snapshot_created);
        if let Some(snapshot) = snapshot
            && let Some(mut last) = self.app.world_mut().get_resource_mut::<LastStepResponse>()
            && let Some(response) = last.0.as_mut()
        {
            response.info.snapshot_created = Some(snapshot);
        }
    }

    pub(super) fn has_snapshot_support(&self) -> bool {
        self.app.world().contains_resource::<SnapshotStore>()
    }

    pub(super) fn reset_impl(&mut self, options: ResetOptions) -> Result<Observation> {
        self.validate_observation_mode(&options.observation_mode)?;
        self.ensure_started();
        let tick_before = self.current_tick();
        let result = self.apply_reset(options);
        match result {
            Ok(observation) => {
                self.reset_once = true;
                self.app.world_mut().remove_resource::<FaultState>();
                Ok(observation)
            }
            Err(error) => {
                self.reset_once = false;
                Err(self.mutation_failed("reset", tick_before, error))
            }
        }
    }

    fn apply_reset(&mut self, options: ResetOptions) -> Result<Observation> {
        {
            let world = self.app.world_mut();
            world.resource_mut::<ObservationConfig>().mode = options.observation_mode;
            if let Some(seed) = options.seed {
                world.insert_resource(DeterministicRng::seeded(seed));
            }
        }

        self.app.world_mut().run_schedule(AgentReset);
        if let Some(error) = self
            .app
            .world()
            .resource::<bevy_agent_core::AgentTickFailure>()
            .error()
        {
            return Err(anyhow!("reset observation failed: {error}"));
        }
        reset_replay_and_timeline(self.app.world_mut())?;

        let mut response = self.last_response()?;
        {
            let control = self.app.world().resource::<AgentControlState>();
            response.info.timeline_id = control.timeline_id;
            response.info.branch_id = control.branch_id;
        }
        if options.create_initial_snapshot && self.has_snapshot_support() {
            let snapshot = self.create_owned_checkpoint(
                Some(format!("reset-{}", response.tick)),
                SnapshotRole::Initial,
            )?;
            let branch = self
                .app
                .world()
                .get_resource::<AgentControlState>()
                .map(|control| control.branch_id);
            let checksum = self
                .app
                .world()
                .get_resource::<SnapshotStore>()
                .and_then(|store| store.get(snapshot.snapshot_id))
                .and_then(|stored| queue_excluded_checksum(stored).ok())
                .unwrap_or_else(|| snapshot.checksum.clone());
            if self.app.world().contains_resource::<ReplayRecorder>() {
                let world = self.app.world_mut();
                world.resource_scope(|world, mut recorder: Mut<ReplayRecorder>| {
                    recorder
                        .set_baseline(
                            snapshot.snapshot_id,
                            snapshot.tick,
                            world.resource::<Timeline>(),
                        )
                        .map_err(anyhow::Error::msg)
                })?;
                self.index_checkpoint(
                    branch.expect("core control exists"),
                    snapshot.tick,
                    snapshot.snapshot_id,
                    checksum,
                )?;
            }
            response.info.snapshot_created = Some(snapshot.snapshot_id);
        }
        self.app.world_mut().resource_mut::<LastStepResponse>().0 = Some(response.clone());
        self.prune_with_live_refs();
        Ok(response.observation)
    }

    pub(super) fn snapshot_impl(&mut self) -> Result<SnapshotCreateResult> {
        self.ensure_no_fault()?;
        self.ensure_started();
        self.ensure_reset()?;
        if !self.has_snapshot_support() {
            return Err(anyhow!("AgentSnapshotPlugin is not installed"));
        }

        let tick = self.current_tick();
        if let Some(recorder) = self.app.world().get_resource::<ReplayRecorder>() {
            let branch = self.app.world().resource::<AgentControlState>().branch_id;
            let timeline = self.app.world().resource::<Timeline>();
            let info = timeline
                .branches()
                .get(&branch)
                .ok_or_else(|| anyhow!("active branch is missing"))?;
            if tick < info.fork_tick || tick < recorder.log().initial_tick {
                return Err(anyhow!(
                    "cannot checkpoint before active branch recording interval; create a branch at tick {tick} first"
                ));
            }
        }
        if let Some(recorder) = self.app.world().get_resource::<ReplayRecorder>() {
            recorder
                .validate_checkpoint_capacity(
                    self.app.world().resource::<Timeline>(),
                    self.app.world().resource::<AgentControlState>().branch_id,
                    tick,
                )
                .map_err(anyhow::Error::msg)?;
        }
        let result =
            self.create_owned_checkpoint(Some(format!("manual-{tick}")), SnapshotRole::Manual)?;
        // Tag with the current branch to preserve same-tick isolation;
        // checkpoints remain isolated between branches at the same tick.
        let branch = self
            .app
            .world()
            .get_resource::<AgentControlState>()
            .map(|control| control.branch_id)
            .or_else(|| {
                self.app
                    .world()
                    .get_resource::<Timeline>()
                    .map(|timeline| timeline.current_branch())
            });
        let checksum = self
            .app
            .world()
            .get_resource::<SnapshotStore>()
            .and_then(|store| store.get(result.snapshot_id))
            .and_then(|snapshot| queue_excluded_checksum(snapshot).ok())
            .unwrap_or_else(|| result.checksum.clone());
        if let Some(branch) = branch {
            self.index_checkpoint(branch, result.tick, result.snapshot_id, checksum)?;
        }
        self.prune_with_live_refs();
        Ok(result)
    }

    pub(super) fn restore_impl(&mut self, snapshot: SnapshotId) -> Result<()> {
        self.ensure_no_fault()?;
        let stored = self
            .app
            .world()
            .get_resource::<SnapshotStore>()
            .ok_or_else(|| anyhow!("AgentSnapshotPlugin is not installed"))?
            .get(snapshot)
            .ok_or_else(|| anyhow!("snapshot {snapshot:?} not found"))?;
        validate_snapshot_full(self.app.world(), stored)?;
        self.ensure_started();
        self.history_transaction(
            &format!("restore snapshot {snapshot:?}"),
            transaction::TransactionKind::World,
            |agent| agent.apply_stored_snapshot(snapshot),
        )
    }

    /// Applies a checkpoint inside an existing outer navigation transaction.
    pub(super) fn apply_stored_snapshot(&mut self, id: SnapshotId) -> Result<()> {
        let snapshot = self
            .app
            .world()
            .resource::<SnapshotStore>()
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("snapshot {id:?} not found"))?;
        restore_snapshot_backup(self.app.world_mut(), &snapshot)?;
        let restored_tick = self.current_tick();
        let mut control = self.app.world_mut().resource_mut::<AgentControlState>();
        control.frame = restored_tick;
        control.last_snapshot_created = None;
        self.reset_once = true;
        collect_observation(self.app.world_mut())?;
        Ok(())
    }
}
