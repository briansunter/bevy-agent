use super::*;
use bevy_agent_core::{
    AgentActionKind, AgentControlAppExt, AgentControlPlugin, AgentDecision, EnvironmentChecksum,
};

fn configure_test_integration(app: &mut App) {
    app.set_environment_metadata("runner-test", "1", None)
        .set_supported_actions([
            AgentActionKind::Noop,
            AgentActionKind::Move,
            AgentActionKind::Jump,
            AgentActionKind::Interact,
        ])
        .set_supported_observation_modes([
            ObservationMode::Hybrid,
            ObservationMode::PlayerKnowledge,
        ])
        .insert_observation_extractor(|world, _| {
            Observation::default_for_tick(world.resource::<SimClock>().tick)
        })
        .insert_checksum_extractor(|world| EnvironmentChecksum {
            tick: world.resource::<SimClock>().tick,
            hash: world.resource::<SimClock>().tick,
        });
}

fn core_only_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(AgentControlPlugin::default());
    configure_test_integration(&mut app);
    app
}

fn grouped_agent_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins).add_plugins(
        AgentControlPlugins::default().with_snapshot_policy(SnapshotPolicy {
            checkpoint_every_ticks: 7,
            ..Default::default()
        }),
    );
    configure_test_integration(&mut app);
    app
}

pub(super) fn full_history_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(AgentControlPlugins::default());
    configure_test_integration(&mut app);
    app
}

fn frequent_checkpoint_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins).add_plugins(
        AgentControlPlugins::default().with_snapshot_policy(SnapshotPolicy {
            checkpoint_every_ticks: 2,
            ..Default::default()
        }),
    );
    configure_test_integration(&mut app);
    app
}

#[derive(Resource, Default)]
struct PolicyCalls(u64);

fn counting_policy(
    mut calls: ResMut<PolicyCalls>,
    clock: Res<SimClock>,
    mut queue: ResMut<AgentActionQueue>,
    catalog: Res<AgentActionCatalog>,
    control: Res<AgentControlState>,
    context: Res<ExecutionContext>,
) {
    calls.0 += 1;
    queue
        .schedule(
            &catalog,
            &clock,
            &control,
            &context,
            clock.tick + 1,
            ActionSource::Script,
            AgentAction::Jump,
        )
        .unwrap();
}

fn policy_driven_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(AgentControlPlugins::default())
        .init_resource::<PolicyCalls>()
        .add_systems(AgentDecision, counting_policy);
    configure_test_integration(&mut app);
    app
}

fn record_len(env: &AgentApp) -> usize {
    env.replay_log().map(|log| log.records.len()).unwrap_or(0)
}

#[test]
fn reset_without_snapshot_plugin_returns_observation() {
    let mut env = AgentApp::new(core_only_app).unwrap();

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
    let mut env = AgentApp::new(core_only_app).unwrap();

    let response = env.step(AgentAction::Noop).unwrap();

    assert_eq!(response.tick, 1);
    assert_eq!(response.info.actions_applied, 1);
}

#[test]
fn plugin_group_installs_core_snapshot_replay_and_policy() {
    let mut env = AgentApp::new(grouped_agent_app).unwrap();
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
    let mut env = AgentApp::new(core_only_app).unwrap();

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
        source: CaptureSource::Auto,
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
    let mut env = AgentApp::new(core_only_app).unwrap();

    let error = env.snapshot().unwrap_err();

    assert!(error.to_string().contains("AgentSnapshotPlugin"));
}

