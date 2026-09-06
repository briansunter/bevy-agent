use bevy_agent_core::{
    ActionSource, AgentAction, AgentActionQueue, AgentControlState, Observation,
};
use bevy_agent_remote::{AgentCapability, JsonRpcBridge, RemoteSecurity};
use bevy_agent_replay::ReplayRecorder;
use bevy_agent_runner::{AgentApp, AgentEnvironment, ResetOptions};

fn make_env() -> AgentApp {
    AgentApp::new(sample_platformer::build_headless_app)
}

fn symbolic_entities(observation: &Observation) -> Vec<(String, [f32; 3])> {
    match observation {
        Observation::Hybrid { symbolic, .. } | Observation::Symbolic(symbolic) => symbolic
            .visible_entities
            .iter()
            .map(|entity| (entity.kind.clone(), entity.position))
            .collect(),
        other => panic!("unexpected observation: {other:?}"),
    }
}

// 1. Auto retention: interval 1 + keep 1 still exports a resolvable bundle.
#[test]
fn auto_retention_interval_1_keep_1_export_resolves() {
    let mut env = make_env();
    env.reset(ResetOptions::default()).unwrap();
    env.world_mut()
        .resource_mut::<bevy_agent_snapshot::SnapshotPolicy>()
        .checkpoint_every_ticks = 1;
    env.world_mut()
        .resource_mut::<bevy_agent_snapshot::SnapshotPolicy>()
        .keep_last_n_checkpoints = 1;
    // Unpin the initial reset snapshot so this exercises pure eviction.
    let pinned: Vec<_> = env
        .world()
        .resource::<bevy_agent_snapshot::SnapshotStore>()
        .pinned
        .iter()
        .copied()
        .collect();
    for id in pinned {
        bevy_agent_snapshot::unpin_snapshot(env.world_mut(), id);
    }

    for _ in 0..4 {
        env.step(AgentAction::Noop).unwrap();
    }
    // Export must resolve: every replay-referenced snapshot exists.
    let bundle = env.export_replay_bundle().expect("export must resolve");
    let ids: std::collections::BTreeSet<_> = bundle
        .snapshots
        .iter()
        .map(|s| s.manifest.snapshot_id)
        .collect();
    for id in bundle
        .log
        .initial_snapshot
        .into_iter()
        .chain(bundle.log.checkpoints.values().copied())
        .chain(bundle.log.branch_checkpoints.iter().map(|c| c.snapshot_id))
    {
        assert!(ids.contains(&id), "referenced snapshot {id:?} must exist");
    }
}

// 2. Cursor-zero activation: export at tick 0, fresh load has gameplay
// entities and stepping works.
#[test]
fn cursor_zero_activation_fresh_load_has_entities_and_steps() {
    let mut source = make_env();
    source.reset(ResetOptions::default()).unwrap();
    assert_eq!(source.current_tick(), 0);
    let bundle = source.export_replay_bundle().unwrap();
    assert_eq!(bundle.log.cursor_tick, 0);

    let mut fresh = make_env(); // never reset
    fresh.load_replay_bundle(bundle).unwrap();
    // Gameplay entities are present after a tick-0 load.
    let observation = fresh
        .observe(bevy_agent_core::ObservationMode::Hybrid)
        .unwrap();
    let entities = symbolic_entities(&observation);
    assert!(
        entities.iter().any(|(kind, _)| kind == "platform"),
        "platforms must be present: {entities:?}"
    );
    assert!(
        entities.iter().any(|(kind, _)| kind == "coin"),
        "coins must be present: {entities:?}"
    );
    assert!(
        entities.iter().any(|(kind, _)| kind == "goal"),
        "goal must be present: {entities:?}"
    );
    // Stepping works from the tick-0 cursor.
    let stepped = fresh.step(AgentAction::Noop).unwrap();
    assert_eq!(stepped.tick, 1);
    assert_eq!(fresh.current_tick(), 1);
}

// 3. Corrupt import rejected: tampered snapshot payload fails positioning.
#[test]
fn corrupt_import_rejected_tampered_checksum() {
    let mut source = make_env();
    source.reset(ResetOptions::default()).unwrap();
    source.step(AgentAction::Noop).unwrap();
    source.step(AgentAction::Move { x: 1.0, y: 0.0 }).unwrap();
    let mut bundle = source.export_replay_bundle().unwrap();
    assert!(!bundle.snapshots.is_empty());

    // Tamper the first snapshot payload to another valid value while keeping
    // the stored checksum: checksum precondition must fail downstream.
    let mut mutated = false;
    for snapshot in &mut bundle.snapshots {
        for entity in &mut snapshot.entities {
            for component in &mut entity.components {
                if component.type_name.contains("Player") && component.value.get("health").is_some()
                {
                    component.value["health"] = serde_json::json!(1.0);
                    mutated = true;
                }
            }
        }
    }
    assert!(mutated, "expected a Player component to corrupt");

    let mut fresh = make_env();
    // Either the install or the subsequent positioning must reject the
    // tampered bundle (load validates topology/refs; restore verifies
    // checksums).
    match fresh.load_replay_bundle(bundle) {
        Err(error) => {
            let message = error.to_string();
            assert!(
                message.contains("checksum")
                    || message.contains("mismatch")
                    || message.contains("missing")
                    || message.contains("snapshot"),
                "load must name the corruption, got: {message}"
            );
        }
        Ok(()) => {
            let cursor = fresh.replay_log().map(|log| log.cursor_tick).unwrap_or(2);
            let target = cursor.max(1);
            let error = fresh.restore_tick(target).unwrap_err();
            let message = error.to_string();
            assert!(
                message.contains("checksum")
                    || message.contains("mismatch")
                    || message.contains("verification")
                    || message.contains("rolled back"),
                "restore of tampered import must fail verification, got: {message}"
            );
        }
    }
}

