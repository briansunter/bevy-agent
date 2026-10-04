//! App ownership, initialization, and live control operations.

use super::*;

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
/// Episode initialization policy. Defaults to seed zero, hybrid observations,
/// and an initial snapshot suitable for replay reconstruction.
pub struct ResetOptions {
    /// Optional replacement for the simulation's deterministic RNG seed.
    pub seed: Option<u64>,
    /// A mode supported by the game's observation catalog.
    pub observation_mode: ObservationMode,
    /// Whether reset retains a snapshot of the initial world.
    pub create_initial_snapshot: bool,
}

impl Default for ResetOptions {
    fn default() -> Self {
        Self {
            seed: Some(0),
            observation_mode: ObservationMode::Hybrid,
            create_initial_snapshot: true,
        }
    }
}

pub fn set_episode_done(world: &mut World, reason: impl Into<String>) {
    let mut episode = world.resource_mut::<EpisodeState>();
    episode.done = true;
    episode.reason = Some(reason.into());
}

pub fn clear_episode(world: &mut World) {
    *world.resource_mut::<EpisodeState>() = EpisodeState::default();
    *world.resource_mut::<RewardState>() = RewardState::default();
    let mut input = world.resource_mut::<CurrentInputFrame>();
    input.actions.clear();
    input.sources.clear();
    world.resource_mut::<AgentControlState>().last_action_count = 0;
}

impl AgentApp {
    /// Checks integration before lending a world to an owning runner.
    /// This performs no startup, reset, extraction, or world mutation.
    pub fn validate_world(world: &World) -> Result<()> {
        validate_runner_integration(world)
    }

    /// Builds an app and validates its integration without starting gameplay.
    pub fn new(build_app: impl FnOnce() -> App) -> Result<Self> {
        Self::from_app(build_app())
    }

    /// Finishes plugin setup and validates an existing, not-yet-running app.
    pub fn from_app(mut app: App) -> Result<Self> {
        app.finish();
        app.cleanup();
        validate_runner_integration(app.world())?;
        Ok(Self {
            app,
            started: false,
            reset_once: false,
        })
    }

    /// Wraps an app whose plugins have already been finished by an external
    /// runner. This is primarily used by main-thread integrations that lend
    /// their world to the synchronous control API for one request.
    pub fn from_running_app(app: App, reset_once: bool) -> Result<Self> {
        validate_runner_integration(app.world())?;
        Ok(Self {
            app,
            started: true,
            reset_once,
        })
    }

    pub fn into_app(self) -> App {
        self.app
    }

    #[must_use]
    pub const fn has_reset(&self) -> bool {
        self.reset_once
    }

    pub fn app(&self) -> &App {
        &self.app
    }

    pub fn app_mut(&mut self) -> &mut App {
        &mut self.app
    }

    pub fn world(&self) -> &World {
        self.app.world()
    }

    pub fn world_mut(&mut self) -> &mut World {
        self.app.world_mut()
    }

    pub fn step_frame(&mut self) {
        self.ensure_started();
        self.app.update();
    }

    pub fn current_tick(&self) -> u64 {
        self.app.world().resource::<SimClock>().tick
    }

    /// Schedules validated future input without starting or resetting the app.
    /// Rejected actions, sources, and ticks leave the queue unchanged.
    pub fn enqueue_action_at(
        &mut self,
        tick: u64,
        source: ActionSource,
        action: AgentAction,
    ) -> Result<()> {
        self.ensure_no_fault()?;
        schedule_action(self.app.world_mut(), tick, source, action)?;
        Ok(())
    }

    /// Advances with `Noop` actions, stopping at terminal state and returning
    /// the last response. Zero ticks are rejected.
    pub fn fast_forward(&mut self, ticks: u64) -> Result<StepResponse<Observation>> {
        if ticks == 0 {
            return Err(anyhow!("fast_forward called with zero ticks"));
        }
        self.ensure_no_fault()?;
        self.ensure_started();
        self.ensure_reset()?;
        let start_tick = self.current_tick();
        let mut last = None;
        for completed in 0..ticks {
            let response = self.step(AgentAction::Noop).map_err(|error| {
                self.batch_failure("fast_forward", start_tick, completed as usize, error)
            })?;
            let done = response.done || response.truncated;
            last = Some(response);
            if done {
                break;
            }
        }
        last.ok_or_else(|| anyhow!("fast_forward called with zero ticks"))
    }

