//! Bounded replay reconstruction and branch navigation.

use super::*;

/// Resolves the fork parent for `branch(from_tick)` to the ancestor owning
/// the `from_tick` interval.
///
/// Walks the lineage from the current branch (deepest first) and returns the
/// deepest ancestor whose fork interval contains `from_tick`: the target
/// itself when `from_tick >= own_fork`, otherwise an ancestor with
/// `from_tick >= own_fork && from_tick <= child_fork` (inclusive bounds so
/// the tick-0 baseline resolves to the root). Rejects when no ancestor
/// contains the tick so the import validator invariant (child fork >= parent
/// fork) always holds for the new branch.
fn resolve_fork_parent_branch(
    timeline: &Timeline,
    current: BranchId,
    from_tick: u64,
) -> Result<BranchId> {
    let chain = timeline.lineage(current);
    if !chain.contains(&current) && !timeline.branches().contains_key(&current) {
        return Err(anyhow!(
            "branch target {from_tick} references unknown branch {current:?}"
        ));
    }
    // Deepest first: `lineage` returns branch-first, root-last.
    for candidate in &chain {
        let Some(info) = timeline.branches().get(candidate) else {
            continue;
        };
        if *candidate == current {
            if from_tick >= info.fork_tick {
                return Ok(*candidate);
            }
            continue;
        }
        let Some(child_fork) = branch_fork_from_ancestor(timeline, *candidate, current) else {
            continue;
        };
        if from_tick >= info.fork_tick && from_tick <= child_fork {
            return Ok(*candidate);
        }
    }
    Err(anyhow!(
        "branch target {from_tick} is not owned by any ancestor of {current:?}"
    ))
}

