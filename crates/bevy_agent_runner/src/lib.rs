//! Runner API for manually stepping Bevy apps through agent simulation ticks.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use bevy::app::{PluginGroup, PluginGroupBuilder};
use bevy::prelude::*;
use bevy_agent_core::{
    ActionSource, AgentAction, AgentActionQueue, AgentControlPlugin, AgentControlState,
    AgentPostTick, AgentPreTick, AgentReset, AgentTick, ControlMode, CurrentInputFrame,
    DeterministicRng, EpisodeState, LastStepResponse, Observation, ObservationConfig,
    ObservationMode, RewardState, SimClock, SnapshotId, StepResponse, collect_observation,
};
use bevy_agent_replay::{AgentReplayPlugin, ReplayLog, ReplayRecorder, Timeline};
use bevy_agent_snapshot::{
    AgentSnapshotPlugin, SnapshotCreateResult, SnapshotPolicy, SnapshotStore, create_snapshot,
    restore_snapshot,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug)]
pub struct AgentControlPlugins {
    control: AgentControlPlugin,
    snapshots: bool,
    replay: bool,
    snapshot_policy: Option<SnapshotPolicy>,
}

impl AgentControlPlugins {
    #[must_use]
    pub fn deterministic() -> Self {
        Self {
            control: AgentControlPlugin::deterministic(),
            snapshots: true,
            replay: true,
            snapshot_policy: None,
        }
    }

    #[must_use]
    pub fn visual_debug() -> Self {
        Self {
            control: AgentControlPlugin::visual_debug(),
            snapshots: true,
            replay: true,
            snapshot_policy: None,
        }
    }

    #[must_use]
    pub fn remote() -> Self {
        Self {
            control: AgentControlPlugin::remote(),
            snapshots: true,
            replay: true,
            snapshot_policy: None,
        }
    }

    #[must_use]
    pub fn without_snapshots(mut self) -> Self {
        self.snapshots = false;
        self
    }

    #[must_use]
    pub fn without_replay(mut self) -> Self {
        self.replay = false;
        self
    }

    #[must_use]
    pub fn with_snapshot_policy(mut self, policy: SnapshotPolicy) -> Self {
        self.snapshot_policy = Some(policy);
        self
    }
}

impl Default for AgentControlPlugins {
    fn default() -> Self {
        Self::deterministic()
    }
}

impl PluginGroup for AgentControlPlugins {
    fn build(self) -> PluginGroupBuilder {
        let mut builder = PluginGroupBuilder::start::<Self>().add(self.control);
        if self.snapshots {
            builder = builder.add(AgentSnapshotPlugin);
            if let Some(policy) = self.snapshot_policy {
                builder = builder.add(AgentSnapshotPolicyPlugin(policy));
            }
        }
        if self.replay {
            builder = builder.add(AgentReplayPlugin);
        }
        builder
    }
}

struct AgentSnapshotPolicyPlugin(SnapshotPolicy);

impl Plugin for AgentSnapshotPolicyPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(self.0.clone());
    }
}

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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VisualCaptureOptions {
    pub output_dir: PathBuf,
    pub label: Option<String>,
    pub timeout_frames: u32,
}

