use bevy_agent_core::{AgentAction, LastStepResponse, Observation};
use bevy_agent_remote::JsonRpcBridge;
use bevy_agent_replay::Timeline;
use bevy_agent_runner::{AgentApp, AgentEnvironment, ResetOptions};

fn make_env() -> AgentApp {
    AgentApp::new(sample_platformer::build_headless_app)
}

fn player_x(observation: &Observation) -> f32 {
    match observation {
        Observation::Hybrid { symbolic, .. } | Observation::Symbolic(symbolic) => {
            symbolic.player.position[0]
        }
        other => panic!("unexpected observation: {other:?}"),
    }
}

fn player_y(observation: &Observation) -> f32 {
    match observation {
        Observation::Hybrid { symbolic, .. } | Observation::Symbolic(symbolic) => {
            symbolic.player.position[1]
        }
        other => panic!("unexpected observation: {other:?}"),
    }
}

fn checksum(env: &AgentApp) -> u64 {
    env.world()
        .resource::<LastStepResponse>()
        .0
        .as_ref()
        .and_then(|response| response.checksum.as_ref())
        .map(|checksum| checksum.hash)
        .expect("step response checksum")
}

#[test]
fn step_increments_tick_by_one() {
    let mut env = make_env();
    env.reset(ResetOptions::default()).unwrap();

    let response = env.step(AgentAction::Noop).unwrap();

    assert_eq!(response.tick, 1);
    assert_eq!(env.current_tick(), 1);
}

#[test]
fn queued_future_action_applies_on_correct_tick_and_once() {
    let mut env = make_env();
    let initial = env.reset(ResetOptions::default()).unwrap();
    let initial_x = player_x(&initial);

    env.enqueue_action_at(
        2,
        bevy_agent_core::ActionSource::Test,
        AgentAction::Move { x: 1.0, y: 0.0 },
    );
    let tick_1 = env.step(AgentAction::Noop).unwrap();
    let tick_2 = env.step(AgentAction::Noop).unwrap();
    let tick_3 = env.step(AgentAction::Noop).unwrap();

    assert_eq!(tick_1.info.actions_applied, 1);
    assert_eq!(tick_2.info.actions_applied, 2);
    assert_eq!(tick_3.info.actions_applied, 1);
    assert_eq!(player_x(&tick_1.observation), initial_x);
    assert!(player_x(&tick_2.observation) > player_x(&tick_1.observation));
    assert_eq!(player_x(&tick_3.observation), player_x(&tick_2.observation));
}

#[test]
fn step_many_matches_repeated_step() {
    let actions = vec![
        AgentAction::Move { x: 1.0, y: 0.0 },
        AgentAction::Move { x: 1.0, y: 0.0 },
        AgentAction::Jump,
        AgentAction::Noop,
    ];

    let mut env_many = make_env();
    env_many.reset(ResetOptions::default()).unwrap();
    let many = env_many.step_many(actions.clone()).unwrap();

    let mut env_repeated = make_env();
    env_repeated.reset(ResetOptions::default()).unwrap();
    let repeated = actions
        .into_iter()
        .map(|action| env_repeated.step(action).unwrap())
        .collect::<Vec<_>>();

    assert_eq!(many.len(), repeated.len());
    assert_eq!(
        many.last().unwrap().checksum,
        repeated.last().unwrap().checksum
    );
    assert_eq!(
        player_x(&many.last().unwrap().observation),
        player_x(&repeated.last().unwrap().observation)
    );
}