#[test]
fn episode_helpers_update_world_resources() {
    let mut env = AgentApp::new(core_only_app).unwrap();
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

#[test]
fn parent_child_same_tick_checkpoints_are_isolated() {
    let mut env = AgentApp::new(full_history_app).unwrap();
    env.reset(ResetOptions::default()).unwrap();
    for _ in 0..3 {
        env.step(AgentAction::Noop).unwrap();
    }
    let parent = env.world().resource::<AgentControlState>().branch_id;
    let parent_snapshot = env.snapshot().unwrap().snapshot_id;

    let child = env.branch(3, Some("alt".to_string())).unwrap();
    assert_ne!(child, parent);

    let log = env.replay_log().unwrap().clone();
    let parent_entries = log
        .branch_checkpoints
        .iter()
        .filter(|checkpoint| checkpoint.tick == 3 && checkpoint.branch_id == parent)
        .collect::<Vec<_>>();
    let child_entries = log
        .branch_checkpoints
        .iter()
        .filter(|checkpoint| checkpoint.tick == 3 && checkpoint.branch_id == child)
        .collect::<Vec<_>>();
    assert!(
        parent_entries
            .iter()
            .any(|c| c.snapshot_id == parent_snapshot)
    );
    assert_eq!(child_entries.len(), 1);
    assert_ne!(child_entries[0].snapshot_id, parent_snapshot);

    // The child restores from its own fork checkpoint.
    env.restore_tick(3).unwrap();
    assert_eq!(env.current_tick(), 3);
    assert_eq!(env.world().resource::<AgentControlState>().branch_id, child);
}

#[test]
fn restore_then_diverge_truncates_recorded_future() {
    let mut env = AgentApp::new(full_history_app).unwrap();
    env.reset(ResetOptions::default()).unwrap();
    for _ in 0..5 {
        env.step(AgentAction::Noop).unwrap();
    }
    assert_eq!(env.current_tick(), 5);
    assert_eq!(record_len(&env), 5);

    env.restore_tick(2).unwrap();
    assert_eq!(env.current_tick(), 2);
    // Restore alone preserves the recorded future.
    assert_eq!(record_len(&env), 5);

    // Stepping into the recorded future on the same branch diverges and
    // truncates everything beyond the restore point before appending.
    let response = env.step(AgentAction::Jump).unwrap();
    assert_eq!(response.tick, 3);
    let log = env.replay_log().unwrap().clone();
    assert_eq!(log.records.len(), 3);
    assert!(log.records.iter().all(|record| record.tick <= 3));
    assert_eq!(log.records.last().unwrap().action, AgentAction::Jump);
}

#[test]
fn replay_runs_no_policy_and_appends_no_records() {
    let mut env = AgentApp::new(policy_driven_app).unwrap();
    env.reset(ResetOptions::default()).unwrap();
    for _ in 0..3 {
        env.step(AgentAction::Noop).unwrap();
    }
    let calls_after_live = env.world().resource::<PolicyCalls>().0;
    assert!(calls_after_live >= 3);
    let records_after_live = record_len(&env);
    assert!(records_after_live >= 3);

    // Replay must not execute the policy again and must not append records.
    env.restore_tick(1).unwrap();
    assert_eq!(env.current_tick(), 1);
    assert_eq!(env.world().resource::<PolicyCalls>().0, calls_after_live);
    assert_eq!(record_len(&env), records_after_live);
    assert_eq!(
        *env.world().resource::<ExecutionContext>(),
        ExecutionContext::Live
    );
}

#[test]
fn reconstruction_creates_no_snapshots() {
    let mut env = AgentApp::new(frequent_checkpoint_app).unwrap();
    env.reset(ResetOptions::default()).unwrap();
    for _ in 0..5 {
        env.step(AgentAction::Noop).unwrap();
    }
    // Interval checkpoints at ticks 2 and 4 are mirrored into the log.
    let log = env.replay_log().unwrap().clone();
    assert!(log.branch_checkpoints.iter().any(|c| c.tick == 2));
    assert!(log.branch_checkpoints.iter().any(|c| c.tick == 4));
    let snapshots_before = env.world().resource::<SnapshotStore>().len();
    let checkpoints_before = env.world().resource::<SnapshotStore>().checkpoints().len();
    let records_before = log.records.len();

    // Replaying across the tick-4 interval checkpoint must not create
    // additional snapshots (tick 4 is a multiple of the interval).
    env.restore_tick(4).unwrap();

    assert_eq!(env.current_tick(), 4);
    assert_eq!(
        env.world().resource::<SnapshotStore>().len(),
        snapshots_before
    );
    assert_eq!(
        env.world().resource::<SnapshotStore>().checkpoints().len(),
        checkpoints_before
    );
    assert_eq!(record_len(&env), records_before);
    assert_eq!(
        *env.world().resource::<ExecutionContext>(),
        ExecutionContext::Live
    );
}

#[test]
fn step_response_reports_same_tick_snapshot() {
    let mut env = AgentApp::new(frequent_checkpoint_app).unwrap();
    env.reset(ResetOptions::default()).unwrap();

    let response = env.step(AgentAction::Noop).unwrap();

    // Tick 2 hits the interval policy; the response must report the
    // snapshot created on that same tick, and the log must carry it.
    let second = env.step(AgentAction::Noop).unwrap();
    assert_eq!(second.tick, 2);
    assert!(second.info.snapshot_created.is_some());
    assert!(
        env.replay_log()
            .unwrap()
            .branch_checkpoints
            .iter()
            .any(|checkpoint| checkpoint.tick == second.tick)
    );
    let _ = response;
}

#[test]
fn terminal_step_records_checkpoint_on_branch() {
    let mut env = AgentApp::new(full_history_app).unwrap();
    env.reset(ResetOptions::default()).unwrap();
    env.app_mut().add_systems(
        AgentTick,
        (|world: &mut World| set_episode_done(world, "done"))
            .in_set(bevy_agent_core::AgentSet::TerminalCheck),
    );

    let response = env.step(AgentAction::Noop).unwrap();

    assert!(response.done);
    assert!(response.info.snapshot_created.is_some());
    let branch = env.world().resource::<AgentControlState>().branch_id;
    assert!(
        env.replay_log()
            .unwrap()
            .branch_checkpoints
            .iter()
            .any(|checkpoint| checkpoint.branch_id == branch && checkpoint.tick == response.tick)
    );
}

#[test]
fn parent_future_exclusion_restore_child_tick2_is_only_noop() {
    // Parent tick2 MoveRight (fork tick1) must not leak into the child:
    // restoring child tick2 yields only the child's Noop.
    let mut env = AgentApp::new(full_history_app).unwrap();
    env.reset(ResetOptions::default()).unwrap();
    env.step(AgentAction::Noop).unwrap(); // tick1 parent
    let move_right = AgentAction::Move { x: 1.0, y: 0.0 };
    env.step(move_right.clone()).unwrap(); // tick2 parent
    let child = env.branch(1, Some("child".to_string())).unwrap();
    env.step(AgentAction::Noop).unwrap(); // tick2 child
    assert_eq!(env.current_tick(), 2);
    // Restore child tick2 via fork-bounded intervals.
    env.restore_tick(2).unwrap();
    assert_eq!(env.current_tick(), 2);
    assert_eq!(env.world().resource::<AgentControlState>().branch_id, child);
    let input = env.world().resource::<CurrentInputFrame>().clone();
    assert_eq!(input.tick, 2);
    assert_eq!(input.actions, vec![AgentAction::Noop]);
    // Log-level check: child sees shared tick1 + own tick2 only.
    let (log, timeline) = (
        env.replay_log().unwrap().clone(),
        env.world().resource::<Timeline>().clone(),
    );
    let visible = log.actions_for_branch(&timeline, child, 0, 2);
    let tick2: Vec<_> = visible.iter().filter(|r| r.tick == 2).collect();
    assert_eq!(tick2.len(), 1);
    assert_eq!(tick2[0].action, AgentAction::Noop);
}

#[test]
fn paused_and_replay_mode_reconstruction_preserves_inputs() {
    for mode in [ControlMode::Paused, ControlMode::Replay] {
        let mut env = AgentApp::new(full_history_app).unwrap();
        env.reset(ResetOptions::default()).unwrap();
        for _ in 0..3 {
            env.step(AgentAction::Noop).unwrap();
        }
        // Enter behavioral-only mode after recording; reconstruction
        // bypasses `step_with_source` and must still preserve inputs via
        // the reconstruction context in `drain_agent_actions`.
        env.world_mut().resource_mut::<AgentControlState>().mode = mode.clone();
        env.restore_tick(2).unwrap();
        assert_eq!(env.current_tick(), 2);
        let input = env.world().resource::<CurrentInputFrame>().clone();
        assert_eq!(input.tick, 2);
        assert_eq!(input.actions, vec![AgentAction::Noop]);
        assert_eq!(input.sources, vec![ActionSource::Agent]);
    }
}

#[test]
fn import_preserves_three_level_topology_and_active_branch() {
    let mut source = AgentApp::new(full_history_app).unwrap();
    source.reset(ResetOptions::default()).unwrap();
    for _ in 0..2 {
        source.step(AgentAction::Noop).unwrap();
    }
    let mid = source.branch(2, Some("mid".to_string())).unwrap();
    source.step(AgentAction::Jump).unwrap(); // tick3 on mid
    let leaf = source.branch(3, Some("leaf".to_string())).unwrap();
    source.step(AgentAction::Interact).unwrap(); // tick4 on leaf
    let bundle = source.export_replay_bundle().unwrap();
    assert_eq!(bundle.log.timeline_topology.len(), 3);
    assert_eq!(bundle.log.active_branch, Some(leaf));

    let mut fresh = AgentApp::new(full_history_app).unwrap();
    fresh.load_replay_bundle(bundle).unwrap();
    let timeline = fresh.app.world().resource::<Timeline>().clone();
    let control = fresh.app.world().resource::<AgentControlState>().clone();
    assert_eq!(timeline.branches().len(), 3);
    assert_eq!(timeline.current_branch(), leaf);
    assert_eq!(control.branch_id, leaf);
    // Parent links verbatim: leaf -> mid -> root.
    let leaf_branch = timeline.branches().get(&leaf).unwrap();
    assert_eq!(leaf_branch.parent_branch, Some(mid));
    let mid_branch = timeline.branches().get(&mid).unwrap();
    assert!(mid_branch.parent_branch.is_some());
    assert_ne!(mid_branch.parent_branch, Some(leaf));
    // Restorable on the leaf.
    fresh.restore_tick(4).unwrap();
    assert_eq!(fresh.current_tick(), 4);
}

#[test]
fn load_bundle_syncs_timeline_and_restores_history() {
    let mut source = AgentApp::new(full_history_app).unwrap();
    source.reset(ResetOptions::default()).unwrap();
    for _ in 0..3 {
        source.step(AgentAction::Noop).unwrap();
    }
    let manual = source.snapshot().unwrap().snapshot_id;
    let bundle = source.export_replay_bundle().unwrap();

    let mut fresh = AgentApp::new(full_history_app).unwrap();
    // Recording flag is preserved across import.
    fresh.stop_recording().unwrap();
    fresh.load_replay_bundle(bundle).unwrap();

    assert!(
        !fresh
            .app
            .world()
            .resource::<ReplayRecorder>()
            .is_recording()
    );
    // Control/timeline initialized from the imported log.
    let timeline = fresh.app.world().resource::<Timeline>().clone();
    let control = fresh.app.world().resource::<AgentControlState>().clone();
    assert_eq!(control.branch_id, timeline.current_branch());
    assert_eq!(control.timeline_id, timeline.timeline_id());
    assert!(
        fresh
            .app
            .world()
            .resource::<SnapshotStore>()
            .checkpoints()
            .len()
            >= 2
    );
    assert!(fresh.has_reset());
    // Imported history is restorable.
    fresh.restore_tick(2).unwrap();
    assert_eq!(fresh.current_tick(), 2);
    // The manual snapshot survived the round trip.
    assert!(
        fresh
            .app
            .world()
            .resource::<SnapshotStore>()
            .get(manual)
            .is_some()
    );
}
#[test]
fn invalid_step_and_batch_do_not_initialize_or_advance() {
    let invalid = AgentAction::Move { x: 2.0, y: 0.0 };
    let mut env = AgentApp::new(full_history_app).unwrap();
    assert!(env.step(invalid.clone()).is_err());
    assert!(!env.has_reset());
    assert_eq!(env.current_tick(), 0);
    assert!(
        env.step_many(vec![AgentAction::Noop, invalid.clone()])
            .is_err()
    );
    assert!(!env.has_reset());
    env.reset(ResetOptions::default()).unwrap();
    let before = serde_json::to_value(env.replay_log()).unwrap();
    assert!(env.step_many(vec![AgentAction::Noop, invalid]).is_err());
    assert_eq!(env.current_tick(), 0);
    assert_eq!(serde_json::to_value(env.replay_log()).unwrap(), before);
    assert!(env.world().resource::<AgentActionQueue>().is_empty());
}

#[test]
fn terminal_resource_rejects_step_before_response_is_refreshed() {
    let mut env = AgentApp::new(full_history_app).unwrap();
    env.reset(ResetOptions::default()).unwrap();
    set_episode_done(env.world_mut(), "external stop");
    assert!(
        !env.world()
            .resource::<LastStepResponse>()
            .0
            .as_ref()
            .unwrap()
            .done
    );
    let before = serde_json::to_value(env.replay_log()).unwrap();
    let error = env.step(AgentAction::Noop).unwrap_err();
    assert!(error.to_string().contains("terminal"));
    assert_eq!(env.current_tick(), 0);
    assert_eq!(serde_json::to_value(env.replay_log()).unwrap(), before);
    assert!(env.world().resource::<AgentActionQueue>().is_empty());
}

#[derive(Resource, Default)]
struct ObservationCalls(Vec<ObservationMode>);

fn counting_observation_app() -> App {
    use bevy_agent_core::AgentControlAppExt;
    let mut app = core_only_app();
    app.init_resource::<ObservationCalls>()
        .insert_observation_extractor(|world, mode| {
            world.resource_mut::<ObservationCalls>().0.push(mode);
            Observation::default_for_tick(world.resource::<SimClock>().tick)
        });
    app
}

#[test]
fn observation_requests_preserve_default_and_extract_once() {
    let mut env = AgentApp::new(counting_observation_app).unwrap();
    env.reset(ResetOptions::default()).unwrap();
    let default_mode = env.world().resource::<ObservationConfig>().mode.clone();
    env.world_mut().resource_mut::<ObservationCalls>().0.clear();
    env.observe(ObservationMode::PlayerKnowledge).unwrap();
    assert_eq!(
        env.world().resource::<ObservationConfig>().mode,
        default_mode
    );
    assert_eq!(
        env.world().resource::<ObservationCalls>().0,
        vec![ObservationMode::PlayerKnowledge]
    );
    env.world_mut().resource_mut::<ObservationCalls>().0.clear();
    env.step_with_observation_mode(AgentAction::Noop, ObservationMode::PlayerKnowledge)
        .unwrap();
    assert_eq!(
        env.world().resource::<ObservationConfig>().mode,
        default_mode
    );
    assert_eq!(
        env.world().resource::<ObservationCalls>().0,
        vec![ObservationMode::PlayerKnowledge]
    );
    env.world_mut().resource_mut::<AgentControlState>().mode = ControlMode::Paused;
    assert!(
        env.step_with_observation_mode(AgentAction::Noop, ObservationMode::PixelFrame)
            .is_err()
    );
    assert_eq!(
        env.world().resource::<ObservationConfig>().mode,
        default_mode
    );
}

#[test]
fn stopped_recording_preserves_future_and_completed_ticks() {
    let mut env = AgentApp::new(frequent_checkpoint_app).unwrap();
    env.reset(ResetOptions::default()).unwrap();
    for _ in 0..5 {
        env.step(AgentAction::Noop).unwrap();
    }
    env.restore_tick(1).unwrap();
    env.stop_recording().unwrap();
    let before = serde_json::to_value(env.replay_log()).unwrap();
    env.step(AgentAction::Jump).unwrap(); // periodic snapshot still runs independently
    assert_eq!(env.current_tick(), 2);
    assert_eq!(serde_json::to_value(env.replay_log()).unwrap(), before);
}

#[test]
fn snapshotless_history_import_fails_without_changing_destination() {
    let mut source = AgentApp::new(full_history_app).unwrap();
    source
        .reset(ResetOptions {
            create_initial_snapshot: false,
            ..Default::default()
        })
        .unwrap();
    source.step(AgentAction::Noop).unwrap();
    assert!(
        source
            .export_replay_bundle()
            .unwrap_err()
            .to_string()
            .contains("baseline")
    );
    let bundle = ReplayBundle {
        format_version: ReplayBundle::FORMAT_VERSION,
        log: source.replay_log().unwrap().clone(),
        snapshots: Vec::new(),
    };
    assert!(bundle.snapshots.is_empty());
    let mut destination = AgentApp::new(full_history_app).unwrap();
    destination.reset(ResetOptions::default()).unwrap();
    destination.step(AgentAction::Jump).unwrap();
    let before = serde_json::to_value(destination.replay_log()).unwrap();
    let before_clock = destination.world().resource::<SimClock>().tick;
    assert!(
        destination
            .load_replay_bundle(bundle)
            .unwrap_err()
            .to_string()
            .contains("baseline")
    );
    assert_eq!(destination.current_tick(), before_clock);
    assert_eq!(
        serde_json::to_value(destination.replay_log()).unwrap(),
        before
    );
    assert!(destination.has_reset());
}

#[test]
fn duplicate_snapshot_ids_are_rejected_before_install() {
    let mut source = AgentApp::new(full_history_app).unwrap();
    source.reset(ResetOptions::default()).unwrap();
    let mut bundle = source.export_replay_bundle().unwrap();
    bundle.snapshots.push(bundle.snapshots[0].clone());
    let mut destination = AgentApp::new(full_history_app).unwrap();
    assert!(
        destination
            .load_replay_bundle(bundle)
            .unwrap_err()
            .to_string()
            .contains("duplicate snapshot")
    );
    assert!(!destination.has_reset());
    assert!(destination.world().resource::<SnapshotStore>().is_empty());
}

#[test]
fn unreferenced_snapshot_payload_is_fully_validated() {
    let mut source = AgentApp::new(full_history_app).unwrap();
    source.reset(ResetOptions::default()).unwrap();
    let mut bundle = source.export_replay_bundle().unwrap();
    let mut unrelated = bundle.snapshots[0].clone();
    unrelated.manifest.snapshot_id = SnapshotId::new();
    let reward = unrelated
        .resources
        .iter_mut()
        .find(|resource| resource.type_id == <RewardState as SnapshotType>::TYPE_ID)
        .unwrap();
    reward.value = serde_json::json!({"current_reward": "broken", "cumulative_reward": 0.0});
    unrelated.checksum = checksum_snapshot(&unrelated).unwrap();
    bundle.snapshots.push(unrelated);
    let mut destination = AgentApp::new(full_history_app).unwrap();
    assert!(destination.validate_replay_bundle(&bundle).is_err());
    assert!(destination.load_replay_bundle(bundle).is_err());
    assert!(!destination.has_reset());
    assert!(destination.world().resource::<SnapshotStore>().is_empty());
}

#[test]
fn imported_checkpoint_queue_contains_only_inputs_after_cursor() {
    let mut source = AgentApp::new(full_history_app).unwrap();
    source.reset(ResetOptions::default()).unwrap();
    source
        .enqueue_action_at(2, ActionSource::Test, AgentAction::Jump)
        .unwrap();
    source
        .enqueue_action_at(5, ActionSource::Test, AgentAction::Jump)
        .unwrap();
    source.snapshot().unwrap();
    source.step(AgentAction::Noop).unwrap();
    source.step(AgentAction::Noop).unwrap();
    let bundle = source.export_replay_bundle().unwrap();
    let mut destination = AgentApp::new(full_history_app).unwrap();
    destination.load_replay_bundle(bundle).unwrap();
    assert_eq!(destination.current_tick(), 2);
    let pending = destination
        .world()
        .resource::<AgentActionQueue>()
        .iter()
        .collect::<Vec<_>>();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].tick, 5);
}