// 4. Fork-snapshot completeness: branch, then a child snapshot replaces the
// same-tick checkpoint; export-import-reexport stays consistent.
#[test]
fn fork_snapshot_completeness_branch_child_replace_reexport() {
    let mut env = make_env();
    env.reset(ResetOptions::default()).unwrap();
    env.step(AgentAction::Noop).unwrap();
    env.step(AgentAction::Move { x: 1.0, y: 0.0 }).unwrap();
    let child = env.branch(1, Some("fork-child".to_string())).unwrap();
    assert_eq!(env.current_tick(), 1);
    // Child snapshot at the fork tick replaces the fork checkpoint entry.
    let replaced = env.snapshot().unwrap();
    assert_eq!(replaced.tick, 1);

    let bundle = env.export_replay_bundle().unwrap();
    let child_checkpoints: Vec<_> = bundle
        .log
        .branch_checkpoints
        .iter()
        .filter(|c| c.branch_id == child && c.tick == 1)
        .collect();
    assert_eq!(child_checkpoints.len(), 1);
    assert_eq!(child_checkpoints[0].snapshot_id, replaced.snapshot_id);

    let mut fresh = make_env();
    fresh.load_replay_bundle(bundle).unwrap();
    let reexported = fresh.export_replay_bundle().unwrap();
    assert!(!reexported.snapshots.is_empty());
    // Re-exported history is still positionable on the child lineage.
    fresh.restore_tick(1).unwrap();
    assert_eq!(fresh.current_tick(), 1);
    assert_eq!(
        fresh.world().resource::<AgentControlState>().branch_id,
        child
    );
}

// 5a. Branch checksum lookup is branch-distinct at the same tick.
#[test]
fn branch_checksum_lookup_distinct_per_branch() {
    use bevy_agent_core::SnapshotChecksum;

    let mut env = make_env();
    env.reset(ResetOptions::default()).unwrap();
    env.step(AgentAction::Noop).unwrap();
    let parent = env.world().resource::<AgentControlState>().branch_id;
    let child = env.branch(1, Some("checksum-iso".to_string())).unwrap();
    assert_ne!(parent, child);

    // Modern log (exported topology) isolates same-tick checksums per branch.
    let mut log = env.export_replay_bundle().unwrap().log;
    assert!(!log.timeline_topology.is_empty());
    log.insert_branch_checksum(parent, 1, SnapshotChecksum { tick: 1, hash: 111 });
    log.insert_branch_checksum(child, 1, SnapshotChecksum { tick: 1, hash: 222 });
    assert_eq!(log.expected_checksum(parent, 1).unwrap().hash, 111);
    assert_eq!(log.expected_checksum(child, 1).unwrap().hash, 222);
}

// 5b. Parent checksum expectation survives a child step.
#[test]
fn branch_checksum_preserved_after_child_step() {
    use bevy_agent_core::SnapshotChecksum;

    let mut env = make_env();
    env.reset(ResetOptions::default()).unwrap();
    env.step(AgentAction::Noop).unwrap();
    let parent = env.world().resource::<AgentControlState>().branch_id;
    let child = env.branch(1, Some("checksum-keep".to_string())).unwrap();
    // Seed a parent expectation in the live log, then step the child.
    env.world_mut()
        .resource_mut::<ReplayRecorder>()
        .log
        .insert_branch_checksum(
            parent,
            1,
            SnapshotChecksum {
                tick: 1,
                hash: 4242,
            },
        );
    env.step(AgentAction::Jump).unwrap();
    assert_eq!(env.current_tick(), 2);

    let log = env.replay_log().unwrap().clone();
    let key = bevy_agent_replay::branch_checksum_key(parent);
    assert!(
        log.branch_checksums
            .get(&key)
            .and_then(|per_branch| per_branch.get(&1))
            .is_some_and(|checksum| checksum.hash == 4242),
        "parent branch checksum must survive child stepping (child {child:?})"
    );
}