#[test]
fn snapshot_restore_exact_state() {
    let mut env = make_env();
    env.reset(ResetOptions::default()).unwrap();
    for _ in 0..10 {
        env.step(AgentAction::Move { x: 1.0, y: 0.0 }).unwrap();
    }

    let before_hash = checksum(&env);
    let before_observation = env
        .observe(bevy_agent_core::ObservationMode::Hybrid)
        .unwrap();
    let snapshot = env.snapshot().unwrap();

    for _ in 0..20 {
        env.step(AgentAction::Noop).unwrap();
    }
    assert_ne!(checksum(&env), before_hash);

    env.restore(snapshot.snapshot_id).unwrap();
    let restored_hash = checksum(&env);
    let restored_observation = env
        .observe(bevy_agent_core::ObservationMode::Hybrid)
        .unwrap();

    assert_eq!(restored_hash, before_hash);
    assert_eq!(
        player_x(&restored_observation),
        player_x(&before_observation)
    );
    assert_eq!(
        player_y(&restored_observation),
        player_y(&before_observation)
    );
}

#[test]
fn restore_then_step_matches_original() {
    let mut env = make_env();
    env.reset(ResetOptions::default()).unwrap();
    for _ in 0..15 {
        env.step(AgentAction::Move { x: 1.0, y: 0.0 }).unwrap();
    }
    let snapshot = env.snapshot().unwrap();

    let original = env.step(AgentAction::Jump).unwrap();
    env.restore(snapshot.snapshot_id).unwrap();
    let replayed = env.step(AgentAction::Jump).unwrap();

    assert_eq!(original.checksum, replayed.checksum);
    assert_eq!(
        player_y(&original.observation),
        player_y(&replayed.observation)
    );
}

#[test]
fn replay_matches_recording() {
    let actions = (0..120)
        .map(|tick| {
            if tick == 35 {
                AgentAction::Jump
            } else {
                AgentAction::Move { x: 1.0, y: 0.0 }
            }
        })
        .collect::<Vec<_>>();

    let mut env1 = make_env();
    env1.reset(ResetOptions::default()).unwrap();
    for action in actions.clone() {
        env1.step(action).unwrap();
    }
    let final_1 = checksum(&env1);

    let mut env2 = make_env();
    env2.reset(ResetOptions::default()).unwrap();
    for record in env1.replay_log().unwrap().records.clone() {
        env2.step(record.action).unwrap();
    }
    let final_2 = checksum(&env2);

    assert_eq!(final_1, final_2);
}

#[test]
fn branch_does_not_mutate_parent_timeline() {
    let mut env = make_env();
    env.reset(ResetOptions::default()).unwrap();
    env.step(AgentAction::Move { x: 1.0, y: 0.0 }).unwrap();
    env.step(AgentAction::Move { x: 1.0, y: 0.0 }).unwrap();
    env.snapshot().unwrap();

    let timeline_before = env.world().resource::<Timeline>().clone();
    let parent_branch = timeline_before.current_branch;
    let parent_actions_before = timeline_before
        .branches
        .get(&parent_branch)
        .unwrap()
        .actions
        .len();

    let child = env.branch(1, Some("alternate".to_string())).unwrap();
    env.step(AgentAction::Jump).unwrap();

    let timeline_after = env.world().resource::<Timeline>();
    assert_ne!(child, parent_branch);
    assert_eq!(
        timeline_after
            .branches
            .get(&parent_branch)
            .unwrap()
            .actions
            .len(),
        parent_actions_before
    );
    assert_eq!(
        timeline_after.branches.get(&child).unwrap().parent_branch,
        Some(parent_branch)
    );
}

#[test]
fn remote_step_returns_valid_schema() {
    let mut env = make_env();
    let bridge = JsonRpcBridge::default();

    let response = bridge.handle_json(
        &mut env,
        r#"{
            "jsonrpc": "2.0",
            "id": 1,
            "method": "agent.step",
            "params": {
                "action": { "type": "Move", "x": 1.0, "y": 0.0 },
                "observation_mode": "Hybrid"
            }
        }"#,
    );
    let value: serde_json::Value = serde_json::from_str(&response).unwrap();

    assert_eq!(value["jsonrpc"], "2.0");
    assert_eq!(value["id"], 1);
    assert_eq!(value["result"]["tick"], 1);
    assert_eq!(value["result"]["done"], false);
    assert!(value["result"]["observation"].is_object());
    assert!(value["result"]["checksum"]["hash"].is_u64());
}