#[test]
fn stopped_recording_exports_a_restorable_recorded_cursor() {
    let mut source = AgentApp::new(full_history_app).unwrap();
    source.reset(ResetOptions::default()).unwrap();
    source.step(AgentAction::Jump).unwrap();
    source.stop_recording().unwrap();
    source.step(AgentAction::Noop).unwrap();
    source.step(AgentAction::Noop).unwrap();
    assert_eq!(source.current_tick(), 3);
    let bundle = source.export_replay_bundle().unwrap();
    assert_eq!(bundle.log.cursor_tick, 1);
    assert_eq!(bundle.log.end_tick, 1);
    let mut destination = AgentApp::new(full_history_app).unwrap();
    destination.load_replay_bundle(bundle).unwrap();
    assert_eq!(destination.current_tick(), 1);
    assert_eq!(record_len(&destination), 1);
}

fn topology_bundle(branches: Vec<TimelineBranch>) -> ReplayBundle {
    ReplayBundle {
        format_version: ReplayBundle::FORMAT_VERSION,
        log: ReplayLog {
            active_branch: branches
                .iter()
                .find(|branch| branch.parent_branch.is_none())
                .map(|branch| branch.branch_id),
            timeline_topology: branches,
            ..Default::default()
        },
        snapshots: Vec::new(),
    }
}

