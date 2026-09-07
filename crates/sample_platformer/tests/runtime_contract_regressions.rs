use bevy::prelude::{App, MinimalPlugins};
use bevy_agent_core::{
    ActionSource, AgentAction, AgentActionQueue, AgentControlState, ControlMode,
};
use bevy_agent_remote::{AgentCapability, JsonRpcBridge, RemoteSecurity};
use bevy_agent_runner::{
    AgentApp, AgentControlPlugins, AgentEnvironment, ResetOptions, expected_checksum_for_tick,
};
use bevy_agent_snapshot::{SnapshotPolicy, SnapshotStore};

fn make_env() -> AgentApp {
    AgentApp::new(sample_platformer::build_headless_app)
}

// 1. Same-tick parent/child checkpoints stay isolated: the live per-branch
// checksum lookup never falls back across branches, and restoring the child
// tick succeeds on the child branch.
#[test]
fn live_checksum_no_fallback() {
    let mut env = make_env();
    env.reset(ResetOptions::default()).unwrap();
    env.step(AgentAction::Noop).unwrap(); // tick 1 (shared)
    env.step(AgentAction::Move { x: 1.0, y: 0.0 }).unwrap(); // tick 2 parent
    let parent_snapshot = env.snapshot().unwrap(); // tick 2 parent checkpoint
    assert_eq!(parent_snapshot.tick, 2);
    let parent = env.world().resource::<AgentControlState>().branch_id;

    let child = env.branch(1, Some("live-checksum".to_string())).unwrap();
    assert_ne!(child, parent);
    env.step(AgentAction::Noop).unwrap(); // tick 2 child (different action)
    let child_snapshot = env.snapshot().unwrap(); // tick 2 child checkpoint
    assert_eq!(child_snapshot.tick, 2);
    assert_ne!(
        child_snapshot.snapshot_id, parent_snapshot.snapshot_id,
        "same-tick parent/child checkpoints must be distinct"
    );

    // Live lookup is branch-distinct: no cross-branch fallback.
    let log = env.replay_log().unwrap().clone();
    let parent_expected = expected_checksum_for_tick(&log, parent, 2).expect("parent tick-2");
    let child_expected = expected_checksum_for_tick(&log, child, 2).expect("child tick-2");
    assert_ne!(
        parent_expected.hash, child_expected.hash,
        "Move (parent) vs Noop (child) at tick 2 must checksum differently"
    );

    env.restore_tick(2)
        .expect("child tick 2 restore must succeed");
    assert_eq!(env.current_tick(), 2);
    assert_eq!(
        env.world().resource::<AgentControlState>().branch_id,
        child,
        "restore must stay on the child branch"
    );
}

// 2. Bundle import replaces the destination queue: a future queued on the
// destination never merges into the imported history.
#[test]
fn import_queue_replace() {
    let mut source = make_env();
    source.reset(ResetOptions::default()).unwrap();
    source.step(AgentAction::Noop).unwrap(); // tick 1, no pendings
    assert!(
        source
            .world()
            .resource::<AgentActionQueue>()
            .pending
            .is_empty()
    );
    let bundle = source.export_replay_bundle().unwrap();

    let mut dest = make_env();
    dest.reset(ResetOptions::default()).unwrap();
    dest.enqueue_action_at(2, ActionSource::Test, AgentAction::Move { x: 1.0, y: 0.0 });
    assert_eq!(dest.world().resource::<AgentActionQueue>().pending.len(), 1);

    dest.load_replay_bundle(bundle).unwrap();
    assert_eq!(dest.current_tick(), 1);
    let pending = dest.world().resource::<AgentActionQueue>().pending.clone();
    assert!(
        pending.iter().all(|scheduled| scheduled.tick != 2),
        "destination future must be replaced on import, got: {pending:?}"
    );
}

// 3. Checkpoint index tick vs snapshot clock mismatch is rejected on import
// (public bundle mutation of the `log.checkpoints` tick key).
#[test]
fn checkpoint_clock_mismatch_rejected() {
    let mut source = make_env();
    source.reset(ResetOptions::default()).unwrap();
    source.step(AgentAction::Noop).unwrap();
    source.step(AgentAction::Move { x: 1.0, y: 0.0 }).unwrap();
    let created = source.snapshot().unwrap();
    assert_eq!(created.tick, 2);
    let mut bundle = source.export_replay_bundle().unwrap();

    let victim_tick = *bundle
        .log
        .checkpoints
        .keys()
        .max()
        .expect("checkpoint index must be populated");
    let victim_id = bundle.log.checkpoints.remove(&victim_tick).unwrap();
    bundle.log.checkpoints.insert(victim_tick + 5, victim_id);

    let mut fresh = make_env();
    let error = fresh.load_replay_bundle(bundle).unwrap_err();
    assert!(
        error.to_string().contains("mismatch"),
        "index/clock mismatch must be rejected, got: {error}"
    );
}

