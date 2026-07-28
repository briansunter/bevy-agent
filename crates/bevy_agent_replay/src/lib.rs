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
    pub source: ActionSource,
    pub action: AgentAction,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ReplayLog {
    pub manifest: ReplayManifest,
    pub initial_snapshot: Option<SnapshotId>,
    pub records: Vec<ActionRecord>,
    pub checkpoints: BTreeMap<u64, SnapshotId>,
    #[serde(default, alias = "checksums")]
    pub snapshot_checksums: BTreeMap<u64, SnapshotChecksum>,
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

    #[must_use]
    pub fn nearest_checkpoint_at_or_before(&self, tick: u64) -> Option<(u64, SnapshotId)> {
        self.checkpoints
            .range(..=tick)
            .next_back()
            .map(|(tick, snapshot_id)| (*tick, *snapshot_id))
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
}

pub struct AgentReplayPlugin;

impl Plugin for AgentReplayPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ReplayRecorder>()
            .init_resource::<Timeline>()
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
) {
    if !recorder.recording {
        return;
    }

    let mut records = Vec::new();
    for (index, action) in input.actions.iter().cloned().enumerate() {
        records.push(ActionRecord {
            tick: input.tick,
            source: input
                .sources
                .get(index)
                .cloned()
                .unwrap_or(ActionSource::Agent),
            action,
        });
    }

    recorder.log.records.extend(records.iter().cloned());
    let current_branch = timeline.current_branch;
    if let Some(branch) = timeline.branches.get_mut(&control.branch_id) {
        branch.actions.extend(records);
    } else if let Some(branch) = timeline.branches.get_mut(&current_branch) {
        branch.actions.extend(records);
    }
}

pub fn start_recording(world: &mut World, initial_snapshot: Option<SnapshotId>) {
    let metadata = world
        .get_resource::<EnvironmentMetadata>()
        .cloned()
        .unwrap_or_default();
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
}
