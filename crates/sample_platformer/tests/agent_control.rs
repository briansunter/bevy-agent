use bevy_agent_core::{
    ActionSource, AgentAction, AgentActionQueue, AgentControlState, ControlMode, LastStepResponse,
    Observation,
};
use bevy_agent_remote::{AgentCapability, JsonRpcBridge, RemoteSecurity};
use bevy_agent_replay::{ReplayRecorder, Timeline};
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
    assert_eq!(
        action["result"]["actions"],
        serde_json::json!(["Noop", "Move", "Jump", "Dodge"])
    );

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
    let artifact_root = capture_temp_dir();
    std::fs::create_dir_all(&artifact_root).unwrap();
    let bridge = JsonRpcBridge::new(RemoteSecurity {
        artifact_root: Some(artifact_root.clone()),
        capabilities: AgentCapability::default() | AgentCapability::FILESYSTEM,
        allow_absolute_paths: true,
        ..Default::default()
    });
    bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":1,"method":"agent.reset","params":{"options":{"seed":7,"observation_mode":"Hybrid","create_initial_snapshot":true}}}"#,
    );
    bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":2,"method":"agent.step_many","params":{"actions":[{"type":"Move","x":1.0,"y":0.0},{"type":"Jump"}],"return_observations":"last"}}"#,
    );

    let path = artifact_root.join("replay.json");
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

    let _ = std::fs::remove_dir_all(&artifact_root);
}

#[test]
fn portable_replay_bundle_restores_in_a_fresh_environment() {
    let mut source = make_env();
    source.reset(ResetOptions::default()).unwrap();
    source.step(AgentAction::Move { x: 1.0, y: 0.0 }).unwrap();
    let expected = source.step(AgentAction::Jump).unwrap().checksum.unwrap();
    let bundle = source.export_replay_bundle().unwrap();

    assert_eq!(bundle.log.manifest.game_id, "sample_platformer");
    assert!(!bundle.snapshots.is_empty());

    let mut fresh = make_env();
    fresh
        .reset(ResetOptions {
            create_initial_snapshot: false,
            ..Default::default()
        })
        .unwrap();
    fresh.load_replay_bundle(bundle).unwrap();
    fresh.restore_tick(2).unwrap();

    let restored = fresh
        .world()
        .resource::<LastStepResponse>()
        .0
        .as_ref()
        .and_then(|response| response.checksum.clone())
        .unwrap();
    assert_eq!(restored, expected);
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
    let artifact_root = capture_temp_dir();
    std::fs::create_dir_all(&artifact_root).unwrap();
    let bridge = JsonRpcBridge::new(RemoteSecurity {
        artifact_root: Some(artifact_root.clone()),
        capabilities: AgentCapability::default() | AgentCapability::FILESYSTEM,
        allow_absolute_paths: true,
        ..Default::default()
    });
    bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":1,"method":"agent.reset","params":{"options":{"seed":1,"observation_mode":"Hybrid","create_initial_snapshot":true}}}"#,
    );
    bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":2,"method":"agent.step","params":{"action":{"type":"Move","x":1.0,"y":0.0}}}"#,
    );

    let output_dir = artifact_root.join("capture");
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

    let _ = std::fs::remove_dir_all(&artifact_root);
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
    assert_eq!(info["result"]["name"], "sample_platformer");
    assert_eq!(info["result"]["agent_control_version"], "0.1.0");

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
    assert_eq!(all["result"]["responses"].as_array().unwrap().len(), 2);
    assert!(all["result"]["observation"].is_object());
    assert!(all["result"]["info"].is_object());
    assert!(all["result"]["reward"].is_number());
    assert_eq!(all["result"]["truncated"], false);

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
            "params": { "bundle": export["result"]["bundle"].clone() }
        })
        .to_string(),
    );
    let load: serde_json::Value = serde_json::from_str(&load).unwrap();
    assert!(load["result"]["records"].as_u64().unwrap() >= 1);
}

