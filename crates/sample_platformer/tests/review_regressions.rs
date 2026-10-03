use bevy_agent_core::{AgentAction, ControlMode};
use bevy_agent_remote::{AgentCapability, JsonRpcBridge, RemoteSecurity};
use bevy_agent_runner::{AgentApp, AgentEnvironment, ResetOptions};

fn make_env() -> AgentApp {
    AgentApp::new(sample_platformer::build_headless_app).unwrap()
}

fn parse_response(input: &str) -> serde_json::Value {
    serde_json::from_str(input).expect("valid JSON-RPC response")
}

// 1. Retention protects a snapshot referenced by the replay log.
// keep=1, A referenced by the log, creating B must not evict A;
// export must still succeed.
#[test]
fn retention_protects_referenced_snapshot() {
    let mut env = make_env();
    env.reset(ResetOptions::default()).unwrap();
    env.world_mut()
        .resource_mut::<bevy_agent_snapshot::SnapshotPolicy>()
        .keep_last_n_checkpoints = 1;
    // Unpin the initial reset snapshot so this exercises pure retention.
    let pinned: Vec<_> = env
        .world()
        .resource::<bevy_agent_snapshot::SnapshotStore>()
        .pinned()
        .iter()
        .copied()
        .collect();
    for id in pinned {
        bevy_agent_snapshot::unpin_snapshot(env.world_mut(), id).unwrap();
    }

    let a = env.snapshot().unwrap().snapshot_id;
    env.step(AgentAction::Noop).unwrap();
    let _b = env.snapshot().unwrap();

    let store = env.world().resource::<bevy_agent_snapshot::SnapshotStore>();
    assert!(
        store.get(a).is_some(),
        "retention must protect log-referenced snapshot A with keep=1"
    );
    env.export_replay_bundle()
        .expect("export must succeed when referenced snapshot is retained");
}

// 2. Remote delete must reject pinned / referenced snapshots.
#[test]
fn remote_delete_rejects_pinned_or_referenced() {
    let mut env = make_env();
    let bridge = JsonRpcBridge::default();
    bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":1,"method":"agent.reset","params":{"options":{"seed":1,"observation_mode":"Hybrid","create_initial_snapshot":true}}}"#,
    );
    let created = bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":2,"method":"agent.snapshot.create","params":{}}"#,
    );
    let created: serde_json::Value = parse_response(&created);
    let snapshot_id = created["result"]["snapshot_id"].clone();
    assert!(snapshot_id.is_string(), "snapshot create must succeed");

    // Pin it: coordinated deletes must refuse pinned ids.
    let id: bevy_agent_core::SnapshotId =
        serde_json::from_value(snapshot_id.clone()).expect("snapshot id");
    bevy_agent_snapshot::pin_snapshot(env.world_mut(), id).unwrap();

    let delete = bridge.handle_json(
        &mut env,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "agent.snapshot.delete",
            "params": { "snapshot_id": snapshot_id }
        })
        .to_string(),
    );
    let delete: serde_json::Value = parse_response(&delete);
    assert!(
        delete.get("error").is_some(),
        "remote delete of a pinned/referenced snapshot must be rejected, got: {delete}"
    );
    let message = delete["error"]["message"].as_str().unwrap_or("");
    assert!(
        message.contains("pinned") || message.contains("referenced"),
        "delete error must name pin/reference, got: {message}"
    );
}

// 3. Recording baseline is fresh: start_recording(None) after 3 ticks,
// then restore_tick(3) lands on tick 3 (not 0).
#[test]
fn recording_baseline_fresh_after_ticks() {
    let mut env = make_env();
    env.reset(ResetOptions::default()).unwrap();
    for _ in 0..3 {
        env.step(AgentAction::Noop).unwrap();
    }
    assert_eq!(env.current_tick(), 3);
    env.start_recording(None).unwrap();
    env.restore_tick(3).unwrap();
    assert_eq!(
        env.current_tick(),
        3,
        "restore_tick(3) after fresh baseline must land on tick 3, not 0"
    );
}

// 4. Branch checksum provenance: parent MoveRight vs child Noop at the
// same tick stay isolated; restoring the child tick succeeds.
#[test]
fn branch_checksum_provenance_child_restore_succeeds() {
    let mut env = make_env();
    env.reset(ResetOptions::default()).unwrap();
    env.step(AgentAction::Noop).unwrap(); // tick 1 (shared)
    env.step(AgentAction::Move { x: 1.0, y: 0.0 }).unwrap(); // tick 2 parent
    let child = env.branch(1, Some("provenance".to_string())).unwrap();
    env.step(AgentAction::Noop).unwrap(); // tick 2 child (different action)
    assert_eq!(env.current_tick(), 2);
    env.restore_tick(2)
        .expect("child tick 2 restore must succeed");
    assert_eq!(env.current_tick(), 2);
    assert_eq!(
        env.world()
            .resource::<bevy_agent_core::AgentControlState>()
            .branch_id,
        child,
        "restore must stay on the child branch"
    );
}

// 5. Cyclic import is rejected: a branch whose parent is itself.
#[test]
fn cyclic_import_self_parent_rejected() {
    let mut source = make_env();
    source.reset(ResetOptions::default()).unwrap();
    source.step(AgentAction::Noop).unwrap();
    let child = source.branch(1, Some("self-parent".to_owned())).unwrap();
    let mut bundle = source.export_replay_bundle().unwrap();
    let victim = bundle
        .log
        .timeline_topology
        .iter_mut()
        .find(|branch| branch.branch_id == child)
        .unwrap();
    victim.parent_branch = Some(child);

    let mut fresh = make_env();
    let error = fresh
        .load_replay_bundle(bundle)
        .expect_err("self-parent cycle must be rejected");
    let message = error.to_string();
    assert!(
        message.contains("cyclic"),
        "cycle error must name the topology problem, got: {message}"
    );
}

