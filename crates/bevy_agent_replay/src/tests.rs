use super::*;
use bevy::prelude::*;
use bevy_agent_core::{ActionSource, AgentAction, BranchId, SnapshotId};

fn record(tick: u64, action: AgentAction) -> ActionRecord {
    ActionRecord {
        tick,
        branch_id: BranchId::default(),
        source: ActionSource::Agent,
        action,
    }
}

fn record_on(branch_id: BranchId, tick: u64, action: AgentAction) -> ActionRecord {
    ActionRecord {
        tick,
        branch_id,
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
fn timeline_create_branch_tracks_parent_and_switches_current_branch() {
    let mut timeline = Timeline::default();
    let parent = timeline.current_branch;
    let snapshot = SnapshotId::new();

    let child = timeline
        .create_branch(42, Some(snapshot), Some("try-alt".to_string()))
        .unwrap();

    assert_ne!(child, parent);
    assert_eq!(timeline.current_branch, child);
    let branch = timeline.branches.get(&child).unwrap();
    assert_eq!(branch.parent_branch, Some(parent));
    assert_eq!(branch.fork_tick, 42);
    assert_eq!(branch.fork_snapshot, Some(snapshot));
    assert_eq!(branch.label.as_deref(), Some("try-alt"));
}

#[test]
fn start_and_stop_recording_resets_and_returns_count() {
    let mut world = World::new();
    world.insert_resource(ReplayRecorder::default());
    let snapshot = SnapshotId::new();

    start_recording(&mut world, Some(snapshot)).unwrap();
    {
        let mut recorder = world.resource_mut::<ReplayRecorder>();
        recorder.log.records.push(record(1, AgentAction::Jump));
    }
    let count = stop_recording(&mut world).unwrap();
    let log = &world.resource::<ReplayRecorder>().log;
    assert_eq!(count, 1);
    assert!(!world.resource::<ReplayRecorder>().recording);
    assert_eq!(log.initial_snapshot, Some(snapshot));
    assert_eq!(log.records.len(), 1);
}

#[test]
fn actions_for_branch_includes_ancestors_but_not_siblings() {
    let mut timeline = Timeline::default();
    let root = timeline.current_branch;
    let child = timeline.create_branch(5, None, None).unwrap();
    // Sibling forked off root while keeping `child` addressable.
    timeline.current_branch = root;
    let sibling = timeline.create_branch(5, None, None).unwrap();
    timeline.current_branch = child;

    let log = ReplayLog {
        records: vec![
            record_on(root, 3, AgentAction::Noop),
            record_on(child, 7, AgentAction::Jump),
            record_on(sibling, 7, AgentAction::Interact),
        ],
        ..Default::default()
    };

    let child_actions = log.actions_for_branch(&timeline, child, 0, 10);
    assert_eq!(child_actions.len(), 2);
    assert!(child_actions.iter().any(|r| r.tick == 3));
    assert!(child_actions.iter().any(|r| r.action == AgentAction::Jump));

    let sibling_actions = log.actions_for_branch(&timeline, sibling, 0, 10);
    assert_eq!(sibling_actions.len(), 2);
    assert!(
        sibling_actions
            .iter()
            .any(|r| r.action == AgentAction::Interact)
    );
}

#[test]
fn checkpoint_selection_excludes_parent_future_beyond_fork() {
    let mut timeline = Timeline::default();
    let root = timeline.current_branch;
    let child = timeline.create_branch(5, None, None).unwrap();

    let parent_early = SnapshotId::new();
    let parent_late = SnapshotId::new();
    let mut log = ReplayLog::default();
    log.push_checkpoint(root, 3, parent_early);
    log.push_checkpoint(root, 8, parent_late);

    // Child restoring at tick 6 must see the tick-3 ancestor checkpoint,
    // not the tick-8 parent-future checkpoint recorded after the fork.
    assert_eq!(
        log.nearest_checkpoint_for_branch(&timeline, child, 6),
        Some((3, parent_early))
    );
    // Root itself still sees its own tick-8 checkpoint.
    assert_eq!(
        log.nearest_checkpoint_for_branch(&timeline, root, 9),
        Some((8, parent_late))
    );
}

#[test]
fn same_tick_checkpoints_are_isolated_per_branch() {
    let mut timeline = Timeline::default();
    let root = timeline.current_branch;
    let child = timeline.create_branch(5, None, None).unwrap();

    let parent_snap = SnapshotId::new();
    let child_snap = SnapshotId::new();
    let mut log = ReplayLog::default();
    log.push_checkpoint(root, 5, parent_snap);
    log.push_checkpoint(child, 5, child_snap);

    assert_eq!(
        log.nearest_checkpoint_for_branch(&timeline, child, 5),
        Some((5, child_snap))
    );
    assert_eq!(
        log.nearest_checkpoint_for_branch(&timeline, root, 5),
        Some((5, parent_snap))
    );
}

#[test]
fn parent_future_excluded_via_fork_bounded_intervals() {
    // Parent tick2 MoveRight must not leak into a child forked at tick1.
    let mut timeline = Timeline::default();
    let root = timeline.current_branch;
    let child = timeline.create_branch(1, None, None).unwrap();
    let move_right = AgentAction::Move { x: 1.0, y: 0.0 };
    let log = ReplayLog {
        records: vec![
            record_on(root, 1, AgentAction::Noop),
            record_on(root, 2, move_right.clone()),
            record_on(child, 2, AgentAction::Noop),
        ],
        ..Default::default()
    };
    let child_actions = log.actions_for_branch(&timeline, child, 0, 2);
    // Shared tick1 + child-only tick2; parent tick2 excluded.
    assert_eq!(child_actions.len(), 2);
    assert!(child_actions.iter().any(|r| r.tick == 1));
    let tick2: Vec<_> = child_actions.iter().filter(|r| r.tick == 2).collect();
    assert_eq!(tick2.len(), 1);
    assert_eq!(tick2[0].action, AgentAction::Noop);
    assert_eq!(tick2[0].branch_id, child);
}

#[test]
fn truncate_future_diverges_same_branch_only() {
    let mut timeline = Timeline::default();
    let root = timeline.current_branch;
    let child = timeline.create_branch(5, None, None).unwrap();

    let mut log = ReplayLog {
        records: vec![
            record_on(root, 6, AgentAction::Noop),
            record_on(root, 9, AgentAction::Jump),
            record_on(child, 9, AgentAction::Interact),
        ],
        ..Default::default()
    };
    let doomed = SnapshotId::new();
    let kept = SnapshotId::new();
    log.push_checkpoint(root, 9, doomed);
    log.push_checkpoint(child, 9, kept);

    log.truncate_future(root, 6);

    assert!(
        log.records
            .iter()
            .all(|r| r.branch_id != root || r.tick <= 6)
    );
    assert!(log.records.iter().any(|r| r.branch_id == child));
    assert!(
        log.branch_checkpoints
            .iter()
            .all(|c| c.branch_id != root || c.tick <= 6)
    );
    assert!(log.branch_checkpoints.iter().any(|c| c.snapshot_id == kept));
    assert!(
        !log.branch_checkpoints
            .iter()
            .any(|c| c.snapshot_id == doomed)
    );
}

#[test]
fn nonzero_recording_baseline_survives_export_and_checkpoint_lookup() {
    let mut world = World::new();
    let mut clock = bevy_agent_core::SimClock::new(60);
    clock.tick = 30;
    world.insert_resource(clock);
    let baseline = SnapshotId::new();
    start_recording(&mut world, Some(baseline)).unwrap();
    let timeline = world.resource::<Timeline>().clone();
    let root = timeline.current_branch;
    let mut log = world.resource::<ReplayRecorder>().log.clone();
    log.records.push(record_on(root, 31, AgentAction::Jump));
    log.record_tick(root, 34);
    log.sync_topology(&timeline, 34);
    assert_eq!(log.initial_tick, 30);
    assert_eq!(log.recorded_range(&timeline, root), (30, 34));
    assert_eq!(log.nearest_checkpoint_for_branch(&timeline, root, 29), None);
    assert_eq!(
        log.nearest_checkpoint_for_branch(&timeline, root, 30),
        Some((30, baseline))
    );
    assert_eq!(
        log.nearest_checkpoint_for_branch(&timeline, root, 33),
        Some((30, baseline))
    );
}

#[test]
fn branch_bounds_include_empty_frames_and_exclude_sibling_futures() {
    let mut timeline = Timeline::default();
    let root = timeline.current_branch;
    let child = timeline.create_branch(5, None, None).unwrap();
    timeline.current_branch = root;
    let sibling = timeline.create_branch(5, None, None).unwrap();
    let mut log = ReplayLog::default();
    log.record_tick(root, 4);
    log.record_tick(root, 90);
    log.record_tick(child, 6);
    log.record_tick(sibling, 200);
    assert_eq!(log.recorded_range(&timeline, child), (0, 6));
    assert_eq!(log.recorded_range(&timeline, root), (0, 90));
    assert_eq!(log.recorded_range(&timeline, sibling), (0, 200));
    assert_eq!(log.end_tick, 200);
}

#[test]
fn unknown_branches_are_never_visible() {
    let timeline = Timeline::default();
    let root = timeline.current_branch;
    let unknown = BranchId::new();
    let log = ReplayLog {
        initial_snapshot: Some(SnapshotId::new()),
        records: vec![
            record_on(root, 1, AgentAction::Noop),
            record_on(unknown, 2, AgentAction::Jump),
        ],
        ..Default::default()
    };
    assert_eq!(log.actions_for_branch(&timeline, root, 0, 10).len(), 1);
    assert!(log.actions_for_branch(&timeline, unknown, 0, 10).is_empty());
    assert_eq!(
        log.nearest_checkpoint_for_branch(&timeline, unknown, 10),
        None
    );
    assert_eq!(log.branch_end_tick(&timeline, unknown), 0);
}

#[test]
fn all_readers_terminate_on_cycles_and_missing_parents() {
    let mut timeline = Timeline::default();
    let root = timeline.current_branch;
    let child = timeline.create_branch(5, None, None).unwrap();
    timeline.branches.get_mut(&root).unwrap().parent_branch = Some(child);
    assert!(timeline.ancestors_bounded(child).is_err());
    assert!(timeline.lineage(child).is_empty());
    assert!(!lineage_contains(&timeline, root, child));
    assert_eq!(branch_fork_from_ancestor(&timeline, root, child), None);
    assert!(!branch_record_visible(&timeline, root, 2, child));
    assert!(!branch_record_visible(&timeline, child, 6, child));
    let mut log = ReplayLog::default();
    log.records.push(record_on(child, 6, AgentAction::Jump));
    assert!(log.actions_for_branch(&timeline, child, 0, 10).is_empty());
    assert!(timeline.validate().is_err());
    timeline.branches.get_mut(&root).unwrap().parent_branch = Some(BranchId::new());
    assert!(!branch_record_visible(&timeline, root, 2, child));
    assert!(timeline.validate().is_err());
}

#[test]
fn lineage_depth_bound_accepts_exact_limit_and_rejects_one_more() {
    let mut timeline = Timeline::default();
    for _ in 1..MAX_LINEAGE_DEPTH {
        timeline.create_branch(0, None, None).unwrap();
    }
    assert_eq!(
        timeline
            .ancestors_bounded(timeline.current_branch)
            .unwrap()
            .len(),
        MAX_LINEAGE_DEPTH
    );
    let before = serde_json::to_value(&timeline).unwrap();
    assert!(timeline.create_branch(0, None, None).is_err());
    assert_eq!(serde_json::to_value(&timeline).unwrap(), before);
}

#[test]
fn truncation_keeps_other_branch_checkpoints_checksums_and_empty_frames() {
    let mut timeline = Timeline::default();
    let root = timeline.current_branch;
    let child = timeline.create_branch(5, None, None).unwrap();
    let child_snapshot = SnapshotId::new();
    let doomed = SnapshotId::new();
    let mut log = ReplayLog::default();
    log.push_checkpoint(child, 10, child_snapshot);
    log.push_checkpoint(root, 10, doomed);
    let child_checksum = bevy_agent_core::SnapshotChecksum {
        tick: 10,
        hash: 123,
    };
    log.insert_branch_checksum(child, 10, child_checksum.clone());
    log.insert_branch_checksum(
        root,
        10,
        bevy_agent_core::SnapshotChecksum {
            tick: 10,
            hash: 456,
        },
    );
    log.record_tick(child, 10);
    log.record_tick(root, 20);
    log.truncate_future(root, 5);
    assert_eq!(log.expected_checksum(root, 10), None);
    assert_eq!(log.expected_checksum(child, 10), Some(&child_checksum));
    assert_eq!(log.end_tick, 10);
    assert_eq!(log.branch_end_tick(&timeline, child), 10);
    let referenced = collect_replay_references(&log);
    assert!(referenced.contains(&child_snapshot));
    assert!(!referenced.contains(&doomed));
}

#[test]
fn replay_serialization_round_trips_typed_branch_keys_and_rejects_old_shapes() {
    let timeline = Timeline::default();
    let root = timeline.current_branch;
    let mut log = ReplayLog::default();
    log.sync_topology(&timeline, 0);
    log.record_tick(root, 1);
    log.insert_branch_checksum(
        root,
        1,
        bevy_agent_core::SnapshotChecksum { tick: 1, hash: 99 },
    );
    let encoded = serde_json::to_value(&log).unwrap();
    let round_trip: ReplayLog = serde_json::from_value(encoded.clone()).unwrap();
    assert_eq!(round_trip.completed_ticks, log.completed_ticks);
    assert_eq!(round_trip.branch_checksums, log.branch_checksums);
    let mut malformed = encoded.clone();
    malformed["completed_ticks"] = serde_json::json!({ "bad-uuid": [1000] });
    assert!(serde_json::from_value::<ReplayLog>(malformed).is_err());
    let mut old_shape = encoded.clone();
    old_shape["checkpoints"] = serde_json::json!({});
    assert!(serde_json::from_value::<ReplayLog>(old_shape).is_err());
    let mut missing_version = encoded;
    missing_version["manifest"]
        .as_object_mut()
        .unwrap()
        .remove("schema_version");
    assert!(serde_json::from_value::<ReplayLog>(missing_version).is_err());
}

#[test]
fn restarting_recording_preserves_the_live_episode() {
    let mut world = World::new();
    let mut recorder = ReplayRecorder::default();
    recorder.log.manifest.episode_id = 8;
    world.insert_resource(recorder);
    start_recording(&mut world, Some(SnapshotId::new())).unwrap();
    assert_eq!(
        world.resource::<ReplayRecorder>().log.manifest.episode_id,
        8
    );
}

#[test]
fn stopping_an_already_stopped_recording_preserves_its_cursor_and_history() {
    let mut world = World::new();
    world.insert_resource(bevy_agent_core::SimClock::new(60));
    start_recording(&mut world, None).unwrap();
    let branch = world.resource::<Timeline>().current_branch;
    world
        .resource_mut::<ReplayRecorder>()
        .log
        .record_tick(branch, 2);
    world.resource_mut::<bevy_agent_core::SimClock>().tick = 2;
    let count = stop_recording(&mut world).unwrap();
    let sealed = serde_json::to_value(&world.resource::<ReplayRecorder>().log).unwrap();
    world.resource_mut::<bevy_agent_core::SimClock>().tick = 10;
    assert_eq!(stop_recording(&mut world).unwrap(), count);
    assert_eq!(
        serde_json::to_value(&world.resource::<ReplayRecorder>().log).unwrap(),
        sealed
    );
}

#[test]
fn plugin_finishing_uses_game_metadata_configured_after_replay_build() {
    use bevy_agent_core::{AgentActionKind, AgentControlAppExt, Observation, ObservationMode};
    let mut app = App::new();
    app.add_plugins(bevy_agent_core::AgentControlPlugin::default())
        .add_plugins(AgentReplayPlugin)
        .insert_resource(bevy_agent_core::EnvironmentMetadata {
            name: "direct-game".to_string(),
            version: "8.2".to_string(),
            description: None,
        });
    app.set_supported_actions([AgentActionKind::Noop])
        .set_supported_observation_modes([ObservationMode::Hybrid]);
    app.set_observation_schema(serde_json::json!({"type": "object"}))
        .unwrap();
    app.insert_observation_extractor(|world, _| {
        Observation::default_for_tick(world.resource::<bevy_agent_core::SimClock>().tick)
    });
    app.insert_checksum_extractor(|world| bevy_agent_core::EnvironmentChecksum {
        tick: world.resource::<bevy_agent_core::SimClock>().tick,
        hash: 0,
    });
    app.finish();
    app.cleanup();
    let timeline = app.world().resource::<Timeline>();
    let control = app.world().resource::<bevy_agent_core::AgentControlState>();
    assert_eq!(control.branch_id, timeline.current_branch);
    assert_eq!(control.timeline_id, timeline.timeline_id);
    bevy_agent_core::run_agent_tick(app.world_mut()).unwrap();
    stop_recording(app.world_mut()).unwrap();
    let log = &app.world().resource::<ReplayRecorder>().log;
    assert_eq!(log.manifest.game_id, "direct-game");
    assert_eq!(log.manifest.game_version, "8.2");
    assert_eq!(log.completed_ticks.values().next().unwrap().len(), 1);
}

#[test]
fn checked_topology_admission_and_selection_are_atomic() {
    let mut timeline = Timeline::default();
    let root = timeline.current_branch();
    let before = serde_json::to_value(&timeline).unwrap();
    assert!(timeline.select_branch(BranchId::new()).is_err());
    assert_eq!(serde_json::to_value(&timeline).unwrap(), before);
    let root_info = timeline.branches()[&root].clone();
    assert!(
        Timeline::from_branches(
            timeline.timeline_id(),
            root,
            vec![root_info.clone(), root_info]
        )
        .is_err()
    );
    let child = timeline.create_branch(5, None, None).unwrap();
    let before = serde_json::to_value(&timeline).unwrap();
    assert!(timeline.create_branch(4, None, None).is_err());
    assert_eq!(timeline.current_branch(), child);
    assert_eq!(serde_json::to_value(&timeline).unwrap(), before);
}

#[test]
fn rejected_live_history_replacement_preserves_the_owner() {
    let timeline = Timeline::default();
    let root = timeline.current_branch();
    let mut recorder = ReplayRecorder::default();
    let mut log = ReplayLog::default();
    log.sync_topology(&timeline, 0);
    log.record_tick(root, 1);
    recorder.replace_log(log.clone()).unwrap();
    let before = serde_json::to_value(&recorder).unwrap();
    log.branch_checksums
        .insert(BranchId::new(), Default::default());
    assert!(recorder.replace_log(log).is_err());
    assert_eq!(serde_json::to_value(&recorder).unwrap(), before);
}

#[test]
fn live_owner_preserves_shared_prefixes_and_rejects_bad_checkpoints() {
    let mut timeline = Timeline::default();
    let root = timeline.current_branch();
    timeline.create_branch(5, None, None).unwrap();
    timeline.select_branch(root).unwrap();
    let mut log = ReplayLog::default();
    log.sync_topology(&timeline, 0);
    log.record_tick(root, 10);
    let mut recorder = ReplayRecorder::default();
    recorder.replace_log(log).unwrap();
    let before = serde_json::to_value(&recorder).unwrap();
    assert!(recorder.truncate_future(&timeline, root, 4).is_err());
    assert_eq!(serde_json::to_value(&recorder).unwrap(), before);
    assert!(
        recorder
            .index_checkpoint(
                &timeline,
                root,
                4,
                SnapshotId::new(),
                bevy_agent_core::SnapshotChecksum { tick: 5, hash: 0 }
            )
            .is_err()
    );
    assert_eq!(serde_json::to_value(&recorder).unwrap(), before);
    recorder.truncate_future(&timeline, root, 5).unwrap();
    assert!(!recorder.log().completed_ticks.contains_key(&root));
}

#[test]
fn indexed_history_queries_match_reference_queries_through_import_and_truncation() {
    for seed in 0..24_u64 {
        let mut timeline = Timeline::default();
        let root = timeline.current_branch();
        let child = timeline.create_branch(10, None, None).unwrap();
        timeline.select_branch(root).unwrap();
        let sibling = timeline.create_branch(10, None, None).unwrap();
        let mut log = ReplayLog::default();
        log.sync_topology(&timeline, 10);
        for index in 1..=60 {
            let tick = 11 + (index * 17 + seed) % 30;
            let branch = [root, child, sibling][(index % 3) as usize];
            log.records.push(record_on(branch, tick, AgentAction::Noop));
            log.record_tick(branch, tick);
        }
        log.cursor_tick = 10;
        log.active_branch = Some(sibling);
        let mut recorder = ReplayRecorder::default();
        recorder.replace_log(log).unwrap();
        for branch in [root, child, sibling] {
            for start in [0, 10, 15, 20, 40] {
                assert_eq!(
                    recorder.actions_for_branch(&timeline, branch, start, 45),
                    recorder
                        .log()
                        .actions_for_branch(&timeline, branch, start, 45)
                );
            }
            assert_eq!(
                recorder.recorded_range(&timeline, branch),
                recorder.log().recorded_range(&timeline, branch)
            );
        }
        timeline.select_branch(sibling).unwrap();
        recorder.truncate_future(&timeline, sibling, 20).unwrap();
        assert_eq!(
            recorder.actions_for_branch(&timeline, sibling, 0, 45),
            recorder.log().actions_for_branch(&timeline, sibling, 0, 45)
        );
        assert_eq!(
            recorder.recorded_range(&timeline, sibling),
            recorder.log().recorded_range(&timeline, sibling)
        );
    }
}

#[test]
fn topology_synchronization_preserves_admitted_history_before_mutation() {
    let mut timeline = Timeline::default();
    let root = timeline.current_branch();
    let child = timeline.create_branch(3, None, None).unwrap();
    let mut log = ReplayLog {
        records: vec![
            record_on(root, 1, AgentAction::Noop),
            record_on(child, 5, AgentAction::Jump),
        ],
        ..Default::default()
    };
    log.sync_topology(&timeline, 10);
    let mut recorder = ReplayRecorder::default();
    recorder.replace_log(log).unwrap();
    let before = serde_json::to_value(&recorder).unwrap();

    let foreign = Timeline::default();
    assert!(recorder.sync_topology(&foreign, 10).is_err());
    assert!(
        recorder
            .index_checkpoint(
                &foreign,
                foreign.current_branch(),
                10,
                SnapshotId::new(),
                bevy_agent_core::SnapshotChecksum { tick: 10, hash: 0 },
            )
            .is_err()
    );
    assert_eq!(serde_json::to_value(&recorder).unwrap(), before);

    let mut branches = timeline.branches().values().cloned().collect::<Vec<_>>();
    branches
        .iter_mut()
        .find(|branch| branch.branch_id == child)
        .unwrap()
        .fork_tick = 6;
    let moved = Timeline::from_branches(timeline.timeline_id(), child, branches).unwrap();
    assert!(recorder.sync_topology(&moved, 10).is_err());
    assert_eq!(serde_json::to_value(&recorder).unwrap(), before);
    recorder.log().validate_history().unwrap();

    timeline.create_branch(7, None, None).unwrap();
    recorder.sync_topology(&timeline, 10).unwrap();
    recorder.log().validate_history().unwrap();
}
