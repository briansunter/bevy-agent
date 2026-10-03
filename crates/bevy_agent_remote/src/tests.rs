use super::*;
use crate::rpc::{
    constant_time_eq, parse_jsonrpc_request, read_replay_file, serialize_response,
    validate_jsonrpc_version,
};
use crate::security::require_safe_bind;
use crate::{http::*, websocket::*};
use bevy_agent_core::{
    AgentActionCatalog, AgentActionKind, AgentControlAppExt, AgentObservationCatalog,
    ObservationMode,
};
use serde_json::{Value, json};
use std::thread;
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    time::{Duration, Instant},
};

fn configure_test_identity(app: &mut bevy::app::App) {
    app.set_environment_metadata("remote-contract-test", "1.0", None);
    app.insert_checksum_extractor(|world| bevy_agent_core::default_checksum(world));
}

fn configure_test_observer(app: &mut bevy::app::App) {
    app.insert_observation_extractor(|world, mode| {
        let tick = world.resource::<bevy_agent_core::SimClock>().tick;
        match mode {
            ObservationMode::PlayerKnowledge => {
                bevy_agent_core::Observation::Symbolic(bevy_agent_core::SymbolicObservation {
                    tick,
                    ..Default::default()
                })
            }
            ObservationMode::Hybrid => bevy_agent_core::Observation::default_for_tick(tick),
            _ => bevy_agent_core::Observation::Error {
                message: "unsupported test mode".to_owned(),
            },
        }
    });
}

#[test]
fn jsonrpc_distinguishes_syntax_errors_from_invalid_envelopes() {
    let error = serde_json::to_value(parse_jsonrpc_request("{").unwrap_err()).unwrap();
    assert_eq!(error["error"]["code"], RPC_PARSE_ERROR);
    for request in [
        json!([]),
        json!(false),
        json!({"jsonrpc":"2.0","method":"agent.info"}),
        json!({"id":8,"method":"agent.info"}),
        json!({"jsonrpc":"2.0","id":8,"method":true}),
        json!({"jsonrpc":"2.0","id":[],"method":"agent.info"}),
        json!({"jsonrpc":"2.0","id":8,"method":"agent.info","params":9}),
    ] {
        let response =
            serde_json::to_value(parse_jsonrpc_request(&request.to_string()).unwrap_err()).unwrap();
        assert_eq!(response["error"]["code"], RPC_INVALID_REQUEST, "{request}");
    }
    let error = serde_json::to_value(
        parse_jsonrpc_request(r#"{"jsonrpc":"2.0","id":"request-42","method":9}"#).unwrap_err(),
    )
    .unwrap();
    assert_eq!(error["id"], "request-42");
}

#[test]
fn invalid_remote_parameters_do_not_initialize_the_environment() {
    use bevy::prelude::*;
    use bevy_agent_runner::{AgentApp, AgentControlPlugins};
    let mut env = AgentApp::new(|| {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(AgentControlPlugins::default());
        configure_test_identity(&mut app);
        configure_test_observer(&mut app);
        app.set_supported_actions([AgentActionKind::Noop, AgentActionKind::Move]);
        app.set_supported_observation_modes([
            ObservationMode::PlayerKnowledge,
            ObservationMode::Hybrid,
        ]);
        app
    })
    .unwrap();
    let bridge = JsonRpcBridge::default();
    for (method, params) in [
        (
            "agent.step_many",
            json!({"actions":[], "return_observations":"invalid"}),
        ),
        ("agent.fast_forward", json!({"ticks":0})),
        (
            "agent.step",
            json!({"action":{"type":"Move","x":2.0,"y":0.0}}),
        ),
        (
            "agent.replay.load",
            json!({"path":"a.json", "bundle_base64":""}),
        ),
        ("agent.replay.load", json!({"log":{}})),
    ] {
        let response = bridge.handle_json(
            &mut env,
            &json!({"jsonrpc":"2.0","id":7,"method":method,"params":params}).to_string(),
        );
        let response: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["error"]["code"], RPC_INVALID_PARAMS, "{response}");
        assert!(!env.has_reset(), "{method} initialized the environment");
        assert_eq!(env.current_tick(), 0);
    }
}

#[test]
fn observation_discovery_and_omitted_mode_use_the_games_configured_mode() {
    use bevy::prelude::*;
    use bevy_agent_core::{Observation, ObservationConfig, SimClock, SymbolicObservation};
    use bevy_agent_runner::{AgentApp, AgentControlPlugins};
    let mut env = AgentApp::new(|| {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(AgentControlPlugins::default());
        configure_test_identity(&mut app);
        app.set_supported_actions([AgentActionKind::Noop]);
        app.set_supported_observation_modes([ObservationMode::PlayerKnowledge]);
        app.insert_resource(ObservationConfig {
            mode: ObservationMode::PlayerKnowledge,
        });
        app.insert_observation_extractor(|world, mode| {
            assert_eq!(mode, ObservationMode::PlayerKnowledge);
            Observation::Symbolic(SymbolicObservation {
                tick: world.resource::<SimClock>().tick,
                ..Default::default()
            })
        });
        app
    })
    .unwrap();
    let bridge = JsonRpcBridge::default();
    let call = |env: &mut AgentApp, method, params| -> Value {
        serde_json::from_str(&bridge.handle_json(
            env,
            &json!({"jsonrpc":"2.0","id":7,"method":method,"params":params}).to_string(),
        ))
        .unwrap()
    };
    let discovery = call(&mut env, "agent.observation_space", json!({}));
    assert_eq!(discovery["result"]["modes"], json!(["PlayerKnowledge"]));
    assert_eq!(discovery["result"]["default"], "PlayerKnowledge");
    let observation = call(&mut env, "agent.observe", json!({}));
    assert_eq!(observation["result"]["kind"], "Symbolic", "{observation}");
    let unsupported = call(
        &mut env,
        "agent.observe",
        json!({"observation_mode":"PixelFrame"}),
    );
    assert_eq!(unsupported["error"]["code"], RPC_INVALID_PARAMS);
    assert_eq!(
        env.world().resource::<ObservationConfig>().mode,
        ObservationMode::PlayerKnowledge
    );
}