impl Default for VisualCaptureOptions {
    fn default() -> Self {
        Self {
            output_dir: PathBuf::from("screenshots"),
            label: None,
            timeout_frames: 8,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct VisualCaptureResult {
    pub tick: u64,
    pub frame: u64,
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub format: String,
}

type VisualCaptureFn =
    dyn Fn(&mut World, &VisualCaptureOptions) -> Result<VisualCaptureResult> + Send + Sync;

#[derive(Resource)]
pub struct AgentVisualCaptureRenderer {
    capture: Box<VisualCaptureFn>,
}

impl AgentVisualCaptureRenderer {
    pub fn new<F>(capture: F) -> Self
    where
        F: Fn(&mut World, &VisualCaptureOptions) -> Result<VisualCaptureResult>
            + Send
            + Sync
            + 'static,
    {
        Self {
            capture: Box::new(capture),
        }
    }

    pub fn capture(
        &self,
        world: &mut World,
        options: &VisualCaptureOptions,
    ) -> Result<VisualCaptureResult> {
        (self.capture)(world, options)
    }
}

pub trait VisualCaptureAppExt {
    fn insert_visual_capture_renderer<F>(&mut self, capture: F) -> &mut Self
    where
        F: Fn(&mut World, &VisualCaptureOptions) -> Result<VisualCaptureResult>
            + Send
            + Sync
            + 'static;
}

impl VisualCaptureAppExt for App {
    fn insert_visual_capture_renderer<F>(&mut self, capture: F) -> &mut Self
    where
        F: Fn(&mut World, &VisualCaptureOptions) -> Result<VisualCaptureResult>
            + Send
            + Sync
            + 'static,
    {
        self.insert_resource(AgentVisualCaptureRenderer::new(capture))
    }
}

#[must_use]
pub fn sanitized_capture_label(label: Option<&str>) -> String {
    let label = label.unwrap_or("capture");
    let mut sanitized = String::new();
    let mut previous_dash = false;
    for character in label.chars() {
        let next = if character.is_ascii_alphanumeric() {
            previous_dash = false;
            Some(character.to_ascii_lowercase())
        } else if character == '-' || character == '_' || character.is_ascii_whitespace() {
            if previous_dash {
                None
            } else {
                previous_dash = true;
                Some('-')
            }
        } else {
            None
        };
        if let Some(next) = next {
            sanitized.push(next);
        }
    }
    let sanitized = sanitized.trim_matches('-');
    if sanitized.is_empty() {
        "capture".to_string()
    } else {
        sanitized.to_string()
    }
}

pub fn visual_capture_path(
    options: &VisualCaptureOptions,
    tick: u64,
    frame: u64,
) -> Result<PathBuf> {
    std::fs::create_dir_all(&options.output_dir)?;
    let label = sanitized_capture_label(options.label.as_deref());
    let stem = format!("tick-{tick:06}-frame-{frame:06}-{label}");
    unique_path(&options.output_dir, &stem, "png")
}

fn unique_path(directory: &Path, stem: &str, extension: &str) -> Result<PathBuf> {
    let first = directory.join(format!("{stem}.{extension}"));
    if !first.exists() {
        return Ok(first);
    }

    for index in 1..10_000 {
        let candidate = directory.join(format!("{stem}-{index}.{extension}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }

    Err(anyhow!(
        "could not find an unused capture path for {}",
        directory.display()
    ))
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

    pub fn capture_visual(&mut self, options: VisualCaptureOptions) -> Result<VisualCaptureResult> {
        self.ensure_started();
        self.ensure_reset()?;

        if self
            .app
            .world()
            .contains_resource::<AgentVisualCaptureRenderer>()
        {
            return self.app.world_mut().resource_scope(
                |world, renderer: Mut<AgentVisualCaptureRenderer>| {
                    renderer.capture(world, &options)
                },
            );
        }

        self.capture_primary_window(options)
    }

    pub fn capture_primary_window(
        &mut self,
        options: VisualCaptureOptions,
    ) -> Result<VisualCaptureResult> {
        #[cfg(feature = "visual")]
        {
            self.capture_primary_window_impl(options)
        }

        #[cfg(not(feature = "visual"))]
        {
            let _ = options;
            Err(anyhow!(
                "visual capture requires a registered AgentVisualCaptureRenderer or the bevy_agent_runner visual feature"
            ))
        }
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
        self.reject_paused_or_inspect_only()?;
        let next_tick = self.current_tick() + 1;
        self.enqueue_action_at(next_tick, source, action);
        self.run_one_agent_tick();
        self.last_response()
    }

    /// External stepping (`step`/`step_many`/`fast_forward`) is rejected while
    /// the control mode is behavioral-only. Internal reconstruction
    /// (`restore`/`restore_tick`/`branch`) bypasses `step_with_source` and may
    /// still rebuild state. Returns before any action is enqueued so the tick is
    /// left unchanged.
    fn reject_paused_or_inspect_only(&self) -> Result<()> {
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

    /// Replays one `AgentTick` per recorded tick in `(checkpoint_tick,
    /// target_tick]`, grouping all actions sharing a recorded tick into that
    /// tick while preserving their recorded source and order. Empty ticks with
    /// no recorded actions still run. Recording is expected to be disabled by
    /// the caller so the replay is not appended to the log.
    fn replay_tick_interval(
        &mut self,
        log: &ReplayLog,
        checkpoint_tick: u64,
        target_tick: u64,
    ) -> Result<()> {
        // Preserve actions that were already scheduled beyond the replay target.
        // They are intentionally not in the replay log yet, but should remain
        // pending when the caller continues from the restored tick.
        let future_actions = self
            .app
            .world()
            .get_resource::<AgentActionQueue>()
            .map(|queue| {
                queue
                    .pending
                    .iter()
                    .filter(|scheduled| scheduled.tick > target_tick)
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        // The recorded actions fully define the interval; drop any pending
        // actions restored from the checkpoint so each tick matches the recorded
        // history exactly. Future actions are restored below after replay.
        if let Some(mut queue) = self.app.world_mut().get_resource_mut::<AgentActionQueue>() {
            queue.clear();
        }

        // Schedule each recorded action at its recorded tick, preserving its
        // recorded source and relative order.
        for record in log.actions_between(checkpoint_tick, target_tick) {
            self.enqueue_action_at(record.tick, record.source, record.action);
        }
        for scheduled in future_actions {
            self.enqueue_action_at(scheduled.tick, scheduled.source, scheduled.action);
        }

        // Run exactly one AgentTick per tick in the interval, including empty
        // ticks where no actions were recorded, and verify each produced a
        // response.
        for _ in 0..target_tick.saturating_sub(checkpoint_tick) {
            self.run_one_agent_tick();
            if self.app.world().resource::<LastStepResponse>().0.is_none() {
                return Err(anyhow!("replay tick produced no StepResponse"));
            }
        }

        Ok(())
    }

    fn has_snapshot_support(&self) -> bool {
        self.app.world().contains_resource::<SnapshotStore>()
    }

    #[cfg(feature = "visual")]
    fn capture_primary_window_impl(
        &mut self,
        options: VisualCaptureOptions,
    ) -> Result<VisualCaptureResult> {
        use bevy::render::view::screenshot::{Screenshot, save_to_disk};
        use bevy::window::{PrimaryWindow, Window};

        let (tick, frame, width, height) = {
            let world = self.app.world_mut();
            let tick = world.resource::<SimClock>().tick;
            let frame = world.resource::<AgentControlState>().frame;
            let mut windows = world.query_filtered::<&Window, With<PrimaryWindow>>();
            let window = windows
                .iter(world)
                .next()
                .ok_or_else(|| anyhow!("visual capture requires a primary window"))?;
            (
                tick,
                frame,
                window.physical_width(),
                window.physical_height(),
            )
        };
        let path = visual_capture_path(&options, tick, frame)?;
        let timeout_frames = options.timeout_frames.max(1);

        self.app
            .world_mut()
            .spawn(Screenshot::primary_window())
            .observe(save_to_disk(path.clone()));

        for _ in 0..=timeout_frames {
            self.app.update();
            if file_is_nonempty(&path) {
                return Ok(VisualCaptureResult {
                    tick,
                    frame,
                    path,
                    width,
                    height,
                    format: "png".to_string(),
                });
            }
            std::thread::sleep(std::time::Duration::from_millis(8));
        }

        Err(anyhow!(
            "visual capture timed out after {timeout_frames} frames: {}",
            path.display()
        ))
    }
}

#[cfg(feature = "visual")]
fn file_is_nonempty(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|metadata| metadata.len() > 0)
        .unwrap_or(false)
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
        reset_replay_and_timeline(self.app.world_mut());

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
        {
            // The snapshot restores the simulation clock but not the control
            // frame or snapshot bookkeeping. Realign the frame to the restored
            // tick and clear any stale snapshot marker so the next response does
            // not falsely report a freshly created snapshot.
            let world = self.app.world_mut();
            let restored_tick = world.resource::<SimClock>().tick;
            let mut control = world.resource_mut::<AgentControlState>();
            control.frame = restored_tick;
            control.last_snapshot_created = None;
        }
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

        // Start at the selected checkpoint.
        self.restore(snapshot_id)?;

        // Replay must never be recorded into the log. Disable recording for the
        // duration of the replay and restore the previous flag on both the
        // success and error paths.
        let old_recording = self
            .app
            .world()
            .get_resource::<ReplayRecorder>()
            .map(|recorder| recorder.recording)
            .unwrap_or(false);
        if let Some(mut recorder) = self.app.world_mut().get_resource_mut::<ReplayRecorder>() {
            recorder.recording = false;
        }

        let replay_result = self.replay_tick_interval(&log, checkpoint_tick, tick);

        if let Some(mut recorder) = self.app.world_mut().get_resource_mut::<ReplayRecorder>() {
            recorder.recording = old_recording;
        }
        replay_result?;

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

/// Marks an episode boundary by starting a fresh replay log and timeline root.
///
/// Called from `AgentApp::reset` after the `AgentReset` schedule and before the
/// optional initial snapshot. It replaces the `ReplayLog` with a fresh one while
/// preserving the `ReplayRecorder.recording` flag, installs a fresh `Timeline`
/// root, and synchronizes `AgentControlState.timeline_id`/`branch_id` to that
/// new root. It is a no-op for apps that do not install the replay resources, so
/// core-only and snapshot-less configurations remain supported.
fn reset_replay_and_timeline(world: &mut World) {
    let new_root = world.get_resource_mut::<Timeline>().map(|mut timeline| {
        *timeline = Timeline::default();
        (timeline.timeline_id, timeline.current_branch)
    });

    if let Some(mut recorder) = world.get_resource_mut::<ReplayRecorder>() {
        let recording = recorder.recording;
        recorder.log = ReplayLog::default();
        recorder.recording = recording;
    }

    if let Some((timeline_id, branch_id)) = new_root
        && let Some(mut control) = world.get_resource_mut::<AgentControlState>()
    {
        control.timeline_id = timeline_id;
        control.branch_id = branch_id;
    }
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

    fn grouped_agent_app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins).add_plugins(
            AgentControlPlugins::deterministic().with_snapshot_policy(SnapshotPolicy {
                checkpoint_every_ticks: 7,
                ..Default::default()
            }),
        );
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
    fn plugin_group_installs_core_snapshot_replay_and_policy() {
        let mut env = AgentApp::new(grouped_agent_app);
        env.reset(ResetOptions::default()).unwrap();

        assert!(env.world().contains_resource::<SnapshotStore>());
        assert!(env.world().contains_resource::<ReplayRecorder>());
        assert_eq!(
            env.world()
                .resource::<SnapshotPolicy>()
                .checkpoint_every_ticks,
            7
        );
    }

    #[test]
    fn fast_forward_zero_returns_error() {
        let mut env = AgentApp::new(core_only_app);

        let error = env.fast_forward(0).unwrap_err();

        assert!(error.to_string().contains("zero ticks"));
    }

    #[test]
    fn capture_label_is_filesystem_safe() {
        assert_eq!(sanitized_capture_label(Some("After Jump!")), "after-jump");
        assert_eq!(sanitized_capture_label(Some("../bad/name")), "badname");
        assert_eq!(sanitized_capture_label(Some("   ")), "capture");
    }

    #[test]
    fn visual_capture_path_is_unique() {
        let mut output_dir = std::env::temp_dir();
        output_dir.push(format!(
            "bevy-agent-runner-path-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let options = VisualCaptureOptions {
            output_dir: output_dir.clone(),
            label: Some("Test Capture".to_string()),
            timeout_frames: 1,
        };

        let first = visual_capture_path(&options, 3, 4).unwrap();
        std::fs::write(&first, b"exists").unwrap();
        let second = visual_capture_path(&options, 3, 4).unwrap();

        assert_eq!(
            first.file_name().unwrap().to_str().unwrap(),
            "tick-000003-frame-000004-test-capture.png"
        );
        assert_eq!(
            second.file_name().unwrap().to_str().unwrap(),
            "tick-000003-frame-000004-test-capture-1.png"
        );
        let _ = std::fs::remove_dir_all(output_dir);
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