#[test]
fn reset_clears_replay_log_and_starts_root_timeline_preserving_recording() {
    let mut env = make_env();
    env.reset(ResetOptions::default()).unwrap();
    env.step(AgentAction::Move { x: 1.0, y: 0.0 }).unwrap();
    env.step(AgentAction::Jump).unwrap();
    let records_before = env.replay_log().unwrap().records.len();
    assert!(records_before >= 2);

    // Flip recording off and capture the current timeline identity.
    env.world_mut().resource_mut::<ReplayRecorder>().recording = false;
    let timeline_before = env.world().resource::<Timeline>().timeline_id;

    env.reset(ResetOptions::default()).unwrap();

    // The recording flag is preserved across the episode boundary.
    assert!(!env.world().resource::<ReplayRecorder>().recording);
    // Prior replay records are cleared (the fresh initial snapshot seeds the log
    // metadata but adds no step records).
    assert!(env.replay_log().unwrap().records.is_empty());

    // A fresh timeline root was started and the control state tracks it.
    let timeline = env.world().resource::<Timeline>();
    let control = env.world().resource::<AgentControlState>();
    assert_ne!(timeline.timeline_id, timeline_before);
    assert_eq!(timeline.branches.len(), 1);
    assert_eq!(
        timeline
            .branches
            .get(&timeline.current_branch)
            .unwrap()
            .parent_branch,
        None
    );
    assert_eq!(timeline.timeline_id, control.timeline_id);
    assert_eq!(timeline.current_branch, control.branch_id);
}

#[test]
fn reset_response_reports_the_new_root_timeline() {
    let mut env = make_env();
    let reset = env.reset_with_response(ResetOptions::default()).unwrap();
    let control = env.world().resource::<AgentControlState>();

    assert_eq!(reset.timeline_id, control.timeline_id);
    assert_eq!(reset.branch_id, control.branch_id);
    let last = env
        .world()
        .resource::<LastStepResponse>()
        .0
        .as_ref()
        .unwrap();
    assert_eq!(last.info.timeline_id, control.timeline_id);
    assert_eq!(last.info.branch_id, control.branch_id);
}

#[test]
fn restore_realigns_frame_to_next_tick() {
    let mut env = make_env();
    env.reset(ResetOptions::default()).unwrap();
    for _ in 0..10 {
        env.step(AgentAction::Move { x: 1.0, y: 0.0 }).unwrap();
    }
    let snapshot = env.snapshot().unwrap();
    // Advance well past the snapshot so the frame would otherwise drift forward.
    for _ in 0..8 {
        env.step(AgentAction::Noop).unwrap();
    }

    env.restore(snapshot.snapshot_id).unwrap();
    let response = env.step(AgentAction::Noop).unwrap();

    // After restore, the next step's frame realigns to its tick.
    assert_eq!(response.tick, snapshot.tick + 1);
    assert_eq!(response.info.frame, response.tick);
}

#[test]
fn restore_tick_reproduces_multi_action_tick_without_appending_records() {
    let mut env = make_env();
    env.reset(ResetOptions::default()).unwrap();
    // Two opposing moves at the same tick make the outcome order-sensitive: the
    // last move wins for horizontal velocity.
    env.enqueue_action_at(1, ActionSource::Agent, AgentAction::Move { x: 1.0, y: 0.0 });
    env.enqueue_action_at(1, ActionSource::Test, AgentAction::Move { x: -1.0, y: 0.0 });
    let original = env.step(AgentAction::Noop).unwrap();
    // The two enqueued moves plus the step's Noop all land on tick 1.
    assert_eq!(original.info.actions_applied, 3);
    let checksum = original.checksum.clone().expect("checksum");
    let records_before = env.replay_log().unwrap().records.len();

    env.restore_tick(1).unwrap();

    let restored = env
        .world()
        .resource::<LastStepResponse>()
        .0
        .clone()
        .expect("restored response");
    assert_eq!(restored.tick, 1);
    assert_eq!(restored.info.actions_applied, original.info.actions_applied);
    assert_eq!(restored.checksum, Some(checksum));
    // The replay was not appended to the recorder log.
    assert_eq!(env.replay_log().unwrap().records.len(), records_before);
}

#[test]
fn restore_tick_preserves_pending_actions_after_target_tick() {
    let mut env = make_env();
    env.reset(ResetOptions::default()).unwrap();
    env.enqueue_action_at(5, ActionSource::Test, AgentAction::Jump);
    env.snapshot().unwrap();
    env.step(AgentAction::Noop).unwrap();

    env.restore_tick(1).unwrap();

    let queue = env.world().resource::<AgentActionQueue>();
    assert_eq!(queue.pending.len(), 1);
    assert_eq!(queue.pending.front().unwrap().tick, 5);
}