#[test]
fn direct_bridge_construction_cannot_bypass_empty_token_guard() {
    let bridge = JsonRpcBridge {
        security: RemoteSecurity {
            session_token: Some(String::new()),
            ..Default::default()
        },
    };
    assert!(bridge.check_token(Some("")).is_err());
    assert!(require_safe_bind("0.0.0.0:4000".parse().unwrap(), &bridge.security).is_err());
    assert!(BevyRemoteControlPlugin::bind("127.0.0.1:0", bridge).is_err());
    assert!(
        JsonRpcBridge::new(RemoteSecurity {
            allowed_origin: Some("https://example.test\r\nInjected: yes".into()),
            ..Default::default()
        })
        .is_err()
    );
}

fn read_request_bytes(bytes: Vec<u8>) -> Result<HttpRequest, anyhow::Error> {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let writer = thread::spawn(move || {
        let mut stream = TcpStream::connect(address).unwrap();
        let _ = stream.write_all(&bytes);
    });
    let (mut stream, _) = listener.accept().unwrap();
    let result = read_http_request(&mut stream);
    drop(stream);
    writer.join().unwrap();
    result
}

#[test]
fn http_rejects_ambiguous_framing_and_malformed_headers() {
    for request in [
        "POST /rpc HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\na",
        "POST /rpc HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n",
        "POST /rpc HTTP/1.1\r\nContent-Length: +1\r\n\r\na",
        "POST /rpc HTTP/9.9\r\n\r\n",
        "GET /health HTTP/1.1 extra\r\n\r\n",
        "GET /health HTTP/1.1\r\nMalformed\r\n\r\n",
        "GET /health HTTP/1.1\r\nBad Header: x\r\n\r\n",
    ] {
        assert!(
            read_request_bytes(request.as_bytes().to_vec()).is_err(),
            "{request}"
        );
    }
    let request = format!(
        "GET /health HTTP/1.1\r\nX-Large: {}\r\n\r\n",
        "x".repeat(MAX_HTTP_HEADER_BYTES)
    );
    assert!(
        read_request_bytes(request.into_bytes())
            .unwrap_err()
            .to_string()
            .contains("exceed")
    );
}

#[test]
fn coalesced_websocket_handshake_preserves_the_first_frame() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let writer = thread::spawn(move || {
        let mut stream = TcpStream::connect(address).unwrap();
        let mut packet = b"GET /ws HTTP/1.1\r\nHost: localhost\r\n\r\n".to_vec();
        packet.extend(masked_ws_frame(1, b"first"));
        stream.write_all(&packet).unwrap();
    });
    let (mut stream, _) = listener.accept().unwrap();
    let request = read_http_request(&mut stream).unwrap();
    assert_eq!(request.path, "/ws");
    assert!(
        matches!(read_websocket_text(&mut stream).unwrap(), WebSocketMessage::Text(text) if text == "first")
    );
    writer.join().unwrap();
}

#[test]
fn slow_drip_http_is_bounded_by_one_absolute_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let writer = thread::spawn(move || {
        let mut stream = TcpStream::connect(address).unwrap();
        for byte in b"GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n" {
            if stream.write_all(&[*byte]).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
    });
    let (mut stream, _) = listener.accept().unwrap();
    let started = Instant::now();
    assert!(read_http_request_until(&mut stream, started + Duration::from_millis(40)).is_err());
    assert!(started.elapsed() < Duration::from_millis(200));
    drop(stream);
    writer.join().unwrap();
}

#[test]
fn replay_file_reads_are_bounded() {
    let path = std::env::temp_dir().join(format!(
        "bevy-remote-large-{}-{:?}.json",
        std::process::id(),
        thread::current().id()
    ));
    let file = std::fs::File::create(&path).unwrap();
    file.set_len((MAX_MESSAGE_BYTES + 1) as u64).unwrap();
    assert!(
        read_replay_file(&path)
            .unwrap_err()
            .to_string()
            .contains("limit")
    );
    std::fs::remove_file(path).unwrap();
}

#[test]
fn oversized_responses_return_a_bounded_correlated_error() {
    let encoded = serialize_response(&JsonRpcResponse::Result {
        jsonrpc: "2.0",
        id: json!(42),
        result: Value::String("x".repeat(MAX_MESSAGE_BYTES)),
    });
    assert!(encoded.len() < MAX_MESSAGE_BYTES);
    let response: Value = serde_json::from_str(&encoded).unwrap();
    assert_eq!(response["id"], 42);
    assert_eq!(response["error"]["code"], RPC_INTERNAL_ERROR);
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("limit")
    );
}

