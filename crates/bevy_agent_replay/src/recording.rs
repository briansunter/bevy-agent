use bevy::prelude::*;
use bevy_agent_core::{
    ActionSource, AgentControlState, AgentSet, CurrentInputFrame, EnvironmentMetadata,
    ExecutionContext, SimClock, SnapshotId,
};

use crate::{ActionRecord, ReplayLog, ReplayRecorder, Timeline};
pub struct AgentReplayPlugin;

impl Plugin for AgentReplayPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ReplayRecorder>()
            .init_resource::<Timeline>()
            .init_resource::<ExecutionContext>()
            .add_systems(
                bevy_agent_core::AgentFinalize,
                record_replay_step.in_set(AgentSet::ReplayRecord),
            );
        let world = app.world_mut();
        let timeline = world.resource::<Timeline>().clone();
        if let Some(mut control) = world.get_resource_mut::<AgentControlState>() {
            control.timeline_id = timeline.timeline_id;
            control.branch_id = timeline.current_branch;
        }
        let tick = world
            .get_resource::<SimClock>()
            .map_or(0, |clock| clock.tick);
        let mut recorder = world.resource_mut::<ReplayRecorder>();
        if recorder.log.timeline_topology.is_empty() {
            recorder.log.initial_tick = tick;
            recorder.log.sync_topology(&timeline, tick);
        }
    }

    fn finish(&self, app: &mut App) {
        // Game plugins can establish their environment metadata after this
        // plugin's build method. Finish sees the complete installed app.
        let metadata = app
            .world()
            .get_resource::<EnvironmentMetadata>()
            .cloned()
            .unwrap_or_default();
        let mut recorder = app.world_mut().resource_mut::<ReplayRecorder>();
        let log = &mut recorder.log;
        let fresh = log.initial_snapshot.is_none()
            && log.records.is_empty()
            && log.completed_ticks.is_empty()
            && log.branch_checkpoints.is_empty()
            && log.branch_checksums.is_empty();
        if fresh {
            log.manifest.game_id = metadata.name;
            log.manifest.game_version = metadata.version;
        }
    }
}

pub fn record_replay_step(
    input: Res<CurrentInputFrame>,
    mut recorder: ResMut<ReplayRecorder>,
    timeline: Res<Timeline>,
    control: Res<AgentControlState>,
    context: Res<ExecutionContext>,
    mut failure: ResMut<bevy_agent_core::AgentTickFailure>,
) {
    if !recorder.recording || failure.error().is_some() {
        return;
    }
    // No recording append while reconstructing history.
    if *context == ExecutionContext::Reconstructing {
        return;
    }

    let branch_id = control.branch_id;
    if !timeline.branches.contains_key(&branch_id) {
        bevy::log::error!("cannot record input for unknown branch {branch_id:?}");
        return;
    }
    let mut records = Vec::new();
    for (index, action) in input.actions.iter().cloned().enumerate() {
        records.push(ActionRecord {
            tick: input.tick,
            branch_id,
            source: input
                .sources
                .get(index)
                .cloned()
                .unwrap_or(ActionSource::Agent),
            action,
        });
    }

    if let Err(error) = recorder.record_frame(branch_id, input.tick, records) {
        failure.set(bevy_agent_core::AgentControlError::Message(error));
    }
}

/// Replace recording owners only after the fresh history fits its byte budget.
pub fn start_recording(
    world: &mut World,
    initial_snapshot: Option<SnapshotId>,
) -> Result<(), String> {
    let previous = world.get_resource::<ReplayRecorder>();
    let episode_id = previous.map_or(0, |r| r.log.manifest.episode_id);
    let mut recorder =
        previous.map_or_else(ReplayRecorder::default, ReplayRecorder::empty_replacement);
    let metadata = world
        .get_resource::<EnvironmentMetadata>()
        .cloned()
        .unwrap_or_default();
    let tick = world
        .get_resource::<SimClock>()
        .map_or(0, |clock| clock.tick);
    let timeline = Timeline::default();
    let mut log = ReplayLog {
        initial_snapshot,
        initial_tick: tick,
        cursor_tick: tick,
        end_tick: tick,
        ..Default::default()
    };
    log.manifest.game_id = metadata.name;
    log.manifest.game_version = metadata.version;
    log.manifest.episode_id = episode_id;
    log.sync_topology(&timeline, tick);
    recorder.replace_log(log)?;
    recorder.recording = true;
    if let Some(mut control) = world.get_resource_mut::<AgentControlState>() {
        control.timeline_id = timeline.timeline_id;
        control.branch_id = timeline.current_branch;
    }
    world.insert_resource(timeline);
    world.insert_resource(recorder);
    Ok(())
}

/// Seal the current recording and return its action count. Repeated stops
/// leave the sealed history untouched; payload retrieval is explicit.
pub fn stop_recording(world: &mut World) -> Result<usize, String> {
    let recorder = world.resource::<ReplayRecorder>();
    if !recorder.recording {
        return Ok(recorder.log.records.len());
    }
    let tick = world
        .get_resource::<SimClock>()
        .map_or(0, |clock| clock.tick);
    let timeline = world.get_resource::<Timeline>().cloned();
    let mut recorder = world.resource_mut::<ReplayRecorder>();
    if let Some(timeline) = timeline {
        recorder.sync_topology(&timeline, tick)?;
    }
    recorder.recording = false;
    Ok(recorder.log.records.len())
}
