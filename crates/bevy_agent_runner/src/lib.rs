#![doc = include_str!("../README.md")]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use bevy::app::{PluginGroup, PluginGroupBuilder};
use bevy::prelude::*;
use bevy_agent_core::{
    ActionSource, AgentAction, AgentActionCatalog, AgentActionQueue, AgentControlPlugin,
    AgentControlState, AgentFinalize, AgentReset, AgentTick, BranchId, ControlMode,
    CurrentInputFrame, DeterministicRng, EnvironmentMetadata, EpisodeState, ExecutionContext,
    LastStepResponse, Observation, ObservationConfig, ObservationMode, ResetResponse, RewardState,
    SimClock, SnapshotChecksum, SnapshotId, StepManyResponse, StepResponse, collect_observation,
    collect_observation_with_mode, merge_pending_actions, run_agent_tick, schedule_action,
    validate_action_against_catalog, validate_clock_tick, validate_integration,
};
use bevy_agent_core::{AgentPostTick, AgentPreTick};
use bevy_agent_replay::{
    AgentReplayPlugin, MAX_RECONSTRUCTION_TICKS, ReplayLog, ReplayRecorder, Timeline,
    branch_fork_from_ancestor, collect_replay_references, start_recording,
};
use bevy_agent_snapshot::{
    AgentSnapshotPlugin, FaultState, Snapshot, SnapshotCreateResult, SnapshotPolicy, SnapshotRole,
    SnapshotStore, SnapshotType, capture_snapshot, checksum_snapshot, create_snapshot_with_role,
    delete_snapshot_checked, enforce_retention, install_snapshots, prune_checkpoints_with_refs,
    restore_snapshot_backup, set_snapshot_episode, validate_snapshot_full,
};
use serde::{Deserialize, Serialize};

mod bundle;
mod capture;
mod checkpoints;
mod control;
mod history;
mod outcome;
mod plugins;
mod transaction;

pub use bevy_agent_replay::MAX_LINEAGE_DEPTH;
pub use bundle::{MAX_IMPORT_BRANCHES, ReplayBundle};
pub use capture::{
    AgentVisualCaptureRenderer, CaptureSource, VisualCaptureAppExt, VisualCaptureOptions,
    VisualCaptureResult, sanitized_capture_label, visual_capture_path,
};
pub use checkpoints::queue_excluded_checksum;
pub use control::{ResetOptions, clear_episode, set_episode_done};
pub use outcome::MutationFailure;
pub use plugins::AgentControlPlugins;

#[cfg(test)]
use bevy_agent_replay::TimelineBranch;
#[cfg(test)]
use bundle::validate_bundle_topology;
use checkpoints::capture_queue_excluded_checksum;

/// A synchronous control loop for an integrated simulation.
///
/// Implemented by [`AgentApp`]. Fallible operations validate the game's declared
/// action, observation, and snapshot contracts. Inspect terminal flags in step
/// responses before choosing the next action.
pub trait AgentEnvironment {
    /// Domain input admitted by the environment.
    type Action;
    /// State exposed to the controlling client.
    type Observation;

    /// Starts a fresh episode and returns its initial observation.
    fn reset(&mut self, options: ResetOptions) -> Result<Self::Observation>;

    /// Applies an action and advances one controlled simulation tick.
    fn step(&mut self, action: Self::Action) -> Result<StepResponse<Self::Observation>>;

    /// Steps actions in order, stopping when the episode is done or truncated.
    fn step_many(
        &mut self,
        actions: Vec<Self::Action>,
    ) -> Result<Vec<StepResponse<Self::Observation>>>;

    /// Collects a supported observation without advancing the simulation tick.
    fn observe(&mut self, mode: ObservationMode) -> Result<Self::Observation>;

    /// Captures registered gameplay state and returns a retained snapshot ID.
    fn snapshot(&mut self) -> Result<SnapshotCreateResult>;

    /// Restores a retained snapshot through the coordinated world/history path.
    fn restore(&mut self, snapshot: SnapshotId) -> Result<()>;

    /// Reconstructs a recorded tick using retained checkpoints and replay input.
    fn restore_tick(&mut self, tick: u64) -> Result<()>;

    /// Forks history at a recorded tick and activates the new branch.
    fn branch(
        &mut self,
        from_tick: u64,
        label: Option<String>,
    ) -> Result<bevy_agent_core::BranchId>;
}

/// Owns a validated Bevy app, controlled ticks, and coordinated history.
///
/// Construct with [`AgentApp::new`] after installing [`AgentControlPlugins`] and
/// the game's metadata, catalogs, extractors, and snapshot registrations.
/// [`AgentEnvironment`] exposes the basic control loop; inherent methods provide
/// batch responses, replay bundles, capture, and lower-level app access.
pub struct AgentApp {
    app: App,
    started: bool,
    reset_once: bool,
}

impl AgentEnvironment for AgentApp {
    type Action = AgentAction;
    type Observation = Observation;

    fn reset(&mut self, options: ResetOptions) -> Result<Observation> {
        self.reset_impl(options)
    }
    fn step(&mut self, action: AgentAction) -> Result<StepResponse<Observation>> {
        self.step_with_source(action, ActionSource::Agent)
    }
    fn step_many(&mut self, actions: Vec<AgentAction>) -> Result<Vec<StepResponse<Observation>>> {
        self.step_many_impl(actions)
    }
    fn observe(&mut self, mode: ObservationMode) -> Result<Observation> {
        self.observe_impl(mode)
    }
    fn snapshot(&mut self) -> Result<SnapshotCreateResult> {
        self.snapshot_impl()
    }
    fn restore(&mut self, snapshot: SnapshotId) -> Result<()> {
        self.restore_impl(snapshot)
    }
    fn restore_tick(&mut self, tick: u64) -> Result<()> {
        self.restore_tick_impl(tick)
    }
    fn branch(&mut self, from_tick: u64, label: Option<String>) -> Result<BranchId> {
        self.branch_impl(from_tick, label)
    }
}

#[cfg(test)]
mod tests;
