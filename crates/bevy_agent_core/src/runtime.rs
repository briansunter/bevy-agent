//! Schedule installation and simulation lifecycle systems.

use crate::integration::{validate_resources, validate_runtime_config};
use crate::*;

/// The failure from scheduled observation collection. It is transient and is
/// cleared by reset or a subsequent successful collection.
#[derive(Resource, Clone, Debug, Default)]
pub struct AgentTickFailure(Option<AgentControlError>);

impl AgentTickFailure {
    pub fn set(&mut self, error: AgentControlError) {
        self.0 = Some(error);
    }
    #[must_use]
    pub fn error(&self) -> Option<&AgentControlError> {
        self.0.as_ref()
    }
}

fn validate_tick_input(world: &World) -> ControlResult<()> {
    let clock = world.resource::<SimClock>();
    clock.validate()?;
    let next_tick = clock
        .tick
        .checked_add(1)
        .ok_or_else(|| AgentControlError::InvalidIntegration("tick overflow".into()))?;
    validate_clock_tick(next_tick)?;
    let control = world.resource::<AgentControlState>();
    if !control.mode.allows_stepping() {
        return Err(AgentControlError::InvalidAction(format!(
            "stepping is disabled in mode {:?}",
            control.mode
        )));
    }
    if control.frame == u64::MAX
        || !(clock.elapsed_seconds + f64::from(clock.dt_seconds)).is_finite()
    {
        return Err(AgentControlError::InvalidIntegration(
            "frame or elapsed time overflow".into(),
        ));
    }
    world.resource::<EpisodeState>().ensure_not_terminal()?;
    let catalog = world.resource::<AgentActionCatalog>();
    let queue = world.resource::<AgentActionQueue>();
    if queue
        .iter()
        .next()
        .is_some_and(|action| action.tick <= clock.tick)
    {
        return Err(AgentControlError::InvalidAction(
            "pending input contains an expired tick".into(),
        ));
    }
    for scheduled in queue.at_tick(next_tick) {
        validate_future_action(catalog, clock.tick, scheduled)?;
    }
    Ok(())
}

pub fn validate_next_tick(world: &World) -> ControlResult<()> {
    validate_runtime_config(world)?;
    validate_tick_input(world)
}

/// Runs exactly one agent-controlled simulation tick. Invalid integration or
/// pending actions are rejected before gameplay advances; observation failure
/// is returned and cannot reuse the previous tick's response.
pub fn run_agent_tick(world: &mut World) -> ControlResult<()> {
    validate_runtime_config(world)?;
    validate_tick_input(world)?;
    world.resource_mut::<AgentTickFailure>().0 = None;
    world.resource_mut::<LastStepResponse>().0 = None;
    world.run_schedule(AgentDecision);
    world.run_schedule(AgentPreTick);
    validate_runtime_config(world)?;
    validate_tick_input(world)?;
    world.run_schedule(AgentTick);
    world.run_schedule(AgentPostTick);
    world.run_schedule(AgentFinalize);
    if let Some(error) = world.resource::<AgentTickFailure>().error() {
        return Err(error.clone());
    }
    let response = world
        .resource::<LastStepResponse>()
        .0
        .as_ref()
        .ok_or(AgentControlError::MissingStepResponse)?;
    if response.tick != world.resource::<SimClock>().tick {
        return Err(AgentControlError::MissingStepResponse);
    }
    Ok(())
}

impl Plugin for AgentControlPlugin {
    fn build(&self, app: &mut App) {
        app.init_schedule(AgentReset)
            .init_schedule(AgentDecision)
            .init_schedule(AgentPreTick)
            .init_schedule(AgentTick)
            .init_schedule(AgentPostTick)
            .init_schedule(AgentFinalize)
            .insert_resource(SimClock::new(self.tick_hz))
            .init_resource::<StableIdAllocator>()
            .insert_resource(DeterministicRng::seeded(0))
            .init_resource::<AgentControlState>()
            .init_resource::<AgentActionQueue>()
            .init_resource::<CurrentInputFrame>()
            .init_resource::<AgentActionCatalog>()
            .init_resource::<AgentObservationCatalog>()
            .init_resource::<EnvironmentMetadata>()
            .init_resource::<ObservationConfig>()
            .init_resource::<RewardState>()
            .init_resource::<EpisodeState>()
            .init_resource::<LastStepResponse>()
            .init_resource::<AgentTickFailure>()
            .init_resource::<ExecutionContext>()
            .configure_sets(
                AgentReset,
                (
                    AgentResetSet::Core,
                    AgentResetSet::Game,
                    AgentResetSet::Observation,
                )
                    .chain(),
            )
            .configure_sets(
                AgentTick,
                (
                    AgentSet::BeginTick,
                    AgentSet::DrainActions,
                    AgentSet::ApplyInput,
                    AgentSet::Simulation,
                    AgentSet::TerminalCheck,
                )
                    .chain(),
            )
            .configure_sets(
                AgentFinalize,
                (
                    AgentSet::Observation,
                    AgentSet::ReplayRecord,
                    AgentSet::Snapshot,
                    AgentSet::EndTick,
                )
                    .chain(),
            )
            .add_systems(AgentReset, reset_agent_core.in_set(AgentResetSet::Core))
            .add_systems(
                AgentReset,
                collect_observation_system.in_set(AgentResetSet::Observation),
            )
            .add_systems(
                AgentTick,
                (
                    begin_tick.in_set(AgentSet::BeginTick),
                    drain_agent_actions.in_set(AgentSet::DrainActions),
                ),
            )
            .add_systems(
                AgentFinalize,
                collect_observation_system.in_set(AgentSet::Observation),
            );
    }
}