#[test]
fn oversized_replay_exports_do_not_create_files() {
    use bevy::prelude::*;
    use bevy_agent_runner::{AgentApp, AgentControlPlugins, ResetOptions};
    let mut env = AgentApp::new(|| {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(AgentControlPlugins::default());
        configure_test_identity(&mut app);
        configure_test_observer(&mut app);
        app.set_supported_actions([AgentActionKind::Noop, AgentActionKind::Move]);
        app.set_supported_observation_modes([
            ObservationMode::PlayerKnowledge,
            ObservationMode::Hybrid,
        ]);
        app
    })
    .unwrap();
    env.reset_with_response(ResetOptions {
        create_initial_snapshot: true,
        ..Default::default()
    })
    .unwrap();
    use bevy_agent_runner::AgentEnvironment;
    env.branch(0, Some("x".repeat(MAX_MESSAGE_BYTES))).unwrap();
    let root = std::env::temp_dir().join(format!(
        "bevy-remote-export-{}-{:?}",
        std::process::id(),
        thread::current().id()
    ));
    let bridge = JsonRpcBridge::new(RemoteSecurity {
        capabilities: AgentCapability::default() | AgentCapability::FILESYSTEM,
        artifact_root: Some(root.clone()),
        ..Default::default()
    })
    .unwrap();
    let response = bridge.handle_json(&mut env, r#"{"jsonrpc":"2.0","id":1,"method":"agent.replay.export","params":{"path":"too-large.json"}}"#);
    let response: Value = serde_json::from_str(&response).unwrap();
    assert_eq!(response["error"]["code"], RPC_INTERNAL_ERROR, "{response}");
    assert!(!root.exists(), "rejected export created its artifact root");
}

#[test]
fn token_check_accepts_missing_token_when_no_token_is_configured() {
    let bridge = JsonRpcBridge::default();

    assert!(bridge.check_token(None).is_ok());
}

#[test]
fn token_check_rejects_missing_or_wrong_token_when_configured() {
    let bridge = JsonRpcBridge::new(RemoteSecurity {
        session_token: Some("secret".to_string()),
        ..Default::default()
    })
    .unwrap();

    assert!(bridge.check_token(None).is_err());
    assert!(bridge.check_token(Some("wrong")).is_err());
    assert!(bridge.check_token(Some("secret")).is_ok());
}

#[test]
fn capability_check_rejects_missing_capability() {
    let bridge = JsonRpcBridge::new(RemoteSecurity {
        capabilities: AgentCapability::STEP,
        ..Default::default()
    })
    .unwrap();

    assert!(bridge.require_capability(AgentCapability::STEP).is_ok());
    assert!(
        bridge
            .require_capability(AgentCapability::SNAPSHOT)
            .is_err()
    );
}

#[test]
fn schema_helpers_expose_required_shapes() {
    let action = agent_action_schema();
    let observation = observation_schema();
    let step = step_response_schema();
    let visual = visual_capture_schema();

    assert_eq!(action["title"], "AgentAction");
    assert!(action["oneOf"].as_array().unwrap().len() >= 10);
    let action_variant = |name: &str| {
        action["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .find(|variant| {
                variant["properties"]["type"]["const"]
                    .as_str()
                    .is_some_and(|kind| kind == name)
            })
            .unwrap_or_else(|| panic!("{name} action schema is missing"))
    };
    let move_action = action_variant("Move");
    assert_eq!(move_action["properties"]["x"]["minimum"], -1.0);
    assert_eq!(move_action["properties"]["x"]["maximum"], 1.0);
    assert_eq!(move_action["properties"]["y"]["minimum"], -1.0);
    assert_eq!(move_action["properties"]["y"]["maximum"], 1.0);
    let look_action = action_variant("Look");
    assert_eq!(
        look_action["properties"]["yaw_delta"]["minimum"],
        -f64::from(bevy_agent_core::LOOK_YAW_DELTA_LIMIT_RADIANS)
    );
    assert_eq!(
        look_action["properties"]["yaw_delta"]["maximum"],
        f64::from(bevy_agent_core::LOOK_YAW_DELTA_LIMIT_RADIANS)
    );
    assert_eq!(
        look_action["properties"]["pitch_delta"]["minimum"],
        -f64::from(bevy_agent_core::LOOK_PITCH_DELTA_LIMIT_RADIANS)
    );
    assert_eq!(
        look_action["properties"]["pitch_delta"]["maximum"],
        f64::from(bevy_agent_core::LOOK_PITCH_DELTA_LIMIT_RADIANS)
    );
    assert_eq!(observation["title"], "Observation");
    assert!(observation["$defs"]["PlayerObservation"].is_object());
    assert!(observation["$defs"]["PixelObservation"].is_object());
    assert!(observation["$defs"]["StateDelta"].is_object());
    assert!(
        observation["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .any(|variant| variant["properties"]["kind"]["const"] == "FullState")
    );
    assert_eq!(step["title"], "StepResponse");
    assert_eq!(visual["title"], "VisualCapture");
    assert_eq!(visual["properties"]["format"]["const"], "png");
}

#[test]
fn action_schema_includes_registered_custom_actions() {
    let mut catalog = AgentActionCatalog::default();
    catalog.set_supported_actions([AgentActionKind::Custom]);
    catalog
        .register_custom_action_schema(
            "Input",
            serde_json::json!({
                "type": "object",
                "required": ["type"],
                "properties": { "type": { "const": "Input" } }
            }),
        )
        .unwrap();

    let action = agent_action_schema_with_custom_actions(Some(&catalog));
    let custom_actions = custom_action_schema_map(Some(&catalog));

    assert_eq!(
        custom_actions["Input"]["properties"]["type"]["const"],
        "Input"
    );
    assert_eq!(
        action["oneOf"][0]["properties"]["value"]["anyOf"][0]["properties"]["type"]["const"],
        "Input"
    );
}

#[test]
fn discovery_filters_actions_and_embeds_domain_observation_schema() {
    let mut actions = AgentActionCatalog::default();
    actions.set_supported_actions([
        AgentActionKind::Noop,
        AgentActionKind::Move,
        AgentActionKind::Custom,
    ]);
    let action = agent_action_schema_with_custom_actions(Some(&actions));
    assert_eq!(
        supported_action_names(Some(&actions)),
        vec!["Noop", "Move", "Custom"]
    );
    assert_eq!(action["oneOf"].as_array().unwrap().len(), 3);

    let mut observations = AgentObservationCatalog::default();
    observations.set_supported_modes([ObservationMode::PlayerKnowledge]);
    observations.set_schema(json!({
        "type": "object",
        "required": ["kind", "tick", "value"],
        "properties": {
            "kind": {"const": "Domain"},
            "tick": {"type": "integer", "minimum": 0},
            "value": {"type": "object", "required": ["phase"], "properties": {"phase": {"type": "string"}}}
        },
        "additionalProperties": false
    })).unwrap();
    let observation = observation_schema_with_catalog(Some(&observations));
    assert_eq!(
        observation["properties"]["value"]["required"],
        json!(["phase"])
    );
}

#[test]
fn unauthenticated_public_bind_is_rejected() {
    let public: SocketAddr = "0.0.0.0:4000".parse().unwrap();
    let loopback: SocketAddr = "127.0.0.1:4000".parse().unwrap();

    assert!(require_safe_bind(public, &RemoteSecurity::default()).is_err());
    assert!(require_safe_bind(loopback, &RemoteSecurity::default()).is_ok());
    assert!(
        require_safe_bind(
            public,
            &RemoteSecurity {
                session_token: Some("secret".to_string()),
                ..Default::default()
            },
        )
        .is_ok()
    );
}

#[test]
fn typed_session_params_reject_nonstring_tokens() {
    let bridge = JsonRpcBridge::default();
    assert!(
        bridge
            .prepare_request(
                r#"{"jsonrpc":"2.0","id":1,"method":"agent.info","params":{"session_token":"abc"}}"#
            )
            .is_ok()
    );
    let error = bridge
        .prepare_request(
            r#"{"jsonrpc":"2.0","id":1,"method":"agent.info","params":{"session_token":123}}"#,
        )
        .unwrap_err();
    assert_eq!(
        serde_json::to_value(error).unwrap()["error"]["code"],
        RPC_INVALID_PARAMS
    );
}

#[test]
fn websocket_accept_key_matches_rfc_example() {
    assert_eq!(
        websocket_accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
        "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
    );
}

#[test]
fn websocket_handshake_requires_protocol_headers_and_token_for_origin() {
    let request = HttpRequest {
        method: "GET".to_string(),
        path: "/ws".to_string(),
        headers: vec![
            ("Upgrade".to_string(), "websocket".to_string()),
            ("Connection".to_string(), "keep-alive, Upgrade".to_string()),
            ("Sec-WebSocket-Version".to_string(), "13".to_string()),
            (
                "Sec-WebSocket-Key".to_string(),
                "dGhlIHNhbXBsZSBub25jZQ==".to_string(),
            ),
        ],
        body: String::new(),
    };

    assert!(validate_websocket_handshake(&request, &RemoteSecurity::default()).is_ok());

    let mut origin_request = request;
    origin_request
        .headers
        .push(("Origin".to_string(), "http://evil.example".to_string()));
    let error =
        validate_websocket_handshake(&origin_request, &RemoteSecurity::default()).unwrap_err();
    assert!(error.to_string().contains("Origin"));

    assert!(
        validate_websocket_handshake(
            &origin_request,
            &RemoteSecurity {
                session_token: Some("secret".to_string()),
                ..Default::default()
            }
        )
        .is_ok()
    );
}

#[test]
fn read_http_request_parses_headers_and_body() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .write_all(
                b"POST /rpc HTTP/1.1\r\nHost: localhost\r\nContent-Length: 7\r\n\r\n{\"x\":1}",
            )
            .unwrap();
    });
    let (mut stream, _) = listener.accept().unwrap();

    let request = read_http_request(&mut stream).unwrap();
    handle.join().unwrap();

    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/rpc");
    assert_eq!(request.header("host"), Some("localhost"));
    assert_eq!(request.body, "{\"x\":1}");
}