#[test]
fn paused_and_inspect_only_modes_block_step_without_advancing() {
    let mut env = make_env();
    env.reset(ResetOptions::default()).unwrap();
    let tick_before = env.current_tick();

    env.world_mut().resource_mut::<AgentControlState>().mode = ControlMode::Paused;
    let paused_error = env.step(AgentAction::Move { x: 1.0, y: 0.0 }).unwrap_err();
    assert_eq!(env.current_tick(), tick_before);
    assert!(paused_error.to_string().contains("Paused"));

    env.world_mut().resource_mut::<AgentControlState>().mode = ControlMode::InspectOnly;
    let inspect_error = env.step(AgentAction::Move { x: 1.0, y: 0.0 }).unwrap_err();
    assert_eq!(env.current_tick(), tick_before);
    assert!(inspect_error.to_string().contains("InspectOnly"));
}

#[allow(dead_code)]
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

// ---- Adversarial coverage (append-only; existing tests untouched) ----

#[test]
fn adversarial_parent_future_excluded_from_child_restore_state() {
    // Parent checkpoints at tick 3 and tick 6; child forks at tick 5.
    // A tick-6 parent checkpoint is parent-future beyond the fork and must
    // never be selected for the child: the child must resolve to its own
    // fork checkpoint (tick 5) or the tick-3 ancestor.
    let mut env = make_env();
    env.reset(ResetOptions::default()).unwrap();
    for _ in 0..3 {
        env.step(AgentAction::Noop).unwrap();
    }
    let parent_early = env.snapshot().unwrap().snapshot_id;
    for _ in 0..3 {
        env.step(AgentAction::Noop).unwrap();
    }
    assert_eq!(env.current_tick(), 6);
    let parent_late = env.snapshot().unwrap().snapshot_id;

    let child = env.branch(5, Some("child-exclusion".to_string())).unwrap();
    assert_eq!(env.current_tick(), 5);

    let (log, timeline) = {
        let log = env.replay_log().unwrap().clone();
        let timeline = env.world().resource::<Timeline>().clone();
        (log, timeline)
    };
    let selected = log
        .nearest_checkpoint_for_branch(&timeline, child, 6)
        .expect("child must resolve a checkpoint at-or-before tick 6");
    // Parent-future (tick 6 on the parent, beyond fork 5) is excluded.
    assert_ne!(
        selected.1, parent_late,
        "child must not select parent-future checkpoint beyond fork"
    );
    // The winner is the child's own fork checkpoint at tick 5.
    assert_eq!(selected.0, 5);
    let child_fork: Vec<_> = log
        .branch_checkpoints
        .iter()
        .filter(|c| c.branch_id == child && c.tick == 5)
        .collect();
    assert_eq!(child_fork.len(), 1);
    assert_eq!(selected.1, child_fork[0].snapshot_id);

    // State check: restoring tick 6 on the child lands on tick 6 on the
    // child branch (replaying from the fork), not on parent state.
    env.step(AgentAction::Noop).unwrap(); // child tick 6
    env.restore_tick(6).unwrap();
    assert_eq!(env.current_tick(), 6);
    assert_eq!(env.world().resource::<AgentControlState>().branch_id, child);
    // Parent early checkpoint is still addressable on the parent lineage.
    let parent = timeline
        .branches
        .get(&child)
        .unwrap()
        .parent_branch
        .unwrap();
    let parent_selected = log
        .nearest_checkpoint_for_branch(&timeline, parent, 6)
        .unwrap();
    assert_eq!(parent_selected.1, parent_late);
    let _ = parent_early;
}

#[test]
fn adversarial_retention_1_returns_surviving_id() {
    let mut env = make_env();
    env.reset(ResetOptions::default()).unwrap();
    // Retention-1: only the newest checkpoint may survive (plus pins).
    env.world_mut()
        .resource_mut::<bevy_agent_snapshot::SnapshotPolicy>()
        .keep_last_n_checkpoints = 1;
    // Unpin the initial reset snapshot so this exercises pure eviction.
    // Note: snapshots referenced by the live replay log are still protected;
    // retention may exceed the limit rather than leave dangling references.
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

    let s1 = env.snapshot().unwrap().snapshot_id;
    let s2 = env.snapshot().unwrap().snapshot_id;
    let s3 = env.snapshot().unwrap().snapshot_id;

    let store = env.world().resource::<bevy_agent_snapshot::SnapshotStore>();
    assert!(
        store.snapshots.contains_key(&s3),
        "retention-1 must return surviving id"
    );
    // Surviving id restores.
    let _ = store;
    env.restore(s3).unwrap();
    // Export must remain valid: every replay-referenced snapshot exists.
    let bundle = env.export_replay_bundle().unwrap();
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
    let _ = (s1, s2);
}