// 4. A bundle with actions but no snapshots has no restorable baseline:
// the import fails, or (log-only history) leaves the env unactivated.
#[test]
fn missing_baseline_no_init() {
    let mut source = make_env();
    source.reset(ResetOptions::default()).unwrap();
    source.step(AgentAction::Noop).unwrap();
    let mut bundle = source.export_replay_bundle().unwrap();
    assert!(!bundle.log.records.is_empty(), "bundle must carry actions");
    bundle.snapshots.clear();
    assert!(bundle.snapshots.is_empty());

    let mut fresh = make_env(); // never reset
    match fresh.load_replay_bundle(bundle) {
        Err(error) => assert!(!error.to_string().is_empty()),
        Ok(()) => assert!(
            !fresh.has_reset(),
            "log-only import without a baseline must not activate"
        ),
    }
}

// 5. In Replay mode an Agent-source Noop is filtered (0 applied) yet the
// empty tick is still recorded in `completed_ticks` (no record appended).
// Empty ticks advance the recorded range and restore successfully.
#[test]
fn empty_tick_recorded() {
    let mut env = make_env();
    env.reset(ResetOptions::default()).unwrap();
    env.world_mut().resource_mut::<AgentControlState>().mode = ControlMode::Replay;

    let response = env.step(AgentAction::Noop).unwrap();
    assert_eq!(response.tick, 1);
    assert_eq!(
        response.info.actions_applied, 0,
        "Replay mode must filter Agent-source input"
    );
    let log = env.replay_log().unwrap();
    assert!(
        log.records.is_empty(),
        "filtered input must not append records"
    );
    assert!(
        log.completed_ticks.values().any(|ticks| ticks.contains(&1)),
        "the empty tick must still be recorded"
    );

    env.restore_tick(1).unwrap();
    assert_eq!(env.current_tick(), 1);
}

// 6. A backward fork (branch to an ancestor tick while on a child) keeps
// the fork-ordering invariant, so export-import round-trips.
#[test]
fn backward_fork_importable() {
    let mut env = make_env();
    env.reset(ResetOptions::default()).unwrap();
    env.step(AgentAction::Noop).unwrap();
    env.step(AgentAction::Noop).unwrap();
    assert_eq!(env.current_tick(), 2);

    let child = env.branch(2, Some("child".to_string())).unwrap();
    assert_eq!(env.current_tick(), 2);
    let backward = env.branch(1, Some("backward".to_string())).unwrap();
    assert_ne!(backward, child);
    let cursor = env.current_tick();
    assert_eq!(cursor, 1);

    let bundle = env.export_replay_bundle().unwrap();
    let mut fresh = make_env();
    fresh
        .load_replay_bundle(bundle)
        .expect("backward-fork bundle must import");
    assert_eq!(fresh.current_tick(), cursor);
}

// 7. An invalid typed resource value in a non-cursor snapshot is rejected
// on import (checksum precondition over the tampered payload).
#[test]
fn typed_non_cursor_invalid_rejected() {
    let mut source = make_env();
    source.reset(ResetOptions::default()).unwrap();
    source.step(AgentAction::Noop).unwrap(); // tick 1
    let first = source.snapshot().unwrap(); // non-cursor snapshot
    source.step(AgentAction::Move { x: 1.0, y: 0.0 }).unwrap(); // tick 2 cursor
    let second = source.snapshot().unwrap();
    assert_eq!(second.tick, 2);
    let mut bundle = source.export_replay_bundle().unwrap();

    let mut mutated = false;
    for snapshot in &mut bundle.snapshots {
        if snapshot.manifest.snapshot_id == first.snapshot_id {
            for resource in &mut snapshot.resources {
                if resource.type_name.contains("GameScore") {
                    resource.value = serde_json::json!("not-a-score");
                    mutated = true;
                }
            }
        }
    }
    assert!(mutated, "expected a GameScore resource to corrupt");

    let mut fresh = make_env();
    let error = fresh.load_replay_bundle(bundle).unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("checksum") || message.contains("decode"),
        "invalid non-cursor payload must be rejected, got: {message}"
    );
}

// 8. With replay disabled, snapshot-only retention still applies: a keep-1 /
// interval-1 policy stays bounded over many steps.
#[test]
fn replay_disabled_retention() {
    let mut env = AgentApp::new(|| {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(
                AgentControlPlugins::deterministic()
                    .without_replay()
                    .with_snapshot_policy(SnapshotPolicy {
                        checkpoint_every_ticks: 1,
                        keep_last_n_checkpoints: 1,
                        checkpoint_on_terminal: false,
                        checkpoint_on_branch: false,
                    }),
            )
            .add_plugins(sample_platformer::PlatformerPlugin);
        app
    });
    env.reset(ResetOptions::default()).unwrap();
    assert!(
        env.replay_log().is_none(),
        "replay disabled: no recorder present"
    );

    for _ in 0..10 {
        env.step(AgentAction::Noop).unwrap();
    }
    let len = env.world().resource::<SnapshotStore>().snapshots.len();
    assert!(
        len <= 3,
        "snapshot-only retention must stay bounded, got {len}"
    );
}

// 9. Remote baseline path: `replay.start` after ticks captures a fresh
// baseline, and restoring that tick over RPC lands on the same tick.
#[test]
fn baseline_remote_path() {
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

    let start = bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":5,"method":"agent.replay.start","params":{}}"#,
    );
    let start: serde_json::Value = serde_json::from_str(&start).unwrap();
    assert_eq!(start["result"]["recording"], true);

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