fn topology_branch(id: BranchId, parent: Option<BranchId>) -> TimelineBranch {
    TimelineBranch {
        branch_id: id,
        parent_branch: parent,
        fork_tick: 0,
        fork_snapshot: None,
        label: None,
    }
}

#[test]
fn canonical_topology_validation_requires_explicit_rooted_forks() {
    assert!(validate_bundle_topology(&topology_bundle(Vec::new())).is_err());
    let root = BranchId::new();
    let child = BranchId::new();
    assert!(
        validate_bundle_topology(&topology_bundle(vec![
            topology_branch(root, None),
            topology_branch(child, Some(root))
        ]))
        .is_ok()
    );
}

#[test]
fn canonical_topology_validation_rejects_corrupt_graphs() {
    let root = BranchId::new();
    let child = BranchId::new();
    let cases = vec![
        vec![topology_branch(root, Some(root))],
        vec![
            topology_branch(root, Some(child)),
            topology_branch(child, Some(root)),
        ],
        vec![topology_branch(root, None), topology_branch(root, None)],
        vec![topology_branch(root, Some(child))],
        vec![topology_branch(root, None), topology_branch(child, None)],
    ];
    for branches in cases {
        assert!(validate_bundle_topology(&topology_bundle(branches)).is_err());
    }
    let too_many = (0..=MAX_IMPORT_BRANCHES)
        .map(|_| topology_branch(BranchId::new(), None))
        .collect();
    assert!(
        validate_bundle_topology(&topology_bundle(too_many))
            .unwrap_err()
            .to_string()
            .contains("limit")
    );
    let mut chain = vec![topology_branch(root, None)];
    let mut parent = root;
    for _ in 0..=MAX_LINEAGE_DEPTH {
        let id = BranchId::new();
        chain.push(topology_branch(id, Some(parent)));
        parent = id;
    }
    assert!(
        validate_bundle_topology(&topology_bundle(chain))
            .unwrap_err()
            .to_string()
            .contains("depth")
    );
}

#[derive(Resource, Clone, Default, Serialize, Deserialize)]
struct PostTickCounter(u64);

impl SnapshotType for PostTickCounter {
    const TYPE_ID: &'static str = "runner_test.post_tick_counter";
    const SCHEMA_VERSION: u32 = 1;
}

fn post_tick_app() -> App {
    use bevy_agent_core::AgentControlAppExt;
    use bevy_agent_snapshot::SnapshotAppExt;
    let mut app = frequent_checkpoint_app();
    app.init_resource::<PostTickCounter>()
        .register_snapshot_resource::<PostTickCounter>()
        .unwrap()
        .insert_observation_extractor(|world, _| {
            Observation::FullState(
                serde_json::json!({"counter": world.resource::<PostTickCounter>().0}),
            )
        })
        .add_systems(AgentPostTick, |mut counter: ResMut<PostTickCounter>| {
            counter.0 += 1
        });
    app
}

#[test]
fn finalization_observes_and_snapshots_completed_post_tick_state() {
    let mut env = AgentApp::new(post_tick_app).unwrap();
    env.reset(ResetOptions::default()).unwrap();
    let first = env.step(AgentAction::Noop).unwrap();
    assert_eq!(
        first.observation,
        Observation::FullState(serde_json::json!({"counter": 1}))
    );
    let second = env.step(AgentAction::Noop).unwrap();
    let checkpoint = env
        .world()
        .resource::<SnapshotStore>()
        .get(second.info.snapshot_created.unwrap())
        .unwrap();
    let counter = checkpoint
        .resources
        .iter()
        .find(|resource| resource.type_id == PostTickCounter::TYPE_ID)
        .unwrap();
    assert_eq!(counter.value, serde_json::json!(2));
    let third = env.step(AgentAction::Noop).unwrap();
    env.restore_tick(1).unwrap();
    assert_eq!(env.world().resource::<PostTickCounter>().0, 1);
    env.restore_tick(3).unwrap();
    assert_eq!(env.world().resource::<PostTickCounter>().0, 3);
    assert_eq!(
        env.world()
            .resource::<LastStepResponse>()
            .0
            .as_ref()
            .unwrap()
            .observation,
        third.observation
    );
}

#[test]
fn terminal_checkpoint_after_recording_stops_preserves_the_log() {
    let mut env = AgentApp::new(full_history_app).unwrap();
    env.reset(ResetOptions::default()).unwrap();
    env.stop_recording().unwrap();
    env.app_mut()
        .add_systems(AgentPostTick, |world: &mut World| {
            set_episode_done(world, "post hook terminal")
        });
    let before = serde_json::to_value(env.replay_log()).unwrap();
    let response = env.step(AgentAction::Noop).unwrap();
    assert!(response.done);
    assert!(response.info.snapshot_created.is_some());
    assert_eq!(serde_json::to_value(env.replay_log()).unwrap(), before);
}

#[test]
fn navigation_preflight_uses_checkpoint_cost_and_permits_recorded_forward_targets() {
    let mut env = AgentApp::new(frequent_checkpoint_app).unwrap();
    env.reset(ResetOptions::default()).unwrap();
    for _ in 0..5 {
        env.step(AgentAction::Noop).unwrap();
    }
    env.restore_tick(0).unwrap();
    let before = serde_json::to_value(env.replay_log()).unwrap();
    env.validate_history_navigation(4, 0).unwrap(); // existing checkpoint, no replay
    assert!(
        env.validate_history_navigation(5, 0)
            .unwrap_err()
            .to_string()
            .contains("budget")
    );
    env.validate_history_navigation(5, 1).unwrap();
    assert_eq!(env.current_tick(), 0);
    assert_eq!(serde_json::to_value(env.replay_log()).unwrap(), before);
    env.restore_tick(4).unwrap();
    assert_eq!(env.current_tick(), 4);
}