#[test]
fn write_http_response_writes_status_headers_and_body() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let mut stream = TcpStream::connect(addr).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    });
    let (mut stream, _) = listener.accept().unwrap();

    write_http_response(&mut stream, 200, "OK", "application/json", r#"{"ok":true}"#).unwrap();
    drop(stream);
    let response = handle.join().unwrap();

    assert!(response.starts_with("HTTP/1.1 200 OK"));
    assert!(response.contains("Content-Type: application/json"));
    assert!(response.ends_with(r#"{"ok":true}"#));
}

#[test]
fn websocket_read_decodes_masked_text_ping_and_close_frames() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream.write_all(&masked_ws_frame(0x1, b"hello")).unwrap();
        stream.write_all(&masked_ws_frame(0x9, b"ping")).unwrap();
        stream.write_all(&masked_ws_frame(0x8, b"")).unwrap();
    });
    let (mut stream, _) = listener.accept().unwrap();

    match read_websocket_text(&mut stream).unwrap() {
        WebSocketMessage::Text(text) => assert_eq!(text, "hello"),
        _ => panic!("expected text websocket message"),
    }
    match read_websocket_text(&mut stream).unwrap() {
        WebSocketMessage::Ping(payload) => assert_eq!(payload, b"ping"),
        _ => panic!("expected ping websocket message"),
    }
    assert!(matches!(
        read_websocket_text(&mut stream).unwrap(),
        WebSocketMessage::Close
    ));
    handle.join().unwrap();
}