    /// Steps with a request-specific observation mode, collecting the tick response once.
    /// The persistent mode is restored even when stepping returns an error.
    pub fn step_with_observation_mode(
        &mut self,
        action: AgentAction,
        mode: ObservationMode,
    ) -> Result<StepResponse<Observation>> {
        self.validate_actions(std::slice::from_ref(&action))?;
        self.validate_external_source(&ActionSource::Agent)?;
        self.validate_observation_mode(&mode)?;
        self.ensure_no_fault()?;
        self.ensure_started();
        self.ensure_reset()?;
        let previous_mode = std::mem::replace(
            &mut self
                .app
                .world_mut()
                .resource_mut::<ObservationConfig>()
                .mode,
            mode,
        );
        let response = self.step(action);
        self.app
            .world_mut()
            .resource_mut::<ObservationConfig>()
            .mode = previous_mode;
        response
    }

    /// Fault gate: history corruption that failed rollback leaves a
    /// [`FaultState`] resource; all navigation/stepping is rejected until an
    /// explicit `reset` clears it (reset removes the resource).
    pub(super) fn ensure_no_fault(&self) -> Result<()> {
        if let Some(fault) = self.app.world().get_resource::<FaultState>() {
            return Err(anyhow!(
                "world is faulted ({}); reset before stepping",
                fault.message
            ));
        }
        Ok(())
    }

    pub fn reset_with_response(
        &mut self,
        options: ResetOptions,
    ) -> Result<ResetResponse<Observation>> {
        let observation = <Self as AgentEnvironment>::reset(self, options)?;
        let response = self.last_response()?;
        Ok(ResetResponse {
            tick: response.tick,
            observation,
            checksum: response.checksum,
            snapshot_id: response.info.snapshot_created,
            timeline_id: response.info.timeline_id,
            branch_id: response.info.branch_id,
        })
    }

    pub fn step_many_with_response(
        &mut self,
        actions: Vec<AgentAction>,
        include_responses: bool,
    ) -> Result<StepManyResponse<Observation>> {
        // Controller boundary: validate the whole batch BEFORE any
        // truncate/enqueue/advance so an invalid action rejects the batch
        // without partial stepping.
        self.validate_actions(&actions)?;
        if !actions.is_empty() {
            self.validate_external_source(&ActionSource::Agent)?;
            self.ensure_no_fault()?;
            self.ensure_started();
            self.ensure_reset()?;
        }
        let start_tick = self.current_tick();
        // Incremental aggregation: retain only requested observations
        // (last always, all only when `include_responses`), summing rewards
        // as we go instead of retaining then summing.
        let mut retained: Vec<StepResponse<Observation>> = Vec::new();
        if include_responses {
            retained.reserve(actions.len());
        }
        let mut steps = 0usize;
        let mut reward = 0.0f32;
        let mut observation: Option<Observation> = None;
        let mut done = false;
        let mut truncated = false;
        let mut info: Option<bevy_agent_core::StepInfo> = None;
        let mut checksum: Option<bevy_agent_core::EnvironmentChecksum> = None;
        let mut end_tick = start_tick;
        for action in actions {
            let response = self
                .step(action)
                .map_err(|error| self.batch_failure("step_many", start_tick, steps, error))?;
            let terminal = response.done || response.truncated;
            end_tick = response.tick;
            steps += 1;
            reward += response.reward;
            if !reward.is_finite() {
                return Err(self.batch_failure(
                    "step_many",
                    start_tick,
                    steps,
                    anyhow!("batch reward sum exceeds the finite float range"),
                ));
            }
            done = response.done;
            truncated = response.truncated;
            observation = Some(response.observation.clone());
            info = Some(response.info.clone());
            checksum = response.checksum.clone();
            if include_responses {
                retained.push(response);
            }
            if terminal {
                break;
            }
        }
        Ok(StepManyResponse {
            start_tick,
            end_tick,
            steps,
            observation,
            reward,
            done,
            truncated,
            info,
            checksum,
            responses: retained,
        })
    }