/// Bounds + lineage visibility check for history navigation targets.
///
/// Rejects `tick < start` (lower bound), `tick > end` (per-branch upper
/// bound from [`ReplayLog::recorded_range`]), unknown branches, and over-deep/cyclic
/// lineages — all BEFORE any world mutation so failed navigation leaves
/// state untouched. Targets outside this branch's recorded range are rejected.
pub(super) fn validate_history_target(
    log: &ReplayLog,
    timeline: &Timeline,
    branch: BranchId,
    tick: u64,
) -> Result<()> {
    timeline.validate().map_err(anyhow::Error::msg)?;
    if !timeline.branches().contains_key(&branch) {
        return Err(anyhow!(
            "history target {tick} references unknown branch {branch:?}"
        ));
    }
    let (start, end) = log.recorded_range(timeline, branch);
    if tick < start {
        return Err(anyhow!(
            "history target {tick} precedes recording baseline {start}"
        ));
    }
    if tick > end {
        return Err(anyhow!(
            "restore target {tick} is beyond recorded end {end}"
        ));
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct HistoryNavigation {
    branch: BranchId,
    checkpoint_tick: u64,
    snapshot_id: SnapshotId,
}

/// Copy only fork-visible checksum expectations in the requested interval.
/// Root-first insertion gives the deepest branch precedence at shared fork ticks.
fn visible_checksums(
    log: &ReplayLog,
    timeline: &Timeline,
    branch: BranchId,
    start_inclusive: u64,
    end: u64,
) -> BTreeMap<u64, SnapshotChecksum> {
    let mut result = BTreeMap::new();
    for ancestor in timeline.lineage(branch).into_iter().rev() {
        let upper = if ancestor == branch {
            end
        } else {
            end.min(branch_fork_from_ancestor(timeline, ancestor, branch).unwrap_or(0))
        };
        if start_inclusive > upper {
            continue;
        }
        if let Some(checksums) = log.branch_checksums.get(&ancestor) {
            result.extend(
                checksums
                    .range(start_inclusive..=upper)
                    .map(|(tick, checksum)| (*tick, checksum.clone())),
            );
        }
    }
    result
}

impl AgentApp {
    /// Validates recorded navigation and its actual checkpoint-to-target cost
    /// without resetting, restoring, or changing the world.
    pub fn validate_history_navigation(
        &self,
        tick: u64,
        max_reconstruction_ticks: u64,
    ) -> Result<()> {
        self.ensure_no_fault()?;
        self.history_navigation(tick, max_reconstruction_ticks)
            .map(|_| ())
    }

    fn history_navigation(
        &self,
        tick: u64,
        max_reconstruction_ticks: u64,
    ) -> Result<HistoryNavigation> {
        let recorder = self
            .app
            .world()
            .get_resource::<ReplayRecorder>()
            .ok_or_else(|| anyhow!("AgentReplayPlugin is not installed"))?;
        let log = recorder.log();
        let timeline = self
            .app
            .world()
            .get_resource::<Timeline>()
            .ok_or_else(|| anyhow!("AgentReplayPlugin is not installed"))?;
        let branch = self.app.world().resource::<AgentControlState>().branch_id;
        timeline
            .ancestors_bounded(branch)
            .map_err(anyhow::Error::msg)?;
        let control = self.app.world().resource::<AgentControlState>();
        if branch != timeline.current_branch() || control.timeline_id != timeline.timeline_id() {
            return Err(anyhow!("control identity disagrees with active timeline"));
        }
        let (start, end) = recorder.recorded_range(timeline, branch);
        if tick < start {
            return Err(anyhow!(
                "history target {tick} precedes recording baseline {start}"
            ));
        }
        if tick > end {
            return Err(anyhow!(
                "restore target {tick} is beyond recorded end {end}"
            ));
        }
        let (checkpoint_tick, snapshot_id) = log
            .nearest_checkpoint_for_branch(timeline, branch, tick)
            .ok_or_else(|| anyhow!("no checkpoint exists at or before tick {tick}"))?;
        let budget = max_reconstruction_ticks.min(MAX_RECONSTRUCTION_TICKS);
        let interval = tick - checkpoint_tick;
        if interval > budget {
            return Err(anyhow!(
                "restore interval {interval} exceeds budget {budget}"
            ));
        }
        let snapshot = self
            .app
            .world()
            .get_resource::<SnapshotStore>()
            .ok_or_else(|| anyhow!("AgentSnapshotPlugin is not installed"))?
            .get(snapshot_id)
            .ok_or_else(|| anyhow!("replay references missing snapshot {snapshot_id:?}"))?;
        if snapshot.clock.tick != checkpoint_tick || snapshot.manifest.tick != checkpoint_tick {
            return Err(anyhow!(
                "checkpoint tick {checkpoint_tick} does not match its snapshot"
            ));
        }
        validate_snapshot_full(self.app.world(), snapshot)?;
        if let Some(expected) =
            visible_checksums(log, timeline, branch, checkpoint_tick, checkpoint_tick)
                .get(&checkpoint_tick)
            && &queue_excluded_checksum(snapshot)? != expected
        {
            return Err(anyhow!(
                "checkpoint tick {checkpoint_tick} checksum mismatch"
            ));
        }
        Ok(HistoryNavigation {
            branch,
            checkpoint_tick,
            snapshot_id,
        })
    }

    pub fn replay_log(&self) -> Option<&ReplayLog> {
        self.app
            .world()
            .get_resource::<ReplayRecorder>()
            .map(ReplayRecorder::log)
    }

    /// Seals the active recording and returns its action count.
    /// Repeated calls leave already sealed history unchanged.
    pub fn stop_recording(&mut self) -> Result<usize> {
        self.ensure_no_fault()?;
        if !self.app.world().contains_resource::<ReplayRecorder>() {
            return Err(anyhow!("AgentReplayPlugin is not installed"));
        }
        bevy_agent_replay::stop_recording(self.app.world_mut()).map_err(anyhow::Error::msg)
    }

    /// Starts a recording from the current live state. An explicit baseline
    /// requires a reset environment and must exactly match its state and episode.
    /// Omitting the baseline captures a fresh snapshot when supported.
    pub fn start_recording(&mut self, initial_snapshot: Option<SnapshotId>) -> Result<()> {
        self.ensure_no_fault()?;
        if !self.app.world().contains_resource::<ReplayRecorder>() {
            return Err(anyhow!("AgentReplayPlugin is not installed"));
        }
        if let Some(id) = initial_snapshot {
            let snapshot = self
                .app
                .world()
                .get_resource::<SnapshotStore>()
                .ok_or_else(|| anyhow!("explicit recording baseline requires AgentSnapshotPlugin"))?
                .shared(id)
                .ok_or_else(|| anyhow!("recording baseline {id:?} not found"))?;
            if !self.has_reset() {
                return Err(anyhow!(
                    "reset before starting a recording with an explicit baseline"
                ));
            }
            let episode = self
                .replay_log()
                .expect("checked recorder")
                .manifest
                .episode_id;
            if snapshot.clock.tick != self.current_tick() || snapshot.manifest.episode_id != episode
            {
                return Err(anyhow!(
                    "recording baseline does not match current tick and episode"
                ));
            }
            validate_snapshot_full(self.app.world(), &snapshot)?;
            let live = capture_snapshot(self.app.world_mut(), None)?;
            if live.checksum != snapshot.checksum {
                return Err(anyhow!(
                    "recording baseline does not match current live state"
                ));
            }
        }
        self.ensure_started();
        self.ensure_reset()?;
        if !self.has_snapshot_support() {
            return start_recording(self.app.world_mut(), None).map_err(anyhow::Error::msg);
        }
        self.history_transaction(
            "start recording",
            transaction::TransactionKind::Replace(initial_snapshot),
            |agent| {
                let current = agent.current_tick();
                let baseline = match initial_snapshot {
                    Some(id) => id,
                    None => {
                        agent
                            .create_owned_checkpoint(
                                Some(format!("baseline-{current}")),
                                SnapshotRole::RecordingBaseline,
                            )?
                            .snapshot_id
                    }
                };
                start_recording(agent.app.world_mut(), Some(baseline))
                    .map_err(anyhow::Error::msg)?;
                let episode = agent.replay_log().expect("recorder").manifest.episode_id;
                set_snapshot_episode(agent.app.world_mut(), episode);
                let checksum = queue_excluded_checksum(
                    agent
                        .app
                        .world()
                        .resource::<SnapshotStore>()
                        .get(baseline)
                        .expect("admitted baseline"),
                )?;
                agent.index_checkpoint(
                    agent.app.world().resource::<Timeline>().current_branch(),
                    current,
                    baseline,
                    checksum,
                )?;
                agent.prune_with_live_refs();
                Ok(())
            },
        )
    }

    /// Truncates recorded future actions/checkpoints/checksums on the current
    /// branch beyond the current tick (explicit diverge after restore).
    /// Records on other branches are preserved, including their
    /// branch-aware checksum expectations: only the current branch's future
    /// beyond `tick` is dropped by `ReplayLog::truncate_future`.
    pub(super) fn enforce_diverge_truncation(&mut self) -> Result<()> {
        if !self
            .app
            .world()
            .get_resource::<ReplayRecorder>()
            .is_some_and(ReplayRecorder::is_recording)
        {
            return Ok(());
        }
        let branch = self.app.world().resource::<AgentControlState>().branch_id;
        let tick = self.current_tick();
        let world = self.app.world_mut();
        world.resource_scope(|world, mut recorder: Mut<ReplayRecorder>| {
            recorder
                .truncate_future(world.resource::<Timeline>(), branch, tick)
                .map_err(anyhow::Error::msg)
        })?;
        Ok(())
    }

    /// Replays ticks in `(checkpoint_tick, target_tick]` one frame at a time:
    /// only the current tick's recorded frame is enqueued per iteration (perf:
    /// no whole-interval upfront enqueue), preserving recorded source/order.
    /// Empty ticks still run. Each reconstructed tick's state checksum is
    /// verified queue-excluded against the expected checksum when present;
    /// mismatch fails (see [`queue_excluded_checksum`]: pending futures
    /// beyond the target never affect tick-target state).
    ///
    /// The shared execution context suppresses policy decisions, recording,
    /// and automatic snapshots during reconstruction.
    fn replay_tick_interval(
        &mut self,
        interval: ReplayInterval,
        checkpoint_tick: u64,
        target_tick: u64,
    ) -> Result<()> {
        let future_actions = self
            .app
            .world()
            .resource::<AgentActionQueue>()
            .future_after(target_tick)
            .cloned()
            .collect::<Vec<_>>();
        self.app
            .world_mut()
            .resource_mut::<AgentActionQueue>()
            .clear();
        let previous_context = *self.app.world().resource::<ExecutionContext>();
        self.app
            .world_mut()
            .insert_resource(ExecutionContext::Reconstructing);
        let result = (|| -> Result<()> {
            for tick in (checkpoint_tick + 1)..=target_tick {
                if let Some(frame) = interval.actions.get(&tick) {
                    for (source, action) in frame {
                        schedule_action(
                            self.app.world_mut(),
                            tick,
                            source.clone(),
                            action.clone(),
                        )?;
                    }
                }
                self.run_one_reconstructing_tick()?;
                if self.app.world().resource::<LastStepResponse>().0.is_none() {
                    return Err(anyhow!("replay tick produced no StepResponse"));
                }
                if let Some(expected) = interval.checksums.get(&tick) {
                    let actual = capture_queue_excluded_checksum(self.app.world_mut())?;
                    if &actual != expected {
                        return Err(anyhow!(
                            "reconstructed tick {tick} checksum mismatch: expected {expected:?}, got {actual:?}"
                        ));
                    }
                }
            }
            for scheduled in future_actions {
                schedule_action(
                    self.app.world_mut(),
                    scheduled.tick,
                    scheduled.source,
                    scheduled.action,
                )?;
            }
            Ok(())
        })();
        self.app.world_mut().insert_resource(previous_context);
        result
    }

    fn replay_interval(&self, plan: HistoryNavigation, target: u64) -> ReplayInterval {
        let recorder = self.app.world().resource::<ReplayRecorder>();
        let log = recorder.log();
        let timeline = self.app.world().resource::<Timeline>();
        let mut actions = BTreeMap::<u64, Vec<(ActionSource, AgentAction)>>::new();
        for record in
            recorder.actions_for_branch(timeline, plan.branch, plan.checkpoint_tick, target)
        {
            actions
                .entry(record.tick)
                .or_default()
                .push((record.source, record.action));
        }
        let checksums =
            visible_checksums(log, timeline, plan.branch, plan.checkpoint_tick + 1, target);
        ReplayInterval { actions, checksums }
    }

    /// Runs one simulation tick without the `AgentDecision` schedule, used for
    /// history reconstruction so policy execution is disabled during replay.
    /// `CurrentInputFrame` is installed via the queue while the core
    /// `ExecutionContext::Reconstructing` resource makes
    /// `drain_agent_actions` accept all recorded sources without
    /// arbitration, preserving source as metadata.
    pub(super) fn run_one_reconstructing_tick(&mut self) -> Result<()> {
        validate_integration(self.app.world())?;
        self.app.world_mut().resource_mut::<LastStepResponse>().0 = None;
        self.app.world_mut().run_schedule(AgentPreTick);
        self.app.world_mut().run_schedule(AgentTick);
        self.app.world_mut().run_schedule(AgentPostTick);
        self.app.world_mut().run_schedule(AgentFinalize);
        if let Some(error) = self
            .app
            .world()
            .resource::<bevy_agent_core::AgentTickFailure>()
            .error()
        {
            return Err(anyhow!("replay observation failed: {error}"));
        }
        if self.last_response()?.tick != self.current_tick() {
            return Err(anyhow!(
                "replay observation did not finalize the current tick"
            ));
        }
        Ok(())
    }

    pub(super) fn restore_tick_impl(&mut self, tick: u64) -> Result<()> {
        self.ensure_no_fault()?;
        let plan = self.history_navigation(tick, MAX_RECONSTRUCTION_TICKS)?;
        let interval = self.replay_interval(plan, tick);
        self.ensure_started();
        self.history_transaction(
            &format!("restore_tick to {tick}"),
            transaction::TransactionKind::World,
            |agent| agent.restore_tick_from_plan(tick, plan, interval),
        )
    }

    pub(super) fn activate_history_tick(&mut self, tick: u64) -> Result<()> {
        let plan = self.history_navigation(tick, MAX_RECONSTRUCTION_TICKS)?;
        let interval = self.replay_interval(plan, tick);
        self.restore_tick_from_plan(tick, plan, interval)
    }

    fn restore_tick_from_plan(
        &mut self,
        tick: u64,
        plan: HistoryNavigation,
        interval: ReplayInterval,
    ) -> Result<()> {
        let pending_future_before = self
            .app
            .world()
            .resource::<AgentActionQueue>()
            .future_after(tick)
            .cloned()
            .collect::<Vec<_>>();
        self.apply_stored_snapshot(plan.snapshot_id)?;
        self.replay_tick_interval(interval, plan.checkpoint_tick, tick)?;
        if !pending_future_before.is_empty() {
            let world = self.app.world_mut();
            world.resource_scope(|world, mut queue: Mut<AgentActionQueue>| {
                merge_pending_actions(
                    &mut queue,
                    world.resource::<AgentActionCatalog>(),
                    tick,
                    pending_future_before,
                )
            })?;
        }
        Ok(())
    }

    pub(super) fn branch_impl(
        &mut self,
        from_tick: u64,
        label: Option<String>,
    ) -> Result<bevy_agent_core::BranchId> {
        self.ensure_no_fault()?;
        let plan = self.history_navigation(from_tick, MAX_RECONSTRUCTION_TICKS)?;
        let interval = self.replay_interval(plan, from_tick);
        let fork_parent = resolve_fork_parent_branch(
            self.app.world().resource::<Timeline>(),
            plan.branch,
            from_tick,
        )?;
        self.ensure_started();
        self.history_transaction(
            &format!("branch at {from_tick}"),
            transaction::TransactionKind::Branch,
            |agent| {
                agent.restore_tick_from_plan(from_tick, plan, interval)?;
                agent.finish_branch(from_tick, fork_parent, label)
            },
        )
    }

    fn finish_branch(
        &mut self,
        from_tick: u64,
        fork_parent: BranchId,
        label: Option<String>,
    ) -> Result<BranchId> {
        // `checkpoint_on_branch` (default true) snapshots the fork point so the
        // child starts from a restorable checkpoint on its own branch.
        let checkpoint_on_branch = self
            .app
            .world()
            .get_resource::<SnapshotPolicy>()
            .map(|policy| policy.checkpoint_on_branch)
            .unwrap_or(true);
        let snapshot = if self.has_snapshot_support() && checkpoint_on_branch {
            let tick = self.current_tick();
            let result = self.create_owned_checkpoint(
                Some(format!("branch-{tick}")),
                SnapshotRole::BranchFork,
            )?;
            Some(result)
        } else {
            None
        };

        let branch_id = {
            let mut timeline = self
                .app
                .world_mut()
                .get_resource_mut::<Timeline>()
                .ok_or_else(|| anyhow!("AgentReplayPlugin is not installed"))?;
            // Reparent to the interval owner before forking:
            // `create_branch` parents off `current_branch`.
            timeline
                .select_branch(fork_parent)
                .map_err(anyhow::Error::msg)?;
            timeline
                .create_branch(
                    from_tick,
                    snapshot.as_ref().map(|result| result.snapshot_id),
                    label,
                )
                .map_err(anyhow::Error::msg)?
        };

        // Tag the fork checkpoint on the child branch (same-tick isolated from
        // the parent's own checkpoints) so child restores resolve locally.
        if let Some(result) = snapshot {
            let (tick, id) = (result.tick, result.snapshot_id);
            let checksum = self
                .app
                .world()
                .get_resource::<SnapshotStore>()
                .and_then(|store| store.get(id))
                .and_then(|stored| queue_excluded_checksum(stored).ok())
                .unwrap_or_else(|| result.checksum.clone());
            self.index_checkpoint(branch_id, tick, id, checksum)?;
            self.prune_with_live_refs();
        }

        let world = self.app.world_mut();
        world.resource_scope(|world, mut recorder: Mut<ReplayRecorder>| {
            recorder
                .sync_topology(world.resource::<Timeline>(), from_tick)
                .map_err(anyhow::Error::msg)
        })?;
        let timeline_id = self.app.world().resource::<Timeline>().timeline_id();
        {
            let mut control = self.app.world_mut().resource_mut::<AgentControlState>();
            control.timeline_id = timeline_id;
            control.branch_id = branch_id;
            control.mode = ControlMode::Agent;
        }
        if let Some(response) = self
            .app
            .world_mut()
            .resource_mut::<LastStepResponse>()
            .0
            .as_mut()
        {
            response.info.timeline_id = timeline_id;
            response.info.branch_id = branch_id;
        }
        Ok(branch_id)
    }
}

struct ReplayInterval {
    actions: BTreeMap<u64, Vec<(ActionSource, AgentAction)>>,
    checksums: BTreeMap<u64, SnapshotChecksum>,
}

#[cfg(test)]
mod benchmarks {
    use super::*;
    use bevy_agent_replay::ActionRecord;
    use std::hint::black_box;
    use std::time::Instant;

    /// Reproducible comparison with the former clone-and-scan preflight.
    /// Run with `cargo test -p bevy_agent_runner navigation_preflight_benchmark -- --ignored --nocapture`.
    #[test]
    #[ignore = "manual long-history performance measurement"]
    fn navigation_preflight_benchmark() {
        for size in [1_000u64, 10_000, 100_000] {
            let mut env = AgentApp::new(crate::tests::full_history_app).unwrap();
            env.reset(ResetOptions::default()).unwrap();
            let branch = env.world().resource::<Timeline>().current_branch();
            let mut log = env.replay_log().unwrap().clone();
            log.records = (1..=size)
                .map(|tick| ActionRecord {
                    branch_id: branch,
                    tick,
                    source: ActionSource::Agent,
                    action: AgentAction::Noop,
                })
                .collect();
            log.completed_ticks.insert(branch, (1..=size).collect());
            log.cursor_tick = size;
            log.end_tick = size;
            env.world_mut()
                .resource_mut::<ReplayRecorder>()
                .replace_log(log)
                .unwrap();
            let target = 10;
            let repeats = 50;
            let old_start = Instant::now();
            for _ in 0..repeats {
                let old_log = black_box(env.replay_log().unwrap().clone());
                let old_timeline = black_box(env.world().resource::<Timeline>().clone());
                validate_history_target(&old_log, &old_timeline, branch, target).unwrap();
                let (checkpoint_tick, id) = old_log
                    .nearest_checkpoint_for_branch(&old_timeline, branch, target)
                    .unwrap();
                let snapshot = env.world().resource::<SnapshotStore>().get(id).unwrap();
                validate_snapshot_full(env.world(), snapshot).unwrap();
                if let Some(expected) = visible_checksums(
                    &old_log,
                    &old_timeline,
                    branch,
                    checkpoint_tick,
                    checkpoint_tick,
                )
                .get(&checkpoint_tick)
                {
                    assert_eq!(&queue_excluded_checksum(snapshot).unwrap(), expected);
                }
                black_box((checkpoint_tick, id));
            }
            let old_elapsed = old_start.elapsed();
            let new_start = Instant::now();
            for _ in 0..repeats {
                black_box(
                    env.history_navigation(target, MAX_RECONSTRUCTION_TICKS)
                        .unwrap(),
                );
            }
            let new_elapsed = new_start.elapsed();
            let plan = env
                .history_navigation(target, MAX_RECONSTRUCTION_TICKS)
                .unwrap();
            let interval = env.replay_interval(plan, target);
            assert_eq!(interval.actions.len(), target as usize);
            let old_selection = env
                .replay_log()
                .unwrap()
                .nearest_checkpoint_for_branch(env.world().resource::<Timeline>(), branch, target)
                .unwrap();
            assert_eq!((plan.checkpoint_tick, plan.snapshot_id), old_selection);
            eprintln!(
                "history_records={size} repeats={repeats} clone_scan_us={} borrowed_indexed_us={} interval_frames={}",
                old_elapsed.as_micros(),
                new_elapsed.as_micros(),
                interval.actions.len()
            );
        }
    }
}