// 6. A replay without an explicit topology cannot be activated.
#[test]
fn missing_topology_import_rejected_without_initialization() {
    let mut source = make_env();
    source.reset(ResetOptions::default()).unwrap();
    source.step(AgentAction::Noop).unwrap();
    source.step(AgentAction::Move { x: 1.0, y: 0.0 }).unwrap();
    let mut bundle = source.export_replay_bundle().unwrap();
    bundle.log.timeline_topology.clear();
    bundle.log.active_branch = None;

    let mut fresh = make_env();
    let error = fresh
        .load_replay_bundle(bundle)
        .expect_err("missing topology must be rejected");
    assert!(
        error.to_string().contains("topology") || error.to_string().contains("branch"),
        "missing topology error must identify the problem: {error}"
    );
    assert!(!fresh.has_reset());
    assert_eq!(fresh.current_tick(), 0);
}

// 7. Custom action data cannot choose a privileged execution context.
#[test]
fn custom_action_payload_cannot_change_execution_context() {
    use bevy_agent_core::{AgentActionKind, AgentControlAppExt};

    let mut env = make_env();
    env.app_mut()
        .set_supported_actions([AgentActionKind::Noop, AgentActionKind::Custom])
        .register_custom_action_schema(
            "marker-shaped-data",
            serde_json::json!({
                "type": "object",
                "required": ["execution_context"],
                "properties": { "execution_context": { "type": "string" } }
            }),
        )
        .unwrap();
    env.reset(ResetOptions::default()).unwrap();
    env.world_mut()
        .resource_mut::<bevy_agent_core::AgentControlState>()
        .mode = ControlMode::Replay;
    let forged = AgentAction::Custom {
        value: serde_json::json!({"execution_context": "Reconstructing"}),
    };
    let error = env.step(forged).unwrap_err();
    assert!(error.to_string().contains("source"), "{error}");
    assert_eq!(env.current_tick(), 0);
    assert_eq!(
        *env.world().resource::<bevy_agent_core::ExecutionContext>(),
        bevy_agent_core::ExecutionContext::Live,
    );
}

// 8. Never-reset first-step auth: fresh env via RPC with
// STEP|OBSERVE_PLAYER + PlayerKnowledge returns player-only, no hybrid secret.
// Uses the bridge without any prior reset.
#[test]
fn never_reset_first_step_auth_player_only() {
    let mut env = make_env(); // never reset
    let bridge = JsonRpcBridge::new(RemoteSecurity {
        capabilities: AgentCapability::STEP | AgentCapability::OBSERVE_PLAYER,
        ..Default::default()
    })
    .unwrap();
    let response = bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":1,"method":"agent.step","params":{"action":{"type":"Noop"},"observation_mode":"PlayerKnowledge"}}"#,
    );
    let value: serde_json::Value = parse_response(&response);
    assert!(
        value.get("error").is_none(),
        "PlayerKnowledge first step must be allowed without prior reset: {value}"
    );
    assert_eq!(value["result"]["tick"], 1);
    let obs = &value["result"]["observation"];
    assert_eq!(obs["kind"], "Symbolic", "must be player-only: {obs}");
    assert!(
        obs.get("debug").is_none() || obs["debug"].is_null(),
        "no hybrid debug secret may leak: {obs}"
    );
    assert!(
        obs.get("pixels").is_none() || obs["pixels"].is_null(),
        "no pixels may leak: {obs}"
    );
}

// 9. Cursor activation: after export + fresh import, the world tick equals
// the exported cursor immediately after the successful import.
#[test]
fn cursor_activation_after_import() {
    let mut source = make_env();
    source.reset(ResetOptions::default()).unwrap();
    source.step(AgentAction::Noop).unwrap();
    source.step(AgentAction::Noop).unwrap();
    let bundle = source.export_replay_bundle().unwrap();
    let cursor = bundle.log.cursor_tick;

    let mut fresh = make_env();
    fresh.load_replay_bundle(bundle).unwrap();
    assert!(fresh.has_reset());
    assert_eq!(fresh.current_tick(), cursor);
}

// 10. Truncate clears expectations: after diverge, the old branch future
// (records and checksum expectations) must not apply.
#[test]
fn truncate_clears_expectations_after_diverge() {
    let mut env = make_env();
    env.reset(ResetOptions::default()).unwrap();
    for _ in 0..5 {
        env.step(AgentAction::Noop).unwrap();
    }
    assert_eq!(env.current_tick(), 5);
    env.restore_tick(2).unwrap();
    assert_eq!(env.current_tick(), 2);
    // Diverge into recorded future on the same branch: truncates beyond tick 2.
    let diverged = env.step(AgentAction::Jump).unwrap();
    assert_eq!(diverged.tick, 3);

    let log = env.replay_log().unwrap().clone();
    assert!(
        log.records.iter().all(|record| record.tick <= 3),
        "records beyond the diverge point must be truncated"
    );
    assert!(
        log.expected_checksum(
            env.world()
                .resource::<bevy_agent_core::AgentControlState>()
                .branch_id,
            5,
        )
        .is_none(),
        "old branch checksum expectation for tick 5 must be cleared after diverge"
    );
    // The old future is no longer addressable on this branch.
    assert!(
        env.restore_tick(5).is_err(),
        "restoring the truncated tick 5 must fail after diverge"
    );
}