#[test]
fn replay_bundle_rejects_obsolete_version_and_unknown_fields() {
    let mut source = AgentApp::new(full_history_app).unwrap();
    source.reset(ResetOptions::default()).unwrap();
    let mut bundle = source.export_replay_bundle().unwrap();
    let mut encoded = serde_json::to_value(&bundle).unwrap();
    encoded["obsolete_field"] = serde_json::json!(true);
    assert!(serde_json::from_value::<ReplayBundle>(encoded).is_err());
    bundle.format_version = 1;
    let mut destination = AgentApp::new(full_history_app).unwrap();
    assert!(
        destination
            .load_replay_bundle(bundle)
            .unwrap_err()
            .to_string()
            .contains("unsupported replay bundle format")
    );
    assert!(!destination.has_reset());
}

#[test]
fn episode_ownership_matches_snapshots_across_reset_recording_and_import() {
    let mut source = AgentApp::new(frequent_checkpoint_app).unwrap();
    source.reset(ResetOptions::default()).unwrap();
    let old_snapshot = source.replay_log().unwrap().initial_snapshot.unwrap();
    source.reset(ResetOptions::default()).unwrap();
    let episode = source.replay_log().unwrap().manifest.episode_id;
    assert_eq!(
        source.world().resource::<SnapshotStore>().episode_id(),
        episode
    );
    assert_eq!(
        source
            .world()
            .resource::<SnapshotStore>()
            .get(source.replay_log().unwrap().initial_snapshot.unwrap())
            .unwrap()
            .manifest
            .episode_id,
        episode
    );
    source
        .world_mut()
        .resource_mut::<AgentControlState>()
        .last_snapshot_created = Some(old_snapshot);
    source.sync_auto_checkpoints().unwrap();
    assert!(!collect_replay_references(source.replay_log().unwrap()).contains(&old_snapshot));
    source.start_recording(None).unwrap();
    assert_eq!(source.replay_log().unwrap().manifest.episode_id, episode);
    source.step(AgentAction::Noop).unwrap();
    let bundle = source.export_replay_bundle().unwrap();
    let mut destination = AgentApp::new(frequent_checkpoint_app).unwrap();
    destination.load_replay_bundle(bundle).unwrap();
    assert_eq!(
        destination.world().resource::<SnapshotStore>().episode_id(),
        episode
    );
    destination.step(AgentAction::Noop).unwrap();
    let bundle = destination.export_replay_bundle().unwrap();
    assert!(
        bundle
            .snapshots
            .iter()
            .all(|snapshot| snapshot.manifest.episode_id == episode)
    );
    assert!(
        bundle
            .log
            .branch_checkpoints
            .iter()
            .all(|checkpoint| checkpoint.episode == episode)
    );
}

#[test]
fn replay_preflight_rejects_cross_episode_checkpoints_and_snapshots() {
    let mut source = AgentApp::new(full_history_app).unwrap();
    source.reset(ResetOptions::default()).unwrap();
    let mut checkpoint_bundle = source.export_replay_bundle().unwrap();
    checkpoint_bundle.log.branch_checkpoints[0].episode += 1;
    let mut snapshot_bundle = source.export_replay_bundle().unwrap();
    snapshot_bundle.snapshots[0].manifest.episode_id += 1;
    snapshot_bundle.snapshots[0].checksum =
        checksum_snapshot(&snapshot_bundle.snapshots[0]).unwrap();
    let mut destination = AgentApp::new(full_history_app).unwrap();
    for bundle in [checkpoint_bundle, snapshot_bundle] {
        assert!(
            destination
                .load_replay_bundle(bundle)
                .unwrap_err()
                .to_string()
                .contains("episode")
        );
        assert!(!destination.has_reset());
        assert!(destination.world().resource::<SnapshotStore>().is_empty());
    }
}

#[test]
fn stepping_before_a_branch_fork_requires_a_new_branch() {
    let mut env = AgentApp::new(full_history_app).unwrap();
    env.reset(ResetOptions::default()).unwrap();
    for _ in 0..3 {
        env.step(AgentAction::Noop).unwrap();
    }
    let old_child = env.branch(3, None).unwrap();
    env.restore_tick(1).unwrap();
    let before = serde_json::to_value(env.replay_log()).unwrap();
    assert!(
        env.step(AgentAction::Noop)
            .unwrap_err()
            .to_string()
            .contains("create a branch")
    );
    assert_eq!(env.current_tick(), 1);
    assert_eq!(serde_json::to_value(env.replay_log()).unwrap(), before);
    let new_child = env.branch(1, None).unwrap();
    assert_ne!(new_child, old_child);
    env.step(AgentAction::Noop).unwrap();
    env.export_replay_bundle().unwrap();
    assert!(
        env.world()
            .resource::<Timeline>()
            .branches()
            .contains_key(&old_child)
    );
}

#[test]
fn destructive_parent_divergence_cannot_erase_descendant_history() {
    let mut env = AgentApp::new(full_history_app).unwrap();
    env.reset(ResetOptions::default()).unwrap();
    let parent = env.world().resource::<AgentControlState>().branch_id;
    for _ in 0..3 {
        env.step(AgentAction::Noop).unwrap();
    }
    let child = env.branch(3, None).unwrap();
    env.world_mut()
        .resource_mut::<AgentControlState>()
        .branch_id = parent;
    env.world_mut()
        .resource_mut::<Timeline>()
        .select_branch(parent)
        .unwrap();
    env.restore_tick(1).unwrap();
    let before = serde_json::to_value(env.replay_log()).unwrap();
    assert!(
        env.step(AgentAction::Jump)
            .unwrap_err()
            .to_string()
            .contains("shared history")
    );
    assert_eq!(env.current_tick(), 1);
    assert_eq!(serde_json::to_value(env.replay_log()).unwrap(), before);
    env.world_mut()
        .resource_mut::<AgentControlState>()
        .branch_id = child;
    env.world_mut()
        .resource_mut::<Timeline>()
        .select_branch(child)
        .unwrap();
    env.restore_tick(2).unwrap();
    assert_eq!(
        env.world().resource::<CurrentInputFrame>().actions,
        vec![AgentAction::Noop]
    );
}

#[test]
fn construction_rejects_incomplete_integration() {
    assert!(AgentApp::from_app(App::new()).is_err());
    let mut app = core_only_app();
    app.world_mut()
        .remove_resource::<bevy_agent_core::AgentChecksumExtractor>();
    assert!(AgentApp::from_app(app).is_err());
    let mut app = core_only_app();
    app.world_mut()
        .resource_mut::<AgentActionCatalog>()
        .set_supported_actions([]);
    assert!(AgentApp::from_app(app).is_err());
    let mut app = core_only_app();
    app.world_mut().resource_mut::<ObservationConfig>().mode = ObservationMode::PixelFrame;
    assert!(AgentApp::from_app(app).is_err());
    let mut app = core_only_app();
    app.world_mut()
        .resource_mut::<bevy::ecs::schedule::Schedules>()
        .remove(AgentTick);
    assert!(AgentApp::from_running_app(app, false).is_err());
}

#[test]
fn scheduled_input_validation_is_atomic_and_does_not_initialize() {
    let mut env = AgentApp::new(full_history_app).unwrap();
    env.enqueue_action_at(5, ActionSource::Test, AgentAction::Jump)
        .unwrap();
    let before = serde_json::to_value(env.world().resource::<AgentActionQueue>()).unwrap();
    for (tick, source, action) in [
        (0, ActionSource::Agent, AgentAction::Noop),
        (u64::MAX, ActionSource::Agent, AgentAction::Noop),
        (2, ActionSource::Human, AgentAction::Noop),
        (2, ActionSource::Agent, AgentAction::Attack { target: None }),
        (
            2,
            ActionSource::Agent,
            AgentAction::Move {
                x: f32::NAN,
                y: 0.0,
            },
        ),
    ] {
        assert!(env.enqueue_action_at(tick, source, action).is_err());
        assert_eq!(
            serde_json::to_value(env.world().resource::<AgentActionQueue>()).unwrap(),
            before
        );
        assert!(!env.has_reset());
    }
}