#[test]
fn websocket_write_encodes_unmasked_server_text_frame() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let mut stream = TcpStream::connect(addr).unwrap();
        let mut frame = [0; 7];
        stream.read_exact(&mut frame).unwrap();
        frame
    });
    let (mut stream, _) = listener.accept().unwrap();

    write_websocket_text(&mut stream, "hello").unwrap();
    let frame = handle.join().unwrap();

    assert_eq!(frame[0], 0x81);
    assert_eq!(frame[1], 5);
    assert_eq!(&frame[2..], b"hello");
}

#[test]
fn find_header_end_detects_http_header_separator() {
    assert_eq!(find_header_end(b"GET / HTTP/1.1\r\n\r\nbody"), Some(14));
    assert_eq!(find_header_end(b"GET / HTTP/1.1\r\n"), None);
}

#[test]
fn constant_time_eq_matches_equal_inputs_only() {
    assert!(constant_time_eq(b"", b""));
    assert!(constant_time_eq(b"secret", b"secret"));
    assert!(!constant_time_eq(b"secret", b"secrex"));
    assert!(!constant_time_eq(b"secret", b"secret-extra"));
    assert!(!constant_time_eq(b"", b"x"));
}

#[test]
fn read_http_request_rejects_oversized_headers() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let _writer = thread::spawn(move || {
        let mut stream = TcpStream::connect(addr).unwrap();
        // Stream junk that never contains \r\n\r\n; server must abort once cap is hit.
        let chunk = vec![b'x'; 4096];
        for _ in 0..16 {
            if stream.write_all(&chunk).is_err() {
                break;
            }
        }
    });
    let (mut stream, _) = listener.accept().unwrap();
    let err = read_http_request(&mut stream).unwrap_err();
    assert!(
        err.to_string().contains("exceed"),
        "unexpected error: {err}"
    );
}

#[test]
fn read_http_request_rejects_oversized_content_length() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let mut stream = TcpStream::connect(addr).unwrap();
        let req = format!(
            "POST /rpc HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n",
            MAX_MESSAGE_BYTES + 1
        );
        let _ = stream.write_all(req.as_bytes());
    });
    let (mut stream, _) = listener.accept().unwrap();
    let err = read_http_request(&mut stream).unwrap_err();
    let _ = handle.join();
    assert!(
        err.to_string().contains("exceeds limit"),
        "unexpected error: {err}"
    );
}

#[test]
fn read_http_request_rejects_malformed_content_length() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let mut stream = TcpStream::connect(addr).unwrap();
        let _ = stream.write_all(
            b"POST /rpc HTTP/1.1\r\nHost: localhost\r\nContent-Length: oops\r\n\r\nbody",
        );
    });
    let (mut stream, _) = listener.accept().unwrap();

    let err = read_http_request(&mut stream).unwrap_err();
    let _ = handle.join();

    assert!(
        err.to_string().contains("invalid Content-Length"),
        "unexpected error: {err}"
    );
}

#[test]
fn control_capability_is_part_of_the_default_capability_set() {
    assert!(AgentCapability::default().contains(AgentCapability::CONTROL));
    assert!(!AgentCapability::default().contains(AgentCapability::FILESYSTEM));
}

#[test]
fn authorize_readonly_enforces_session_token_when_configured() {
    let bridge = JsonRpcBridge::new(RemoteSecurity {
        session_token: Some("secret".to_string()),
        ..Default::default()
    })
    .unwrap();

    for method in [
        "agent.info",
        "agent.action_space",
        "agent.observation_space",
        "agent.schema",
        "agent.timeline.current",
    ] {
        let request = |params: Value| {
            json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}).to_string()
        };
        for params in [json!({}), json!({"session_token":"wrong"})] {
            let response = bridge.prepare_request(&request(params)).unwrap_err();
            assert_eq!(
                serde_json::to_value(response).unwrap()["error"]["code"],
                RPC_AUTH_ERROR
            );
        }
        assert!(
            bridge
                .prepare_request(&request(json!({"session_token":"secret"})))
                .is_ok()
        );
        assert!(
            JsonRpcBridge::default()
                .prepare_request(&request(json!({})))
                .is_ok()
        );
    }
}

#[test]
fn validate_jsonrpc_version_rejects_anything_other_than_two_dot_zero() {
    fn req(version: Option<&str>) -> JsonRpcRequest {
        JsonRpcRequest {
            retry_key: None,
            jsonrpc: version.unwrap_or("").to_owned(),
            id: json!(1),
            method: "agent.info".to_string(),
            params: json!({}),
        }
    }

    assert!(validate_jsonrpc_version(&req(Some("2.0"))).is_ok());
    // The current protocol requires an explicit JSON-RPC version.
    assert!(validate_jsonrpc_version(&req(None)).is_err());
    assert!(validate_jsonrpc_version(&req(Some("1.0"))).is_err());
    assert!(validate_jsonrpc_version(&req(Some("2"))).is_err());
    assert!(validate_jsonrpc_version(&req(Some("2.00"))).is_err());
}

#[test]
fn read_websocket_text_rejects_oversized_extended_length_without_allocating() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let mut stream = TcpStream::connect(addr).unwrap();
        // FIN+text, masked, 64-bit extended length claiming a huge payload
        // that we deliberately never transmit.
        let mut frame = vec![0x81, 0xff];
        frame.extend_from_slice(&u64::MAX.to_be_bytes());
        stream.write_all(&frame).unwrap();
    });
    let (mut stream, _) = listener.accept().unwrap();

    let err = read_websocket_text(&mut stream).unwrap_err();
    handle.join().unwrap();

    assert!(
        err.to_string().contains("exceeds limit"),
        "unexpected error: {err}"
    );
}

