use bevy_agent_core::{AgentAction, LastStepResponse, Observation};
use bevy_agent_remote::{AgentCapability, JsonRpcBridge, RemoteSecurity};
use bevy_agent_replay::Timeline;
use bevy_agent_runner::{AgentApp, AgentEnvironment, ResetOptions};
use std::path::PathBuf;

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

#[test]
fn remote_action_and_observation_spaces_include_json_schema() {
    let mut env = make_env();
    let bridge = JsonRpcBridge::default();

    let action_response = bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":1,"method":"agent.action_space","params":{}}"#,
    );
    let action: serde_json::Value = serde_json::from_str(&action_response).unwrap();
    assert_eq!(action["result"]["type"], "json_schema");
    assert!(action["result"]["schema"]["oneOf"].is_array());

    let observation_response = bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":2,"method":"agent.observation_space","params":{}}"#,
    );
    let observation: serde_json::Value = serde_json::from_str(&observation_response).unwrap();
    assert_eq!(observation["result"]["default"], "Hybrid");
    assert!(observation["result"]["schema"]["$defs"]["player"].is_object());
}

#[test]
fn remote_replay_export_and_load_round_trip() {
    let mut env = make_env();
    let bridge = JsonRpcBridge::default();
    bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":1,"method":"agent.reset","params":{"options":{"seed":7,"observation_mode":"Hybrid","create_initial_snapshot":true}}}"#,
    );
    bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":2,"method":"agent.step_many","params":{"actions":[{"type":"Move","x":1.0,"y":0.0},{"type":"Jump"}],"return_observations":"last"}}"#,
    );

    let path = replay_temp_path();
    let export_request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "agent.replay.export",
        "params": { "path": path }
    });
    let export_response = bridge.handle_json(&mut env, &export_request.to_string());
    let export: serde_json::Value = serde_json::from_str(&export_response).unwrap();
    assert_eq!(export["result"]["records"], 2);

    let load_request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "agent.replay.load",
        "params": { "path": export["result"]["path"].as_str().unwrap() }
    });
    let load_response = bridge.handle_json(&mut env, &load_request.to_string());
    let load: serde_json::Value = serde_json::from_str(&load_response).unwrap();
    assert_eq!(load["result"]["records"], 2);

    let _ = std::fs::remove_file(export["result"]["path"].as_str().unwrap());
}

#[test]
fn fast_forward_advances_noop_ticks_and_errors_on_zero() {
    let mut env = make_env();
    env.reset(ResetOptions::default()).unwrap();

    let response = env.fast_forward(3).unwrap();
    assert_eq!(response.tick, 3);

    let error = env.fast_forward(0).unwrap_err();
    assert!(error.to_string().contains("zero ticks"));
}

#[test]
fn remote_snapshot_list_and_delete_update_store() {
    let mut env = make_env();
    let bridge = JsonRpcBridge::default();
    bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":1,"method":"agent.reset","params":{"options":{"seed":1,"observation_mode":"Hybrid","create_initial_snapshot":true}}}"#,
    );
    let snapshot_response = bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":2,"method":"agent.snapshot.create","params":{}}"#,
    );
    let snapshot: serde_json::Value = serde_json::from_str(&snapshot_response).unwrap();
    let snapshot_id = snapshot["result"]["snapshot_id"].clone();

    let list_response = bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":3,"method":"agent.snapshot.list","params":{}}"#,
    );
    let list: serde_json::Value = serde_json::from_str(&list_response).unwrap();
    assert!(list["result"].as_array().unwrap().len() >= 2);

    let delete_response = bridge.handle_json(
        &mut env,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "agent.snapshot.delete",
            "params": { "snapshot_id": snapshot_id }
        })
        .to_string(),
    );
    let delete: serde_json::Value = serde_json::from_str(&delete_response).unwrap();
    assert!(delete["result"].is_null());
}

#[test]
fn remote_rejects_missing_token_and_missing_capability() {
    let mut env = make_env();
    let token_bridge = JsonRpcBridge::new(RemoteSecurity {
        session_token: Some("secret".to_string()),
        ..Default::default()
    });
    let token_response = token_bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":1,"method":"agent.step","params":{"action":{"type":"Noop"}}}"#,
    );
    let token_error: serde_json::Value = serde_json::from_str(&token_response).unwrap();
    assert!(
        token_error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("session token")
    );

    let capability_bridge = JsonRpcBridge::new(RemoteSecurity {
        capabilities: AgentCapability::OBSERVE_PLAYER,
        ..Default::default()
    });
    let capability_response = capability_bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":2,"method":"agent.step","params":{"action":{"type":"Noop"}}}"#,
    );
    let capability_error: serde_json::Value = serde_json::from_str(&capability_response).unwrap();
    assert!(
        capability_error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("missing remote capability")
    );
}

