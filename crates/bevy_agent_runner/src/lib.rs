//! Runner API for manually stepping Bevy apps through agent simulation ticks.

use anyhow::{Result, anyhow};
use bevy::prelude::*;
use bevy_agent_core::{
    ActionSource, AgentAction, AgentActionQueue, AgentControlState, AgentPostTick, AgentPreTick,
    AgentReset, AgentTick, ControlMode, CurrentInputFrame, DeterministicRng, EpisodeState,
    LastStepResponse, Observation, ObservationConfig, ObservationMode, RewardState, SimClock,
    SnapshotId, StepResponse, collect_observation,
};
use bevy_agent_replay::{ReplayLog, ReplayRecorder, Timeline};
use bevy_agent_snapshot::{SnapshotCreateResult, SnapshotStore, create_snapshot, restore_snapshot};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResetOptions {
    pub seed: Option<u64>,
    pub observation_mode: ObservationMode,
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

pub trait AgentEnvironment {
    type Action;
    type Observation;

    fn reset(&mut self, options: ResetOptions) -> Result<Self::Observation>;

    fn step(&mut self, action: Self::Action) -> Result<StepResponse<Self::Observation>>;

    fn step_many(
        &mut self,
        actions: Vec<Self::Action>,
    ) -> Result<Vec<StepResponse<Self::Observation>>>;

    fn observe(&mut self, mode: ObservationMode) -> Result<Self::Observation>;

    fn snapshot(&mut self) -> Result<SnapshotCreateResult>;

    fn restore(&mut self, snapshot: SnapshotId) -> Result<()>;

    fn restore_tick(&mut self, tick: u64) -> Result<()>;

    fn branch(
        &mut self,
        from_tick: u64,
        label: Option<String>,
    ) -> Result<bevy_agent_core::BranchId>;
}

pub struct AgentApp {
    app: App,
    started: bool,
    reset_once: bool,
}

impl AgentApp {
    pub fn new(build_app: impl FnOnce() -> App) -> Self {
        Self::from_app(build_app())
    }

    pub fn from_app(mut app: App) -> Self {
        app.finish();
        app.cleanup();
        Self {
            app,
            started: false,
            reset_once: false,
        }
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

    pub fn enqueue_action_at(&mut self, tick: u64, source: ActionSource, action: AgentAction) {
        self.app
            .world_mut()
            .resource_mut::<AgentActionQueue>()
            .schedule(tick, source, action);
    }

    pub fn fast_forward(&mut self, ticks: u64) -> Result<StepResponse<Observation>> {
        let mut last = None;
        for _ in 0..ticks {
            let response = self.step(AgentAction::Noop)?;
            let done = response.done || response.truncated;
            last = Some(response);
            if done {
                break;
            }
        }
        last.ok_or_else(|| anyhow!("fast_forward called with zero ticks"))
    }

    pub fn replay_log(&self) -> Option<&ReplayLog> {
        self.app
            .world()
            .get_resource::<ReplayRecorder>()
            .map(|recorder| &recorder.log)
    }

    fn ensure_started(&mut self) {
        if self.started {
            return;
        }
        self.app.update();
        self.started = true;
    }

    fn ensure_reset(&mut self) -> Result<()> {
        if !self.reset_once {
            let _ = self.reset(ResetOptions::default())?;
        }
        Ok(())
    }

    fn run_one_agent_tick(&mut self) {
        let world = self.app.world_mut();
        world.run_schedule(AgentPreTick);
        world.run_schedule(AgentTick);
        world.run_schedule(AgentPostTick);
    }

    fn last_response(&self) -> Result<StepResponse<Observation>> {
        self.app
            .world()
            .resource::<LastStepResponse>()
            .0
            .clone()
            .ok_or_else(|| anyhow!("agent tick produced no StepResponse"))
    }

    fn step_with_source(
        &mut self,
        action: AgentAction,
        source: ActionSource,
    ) -> Result<StepResponse<Observation>> {
        self.ensure_started();
        self.ensure_reset()?;
        let next_tick = self.current_tick() + 1;
        self.enqueue_action_at(next_tick, source, action);
        self.run_one_agent_tick();
        self.last_response()
    }

    fn has_snapshot_support(&self) -> bool {
        self.app.world().contains_resource::<SnapshotStore>()
    }
}

impl AgentEnvironment for AgentApp {
    type Action = AgentAction;
    type Observation = Observation;

    fn reset(&mut self, options: ResetOptions) -> Result<Self::Observation> {
        self.ensure_started();
        {
            let world = self.app.world_mut();
            world.resource_mut::<ObservationConfig>().mode = options.observation_mode;
            if let Some(seed) = options.seed {
                world.insert_resource(DeterministicRng::seeded(seed));
            }
        }

        self.app.world_mut().run_schedule(AgentReset);
        self.reset_once = true;

        let mut response = self.last_response()?;
        if options.create_initial_snapshot && self.has_snapshot_support() {
            let snapshot = create_snapshot(
                self.app.world_mut(),
                Some(format!("reset-{}", response.tick)),
            )?;
            if let Some(mut recorder) = self.app.world_mut().get_resource_mut::<ReplayRecorder>() {
                recorder.log.initial_snapshot = Some(snapshot.snapshot_id);
                recorder
                    .log
                    .checkpoints
                    .insert(snapshot.tick, snapshot.snapshot_id);
                recorder
                    .log
                    .checksums
                    .insert(snapshot.tick, snapshot.checksum);
            }
            response.info.snapshot_created = Some(snapshot.snapshot_id);
            self.app.world_mut().resource_mut::<LastStepResponse>().0 = Some(response.clone());
        }

        Ok(response.observation)
    }

    fn step(&mut self, action: Self::Action) -> Result<StepResponse<Self::Observation>> {
        self.step_with_source(action, ActionSource::Agent)
    }

    fn step_many(
        &mut self,
        actions: Vec<Self::Action>,
    ) -> Result<Vec<StepResponse<Self::Observation>>> {
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

    fn observe(&mut self, mode: ObservationMode) -> Result<Self::Observation> {
        self.ensure_started();
        self.ensure_reset()?;
        self.app
            .world_mut()
            .resource_mut::<ObservationConfig>()
            .mode = mode;
        collect_observation(self.app.world_mut());
        Ok(self.last_response()?.observation)
    }

    fn snapshot(&mut self) -> Result<SnapshotCreateResult> {
        self.ensure_started();
        self.ensure_reset()?;
        if !self.has_snapshot_support() {
            return Err(anyhow!("AgentSnapshotPlugin is not installed"));
        }

        let tick = self.current_tick();
        let result = create_snapshot(self.app.world_mut(), Some(format!("manual-{tick}")))?;
        if let Some(mut recorder) = self.app.world_mut().get_resource_mut::<ReplayRecorder>() {
            recorder
                .log
                .checkpoints
                .insert(result.tick, result.snapshot_id);
            recorder
                .log
                .checksums
                .insert(result.tick, result.checksum.clone());
        }
        Ok(result)
    }

    fn restore(&mut self, snapshot: SnapshotId) -> Result<()> {
        self.ensure_started();
        if !self.has_snapshot_support() {
            return Err(anyhow!("AgentSnapshotPlugin is not installed"));
        }

        restore_snapshot(self.app.world_mut(), snapshot)?;
        self.reset_once = true;
        collect_observation(self.app.world_mut());
        Ok(())
    }

    fn restore_tick(&mut self, tick: u64) -> Result<()> {
        self.ensure_started();
        self.ensure_reset()?;

        let log = self
            .app
            .world()
            .get_resource::<ReplayRecorder>()
            .map(|recorder| recorder.log.clone())
            .ok_or_else(|| anyhow!("AgentReplayPlugin is not installed"))?;

        let (checkpoint_tick, snapshot_id) = log
            .nearest_checkpoint_at_or_before(tick)
            .or_else(|| log.initial_snapshot.map(|snapshot_id| (0, snapshot_id)))
            .ok_or_else(|| anyhow!("no checkpoint exists at or before tick {tick}"))?;

        self.restore(snapshot_id)?;

        let old_recording = self
            .app
            .world()
            .get_resource::<ReplayRecorder>()
            .map(|recorder| recorder.recording)
            .unwrap_or(false);
        if let Some(mut recorder) = self.app.world_mut().get_resource_mut::<ReplayRecorder>() {
            recorder.recording = false;
        }

        for record in log.actions_between(checkpoint_tick, tick) {
            self.step_with_source(record.action, ActionSource::Replay)?;
        }

        if let Some(mut recorder) = self.app.world_mut().get_resource_mut::<ReplayRecorder>() {
            recorder.recording = old_recording;
        }
        Ok(())
    }

    fn branch(
        &mut self,
        from_tick: u64,
        label: Option<String>,
    ) -> Result<bevy_agent_core::BranchId> {
        self.restore_tick(from_tick)?;
        let snapshot = if self.has_snapshot_support() {
            Some(self.snapshot()?.snapshot_id)
        } else {
            None
        };

        let branch_id = {
            let mut timeline = self
                .app
                .world_mut()
                .get_resource_mut::<Timeline>()
                .ok_or_else(|| anyhow!("AgentReplayPlugin is not installed"))?;
            timeline.create_branch(from_tick, snapshot, label)
        };

        let timeline_id = self.app.world().resource::<Timeline>().timeline_id;
        let mut control = self.app.world_mut().resource_mut::<AgentControlState>();
        control.timeline_id = timeline_id;
        control.branch_id = branch_id;
        control.mode = ControlMode::Agent;
        Ok(branch_id)
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
    world.resource_mut::<CurrentInputFrame>().actions.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_agent_core::AgentControlPlugin;

    fn core_only_app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(AgentControlPlugin::deterministic());
        app
    }

    #[test]
    fn reset_without_snapshot_plugin_returns_observation() {
        let mut env = AgentApp::new(core_only_app);

        let observation = env
            .reset(ResetOptions {
                create_initial_snapshot: false,
                ..Default::default()
            })
            .unwrap();

        assert_eq!(env.current_tick(), 0);
        assert!(matches!(observation, Observation::Hybrid { .. }));
    }

    #[test]
    fn step_auto_resets_before_first_tick() {
        let mut env = AgentApp::new(core_only_app);

        let response = env.step(AgentAction::Noop).unwrap();

        assert_eq!(response.tick, 1);
        assert_eq!(response.info.actions_applied, 1);
    }

    #[test]
    fn fast_forward_zero_returns_error() {
        let mut env = AgentApp::new(core_only_app);

        let error = env.fast_forward(0).unwrap_err();

        assert!(error.to_string().contains("zero ticks"));
    }

    #[test]
    fn snapshot_without_snapshot_plugin_returns_error() {
        let mut env = AgentApp::new(core_only_app);

        let error = env.snapshot().unwrap_err();

        assert!(error.to_string().contains("AgentSnapshotPlugin"));
    }

    #[test]
    fn episode_helpers_update_world_resources() {
        let mut env = AgentApp::new(core_only_app);
        env.reset(ResetOptions {
            create_initial_snapshot: false,
            ..Default::default()
        })
        .unwrap();

        set_episode_done(env.world_mut(), "done");
        assert!(env.world().resource::<EpisodeState>().done);
        assert_eq!(
            env.world().resource::<EpisodeState>().reason.as_deref(),
            Some("done")
        );

        env.world_mut()
            .resource_mut::<CurrentInputFrame>()
            .actions
            .push(AgentAction::Jump);
        clear_episode(env.world_mut());
        assert!(!env.world().resource::<EpisodeState>().done);
        assert!(
            env.world()
                .resource::<CurrentInputFrame>()
                .actions
                .is_empty()
        );
    }
}
