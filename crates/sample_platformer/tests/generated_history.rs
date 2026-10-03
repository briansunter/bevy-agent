//! Generated domain sequences compare navigation against fresh forward execution.

use bevy_agent_core::{
    ActionSource, AgentAction, AgentControlState, LastStepResponse, ObservationMode,
};
use bevy_agent_runner::{AgentApp, AgentEnvironment, ResetOptions};
use bevy_agent_snapshot::{SnapshotType, capture_snapshot, checksum_snapshot};
use sample_platformer::{GameScore, build_headless_app};

fn environment(seed: u64) -> AgentApp {
    let mut env = AgentApp::new(build_headless_app).unwrap();
    env.reset(ResetOptions {
        seed: Some(seed),
        ..Default::default()
    })
    .unwrap();
    env
}

fn generated_actions(seed: u64, count: usize) -> Vec<AgentAction> {
    let mut state = seed.wrapping_add(1);
    (0..count)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            match (state >> 32) % 7 {
                0 => AgentAction::Move { x: -1.0, y: 0.0 },
                1 => AgentAction::Move { x: 1.0, y: 0.0 },
                2 => AgentAction::Move { x: 0.5, y: 0.0 },
                3 => AgentAction::Jump,
                4 => AgentAction::Dodge,
                _ => AgentAction::Noop,
            }
        })
        .collect()
}

fn assert_same_state(actual: &mut AgentApp, expected: &mut AgentApp, context: &str) {
    let actual_observation = actual.observe(ObservationMode::Hybrid).unwrap();
    let expected_observation = expected.observe(ObservationMode::Hybrid).unwrap();
    assert_eq!(actual_observation, expected_observation, "{context}");
    let actual_response = actual
        .world()
        .resource::<LastStepResponse>()
        .0
        .as_ref()
        .unwrap();
    let expected_response = expected
        .world()
        .resource::<LastStepResponse>()
        .0
        .as_ref()
        .unwrap();
    assert_eq!(
        actual_response.checksum, expected_response.checksum,
        "{context}"
    );
    assert_eq!(
        actual_response.reward.to_bits(),
        expected_response.reward.to_bits(),
        "{context}"
    );
    assert_eq!(
        (actual_response.done, actual_response.truncated),
        (expected_response.done, expected_response.truncated),
        "{context}"
    );
}

fn advance_pair(
    actual: &mut AgentApp,
    expected: &mut AgentApp,
    actions: &[AgentAction],
    context: &str,
) {
    for action in actions {
        let a = actual.step(action.clone()).unwrap();
        let b = expected.step(action.clone()).unwrap();
        assert_eq!(
            a.checksum, b.checksum,
            "{context}: tick {} action {action:?}",
            a.tick
        );
        assert_eq!(a.observation, b.observation, "{context}: tick {}", a.tick);
        assert_eq!(
            a.reward.to_bits(),
            b.reward.to_bits(),
            "{context}: tick {}",
            a.tick
        );
    }
}