// 6. Stale truncation: checkpoint at tick 5, restore 2, diverge, restore 5 fails.
#[test]
fn stale_truncation_diverge_drops_tick5_future() {
    let mut env = make_env();
    env.reset(ResetOptions::default()).unwrap();
    for _ in 0..5 {
        env.step(AgentAction::Noop).unwrap();
    }
    assert_eq!(env.current_tick(), 5);
    let checkpoint = env.snapshot().unwrap();
    assert_eq!(checkpoint.tick, 5);

    env.restore_tick(2).unwrap();
    assert_eq!(env.current_tick(), 2);
    // Diverge into the recorded future on the same branch.
    let diverged = env.step(AgentAction::Jump).unwrap();
    assert_eq!(diverged.tick, 3);

    let error = env.restore_tick(5).unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("beyond")
            || message.contains("recorded")
            || message.contains("end")
            || message.contains("no checkpoint")
            || message.contains("tick 5"),
        "restoring truncated tick 5 must fail, got: {message}"
    );
}

// 7. Rollback metadata: forced verify failure leaves control.frame restored.
#[test]
fn rollback_metadata_frame_restored_on_verify_failure() {
    let mut env = make_env();
    env.reset(ResetOptions::default()).unwrap();
    for _ in 0..5 {
        env.step(AgentAction::Noop).unwrap();
    }
    let created = env.snapshot().unwrap();
    for _ in 0..3 {
        env.step(AgentAction::Noop).unwrap();
    }
    let frame_before = env.world().resource::<AgentControlState>().frame;
    let tick_before = env.current_tick();

    // Corrupt the stored snapshot payload (valid value, stale checksum).
    let mut mutated = false;
    {
        let mut store = env
            .world_mut()
            .resource_mut::<bevy_agent_snapshot::SnapshotStore>();
        let snapshot = store
            .snapshots
            .get_mut(&created.snapshot_id)
            .expect("snapshot in store");
        for entity in &mut snapshot.entities {
            for component in &mut entity.components {
                if component.type_name.contains("Player") && component.value.get("health").is_some()
                {
                    component.value["health"] = serde_json::json!(1.0);
                    mutated = true;
                }
            }
        }
    }
    assert!(mutated, "expected a Player component to corrupt");

    let error = env.restore(created.snapshot_id).unwrap_err();
    assert!(
        error.to_string().contains("checksum"),
        "must fail at checksum, got: {error}"
    );
    // Rollback metadata: frame/tick are unchanged by the failed restore.
    assert_eq!(
        env.world().resource::<AgentControlState>().frame,
        frame_before,
        "control.frame must be restored after failed verify"
    );
    assert_eq!(env.current_tick(), tick_before);
}

// 8. Queue multiplicity: two identical Jumps survive a snapshot roundtrip.
#[test]
fn queue_multiplicity_identical_jumps_preserved() {
    let mut env = make_env();
    env.reset(ResetOptions::default()).unwrap();
    env.enqueue_action_at(5, ActionSource::Test, AgentAction::Jump);
    env.enqueue_action_at(5, ActionSource::Test, AgentAction::Jump);
    assert_eq!(env.world().resource::<AgentActionQueue>().pending.len(), 2);

    let snapshot = env.snapshot().unwrap();
    env.step(AgentAction::Noop).unwrap();
    env.restore(snapshot.snapshot_id).unwrap();

    let pending = env.world().resource::<AgentActionQueue>().pending.clone();
    let jumps = pending
        .iter()
        .filter(|scheduled| scheduled.tick == 5 && scheduled.action == AgentAction::Jump)
        .count();
    assert_eq!(
        jumps, 2,
        "two identical queued Jumps must be preserved, got: {pending:?}"
    );
}

// 9. Remote baseline path: replay.start after ticks then restore lands.
#[test]
fn remote_baseline_path_start_after_ticks_then_restore() {
    let mut env = make_env();
    let bridge = JsonRpcBridge::new(RemoteSecurity {
        capabilities: AgentCapability::default() | AgentCapability::FILESYSTEM,
        allow_absolute_paths: true,
        ..Default::default()
    });
    bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":1,"method":"agent.reset","params":{"options":{"seed":1,"observation_mode":"Hybrid","create_initial_snapshot":true}}}"#,
    );
    for id in 2..=4 {
        let step = bridge.handle_json(
            &mut env,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "agent.step",
                "params": { "action": { "type": "Noop" } }
            })
            .to_string(),
        );
        let step: serde_json::Value = serde_json::from_str(&step).unwrap();
        assert!(step.get("error").is_none(), "step must succeed: {step}");
    }
    assert_eq!(env.current_tick(), 3);

    // Fresh baseline captured at the current tick (not tick 0).
    let start = bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":5,"method":"agent.replay.start","params":{}}"#,
    );
    let start: serde_json::Value = serde_json::from_str(&start).unwrap();
    assert_eq!(start["result"]["recording"], true);

    // Restoring the baseline tick lands on tick 3 via the remote path.
    let restore = bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":6,"method":"agent.timeline.restore_tick","params":{"tick":3}}"#,
    );
    let restore: serde_json::Value = serde_json::from_str(&restore).unwrap();
    assert!(
        restore.get("error").is_none(),
        "remote restore_tick(3) after fresh baseline must succeed: {restore}"
    );
    assert_eq!(restore["result"]["current_tick"], 3);
    assert_eq!(env.current_tick(), 3);
}