#[test]
fn read_websocket_text_rejects_unmasked_client_frame() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let mut stream = TcpStream::connect(addr).unwrap();
        // FIN+text, unmasked, with an unmasked payload.
        stream
            .write_all(&[0x81, 0x05, b'h', b'e', b'l', b'l', b'o'])
            .unwrap();
    });
    let (mut stream, _) = listener.accept().unwrap();

    let err = read_websocket_text(&mut stream).unwrap_err();
    handle.join().unwrap();

    assert!(
        err.to_string().contains("unmasked"),
        "unexpected error: {err}"
    );
}

#[test]
fn empty_session_token_is_invalid_config() {
    assert!(
        JsonRpcBridge::new(RemoteSecurity {
            session_token: Some(String::new()),
            ..Default::default()
        })
        .is_err()
    );
}

#[test]
fn observation_visibility_requires_full_state_for_hybrid_and_debug() {
    assert_eq!(
        capability_for_observation_mode(&ObservationMode::Hybrid),
        AgentCapability::OBSERVE_FULL_STATE
    );
    assert_eq!(
        capability_for_observation_mode(&ObservationMode::FullDebugState),
        AgentCapability::OBSERVE_FULL_STATE
    );
    assert_eq!(
        capability_for_observation_mode(&ObservationMode::PlayerKnowledge),
        AgentCapability::OBSERVE_PLAYER
    );
    // Player-only bridge authorizes player modes but not full-state modes.
    let player_only = JsonRpcBridge::new(RemoteSecurity {
        capabilities: AgentCapability::STEP | AgentCapability::OBSERVE_PLAYER,
        ..Default::default()
    })
    .unwrap();
    assert!(
        player_only
            .require_capability(capability_for_observation_mode(
                &ObservationMode::PlayerKnowledge
            ))
            .is_ok()
    );
    assert!(
        player_only
            .require_capability(capability_for_observation_mode(&ObservationMode::Hybrid))
            .is_err()
    );
}

#[test]
fn filesystem_confinement_rejects_absolute_and_traversal() {
    let security = RemoteSecurity {
        artifact_root: Some(PathBuf::from("/tmp/bevy-test-root")),
        ..Default::default()
    };
    assert!(security.resolve_artifact_path("/etc/passwd").is_err());
    assert!(security.resolve_artifact_path("../escape").is_err());
    assert!(security.resolve_artifact_path("a/../../escape").is_err());
    assert!(security.resolve_artifact_path("replays/a.json").is_ok());
    assert!(
        security
            .resolve_output_dir(&PathBuf::from("/abs/dir"))
            .is_err()
    );
}

#[cfg(unix)]
#[test]
fn filesystem_confinement_rejects_external_and_dangling_symlinks() {
    let base = std::env::temp_dir().join(format!(
        "bevy-confined-links-{}-{:?}",
        std::process::id(),
        thread::current().id()
    ));
    let root = base.join("artifacts");
    let external = base.join("external");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&external).unwrap();
    std::os::unix::fs::symlink(&external, root.join("redirect")).unwrap();
    std::os::unix::fs::symlink(external.join("missing"), root.join("dangling")).unwrap();
    let security = RemoteSecurity {
        artifact_root: Some(root),
        ..Default::default()
    };
    assert!(
        security
            .resolve_artifact_path("redirect/export.json")
            .is_err()
    );
    assert!(
        security
            .resolve_artifact_path("dangling/capture.png")
            .is_err()
    );
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn websocket_rejects_reserved_bits_nonminimal_lengths_and_invalid_close_payloads() {
    let frames = [
        vec![0xc1, 0x80, 1, 2, 3, 4],
        vec![0x81, 0xfe, 0, 1, 1, 2, 3, 4, b'a' ^ 1],
        masked_ws_frame(8, &[1]),
        masked_ws_frame(8, &1005_u16.to_be_bytes()),
        masked_ws_frame(8, &[3, 232, 255]),
    ];
    for frame in frames {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let writer = thread::spawn(move || {
            TcpStream::connect(address)
                .unwrap()
                .write_all(&frame)
                .unwrap()
        });
        let (mut stream, _) = listener.accept().unwrap();
        assert!(read_websocket_text(&mut stream).is_err());
        writer.join().unwrap();
    }
}

#[test]
fn http_origin_rejected_without_token_and_content_type_required() {
    let open = RemoteSecurity::default();
    let origin_req = HttpRequest {
        method: "POST".to_string(),
        path: "/rpc".to_string(),
        headers: vec![
            ("Origin".to_string(), "http://evil.example".to_string()),
            ("Content-Type".to_string(), "application/json".to_string()),
        ],
        body: "{}".to_string(),
    };
    assert!(validate_http_rpc(&origin_req, &open).is_err());
    let authed = RemoteSecurity {
        session_token: Some("secret".to_string()),
        allowed_origin: Some("http://good.example".to_string()),
        ..Default::default()
    };
    let good_req = HttpRequest {
        method: "POST".to_string(),
        path: "/rpc".to_string(),
        headers: vec![
            ("Origin".to_string(), "http://good.example".to_string()),
            ("Content-Type".to_string(), "application/json".to_string()),
        ],
        body: "{}".to_string(),
    };
    assert!(validate_http_rpc(&good_req, &authed).is_ok());
    // Wrong content type rejected even with valid origin/token setup.
    let bad_ct = HttpRequest {
        method: "POST".to_string(),
        path: "/rpc".to_string(),
        headers: vec![("Content-Type".to_string(), "text/plain".to_string())],
        body: "{}".to_string(),
    };
    assert!(validate_http_rpc(&bad_ct, &authed).is_err());
    // CORS echoes only the configured allowed origin.
    assert_eq!(
        cors_allow_origin(&good_req, &authed),
        Some("http://good.example".to_string())
    );
    let evil_req = HttpRequest {
        method: "POST".to_string(),
        path: "/rpc".to_string(),
        headers: vec![
            ("Origin".to_string(), "http://evil.example".to_string()),
            ("Content-Type".to_string(), "application/json".to_string()),
        ],
        body: "{}".to_string(),
    };
    assert!(cors_allow_origin(&evil_req, &authed).is_none());
    assert!(cors_allow_origin(&good_req, &RemoteSecurity::default()).is_none());
}