#[derive(Resource, Clone, Default)]
struct ForkFailureCounter {
    tick: u64,
    fail_rollback: bool,
}

impl SnapshotType for ForkFailureCounter {
    const TYPE_ID: &'static str = "runner_test.fork_failure_counter";
    const SCHEMA_VERSION: u32 = 1;
}

impl Serialize for ForkFailureCounter {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        if self.tick == 1 {
            return Err(serde::ser::Error::custom(
                "injected fork serializer failure",
            ));
        }
        (self.tick, self.fail_rollback).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ForkFailureCounter {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let (tick, fail_rollback) = <(u64, bool)>::deserialize(deserializer)?;
        if tick == 3 && fail_rollback {
            return Err(serde::de::Error::custom(
                "injected rollback deserializer failure",
            ));
        }
        Ok(Self {
            tick,
            fail_rollback,
        })
    }
}

fn fork_failure_app() -> App {
    use bevy_agent_snapshot::SnapshotAppExt;
    let mut app = full_history_app();
    app.world_mut()
        .resource_mut::<SnapshotPolicy>()
        .checkpoint_every_ticks = 0;
    app.init_resource::<ForkFailureCounter>()
        .register_required_snapshot_resource::<ForkFailureCounter>()
        .unwrap()
        .add_systems(
            AgentPostTick,
            |clock: Res<SimClock>, mut counter: ResMut<ForkFailureCounter>| {
                counter.tick = clock.tick;
            },
        );
    app
}

fn history_fingerprint(env: &mut AgentApp) -> serde_json::Value {
    let state = capture_snapshot(env.world_mut(), None).unwrap();
    let store = env.world().resource::<SnapshotStore>();
    let snapshots = store
        .iter()
        .map(|(id, snapshot)| (*id, serde_json::to_value(snapshot).unwrap()))
        .collect::<BTreeMap<_, _>>();
    let timeline = env.world().resource::<Timeline>();
    let branches = timeline
        .branches()
        .iter()
        .map(|(id, branch)| (*id, branch))
        .collect::<BTreeMap<_, _>>();
    serde_json::json!({
        "state": state.checksum,
        "control": env.world().resource::<AgentControlState>(),
        "response": env.world().resource::<LastStepResponse>(),
        "failure": env.world().resource::<bevy_agent_core::AgentTickFailure>().error().map(ToString::to_string),
        "queue": env.world().resource::<AgentActionQueue>(),
        "log": env.replay_log(),
        "recording": env.world().resource::<ReplayRecorder>().is_recording(),
        "timeline_id": timeline.timeline_id(),
        "current_branch": timeline.current_branch(),
        "branches": branches,
        "snapshots": snapshots,
        "checkpoints": store.checkpoints(),
        "pins": store.pinned(),
        "episode": store.episode_id(),
        "reset_once": env.has_reset(),
    })
}

#[test]
fn failed_fork_capture_rolls_back_rewind_and_all_history_owners() {
    let mut env = AgentApp::new(fork_failure_app).unwrap();
    env.reset(ResetOptions::default()).unwrap();
    for _ in 0..3 {
        env.step(AgentAction::Noop).unwrap();
    }
    env.enqueue_action_at(7, ActionSource::Test, AgentAction::Jump)
        .unwrap();
    let before = history_fingerprint(&mut env);
    let error = env.branch(1, Some("rejected".into())).unwrap_err();
    assert!(error.to_string().contains("fork serializer failure"));
    assert!(error.to_string().contains("rolled back"));
    assert_eq!(history_fingerprint(&mut env), before);
    assert!(!env.world().contains_resource::<FaultState>());
    env.step(AgentAction::Noop).unwrap();
}

#[test]
fn unrestorable_backup_rejects_fork_before_mutation() {
    let mut env = AgentApp::new(fork_failure_app).unwrap();
    env.reset(ResetOptions::default()).unwrap();
    for _ in 0..3 {
        env.step(AgentAction::Noop).unwrap();
    }
    env.world_mut()
        .resource_mut::<ForkFailureCounter>()
        .fail_rollback = true;
    let log_before = serde_json::to_value(env.replay_log()).unwrap();
    let branches_before = env.world().resource::<Timeline>().branches().len();
    let store_before = env.world().resource::<SnapshotStore>().len();
    let error = env.branch(1, None).unwrap_err();
    assert!(
        format!("{error:#}").contains("injected rollback deserializer failure"),
        "{error:#}"
    );
    assert!(!env.world().contains_resource::<FaultState>());
    assert_eq!(env.current_tick(), 3);
    assert_eq!(serde_json::to_value(env.replay_log()).unwrap(), log_before);
    assert_eq!(
        env.world().resource::<Timeline>().branches().len(),
        branches_before
    );
    assert_eq!(env.world().resource::<SnapshotStore>().len(), store_before);
    env.world_mut()
        .resource_mut::<ForkFailureCounter>()
        .fail_rollback = false;
    env.step(AgentAction::Noop).unwrap();
}

#[test]
fn generated_history_sequences_preserve_state_and_failure_atomicity() {
    for seed in 0u64..24 {
        let mut random = seed + 1;
        let mut env = AgentApp::new(post_tick_app).unwrap();
        env.reset(ResetOptions::default()).unwrap();
        for index in 0..40 {
            random = random
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            match random % 6 {
                0 | 1 => {
                    if env.step(AgentAction::Noop).is_err() {
                        env.branch(env.current_tick(), Some(format!("seed-{seed}-op-{index}")))
                            .unwrap();
                        env.step(AgentAction::Noop).unwrap();
                    }
                }
                2 => {
                    let target = (random >> 16) % (env.current_tick() + 1);
                    env.restore_tick(target).unwrap();
                }
                3 => {
                    let target = (random >> 16) % (env.current_tick() + 1);
                    env.branch(target, None).unwrap();
                }
                4 => {
                    let bundle = env.export_replay_bundle().unwrap();
                    let tick = env.current_tick();
                    let mut destination = AgentApp::new(post_tick_app).unwrap();
                    destination.load_replay_bundle(bundle).unwrap();
                    assert_eq!(destination.current_tick(), tick, "seed {seed}, op {index}");
                    env = destination;
                }
                _ => {
                    let before = history_fingerprint(&mut env);
                    assert!(
                        env.step_many(vec![
                            AgentAction::Noop,
                            AgentAction::Move { x: 2.0, y: 0.0 }
                        ])
                        .is_err()
                    );
                    assert!(env.restore_tick(u64::MAX).is_err());
                    let mut bundle = env.export_replay_bundle().unwrap();
                    bundle.log.active_branch = Some(BranchId::new());
                    assert!(env.load_replay_bundle(bundle).is_err());
                    assert_eq!(
                        history_fingerprint(&mut env),
                        before,
                        "seed {seed}, op {index}"
                    );
                }
            }
            assert_eq!(
                env.world().resource::<PostTickCounter>().0,
                env.current_tick(),
                "seed {seed}, op {index}"
            );
            env.world().resource::<Timeline>().validate().unwrap();
            env.replay_log().unwrap().validate_history().unwrap();
            let tick = env.current_tick();
            let response = env
                .world()
                .resource::<LastStepResponse>()
                .0
                .as_ref()
                .unwrap();
            assert_eq!(response.tick, tick);
            assert_eq!(response.info.frame, tick);
            let control = env.world().resource::<AgentControlState>();
            assert_eq!(response.info.branch_id, control.branch_id);
            assert_eq!(response.info.timeline_id, control.timeline_id);
            env.export_replay_bundle().unwrap();
        }
    }
}