    /// Validates a whole batch against the action catalog, rejecting the batch
    /// on the first invalid action.
    pub fn validate_actions(&self, actions: &[AgentAction]) -> Result<()> {
        let catalog = self
            .app
            .world()
            .get_resource::<AgentActionCatalog>()
            .ok_or_else(|| anyhow!("missing AgentActionCatalog"))?;
        for action in actions {
            validate_action_against_catalog(catalog, action)
                .map_err(|error| anyhow!("InvalidAction: {error}"))?;
        }
        Ok(())
    }

    pub(super) fn ensure_started(&mut self) {
        if self.started {
            return;
        }
        self.app.update();
        self.started = true;
    }

    pub(super) fn ensure_reset(&mut self) -> Result<()> {
        if !self.reset_once {
            let mode = self
                .app
                .world()
                .resource::<ObservationConfig>()
                .mode
                .clone();
            let _ = self.reset(ResetOptions {
                observation_mode: mode,
                ..ResetOptions::default()
            })?;
        }
        Ok(())
    }

    pub(super) fn run_one_agent_tick(&mut self) -> Result<()> {
        run_agent_tick(self.app.world_mut())?;
        Ok(())
    }

    pub(super) fn last_response(&self) -> Result<StepResponse<Observation>> {
        self.app
            .world()
            .resource::<LastStepResponse>()
            .0
            .clone()
            .ok_or_else(|| anyhow!("agent tick produced no StepResponse"))
    }

    pub(super) fn step_with_source(
        &mut self,
        action: AgentAction,
        source: ActionSource,
    ) -> Result<StepResponse<Observation>> {
        self.validate_actions(std::slice::from_ref(&action))?;
        self.validate_external_source(&source)?;
        self.ensure_no_fault()?;
        self.ensure_started();
        self.ensure_reset()?;
        self.app
            .world()
            .resource::<EpisodeState>()
            .ensure_not_terminal()?;
        self.reject_paused_or_inspect_only()?;
        // Post-restore stepping policy: stepping after a `restore_tick` into
        // recorded future ticks on the same branch explicitly diverges, so the
        // stale future beyond the current tick is truncated first. Forking via
        // `branch` is the non-destructive alternative; truncation here is the
        // documented explicit-diverge behavior.
        self.app.world().resource::<SimClock>().validate()?;
        let next_tick = self.current_tick() + 1;
        validate_clock_tick(next_tick)?;
        bevy_agent_core::validate_next_tick(self.app.world())?;
        if let Some(recorder) = self.app.world().get_resource::<ReplayRecorder>() {
            recorder
                .validate_truncation(
                    self.app.world().resource::<Timeline>(),
                    self.app.world().resource::<AgentControlState>().branch_id,
                    self.current_tick(),
                )
                .map_err(anyhow::Error::msg)?;
        }
        let tick_before = self.current_tick();
        let result = (|| {
            self.enforce_diverge_truncation()?;
            self.enqueue_action_at(next_tick, source, action)?;
            self.run_one_agent_tick()?;
            // Caller-side checkpoint bookkeeping (the snapshot crate owns
            // creation/pruning): mirror any new automatic checkpoints into the
            // replay log, honor `checkpoint_on_terminal`, and make sure the
            // response reports a snapshot created on this same tick.
            self.sync_auto_checkpoints()?;
            self.maybe_terminal_checkpoint()?;
            self.patch_snapshot_created();
            self.last_response()
        })();
        result.map_err(|error| self.mutation_failed("step", tick_before, error))
    }