#[test]
fn rpc_error_codes_are_documented() {
    assert_eq!(RPC_METHOD_NOT_FOUND, -32601);
    assert_eq!(RPC_INVALID_PARAMS, -32602);
    assert_eq!(RPC_AUTH_ERROR, -32001);
    assert_eq!(RPC_INTERNAL_ERROR, -32603);
}

#[test]
fn request_budgets_are_enforced_as_constants() {
    assert_eq!(MAX_ACTIONS_PER_REQUEST, 1024);
    assert_eq!(MAX_TICKS_PER_REQUEST, 10_000);
    assert!(!timeout_frames_to_duration(8).is_zero());
}

#[test]
fn observation_schema_accepts_every_serialized_entity_extra_json_shape() {
    use bevy_agent_core::{EntityObservation, Observation, SymbolicObservation};
    let schema = observation_schema();
    let validator = jsonschema::validator_for(&schema).unwrap();
    for extra in [
        Value::Null,
        json!(17),
        json!(false),
        json!("text"),
        json!([1, 2]),
        json!({"v":1}),
    ] {
        let observation = Observation::Symbolic(SymbolicObservation {
            visible_entities: vec![EntityObservation {
                stable_id: None,
                kind: "entity".to_owned(),
                position: [0.0; 3],
                extra,
            }],
            ..Default::default()
        });
        let encoded = serde_json::to_value(observation).unwrap();
        assert!(
            validator.is_valid(&encoded),
            "{encoded}: {:?}",
            validator.iter_errors(&encoded).collect::<Vec<_>>()
        );
    }
}

#[test]
fn generated_rpc_envelopes_conform_and_reject_mixed_or_unknown_fields() {
    let request_validator = jsonschema::validator_for(&jsonrpc_request_schema()).unwrap();
    let response_validator = jsonschema::validator_for(&jsonrpc_response_schema()).unwrap();
    let error_validator = jsonschema::validator_for(&jsonrpc_error_schema()).unwrap();
    for id in [
        Value::Null,
        json!(37),
        json!("request-37"),
        json!("😀".repeat(256)),
    ] {
        let request = json!({"jsonrpc":"2.0","id":id,"method":"agent.info"});
        assert!(request_validator.is_valid(&request));
        assert!(parse_jsonrpc_request(&request.to_string()).is_ok());
        for response in [
            JsonRpcResponse::Result {
                jsonrpc: "2.0",
                id: id.clone(),
                result: json!({"anything":[null,4]}),
            },
            JsonRpcResponse::Error {
                jsonrpc: "2.0",
                id: id.clone(),
                error: JsonRpcError {
                    code: RPC_INVALID_PARAMS,
                    message: "invalid action".to_owned(),
                    data: None,
                },
            },
        ] {
            let mut encoded = serde_json::to_value(response).unwrap();
            assert!(response_validator.is_valid(&encoded), "{encoded}");
            if encoded.get("error").is_some() {
                assert!(error_validator.is_valid(&encoded["error"]));
            }
            encoded["unexpected"] = json!(true);
            assert!(!response_validator.is_valid(&encoded));
        }
    }
    assert!(!response_validator.is_valid(
        &json!({"jsonrpc":"2.0","id":7,"result":null,"error":{"code":-1,"message":"mixed"}})
    ));
    for id in [json!(true), json!([]), json!({}), json!("😀".repeat(257))] {
        let request = json!({"jsonrpc":"2.0","id":id,"method":"agent.info"});
        assert!(!request_validator.is_valid(&request));
        let error =
            serde_json::to_value(parse_jsonrpc_request(&request.to_string()).unwrap_err()).unwrap();
        assert_eq!(error["id"], Value::Null);
    }
}

#[test]
fn overlapping_custom_action_schemas_share_union_validation() {
    let mut actions = AgentActionCatalog::default();
    actions.set_supported_actions([AgentActionKind::Custom]);
    actions
        .register_custom_action_schema(
            "general",
            json!({"type":"object","required":["type"],"properties":{"type":{"const":"Input"}}}),
        )
        .unwrap();
    actions.register_custom_action_schema("specific", json!({"type":"object","required":["type","x"],"properties":{"type":{"const":"Input"},"x":{"type":"number","minimum":-1,"maximum":1}}})).unwrap();
    let payload = json!({"type":"Input","x":0.5});
    let action = bevy_agent_core::AgentAction::Custom { value: payload };
    actions.validate_action(&action).unwrap();
    assert!(
        jsonschema::validator_for(&agent_action_schema_with_custom_actions(Some(&actions)))
            .unwrap()
            .is_valid(&serde_json::to_value(action).unwrap())
    );
}

#[test]
fn composed_custom_action_schemas_preserve_independent_local_reference_scopes() {
    let mut catalog = AgentActionCatalog::default();
    catalog.set_supported_actions([AgentActionKind::Custom]);
    for (name, kind) in [("first", "First"), ("second", "Second")] {
        catalog.register_custom_action_schema(name, json!({
            "$defs":{"payload":{"type":"object","required":["type"],"properties":{"type":{"const":kind}},"additionalProperties":false}},
            "$ref":"#/$defs/payload"
        })).unwrap();
    }
    let schema = agent_action_schema_with_custom_actions(Some(&catalog));
    let validator = jsonschema::validator_for(&schema).unwrap();
    for kind in ["First", "Second"] {
        let action = bevy_agent_core::AgentAction::Custom {
            value: json!({"type":kind}),
        };
        catalog.validate_action(&action).unwrap();
        assert!(validator.is_valid(&serde_json::to_value(action).unwrap()));
    }
    assert!(!validator.is_valid(&json!({"type":"Custom","value":{"type":"Other"}})));
}