#[test]
fn adversarial_reset_cross_episode_isolation() {
    let mut env = make_env();
    env.reset(ResetOptions::default()).unwrap();
    env.step(AgentAction::Move { x: 1.0, y: 0.0 }).unwrap();
    env.step(AgentAction::Jump).unwrap();
    // Poison episode state: terminal + reward + future action + recording off.
    env.world_mut()
        .resource_mut::<bevy_agent_core::EpisodeState>()
        .done = true;
    env.world_mut()
        .resource_mut::<bevy_agent_core::EpisodeState>()
        .reason = Some("poisoned".to_string());
    env.world_mut()
        .resource_mut::<bevy_agent_core::RewardState>()
        .cumulative_reward = 999.0;
    env.enqueue_action_at(99, ActionSource::Test, AgentAction::Jump);
    let timeline_before = env.world().resource::<Timeline>().timeline_id;
    let records_before = env.replay_log().unwrap().records.len();
    assert!(records_before >= 2);

    env.reset(ResetOptions::default()).unwrap();

    // Fresh episode: tick 0, no terminal, no reward leak, no queued future,
    // empty replay records, fresh timeline root tracked by control state.
    assert_eq!(env.current_tick(), 0);
    assert!(!env.world().resource::<bevy_agent_core::EpisodeState>().done);
    assert!(
        env.world()
            .resource::<bevy_agent_core::EpisodeState>()
            .reason
            .is_none()
    );
    assert_eq!(
        env.world()
            .resource::<bevy_agent_core::RewardState>()
            .cumulative_reward,
        0.0
    );
    assert!(
        env.world()
            .resource::<AgentActionQueue>()
            .pending
            .is_empty()
    );
    assert!(env.replay_log().unwrap().records.is_empty());
    let timeline = env.world().resource::<Timeline>();
    let control = env.world().resource::<AgentControlState>();
    assert_ne!(timeline.timeline_id, timeline_before);
    assert_eq!(timeline.timeline_id, control.timeline_id);
    assert_eq!(timeline.current_branch, control.branch_id);
}

#[test]
fn adversarial_first_step_restricted_capability_player_only() {
    // Fresh env + STEP|OBSERVE_PLAYER requesting PlayerKnowledge must return
    // Symbolic/player-only with no Hybrid secret (no debug/pixels).
    // Note: the env is explicitly reset with PlayerKnowledge first so the
    // first step (tick 1) renders under the requested mode; a step on a
    // never-reset env would auto-reset with the default Hybrid mode via
    // `ensure_reset` before the per-request mode applies (runner behavior).
    let mut env = make_env();
    let bridge = JsonRpcBridge::new(RemoteSecurity {
        capabilities: AgentCapability::STEP | AgentCapability::OBSERVE_PLAYER,
        ..Default::default()
    });
    let reset_response = bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":0,"method":"agent.reset","params":{"options":{"seed":0,"observation_mode":"PlayerKnowledge","create_initial_snapshot":true}}}"#,
    );
    let reset_value: serde_json::Value = serde_json::from_str(&reset_response).unwrap();
    assert!(
        reset_value.get("error").is_none(),
        "PlayerKnowledge reset must be allowed: {reset_value}"
    );
    let response = bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":1,"method":"agent.step","params":{"action":{"type":"Noop"},"observation_mode":"PlayerKnowledge"}}"#,
    );
    let value: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert!(
        value.get("error").is_none(),
        "PlayerKnowledge must be allowed: {value}"
    );
    assert_eq!(value["result"]["tick"], 1);
    let obs = &value["result"]["observation"];
    assert_eq!(
        obs["kind"], "Symbolic",
        "must be player-only Symbolic: {obs}"
    );
    assert!(
        obs.get("debug").is_none() || obs["debug"].is_null(),
        "no Hybrid debug secret may leak: {obs}"
    );
    assert!(
        obs.get("pixels").is_none() || obs["pixels"].is_null(),
        "no pixels may leak: {obs}"
    );
    assert!(obs["player"].is_object(), "player block must exist: {obs}");

    // Same restricted bridge requesting Hybrid must be rejected (needs
    // OBSERVE_FULL_STATE).
    let denied = bridge.handle_json(
        &mut env,
        r#"{"jsonrpc":"2.0","id":2,"method":"agent.step","params":{"action":{"type":"Noop"},"observation_mode":"Hybrid"}}"#,
    );
    let denied_value: serde_json::Value = serde_json::from_str(&denied).unwrap();
    assert!(
        denied_value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("missing remote capability"),
        "Hybrid must require OBSERVE_FULL_STATE: {denied_value}"
    );
}