#[test]
fn remote_visual_capture_writes_png_file() {
    let mut env = make_env();
    let bridge = JsonRpcBridge::default();
    bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":1,"method":"agent.reset","params":{"options":{"seed":1,"observation_mode":"Hybrid","create_initial_snapshot":true}}}"#,
    );
    bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":2,"method":"agent.step","params":{"action":{"type":"Move","x":1.0,"y":0.0}}}"#,
    );

    let output_dir = capture_temp_dir();
    let response = bridge.handle_json(
        &mut env,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "agent.visual.capture",
            "params": {
                "output_dir": output_dir,
                "label": "after step"
            }
        })
        .to_string(),
    );
    let value: serde_json::Value = serde_json::from_str(&response).unwrap();
    let path = PathBuf::from(value["result"]["path"].as_str().unwrap());

    assert_eq!(value["result"]["tick"], 1);
    assert_eq!(value["result"]["format"], "png");
    assert!(value["result"]["width"].as_u64().unwrap() > 0);
    assert!(std::fs::metadata(&path).unwrap().len() > 0);

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn remote_visual_capture_requires_capability() {
    let mut env = make_env();
    let bridge = JsonRpcBridge::new(RemoteSecurity {
        capabilities: AgentCapability::STEP,
        ..Default::default()
    });
    let response = bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":1,"method":"agent.visual.capture","params":{}}"#,
    );
    let value: serde_json::Value = serde_json::from_str(&response).unwrap();

    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("missing remote capability")
    );
}

#[test]
fn remote_control_pause_resume_and_set_mode_update_control_state() {
    let mut env = make_env();
    let bridge = JsonRpcBridge::default();

    bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":1,"method":"agent.control.pause","params":{}}"#,
    );
    assert!(matches!(
        env.world()
            .resource::<bevy_agent_core::AgentControlState>()
            .mode,
        bevy_agent_core::ControlMode::Paused
    ));

    bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":2,"method":"agent.control.resume","params":{}}"#,
    );
    assert!(matches!(
        env.world()
            .resource::<bevy_agent_core::AgentControlState>()
            .mode,
        bevy_agent_core::ControlMode::Agent
    ));

    bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":3,"method":"agent.control.set_mode","params":{"mode":"InspectOnly"}}"#,
    );
    assert!(matches!(
        env.world()
            .resource::<bevy_agent_core::AgentControlState>()
            .mode,
        bevy_agent_core::ControlMode::InspectOnly
    ));
}

#[test]
fn remote_replay_start_and_stop_reset_recording_log() {
    let mut env = make_env();
    let bridge = JsonRpcBridge::default();
    bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":1,"method":"agent.reset","params":{"options":{"seed":1,"observation_mode":"Hybrid","create_initial_snapshot":true}}}"#,
    );
    bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":2,"method":"agent.step","params":{"action":{"type":"Move","x":1.0,"y":0.0}}}"#,
    );

    let start = bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":3,"method":"agent.replay.start","params":{}}"#,
    );
    let start_value: serde_json::Value = serde_json::from_str(&start).unwrap();
    assert_eq!(start_value["result"]["recording"], true);

    bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":4,"method":"agent.step","params":{"action":{"type":"Jump"}}}"#,
    );
    let stop = bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":5,"method":"agent.replay.stop","params":{}}"#,
    );
    let stop_value: serde_json::Value = serde_json::from_str(&stop).unwrap();
    assert_eq!(stop_value["result"]["recording"], false);
    assert_eq!(stop_value["result"]["records"], 1);
}