#[test]
fn observation_failure_during_branch_reconstruction_restores_cached_outcome() {
    let mut env = AgentApp::new(post_tick_app).unwrap();
    env.reset(ResetOptions::default()).unwrap();
    for _ in 0..3 {
        env.step(AgentAction::Noop).unwrap();
    }
    env.app_mut()
        .set_observation_schema(serde_json::json!({
            "type": "object", "properties": {"counter": {"not": {"const": 1}}}
        }))
        .unwrap();
    let before = history_fingerprint(&mut env);
    let error = env.branch(1, None).unwrap_err();
    assert!(error.to_string().contains("observation"));
    assert_eq!(history_fingerprint(&mut env), before);
    assert!(
        env.world()
            .resource::<bevy_agent_core::AgentTickFailure>()
            .error()
            .is_none()
    );
    env.step(AgentAction::Noop).unwrap();
}

#[test]
fn implicit_reset_inherits_configured_game_observation_mode() {
    let mut app = core_only_app();
    app.set_supported_observation_modes([ObservationMode::PlayerKnowledge]);
    app.world_mut().resource_mut::<ObservationConfig>().mode = ObservationMode::PlayerKnowledge;
    let mut env = AgentApp::from_app(app).unwrap();
    env.observe(ObservationMode::PlayerKnowledge).unwrap();
    assert!(env.has_reset());
    assert_eq!(
        env.world().resource::<ObservationConfig>().mode,
        ObservationMode::PlayerKnowledge
    );
    env.step(AgentAction::Noop).unwrap();
}

#[test]
fn batch_response_start_tick_is_measured_after_implicit_reset() {
    let mut app = core_only_app();
    app.world_mut().resource_mut::<SimClock>().tick = 3;
    let mut env = AgentApp::from_app(app).unwrap();
    let response = env
        .step_many_with_response(vec![AgentAction::Noop], false)
        .unwrap();
    assert_eq!(response.start_tick, 0);
    assert_eq!(response.end_tick, 1);
    assert_eq!(response.steps, 1);
}

#[test]
fn recording_rejects_explicit_baseline_without_snapshot_support_before_reset() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(AgentControlPlugins::default().without_snapshots());
    configure_test_integration(&mut app);
    let mut env = AgentApp::from_app(app).unwrap();
    let before = serde_json::to_value(env.replay_log()).unwrap();
    let branch = env.world().resource::<Timeline>().current_branch();
    assert!(
        env.start_recording(Some(SnapshotId::new()))
            .unwrap_err()
            .to_string()
            .contains("SnapshotPlugin")
    );
    assert!(!env.has_reset());
    assert_eq!(serde_json::to_value(env.replay_log()).unwrap(), before);
    assert_eq!(env.world().resource::<Timeline>().current_branch(), branch);
    env.start_recording(None).unwrap();
}

#[test]
fn explicit_recording_baseline_requires_current_tick_episode_and_state() {
    let mut env = AgentApp::new(full_history_app).unwrap();
    env.reset(ResetOptions::default()).unwrap();
    let baseline = env.replay_log().unwrap().initial_snapshot.unwrap();
    env.world_mut().resource_mut::<RewardState>().current_reward = 2.0;
    let before = history_fingerprint(&mut env);
    assert!(
        env.start_recording(Some(baseline))
            .unwrap_err()
            .to_string()
            .contains("live state")
    );
    assert_eq!(history_fingerprint(&mut env), before);
    env.reset(ResetOptions::default()).unwrap();
    let before = history_fingerprint(&mut env);
    assert!(
        env.start_recording(Some(baseline))
            .unwrap_err()
            .to_string()
            .contains("not found")
    );
    assert_eq!(history_fingerprint(&mut env), before);
    let matching = env.replay_log().unwrap().initial_snapshot.unwrap();
    env.start_recording(Some(matching)).unwrap();
    env.step(AgentAction::Noop).unwrap();
    let before = history_fingerprint(&mut env);
    assert!(
        env.start_recording(Some(matching))
            .unwrap_err()
            .to_string()
            .contains("tick")
    );
    assert_eq!(history_fingerprint(&mut env), before);
}

#[test]
fn direct_restore_observation_failure_restores_original_world_and_response() {
    let mut env = AgentApp::new(post_tick_app).unwrap();
    env.reset(ResetOptions::default()).unwrap();
    env.step(AgentAction::Noop).unwrap();
    let rejected = env.snapshot().unwrap().snapshot_id;
    env.step(AgentAction::Noop).unwrap();
    env.step(AgentAction::Noop).unwrap();
    env.app_mut()
        .set_observation_schema(serde_json::json!({
            "type": "object", "properties": {"counter": {"not": {"const": 1}}}
        }))
        .unwrap();
    let before = history_fingerprint(&mut env);
    assert!(
        env.restore(rejected)
            .unwrap_err()
            .to_string()
            .contains("observation")
    );
    assert_eq!(history_fingerprint(&mut env), before);
    env.step(AgentAction::Noop).unwrap();
}

#[test]
fn inherited_checksum_mismatch_is_verified_and_rolled_back() {
    let mut env = AgentApp::new(full_history_app).unwrap();
    env.reset(ResetOptions::default()).unwrap();
    let parent = env.world().resource::<Timeline>().current_branch();
    for _ in 0..3 {
        env.step(AgentAction::Noop).unwrap();
    }
    env.branch(3, None).unwrap();
    let mut log = env.replay_log().unwrap().clone();
    log.branch_checksums
        .entry(parent)
        .or_default()
        .insert(2, SnapshotChecksum { tick: 2, hash: 0 });
    env.world_mut()
        .resource_mut::<ReplayRecorder>()
        .replace_log(log)
        .unwrap();
    let before = history_fingerprint(&mut env);
    assert!(
        env.restore_tick(2)
            .unwrap_err()
            .to_string()
            .contains("checksum mismatch")
    );
    assert_eq!(history_fingerprint(&mut env), before);
}

#[test]
fn inherited_checkpoint_checksum_is_verified_before_mutation() {
    let mut env = AgentApp::new(frequent_checkpoint_app).unwrap();
    env.reset(ResetOptions::default()).unwrap();
    let parent = env.world().resource::<Timeline>().current_branch();
    for _ in 0..3 {
        env.step(AgentAction::Noop).unwrap();
    }
    env.branch(3, None).unwrap();
    let mut log = env.replay_log().unwrap().clone();
    log.branch_checksums
        .get_mut(&parent)
        .unwrap()
        .get_mut(&2)
        .unwrap()
        .hash ^= 1;
    env.world_mut()
        .resource_mut::<ReplayRecorder>()
        .replace_log(log)
        .unwrap();
    let before = history_fingerprint(&mut env);
    assert!(
        env.restore_tick(2)
            .unwrap_err()
            .to_string()
            .contains("checkpoint tick 2 checksum mismatch")
    );
    assert_eq!(history_fingerprint(&mut env), before);
}

#[test]
fn repeated_resets_release_automatic_history_but_keep_manual_pins() {
    let mut env = AgentApp::new(full_history_app).unwrap();
    env.reset(ResetOptions::default()).unwrap();
    let manual = env.snapshot().unwrap().snapshot_id;
    bevy_agent_snapshot::pin_snapshot(env.world_mut(), manual).unwrap();
    for _ in 0..25 {
        env.step(AgentAction::Noop).unwrap();
        env.branch(0, None).unwrap();
        env.reset(ResetOptions::default()).unwrap();
        let store = env.world().resource::<SnapshotStore>();
        assert_eq!(store.len(), 2);
        assert_eq!(store.pinned().len(), 1);
        assert_eq!(store.protected().len(), 1);
        assert!(store.get(manual).is_some());
        assert!(
            store.retained_bytes() <= env.world().resource::<SnapshotPolicy>().max_snapshot_bytes
        );
    }
}