#[test]
fn observation_schema_has_id_and_refs_resolve() {
    for schema in [
        observation_schema(),
        step_response_schema(),
        reset_response_schema(),
        step_many_response_schema(),
        visual_capture_schema(),
    ] {
        assert!(schema.get("$schema").is_some());
        assert!(schema.get("$id").is_some());
        assert_refs_resolve(&schema, &schema);
    }
    // Catalog-aware domain constraints propagate to outer builders.
    let mut observations = AgentObservationCatalog::default();
    observations.set_supported_modes([ObservationMode::PlayerKnowledge]);
    observations.set_schema(json!({
        "type": "object",
        "required": ["kind", "tick", "value"],
        "properties": {
            "kind": {"const": "Domain"},
            "tick": {"type": "integer", "minimum": 0},
            "value": {"type": "object", "required": ["phase"], "properties": {"phase": {"type": "string"}}}
        },
        "additionalProperties": false
    })).unwrap();
    let step = step_response_schema_with_catalog(Some(&observations));
    let text = serde_json::to_string(&step).unwrap();
    assert!(text.contains("phase"));
    assert_refs_resolve(&step, &step);
}

/// Manual `$ref` resolver (no new deps): every `#/...` pointer must
/// resolve against the nearest enclosing `$id` scope (JSON Schema 2020-12
/// base-URI behavior for nested resources).
fn assert_refs_resolve(node: &Value, root: &Value) {
    check_refs_scoped(node, root, root);
}

fn check_refs_scoped(node: &Value, scope: &Value, outer: &Value) {
    match node {
        Value::Object(map) => {
            // A nested $id starts a new resource scope for its children.
            let scope = if map.contains_key("$id") { node } else { scope };
            if let Some(r) = map.get("$ref").and_then(Value::as_str) {
                let resolved =
                    resolve_local_pointer(scope, r).or_else(|| resolve_local_pointer(outer, r));
                assert!(
                    resolved.is_some(),
                    "unresolvable $ref {r} in scope {}",
                    scope.get("$id").unwrap_or(&Value::Null)
                );
            }
            for value in map.values() {
                check_refs_scoped(value, scope, outer);
            }
        }
        Value::Array(items) => {
            for item in items {
                check_refs_scoped(item, scope, outer);
            }
        }
        _ => {}
    }
}

fn resolve_local_pointer(root: &Value, pointer: &str) -> Option<Value> {
    let path = pointer.strip_prefix('#')?;
    if path.is_empty() {
        return Some(root.clone());
    }
    let mut current = root;
    for part in path.split('/').filter(|s| !s.is_empty()) {
        current = current.get(part)?;
    }
    Some(current.clone())
}

fn masked_ws_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mask = [1, 2, 3, 4];
    let mut frame = vec![0x80 | opcode, 0x80 | payload.len() as u8];
    frame.extend_from_slice(&mask);
    frame.extend(
        payload
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ mask[index % mask.len()]),
    );
    frame
}

#[test]
fn removed_terminal_batch_flag_and_ambiguous_status_are_rejected() {
    let bridge = JsonRpcBridge::default();
    assert!(bridge.prepare_request(&json!({"jsonrpc":"2.0","id":1,"method":"agent.step_many","params":{"actions":[],"stop_on_done":false}}).to_string()).is_err());
    for params in [
        json!({}),
        json!({"operation_id":"ab-1","retry_key":"tick1"}),
        json!({"retry_key":" "}),
    ] {
        assert!(bridge.prepare_request(&json!({"jsonrpc":"2.0","id":1,"method":"agent.operations.status","params":params}).to_string()).is_err());
    }
}

#[test]
fn rpc_step_failure_exposes_committed_tick_and_recovery_data() {
    use bevy::prelude::*;
    use bevy_agent_core::{Observation, RewardState, SimClock};
    use bevy_agent_runner::{AgentApp, AgentControlPlugins};
    let mut env = AgentApp::new(|| {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(AgentControlPlugins::default());
        configure_test_identity(&mut app);
        app.set_supported_actions([AgentActionKind::Noop])
            .set_supported_observation_modes([ObservationMode::Hybrid]);
        app.insert_observation_extractor(|world, _| {
            let tick = world.resource::<SimClock>().tick;
            if tick == 1 {
                world.resource_mut::<RewardState>().current_reward = f32::NAN;
            }
            Observation::default_for_tick(tick)
        });
        app
    })
    .unwrap();
    let bridge = JsonRpcBridge::default();
    let response: Value = serde_json::from_str(&bridge.handle_json(&mut env, &json!({"jsonrpc":"2.0","id":"failure","method":"agent.step","params":{"action":{"type":"Noop"}}}).to_string())).unwrap();
    assert_eq!(response["id"], "failure");
    assert_eq!(response["error"]["data"]["tick_after"], 1);
    assert_eq!(response["error"]["data"]["tick_committed"], true);
    assert_eq!(response["error"]["data"]["recovery_required"], true);
    let retry: Value = serde_json::from_str(&bridge.handle_json(&mut env, &json!({"jsonrpc":"2.0","id":"next","method":"agent.step","params":{"action":{"type":"Noop"}}}).to_string())).unwrap();
    assert!(
        retry["error"]["message"]
            .as_str()
            .unwrap()
            .contains("faulted")
    );
    assert_eq!(env.current_tick(), 1);
}