#[test]
fn generated_snapshot_rewind_fork_import_sequences_preserve_future_behavior() {
    for seed in 0..16 {
        let actions = generated_actions(seed, 24 + seed as usize % 12);
        let mut actual = environment(seed);
        let mut expected = environment(seed);
        advance_pair(&mut actual, &mut expected, &actions[..8], "recorded prefix");
        let checkpoint = actual.snapshot().unwrap();
        advance_pair(&mut actual, &mut expected, &actions[8..], "recorded suffix");
        let original = actual.export_replay_bundle().unwrap();

        let mut snapshot_path = environment(seed);
        snapshot_path.load_replay_bundle(original.clone()).unwrap();
        snapshot_path.restore(checkpoint.snapshot_id).unwrap();
        let mut snapshot_oracle = environment(seed);
        snapshot_oracle.step_many(actions[..8].to_vec()).unwrap();
        assert_same_state(
            &mut snapshot_path,
            &mut snapshot_oracle,
            "direct snapshot restore",
        );
        advance_pair(
            &mut snapshot_path,
            &mut snapshot_oracle,
            &actions[8..12],
            "snapshot future",
        );

        let fork_tick = 3 + seed % 12;
        actual.restore_tick(fork_tick).unwrap();
        let parent = actual.world().resource::<AgentControlState>().branch_id;
        let parent_records: Vec<_> = actual
            .replay_log()
            .unwrap()
            .records
            .iter()
            .filter(|record| record.branch_id == parent)
            .cloned()
            .collect();
        actual
            .branch(fork_tick, Some(format!("generated-{seed}")))
            .unwrap();
        let mut fork_oracle = environment(seed);
        fork_oracle
            .step_many(actions[..fork_tick as usize].to_vec())
            .unwrap();
        assert_same_state(&mut actual, &mut fork_oracle, "fork state");
        let alternative = generated_actions(seed + 100, 10);
        advance_pair(&mut actual, &mut fork_oracle, &alternative, "fork future");
        assert_eq!(
            actual
                .replay_log()
                .unwrap()
                .records
                .iter()
                .filter(|record| record.branch_id == parent)
                .cloned()
                .collect::<Vec<_>>(),
            parent_records,
            "child execution must preserve parent history for seed {seed}",
        );

        let mut imported = environment(seed + 999);
        imported
            .load_replay_bundle(actual.export_replay_bundle().unwrap())
            .unwrap();
        assert_same_state(&mut imported, &mut actual, "imported child cursor");
        let future = generated_actions(seed + 200, 6);
        advance_pair(&mut imported, &mut actual, &future, "imported child future");
    }
}

fn fingerprint(env: &mut AgentApp) -> (u64, String, serde_json::Value) {
    let snapshot = capture_snapshot(env.world_mut(), None).unwrap();
    let state = checksum_snapshot(&snapshot).unwrap().hash;
    let export = serde_json::to_string(&env.export_replay_bundle().unwrap()).unwrap();
    let control = serde_json::json!({
        "control": env.world().resource::<AgentControlState>(),
        "last_response": env.world().resource::<LastStepResponse>(),
        "context": env.world().resource::<bevy_agent_core::ExecutionContext>(),
        "has_reset": env.has_reset(),
    });
    (state, export, control)
}

#[test]
fn generated_failed_operations_preserve_state_pending_inputs_and_exports() {
    for seed in 0..16 {
        let mut env = environment(seed);
        env.step_many(generated_actions(seed, 12)).unwrap();
        env.enqueue_action_at(20, ActionSource::Test, AgentAction::Jump)
            .unwrap();
        let before = fingerprint(&mut env);

        assert!(
            env.step_many(vec![
                AgentAction::Noop,
                AgentAction::Move { x: 2.0, y: 0.0 }
            ])
            .is_err()
        );
        assert_eq!(fingerprint(&mut env), before, "invalid batch, seed {seed}");
        assert!(
            env.enqueue_action_at(env.current_tick(), ActionSource::Test, AgentAction::Noop)
                .is_err()
        );
        assert_eq!(
            fingerprint(&mut env),
            before,
            "expired enqueue, seed {seed}"
        );
        assert!(env.observe(ObservationMode::PixelFrame).is_err());
        assert_eq!(
            fingerprint(&mut env),
            before,
            "unsupported observation, seed {seed}"
        );

        let mut corrupt = env.export_replay_bundle().unwrap();
        let snapshot = corrupt.snapshots.first_mut().unwrap();
        let resource = snapshot
            .resources
            .iter_mut()
            .find(|resource| resource.type_id == GameScore::TYPE_ID)
            .unwrap();
        resource.schema_version += 1;
        snapshot.checksum = checksum_snapshot(snapshot).unwrap();
        assert!(env.load_replay_bundle(corrupt).is_err());
        assert_eq!(
            fingerprint(&mut env),
            before,
            "incompatible game type, seed {seed}"
        );

        let mut oracle = environment(seed);
        oracle.step_many(generated_actions(seed, 12)).unwrap();
        oracle
            .enqueue_action_at(20, ActionSource::Test, AgentAction::Jump)
            .unwrap();
        advance_pair(
            &mut env,
            &mut oracle,
            &generated_actions(seed + 300, 10),
            "future after rejected operations",
        );
    }
}