#[allow(clippy::too_many_arguments)]
pub fn reset_agent_core(
    mut clock: ResMut<SimClock>,
    mut ids: ResMut<StableIdAllocator>,
    mut queue: ResMut<AgentActionQueue>,
    mut input: ResMut<CurrentInputFrame>,
    mut reward: ResMut<RewardState>,
    mut episode: ResMut<EpisodeState>,
    mut control: ResMut<AgentControlState>,
    mut last: ResMut<LastStepResponse>,
    mut rng: ResMut<DeterministicRng>,
    mut failure: ResMut<AgentTickFailure>,
) {
    let dt_seconds = clock.dt_seconds;
    *clock = SimClock {
        tick: 0,
        dt_seconds,
        elapsed_seconds: 0.0,
    };
    *ids = StableIdAllocator::default();
    queue.clear();
    input.tick = 0;
    input.actions.clear();
    input.sources.clear();
    *reward = RewardState::default();
    *episode = EpisodeState::default();
    control.frame = 0;
    control.last_action_count = 0;
    control.last_snapshot_created = None;
    *last = LastStepResponse(None);
    failure.0 = None;
    let seed = rng.seed;
    *rng = DeterministicRng::seeded(seed);
}

pub fn begin_tick(mut clock: ResMut<SimClock>, mut control: ResMut<AgentControlState>) {
    clock.advance_one_tick();
    control.frame += 1;
    control.last_snapshot_created = None;
}

pub fn drain_agent_actions(
    clock: Res<SimClock>,
    mut queue: ResMut<AgentActionQueue>,
    mut input: ResMut<CurrentInputFrame>,
    mut control: ResMut<AgentControlState>,
    context: Res<ExecutionContext>,
) {
    input.tick = clock.tick;
    input.actions.clear();
    input.sources.clear();
    let reconstructing = *context == ExecutionContext::Reconstructing;
    for next in queue.drain_before_or_at(clock.tick) {
        if reconstructing || control.mode.accepts_source(&next.source) {
            input.sources.push(next.source);
            input.actions.push(next.action);
        }
    }
    control.last_action_count = input.actions.len();
}

fn collect_observation_system(world: &mut World) {
    // Bevy schedules cannot return a result to their caller. The collector
    // records any error in AgentTickFailure and clears the cached response.
    let _ = collect_observation(world);
}

pub fn collect_observation(world: &mut World) -> ControlResult<()> {
    let Some(config) = world.get_resource::<ObservationConfig>() else {
        let error = AgentControlError::MissingResource("ObservationConfig");
        record_collection_failure(world, &error);
        return Err(error);
    };
    let mode = config.mode.clone();
    collect_observation_with_mode(world, mode)
}

/// Collects an observation in a request-local mode without changing the
/// default mode. Games supply both extractors; there is no fallback state.
pub fn collect_observation_with_mode(
    world: &mut World,
    mode: ObservationMode,
) -> ControlResult<()> {
    if let Some(mut last) = world.get_resource_mut::<LastStepResponse>() {
        last.0 = None;
    }
    let result = collect_response(world, mode);
    match result {
        Ok(response) => {
            world.resource_mut::<LastStepResponse>().0 = Some(response);
            world.resource_mut::<AgentTickFailure>().0 = None;
            Ok(())
        }
        Err(error) => {
            record_collection_failure(world, &error);
            Err(error)
        }
    }
}

fn record_collection_failure(world: &mut World, error: &AgentControlError) {
    if let Some(mut last) = world.get_resource_mut::<LastStepResponse>() {
        last.0 = None;
    }
    if let Some(mut failure) = world.get_resource_mut::<AgentTickFailure>() {
        failure.0 = Some(error.clone());
    }
}

fn collect_response(world: &mut World, mode: ObservationMode) -> ControlResult<StepResponse> {
    validate_resources(world)?;
    world
        .resource::<AgentObservationCatalog>()
        .validate_mode(&mode)?;
    let clock = world.resource::<SimClock>().clone();
    let observation = world.resource_scope(|world, extractor: Mut<AgentObservationExtractor>| {
        extractor.extract(world, mode.clone())
    });
    world
        .resource::<AgentObservationCatalog>()
        .validate_observation(&mode, &observation)?;
    let checksum = world
        .resource_scope(|world, extractor: Mut<AgentChecksumExtractor>| extractor.extract(world));
    if checksum.tick != clock.tick {
        return Err(AgentControlError::InvalidIntegration(
            "checksum tick differs from the collected tick".into(),
        ));
    }
    let reward = world.resource::<RewardState>().clone();
    if !reward.current_reward.is_finite() || !reward.cumulative_reward.is_finite() {
        return Err(AgentControlError::InvalidIntegration(
            "reward must be finite".into(),
        ));
    }
    let episode = world.resource::<EpisodeState>().clone();
    let control = world.resource::<AgentControlState>().clone();
    Ok(StepResponse {
        tick: clock.tick,
        observation,
        reward: reward.current_reward,
        done: episode.done,
        truncated: episode.truncated,
        info: StepInfo {
            frame: control.frame,
            timeline_id: control.timeline_id,
            branch_id: control.branch_id,
            actions_applied: control.last_action_count,
            snapshot_created: control.last_snapshot_created,
            episode_reason: episode.reason,
        },
        checksum: Some(checksum),
    })
}

/// Installs the deterministic simulation schedules at the configured tick rate.
#[derive(Clone, Debug)]
pub struct AgentControlPlugin {
    pub tick_hz: u32,
}

impl Default for AgentControlPlugin {
    fn default() -> Self {
        Self { tick_hz: 60 }
    }
}