    fn validate_external_source(&self, source: &ActionSource) -> Result<()> {
        self.reject_paused_or_inspect_only()?;
        if !self
            .app
            .world()
            .resource::<AgentControlState>()
            .mode
            .accepts_source(source)
        {
            return Err(anyhow!(
                "action source {source:?} is rejected by the current control mode"
            ));
        }
        Ok(())
    }

    /// External stepping (`step`/`step_many`/`fast_forward`) is rejected while
    /// the control mode is behavioral-only. Internal reconstruction
    /// (`restore`/`restore_tick`/`branch`) bypasses `step_with_source` and may
    /// still rebuild state. Returns before any action is enqueued so the tick is
    /// left unchanged.
    pub(super) fn reject_paused_or_inspect_only(&self) -> Result<()> {
        let mode = self
            .app
            .world()
            .resource::<AgentControlState>()
            .mode
            .clone();
        match mode {
            ControlMode::Paused => Err(anyhow!(
                "cannot step while control mode is Paused; resume before stepping"
            )),
            ControlMode::InspectOnly => Err(anyhow!(
                "cannot step while control mode is InspectOnly; switch modes before stepping"
            )),
            _ => Ok(()),
        }
    }

    pub(super) fn step_many_impl(
        &mut self,
        actions: Vec<AgentAction>,
    ) -> Result<Vec<StepResponse<Observation>>> {
        self.validate_actions(&actions)?;
        if !actions.is_empty() {
            self.validate_external_source(&ActionSource::Agent)?;
        }
        let mut responses = Vec::new();
        for action in actions {
            let response = self.step(action)?;
            let done = response.done || response.truncated;
            responses.push(response);
            if done {
                break;
            }
        }
        Ok(responses)
    }

    pub(super) fn validate_observation_mode(&self, mode: &ObservationMode) -> Result<()> {
        self.app
            .world()
            .resource::<bevy_agent_core::AgentObservationCatalog>()
            .validate_mode(mode)?;
        Ok(())
    }

    pub(super) fn observe_impl(&mut self, mode: ObservationMode) -> Result<Observation> {
        self.ensure_no_fault()?;
        self.validate_observation_mode(&mode)?;
        self.ensure_started();
        self.ensure_reset()?;
        collect_observation_with_mode(self.app.world_mut(), mode)?;
        Ok(self.last_response()?.observation)
    }
}

/// Validate optional history plugins together with the shared core contract.
fn validate_runner_integration(world: &World) -> Result<()> {
    validate_integration(world)?;
    if world.contains_resource::<ReplayRecorder>() != world.contains_resource::<Timeline>() {
        return Err(anyhow!(
            "ReplayRecorder and Timeline must be installed together"
        ));
    }
    if let Some(timeline) = world.get_resource::<Timeline>() {
        timeline
            .ancestors_bounded(timeline.current_branch())
            .map_err(anyhow::Error::msg)?;
        let control = world.resource::<AgentControlState>();
        if control.timeline_id != timeline.timeline_id()
            || control.branch_id != timeline.current_branch()
        {
            return Err(anyhow!("control identity disagrees with active timeline"));
        }
        let log = world.resource::<ReplayRecorder>().log();
        // Live owners validate complete history on admission. Construction
        // checks readiness without rescanning accumulated records; lending a
        // running world must remain independent of its recording length.
        if log.timeline_topology.is_empty()
            || log.active_branch.is_none()
            || log.manifest.schema_version != bevy_agent_replay::REPLAY_SCHEMA_VERSION
        {
            return Err(anyhow!("replay owner is not initialized"));
        }
    }
    if world.contains_resource::<SnapshotStore>()
        && (!world.contains_resource::<bevy_agent_snapshot::SnapshotRegistry>()
            || !world.contains_resource::<SnapshotPolicy>())
    {
        return Err(anyhow!(
            "snapshot support requires SnapshotRegistry and SnapshotPolicy"
        ));
    }
    if world
        .get_resource::<SnapshotPolicy>()
        .is_some_and(|policy| policy.max_snapshot_bytes == 0)
    {
        return Err(anyhow!("snapshot byte budget must be positive"));
    }
    Ok(())
}