#[test]
fn periodic_budget_failure_reports_committed_tick_and_requires_reset() {
    let mut env = AgentApp::new(full_history_app).unwrap();
    env.reset(ResetOptions::default()).unwrap();
    let bytes = env.world().resource::<SnapshotStore>().retained_bytes();
    {
        let mut policy = env.world_mut().resource_mut::<SnapshotPolicy>();
        policy.max_snapshot_bytes = bytes + 1;
        policy.checkpoint_every_ticks = 1;
    }
    let error = env.step(AgentAction::Noop).unwrap_err();
    let failure = error.downcast_ref::<MutationFailure>().unwrap();
    assert_eq!((failure.tick_before, failure.tick_after), (0, 1));
    assert!(failure.tick_committed && failure.recovery_required);
    assert!(env.world().resource::<LastStepResponse>().0.is_none());
    assert!(env.observe(ObservationMode::Hybrid).is_err());
    assert!(env.step(AgentAction::Noop).is_err());
    assert!(env.export_replay_bundle().is_err());
    env.reset(ResetOptions::default()).unwrap();
    assert!(!env.world().contains_resource::<FaultState>());
    assert!(env.has_reset());
}

#[test]
fn replay_budget_failure_does_not_append_an_unrecorded_committed_tick() {
    let mut env = AgentApp::new(full_history_app).unwrap();
    env.reset(ResetOptions::default()).unwrap();
    let bytes = env.world().resource::<ReplayRecorder>().retained_bytes();
    env.world_mut()
        .resource_mut::<ReplayRecorder>()
        .set_max_history_bytes(bytes + 1)
        .unwrap();
    let error = env.step(AgentAction::Noop).unwrap_err();
    let failure = error.downcast_ref::<MutationFailure>().unwrap();
    assert!(failure.tick_committed && failure.recovery_required);
    assert!(failure.message.contains("budget"));
    assert!(env.replay_log().unwrap().records.is_empty());
    assert!(env.replay_log().unwrap().completed_ticks.is_empty());
    env.reset(ResetOptions::default()).unwrap();
    assert!(!env.world().contains_resource::<FaultState>());
}

#[test]
fn reset_failure_stays_faulted_until_all_reset_work_succeeds() {
    let mut env = AgentApp::new(full_history_app).unwrap();
    env.world_mut()
        .resource_mut::<SnapshotPolicy>()
        .max_snapshot_bytes = 1;
    let error = env.reset(ResetOptions::default()).unwrap_err();
    let failure = error.downcast_ref::<MutationFailure>().unwrap();
    assert_eq!(failure.operation, "reset");
    assert!(failure.recovery_required);
    assert!(!env.has_reset());
    assert!(env.world().contains_resource::<FaultState>());
    env.world_mut()
        .resource_mut::<SnapshotPolicy>()
        .max_snapshot_bytes = 64 * 1024 * 1024;
    env.reset(ResetOptions::default()).unwrap();
    assert!(env.has_reset());
    assert!(!env.world().contains_resource::<FaultState>());
}

#[test]
fn new_recording_releases_a_full_store_and_keeps_baseline_restorable() {
    let mut env = AgentApp::new(full_history_app).unwrap();
    env.reset(ResetOptions::default()).unwrap();
    for _ in 0..5 {
        env.step(AgentAction::Noop).unwrap();
        env.snapshot().unwrap();
    }
    let bytes = env.world().resource::<SnapshotStore>().retained_bytes();
    env.world_mut()
        .resource_mut::<SnapshotPolicy>()
        .max_snapshot_bytes = bytes;
    env.start_recording(None).unwrap();
    assert_eq!(env.world().resource::<SnapshotStore>().len(), 1);
    let baseline = env.replay_log().unwrap().initial_snapshot.unwrap();
    assert!(
        env.world()
            .resource::<SnapshotStore>()
            .get(baseline)
            .is_some()
    );
    env.restore_tick(5).unwrap();
    env.validate_replay_bundle(&env.export_replay_bundle().unwrap())
        .unwrap();
}

#[test]
fn failed_batch_reports_its_committed_prefix_and_recovery_state() {
    let mut env = AgentApp::new(full_history_app).unwrap();
    env.app.insert_observation_extractor(|world, _| {
        let tick = world.resource::<SimClock>().tick;
        if tick == 2 {
            world.resource_mut::<RewardState>().current_reward = f32::NAN;
        }
        Observation::default_for_tick(tick)
    });
    let error = env
        .step_many_with_response(vec![AgentAction::Noop; 3], true)
        .unwrap_err();
    let failure = error.downcast_ref::<MutationFailure>().unwrap();
    assert_eq!(failure.operation, "step_many");
    assert_eq!(failure.completed_steps, 1);
    assert_eq!((failure.tick_before, failure.tick_after), (0, 2));
    assert!(failure.tick_committed && failure.recovery_required);
}

#[test]
fn exhausted_checkpoint_budget_rejects_without_creating_a_payload() {
    let mut env = AgentApp::new(full_history_app).unwrap();
    env.reset(ResetOptions::default()).unwrap();
    env.step(AgentAction::Noop).unwrap();
    let bytes = env.world().resource::<ReplayRecorder>().retained_bytes();
    env.world_mut()
        .resource_mut::<ReplayRecorder>()
        .set_max_history_bytes(bytes)
        .unwrap();
    let before = history_fingerprint(&mut env);
    assert!(env.snapshot().unwrap_err().to_string().contains("budget"));
    assert_eq!(history_fingerprint(&mut env), before);
    assert!(!env.world().contains_resource::<FaultState>());
}

#[test]
fn exhausted_fork_budget_rolls_back_only_changed_metadata_and_shared_payloads() {
    let mut env = AgentApp::new(full_history_app).unwrap();
    env.reset(ResetOptions::default()).unwrap();
    env.step(AgentAction::Noop).unwrap();
    let bytes = env.world().resource::<ReplayRecorder>().retained_bytes();
    env.world_mut()
        .resource_mut::<ReplayRecorder>()
        .set_max_history_bytes(bytes)
        .unwrap();
    let before = history_fingerprint(&mut env);
    let initial = env.replay_log().unwrap().initial_snapshot.unwrap();
    let payload = env
        .world()
        .resource::<SnapshotStore>()
        .shared(initial)
        .unwrap();
    assert!(
        env.branch(0, Some("new".into()))
            .unwrap_err()
            .to_string()
            .contains("budget")
    );
    assert_eq!(history_fingerprint(&mut env), before);
    assert!(std::sync::Arc::ptr_eq(
        &payload,
        &env.world()
            .resource::<SnapshotStore>()
            .shared(initial)
            .unwrap()
    ));
    assert!(!env.world().contains_resource::<FaultState>());
}

#[test]
fn import_budget_failure_restores_moved_owners_and_their_active_history() {
    let mut source = AgentApp::new(full_history_app).unwrap();
    source.reset(ResetOptions::default()).unwrap();
    for _ in 0..8 {
        source.step(AgentAction::Noop).unwrap();
    }
    let bundle = source.export_replay_bundle().unwrap();
    let mut destination = AgentApp::new(full_history_app).unwrap();
    destination.reset(ResetOptions::default()).unwrap();
    let bytes = destination
        .world()
        .resource::<ReplayRecorder>()
        .retained_bytes();
    destination
        .world_mut()
        .resource_mut::<ReplayRecorder>()
        .set_max_history_bytes(bytes)
        .unwrap();
    let before = history_fingerprint(&mut destination);
    let initial = destination.replay_log().unwrap().initial_snapshot.unwrap();
    let payload = destination
        .world()
        .resource::<SnapshotStore>()
        .shared(initial)
        .unwrap();
    assert!(
        destination
            .load_replay_bundle(bundle)
            .unwrap_err()
            .to_string()
            .contains("budget")
    );
    assert_eq!(history_fingerprint(&mut destination), before);
    assert!(std::sync::Arc::ptr_eq(
        &payload,
        &destination
            .world()
            .resource::<SnapshotStore>()
            .shared(initial)
            .unwrap()
    ));
    assert!(!destination.world().contains_resource::<FaultState>());
}