#[test]
fn adversarial_reconstruction_forces_multi_tick_replay_from_checkpoint_0() {
    // Only checkpoint 0 exists (interval disabled); restoring a later tick
    // must replay every tick in (0, target] — a true multi-tick replay.
    let mut env = make_env();
    env.reset(ResetOptions::default()).unwrap();
    env.world_mut()
        .resource_mut::<bevy_agent_snapshot::SnapshotPolicy>()
        .checkpoint_every_ticks = 0;
    let mut expected_checksums = Vec::new();
    for _ in 0..5 {
        let response = env.step(AgentAction::Move { x: 1.0, y: 0.0 }).unwrap();
        expected_checksums.push((response.tick, response.checksum.clone()));
    }
    assert_eq!(env.current_tick(), 5);
    let records_before = env.replay_log().unwrap().records.len();
    assert_eq!(records_before, 5);

    // Restore tick 3 from checkpoint 0: replays ticks 1..3 (multi-tick).
    env.restore_tick(3).unwrap();
    assert_eq!(env.current_tick(), 3);
    let restored = env
        .world()
        .resource::<LastStepResponse>()
        .0
        .clone()
        .expect("restored response");
    assert_eq!(restored.tick, 3);
    assert_eq!(restored.checksum, expected_checksums[2].1);
    // Reconstruction appends no records.
    assert_eq!(env.replay_log().unwrap().records.len(), records_before);

    // Restore tick 1 from checkpoint 0: single-tick boundary of same path.
    env.restore_tick(1).unwrap();
    assert_eq!(env.current_tick(), 1);
    let restored_1 = env
        .world()
        .resource::<LastStepResponse>()
        .0
        .clone()
        .expect("restored response");
    assert_eq!(restored_1.checksum, expected_checksums[0].1);
}

#[test]
fn adversarial_checksum_post_prepare_failure_valid_decode_mismatch() {
    // Corrupt a component value to another *valid* value (passes typed
    // preflight decode) while keeping the stored checksum: restore must fail
    // at the checksum precondition with world state unchanged.
    // Implemented via the snapshot crate public API
    // (capture/store/checksum/restore_snapshot_value).
    let mut env = make_env();
    env.reset(ResetOptions::default()).unwrap();
    let created = env.snapshot().unwrap();
    let before = bevy_agent_snapshot::capture_snapshot(env.world_mut(), None).unwrap();
    let before_hash = bevy_agent_snapshot::checksum_snapshot(&before)
        .unwrap()
        .hash;

    let mut tampered = env
        .world()
        .resource::<bevy_agent_snapshot::SnapshotStore>()
        .snapshots
        .get(&created.snapshot_id)
        .cloned()
        .expect("snapshot in store");
    // Find the Player component ({"health": 100.0}) and bump health to a
    // different but still valid float.
    let mut mutated = false;
    for entity in &mut tampered.entities {
        for component in &mut entity.components {
            if component.type_name.contains("Player") && component.value.get("health").is_some() {
                component.value["health"] = serde_json::json!(1.0);
                mutated = true;
            }
        }
    }
    assert!(mutated, "expected a Player component to corrupt");
    // Stored checksum intentionally left as the original so the payload now
    // mismatches: decode passes, checksum precondition fails.

    let error =
        bevy_agent_snapshot::restore_snapshot_value(env.world_mut(), &tampered).unwrap_err();
    assert!(
        error.to_string().contains("checksum"),
        "must fail at checksum, got: {error}"
    );

    let after = bevy_agent_snapshot::capture_snapshot(env.world_mut(), None).unwrap();
    assert_eq!(
        bevy_agent_snapshot::checksum_snapshot(&after).unwrap().hash,
        before_hash,
        "failed restore must leave world unchanged"
    );
}