#[test]
fn remote_misc_methods_cover_info_schema_observe_fast_forward_and_errors() {
    let mut env = make_env();
    let bridge = JsonRpcBridge::default();

    let parse_error = bridge.handle_json(&mut env, "{not-json");
    let parse_error: serde_json::Value = serde_json::from_str(&parse_error).unwrap();
    assert_eq!(parse_error["error"]["code"], -32700);

    let unknown = bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":1,"method":"agent.unknown","params":{}}"#,
    );
    let unknown: serde_json::Value = serde_json::from_str(&unknown).unwrap();
    assert!(
        unknown["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown method")
    );

    let info = bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":2,"method":"agent.info","params":{}}"#,
    );
    let info: serde_json::Value = serde_json::from_str(&info).unwrap();
    assert_eq!(info["result"]["name"], "bevy_agent_control");

    let schema = bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":3,"method":"agent.schema","params":{}}"#,
    );
    let schema: serde_json::Value = serde_json::from_str(&schema).unwrap();
    assert_eq!(schema["result"]["step_response"]["title"], "StepResponse");

    bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":4,"method":"agent.reset","params":{"options":{"seed":1,"observation_mode":"Hybrid","create_initial_snapshot":true}}}"#,
    );
    let observe = bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":5,"method":"agent.observe","params":{"observation_mode":"PlayerKnowledge"}}"#,
    );
    let observe: serde_json::Value = serde_json::from_str(&observe).unwrap();
    assert_eq!(observe["result"]["kind"], "Symbolic");

    let fast_forward = bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":6,"method":"agent.fast_forward","params":{"ticks":2}}"#,
    );
    let fast_forward: serde_json::Value = serde_json::from_str(&fast_forward).unwrap();
    assert_eq!(fast_forward["result"]["tick"], 2);

    let current = bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":7,"method":"agent.timeline.current","params":{}}"#,
    );
    let current: serde_json::Value = serde_json::from_str(&current).unwrap();
    assert_eq!(current["result"]["tick"], 2);
}

#[test]
fn remote_step_many_return_modes_and_restore_tick_work() {
    let mut env = make_env();
    let bridge = JsonRpcBridge::default();
    bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":1,"method":"agent.reset","params":{"options":{"seed":1,"observation_mode":"Hybrid","create_initial_snapshot":true}}}"#,
    );

    let all = bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":2,"method":"agent.step_many","params":{"actions":[{"type":"Noop"},{"type":"Noop"}],"return_observations":"all"}}"#,
    );
    let all: serde_json::Value = serde_json::from_str(&all).unwrap();
    assert!(all["result"]["observation"].as_array().unwrap().len() == 2);

    let none = bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":3,"method":"agent.step_many","params":{"actions":[{"type":"Noop"}],"return_observations":"none"}}"#,
    );
    let none: serde_json::Value = serde_json::from_str(&none).unwrap();
    assert!(none["result"]["observation"].is_null());

    let restore = bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":4,"method":"agent.timeline.restore_tick","params":{"tick":1}}"#,
    );
    let restore: serde_json::Value = serde_json::from_str(&restore).unwrap();
    assert_eq!(restore["result"]["current_tick"], 1);
}

#[test]
fn remote_snapshot_restore_and_replay_load_inline_log_work() {
    let mut env = make_env();
    let bridge = JsonRpcBridge::default();
    bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":1,"method":"agent.reset","params":{"options":{"seed":1,"observation_mode":"Hybrid","create_initial_snapshot":true}}}"#,
    );
    let snapshot = bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":2,"method":"agent.snapshot.create","params":{}}"#,
    );
    let snapshot: serde_json::Value = serde_json::from_str(&snapshot).unwrap();
    let snapshot_id = snapshot["result"]["snapshot_id"].clone();

    bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":3,"method":"agent.step","params":{"action":{"type":"Move","x":1.0,"y":0.0}}}"#,
    );
    let restore = bridge.handle_json(
        &mut env,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "agent.snapshot.restore",
            "params": { "snapshot_id": snapshot_id }
        })
        .to_string(),
    );
    let restore: serde_json::Value = serde_json::from_str(&restore).unwrap();
    assert!(restore["result"].is_null());

    let export = bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":5,"method":"agent.replay.export","params":{}}"#,
    );
    let export: serde_json::Value = serde_json::from_str(&export).unwrap();
    let load = bridge.handle_json(
        &mut env,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "agent.replay.load",
            "params": { "log": export["result"]["log"].clone() }
        })
        .to_string(),
    );
    let load: serde_json::Value = serde_json::from_str(&load).unwrap();
    assert!(load["result"]["records"].as_u64().unwrap() >= 1);
}

fn replay_temp_path() -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "bevy-agent-replay-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    path
}

fn capture_temp_dir() -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "bevy-agent-capture-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    path
}
