use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use anyhow::{Context, Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use bevy_agent_core::{AgentAction, ControlMode, ObservationMode, SnapshotId};
use bevy_agent_replay::{ReplayLog, ReplayRecorder, start_recording, stop_recording};
use bevy_agent_runner::{AgentApp, AgentEnvironment, ResetOptions};
use bevy_agent_snapshot::SnapshotStore;
use bitflags::bitflags;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct AgentCapability: u32 {
        const STEP = 1 << 0;
        const OBSERVE_PLAYER = 1 << 1;
        const OBSERVE_FULL_STATE = 1 << 2;
        const MUTATE_ECS = 1 << 3;
        const SNAPSHOT = 1 << 4;
        const RESTORE = 1 << 5;
        const BRANCH = 1 << 6;
        const SPAWN_DESPAWN = 1 << 7;
    }
}

impl Default for AgentCapability {
    fn default() -> Self {
        Self::STEP
            | Self::OBSERVE_PLAYER
            | Self::OBSERVE_FULL_STATE
            | Self::SNAPSHOT
            | Self::RESTORE
            | Self::BRANCH
    }
}

#[derive(Clone, Debug)]
pub struct RemoteSecurity {
    pub bind_host: String,
    pub session_token: Option<String>,
    pub capabilities: AgentCapability,
}

impl Default for RemoteSecurity {
    fn default() -> Self {
        Self {
            bind_host: "127.0.0.1".to_string(),
            session_token: None,
            capabilities: AgentCapability::default(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct JsonRpcBridge {
    pub security: RemoteSecurity,
}

#[derive(Clone, Debug, Deserialize)]
pub struct JsonRpcRequest {
    #[serde(default)]
    pub jsonrpc: Option<String>,
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Clone, Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum JsonRpcResponse {
    Result {
        jsonrpc: &'static str,
        id: Value,
        result: Value,
    },
    Error {
        jsonrpc: &'static str,
        id: Value,
        error: JsonRpcError,
    },
}

#[derive(Clone, Debug, Deserialize)]
struct StepParams {
    pub action: AgentAction,
    #[serde(default)]
    pub observation_mode: Option<ObservationMode>,
    #[serde(default)]
    pub session_token: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct StepManyParams {
    pub actions: Vec<AgentAction>,
    #[serde(default = "default_true")]
    pub stop_on_done: bool,
    #[serde(default = "return_all")]
    pub return_observations: String,
    #[serde(default)]
    pub session_token: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct FastForwardParams {
    pub ticks: u64,
    #[serde(default)]
    pub session_token: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct ObserveParams {
    #[serde(default)]
    pub observation_mode: ObservationMode,
    #[serde(default)]
    pub session_token: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct ResetParams {
    #[serde(default)]
    pub options: ResetOptions,
    #[serde(default)]
    pub session_token: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct SnapshotRestoreParams {
    pub snapshot_id: SnapshotId,
    #[serde(default)]
    pub session_token: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct SnapshotDeleteParams {
    pub snapshot_id: SnapshotId,
    #[serde(default)]
    pub session_token: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct BranchParams {
    pub from_tick: u64,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub session_token: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct RestoreTickParams {
    pub tick: u64,
    #[serde(default)]
    pub session_token: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct ControlModeParams {
    pub mode: ControlMode,
    #[serde(default)]
    pub session_token: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct ReplayExportParams {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub session_token: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct ReplayLoadParams {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub log: Option<ReplayLog>,
    #[serde(default)]
    pub session_token: Option<String>,
}

fn default_true() -> bool {
    true
}

fn return_all() -> String {
    "all".to_string()
}

impl JsonRpcBridge {
    pub fn new(security: RemoteSecurity) -> Self {
        Self { security }
    }

    pub fn handle_json(&self, env: &mut AgentApp, input: &str) -> String {
        let response = match serde_json::from_str::<JsonRpcRequest>(input) {
            Ok(request) => self.handle_request(env, request),
            Err(error) => JsonRpcResponse::Error {
                jsonrpc: "2.0",
                id: Value::Null,
                error: JsonRpcError {
                    code: -32700,
                    message: format!("parse error: {error}"),
                },
            },
        };
        serde_json::to_string(&response).expect("serializing JSON-RPC response")
    }

    pub fn handle_request(&self, env: &mut AgentApp, request: JsonRpcRequest) -> JsonRpcResponse {
        let id = request.id.clone();
        match self.dispatch(env, request) {
            Ok(result) => JsonRpcResponse::Result {
                jsonrpc: "2.0",
                id,
                result,
            },
            Err(error) => JsonRpcResponse::Error {
                jsonrpc: "2.0",
                id,
                error: JsonRpcError {
                    code: -32603,
                    message: error.to_string(),
                },
            },
        }
    }

    pub fn serve_stdio(&self, env: &mut AgentApp) -> Result<()> {
        use std::io::{self, BufRead, Write};

        let stdin = io::stdin();
        let mut stdout = io::stdout();
        for line in stdin.lock().lines() {
            let line = line?;
            let response = self.handle_json(env, &line);
            writeln!(stdout, "{response}")?;
            stdout.flush()?;
        }
        Ok(())
    }

    fn dispatch(&self, env: &mut AgentApp, request: JsonRpcRequest) -> Result<Value> {
        match request.method.as_str() {
            "agent.info" => Ok(json!({
                "name": "bevy_agent_control",
                "version": env!("CARGO_PKG_VERSION"),
                "bevy_version": "0.18.1",
                "tick": env.current_tick(),
                "capabilities": self.security.capabilities.bits(),
            })),
            "agent.action_space" => Ok(json!({
                "type": "json_schema",
                "actions": ["Noop", "Move", "Look", "Jump", "Crouch", "Sprint", "Interact", "Attack", "UseItem", "Dodge", "Custom"],
                "schema": agent_action_schema()
            })),
            "agent.observation_space" => Ok(json!({
                "modes": ["PlayerKnowledge", "FullDebugState", "DiffSinceLastTick", "PixelFrame", "Hybrid"],
                "default": "Hybrid",
                "schema": observation_schema()
            })),
            "agent.schema" => Ok(json!({
                "action": agent_action_schema(),
                "observation": observation_schema(),
                "step_response": step_response_schema(),
            })),
            "agent.reset" => {
                self.require_capability(AgentCapability::STEP)?;
                let params: ResetParams = serde_json::from_value(request.params)?;
                self.check_token(params.session_token.as_deref())?;
                Ok(serde_json::to_value(env.reset(params.options)?)?)
            }
            "agent.step" => {
                self.require_capability(AgentCapability::STEP)?;
                let params: StepParams = serde_json::from_value(request.params)?;
                self.check_token(params.session_token.as_deref())?;
                if let Some(mode) = params.observation_mode {
                    env.world_mut()
                        .resource_mut::<bevy_agent_core::ObservationConfig>()
                        .mode = mode;
                }
                Ok(serde_json::to_value(env.step(params.action)?)?)
            }
            "agent.step_many" => {
                self.require_capability(AgentCapability::STEP)?;
                let params: StepManyParams = serde_json::from_value(request.params)?;
                self.check_token(params.session_token.as_deref())?;
                let mut responses = Vec::new();
                for action in params.actions {
                    let response = env.step(action)?;
                    let done = response.done || response.truncated;
                    responses.push(response);
                    if params.stop_on_done && done {
                        break;
                    }
                }
                let start_tick = responses
                    .first()
                    .map(|response| response.tick.saturating_sub(1))
                    .unwrap_or_else(|| env.current_tick());
                let end_tick = responses
                    .last()
                    .map(|response| response.tick)
                    .unwrap_or(start_tick);
                let done = responses
                    .last()
                    .map(|response| response.done || response.truncated)
                    .unwrap_or(false);
                let checksum = responses
                    .last()
                    .and_then(|response| response.checksum.clone());
                let observation = match params.return_observations.as_str() {
                    "none" => Value::Null,
                    "last" => responses
                        .last()
                        .map(|response| serde_json::to_value(&response.observation))
                        .transpose()?
                        .unwrap_or(Value::Null),
                    _ => serde_json::to_value(&responses)?,
                };
                Ok(json!({
                    "start_tick": start_tick,
                    "end_tick": end_tick,
                    "steps": responses.len(),
                    "observation": observation,
                    "done": done,
                    "checksum": checksum,
                }))
            }
            "agent.fast_forward" => {
                self.require_capability(AgentCapability::STEP)?;
                let params: FastForwardParams = serde_json::from_value(request.params)?;
                self.check_token(params.session_token.as_deref())?;
                Ok(serde_json::to_value(env.fast_forward(params.ticks)?)?)
            }
            "agent.observe" => {
                self.require_capability(AgentCapability::OBSERVE_PLAYER)?;
                let params: ObserveParams = serde_json::from_value(request.params)?;
                self.check_token(params.session_token.as_deref())?;
                Ok(serde_json::to_value(env.observe(params.observation_mode)?)?)
            }
            "agent.snapshot.create" => {
                self.require_capability(AgentCapability::SNAPSHOT)?;
                self.check_token(token_from_params(&request.params).as_deref())?;
                Ok(serde_json::to_value(env.snapshot()?)?)
            }
            "agent.snapshot.restore" => {
                self.require_capability(AgentCapability::RESTORE)?;
                let params: SnapshotRestoreParams = serde_json::from_value(request.params)?;
                self.check_token(params.session_token.as_deref())?;
                env.restore(params.snapshot_id)?;
                Ok(Value::Null)
            }
            "agent.snapshot.list" => {
                self.require_capability(AgentCapability::SNAPSHOT)?;
                self.check_token(token_from_params(&request.params).as_deref())?;
                let store = env.world().resource::<SnapshotStore>();
                let snapshots = store
                    .snapshots
                    .values()
                    .map(|snapshot| {
                        json!({
                            "snapshot_id": snapshot.manifest.snapshot_id,
                            "tick": snapshot.manifest.tick,
                            "label": snapshot.manifest.label,
                            "checksum": snapshot.checksum,
                        })
                    })
                    .collect::<Vec<_>>();
                Ok(json!(snapshots))
            }
            "agent.snapshot.delete" => {
                self.require_capability(AgentCapability::SNAPSHOT)?;
                let params: SnapshotDeleteParams = serde_json::from_value(request.params)?;
                self.check_token(params.session_token.as_deref())?;
                let mut store = env.world_mut().resource_mut::<SnapshotStore>();
                store.snapshots.remove(&params.snapshot_id);
                store.labels.retain(|_, value| *value != params.snapshot_id);
                store
                    .checkpoints
                    .retain(|value| *value != params.snapshot_id);
                Ok(Value::Null)
            }
            "agent.timeline.current" => Ok(json!({
                "tick": env.current_tick(),
                "timeline_id": env.world().resource::<bevy_agent_core::AgentControlState>().timeline_id,
                "branch_id": env.world().resource::<bevy_agent_core::AgentControlState>().branch_id,
            })),
            "agent.timeline.branch" => {
                self.require_capability(AgentCapability::BRANCH)?;
                let params: BranchParams = serde_json::from_value(request.params)?;
                self.check_token(params.session_token.as_deref())?;
                let branch_id = env.branch(params.from_tick, params.label)?;
                Ok(json!({
                    "timeline_id": env.world().resource::<bevy_agent_core::AgentControlState>().timeline_id,
                    "branch_id": branch_id,
                    "current_tick": env.current_tick(),
                }))
            }
            "agent.timeline.restore_tick" => {
                self.require_capability(AgentCapability::RESTORE)?;
                let params: RestoreTickParams = serde_json::from_value(request.params)?;
                self.check_token(params.session_token.as_deref())?;
                env.restore_tick(params.tick)?;
                Ok(json!({ "current_tick": env.current_tick() }))
            }
            "agent.control.set_mode" => {
                let params: ControlModeParams = serde_json::from_value(request.params)?;
                self.check_token(params.session_token.as_deref())?;
                env.world_mut()
                    .resource_mut::<bevy_agent_core::AgentControlState>()
                    .mode = params.mode;
                Ok(Value::Null)
            }
            "agent.control.pause" => {
                self.check_token(token_from_params(&request.params).as_deref())?;
                env.world_mut()
                    .resource_mut::<bevy_agent_core::AgentControlState>()
                    .mode = ControlMode::Paused;
                Ok(Value::Null)
            }
            "agent.control.resume" => {
                self.check_token(token_from_params(&request.params).as_deref())?;
                env.world_mut()
                    .resource_mut::<bevy_agent_core::AgentControlState>()
                    .mode = ControlMode::Agent;
                Ok(Value::Null)
            }
            "agent.replay.start" => {
                self.require_capability(AgentCapability::STEP)?;
                self.check_token(token_from_params(&request.params).as_deref())?;
                let initial_snapshot = env.replay_log().and_then(|log| log.initial_snapshot);
                start_recording(env.world_mut(), initial_snapshot);
                Ok(json!({ "recording": true }))
            }
            "agent.replay.stop" => {
                self.require_capability(AgentCapability::STEP)?;
                self.check_token(token_from_params(&request.params).as_deref())?;
                let log = stop_recording(env.world_mut());
                Ok(json!({
                    "recording": false,
                    "records": log.records.len(),
                    "log": log,
                }))
            }
            "agent.replay.export" => {
                self.require_capability(AgentCapability::STEP)?;
                let params: ReplayExportParams = serde_json::from_value(request.params)?;
                self.check_token(params.session_token.as_deref())?;
                let log = env
                    .world()
                    .get_resource::<ReplayRecorder>()
                    .map(|recorder| recorder.log.clone())
                    .ok_or_else(|| anyhow!("AgentReplayPlugin is not installed"))?;
                if let Some(path) = &params.path {
                    let encoded = serde_json::to_string_pretty(&log)?;
                    std::fs::write(path, encoded)
                        .with_context(|| format!("writing replay log to {path}"))?;
                }
                Ok(json!({
                    "path": params.path,
                    "records": log.records.len(),
                    "checkpoints": log.checkpoints.len(),
                    "log": log,
                }))
            }
            "agent.replay.load" => {
                self.require_capability(AgentCapability::STEP)?;
                let params: ReplayLoadParams = serde_json::from_value(request.params)?;
                self.check_token(params.session_token.as_deref())?;
                let log = if let Some(log) = params.log {
                    log
                } else if let Some(path) = params.path {
                    let bytes = std::fs::read_to_string(&path)
                        .with_context(|| format!("reading replay log from {path}"))?;
                    serde_json::from_str::<ReplayLog>(&bytes)
                        .with_context(|| format!("decoding replay log from {path}"))?
                } else {
                    return Err(anyhow!("agent.replay.load requires either path or log"));
                };
                let records = log.records.len();
                let checkpoints = log.checkpoints.len();
                env.world_mut().resource_mut::<ReplayRecorder>().log = log;
                Ok(json!({
                    "records": records,
                    "checkpoints": checkpoints,
                }))
            }
            other => Err(anyhow!("unknown method {other}")),
        }
    }

    fn check_token(&self, token: Option<&str>) -> Result<()> {
        if let Some(expected) = &self.security.session_token
            && token != Some(expected.as_str())
        {
            return Err(anyhow!("invalid or missing session token"));
        }
        Ok(())
    }

    fn require_capability(&self, capability: AgentCapability) -> Result<()> {
        if !self.security.capabilities.contains(capability) {
            return Err(anyhow!("missing remote capability {capability:?}"));
        }
        Ok(())
    }
}

fn token_from_params(params: &Value) -> Option<String> {
    params
        .get("session_token")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

pub fn agent_action_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "AgentAction",
        "oneOf": [
            { "type": "object", "required": ["type"], "properties": { "type": { "const": "Noop" } }, "additionalProperties": false },
            { "type": "object", "required": ["type", "x", "y"], "properties": { "type": { "const": "Move" }, "x": { "type": "number", "minimum": -1.0, "maximum": 1.0 }, "y": { "type": "number", "minimum": -1.0, "maximum": 1.0 } }, "additionalProperties": false },
            { "type": "object", "required": ["type", "yaw_delta", "pitch_delta"], "properties": { "type": { "const": "Look" }, "yaw_delta": { "type": "number" }, "pitch_delta": { "type": "number" } }, "additionalProperties": false },
            { "type": "object", "required": ["type"], "properties": { "type": { "const": "Jump" } }, "additionalProperties": false },
            { "type": "object", "required": ["type"], "properties": { "type": { "const": "Crouch" } }, "additionalProperties": false },
            { "type": "object", "required": ["type"], "properties": { "type": { "const": "Sprint" } }, "additionalProperties": false },
            { "type": "object", "required": ["type"], "properties": { "type": { "const": "Interact" } }, "additionalProperties": false },
            { "type": "object", "required": ["type"], "properties": { "type": { "const": "Attack" }, "target": { "anyOf": [{ "type": "integer", "minimum": 0 }, { "type": "null" }] } }, "additionalProperties": false },
            { "type": "object", "required": ["type", "slot"], "properties": { "type": { "const": "UseItem" }, "slot": { "type": "integer", "minimum": 0, "maximum": 255 } }, "additionalProperties": false },
            { "type": "object", "required": ["type"], "properties": { "type": { "const": "Dodge" } }, "additionalProperties": false },
            { "type": "object", "required": ["type", "value"], "properties": { "type": { "const": "Custom" }, "value": true }, "additionalProperties": false }
        ]
    })
}

pub fn observation_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "Observation",
        "oneOf": [
            { "$ref": "#/$defs/symbolicObservationEnvelope" },
            { "$ref": "#/$defs/hybridObservation" },
            { "type": "object", "required": ["kind", "message"], "properties": { "kind": { "const": "Error" }, "message": { "type": "string" } } }
        ],
        "$defs": {
            "symbolicObservationEnvelope": {
                "type": "object",
                "required": ["kind", "tick", "player", "visible_entities", "inventory", "objectives"],
                "properties": {
                    "kind": { "const": "Symbolic" },
                    "tick": { "type": "integer", "minimum": 0 },
                    "player": { "$ref": "#/$defs/player" },
                    "visible_entities": { "type": "array", "items": { "$ref": "#/$defs/entity" } },
                    "inventory": { "type": "array" },
                    "objectives": { "type": "array" }
                }
            },
            "hybridObservation": {
                "type": "object",
                "required": ["kind", "symbolic", "pixels", "debug"],
                "properties": {
                    "kind": { "const": "Hybrid" },
                    "symbolic": { "$ref": "#/$defs/symbolic" },
                    "pixels": { "anyOf": [{ "type": "object" }, { "type": "null" }] },
                    "debug": { "anyOf": [{ "type": "object" }, { "type": "null" }] }
                }
            },
            "symbolic": {
                "type": "object",
                "required": ["tick", "player", "visible_entities", "inventory", "objectives"],
                "properties": {
                    "tick": { "type": "integer", "minimum": 0 },
                    "player": { "$ref": "#/$defs/player" },
                    "visible_entities": { "type": "array", "items": { "$ref": "#/$defs/entity" } },
                    "inventory": { "type": "array" },
                    "objectives": { "type": "array" }
                }
            },
            "player": {
                "type": "object",
                "required": ["position", "velocity", "health", "score", "on_ground"],
                "properties": {
                    "stable_id": { "anyOf": [{ "type": "integer", "minimum": 0 }, { "type": "null" }] },
                    "position": { "type": "array", "prefixItems": [{ "type": "number" }, { "type": "number" }, { "type": "number" }], "minItems": 3, "maxItems": 3 },
                    "velocity": { "type": "array", "prefixItems": [{ "type": "number" }, { "type": "number" }], "minItems": 2, "maxItems": 2 },
                    "health": { "type": "number" },
                    "score": { "type": "integer" },
                    "on_ground": { "type": "boolean" }
                }
            },
            "entity": {
                "type": "object",
                "required": ["kind", "position", "extra"],
                "properties": {
                    "stable_id": { "anyOf": [{ "type": "integer", "minimum": 0 }, { "type": "null" }] },
                    "kind": { "type": "string" },
                    "position": { "type": "array", "prefixItems": [{ "type": "number" }, { "type": "number" }, { "type": "number" }], "minItems": 3, "maxItems": 3 },
                    "extra": { "type": "object" }
                }
            }
        }
    })
}

pub fn step_response_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "StepResponse",
        "type": "object",
        "required": ["tick", "observation", "reward", "done", "truncated", "info", "checksum"],
        "properties": {
            "tick": { "type": "integer", "minimum": 0 },
            "observation": observation_schema(),
            "reward": { "type": "number" },
            "done": { "type": "boolean" },
            "truncated": { "type": "boolean" },
            "info": { "type": "object" },
            "checksum": { "anyOf": [{ "type": "object" }, { "type": "null" }] }
        }
    })
}

#[derive(Clone, Debug)]
pub struct HttpRemoteServer {
    pub bind_addr: String,
    pub bridge: JsonRpcBridge,
}

impl HttpRemoteServer {
    pub fn new(bind_addr: impl Into<String>, bridge: JsonRpcBridge) -> Self {
        Self {
            bind_addr: bind_addr.into(),
            bridge,
        }
    }

    pub fn serve(&self, env: &mut AgentApp) -> Result<()> {
        let listener = TcpListener::bind(&self.bind_addr)
            .with_context(|| format!("binding {}", self.bind_addr))?;
        eprintln!(
            "bevy_agent_remote listening on http://{}/rpc",
            self.bind_addr
        );

        for stream in listener.incoming() {
            match stream {
                Ok(mut stream) => {
                    if let Err(error) = self.handle_connection(env, &mut stream) {
                        let _ = write_http_response(
                            &mut stream,
                            500,
                            "Internal Server Error",
                            "application/json",
                            &json!({ "error": error.to_string() }).to_string(),
                        );
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn handle_connection(&self, env: &mut AgentApp, stream: &mut TcpStream) -> Result<()> {
        let request = read_http_request(stream)?;
        if request.method == "GET" && request.path == "/health" {
            return write_http_response(
                stream,
                200,
                "OK",
                "application/json",
                &json!({ "ok": true, "tick": env.current_tick() }).to_string(),
            );
        }

        if request.method == "GET" && request.path == "/ws" {
            return self.handle_websocket(env, stream, &request);
        }

        if request.method == "POST" && request.path == "/rpc" {
            let response = self.bridge.handle_json(env, &request.body);
            return write_http_response(stream, 200, "OK", "application/json", &response);
        }

        write_http_response(
            stream,
            404,
            "Not Found",
            "application/json",
            &json!({ "error": "not found" }).to_string(),
        )
    }

    fn handle_websocket(
        &self,
        env: &mut AgentApp,
        stream: &mut TcpStream,
        request: &HttpRequest,
    ) -> Result<()> {
        let key = request
            .header("sec-websocket-key")
            .ok_or_else(|| anyhow!("missing Sec-WebSocket-Key"))?;
        let accept = websocket_accept_key(key);
        let response = format!(
            "HTTP/1.1 101 Switching Protocols\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Accept: {accept}\r\n\
             \r\n"
        );
        stream.write_all(response.as_bytes())?;
        stream.flush()?;

        loop {
            match read_websocket_text(stream)? {
                WebSocketMessage::Text(text) => {
                    let response = self.bridge.handle_json(env, &text);
                    write_websocket_text(stream, &response)?;
                }
                WebSocketMessage::Ping(payload) => write_websocket_pong(stream, &payload)?,
                WebSocketMessage::Close => return Ok(()),
            }
        }
    }
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

fn read_http_request(stream: &mut TcpStream) -> Result<HttpRequest> {
    let mut bytes = Vec::new();
    let header_end;
    loop {
        let mut buf = [0; 1024];
        let read = stream.read(&mut buf)?;
        if read == 0 {
            return Err(anyhow!("connection closed before HTTP request"));
        }
        bytes.extend_from_slice(&buf[..read]);
        if let Some(index) = find_header_end(&bytes) {
            header_end = index;
            break;
        }
    }

    let headers_text = String::from_utf8(bytes[..header_end].to_vec())?;
    let mut lines = headers_text.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| anyhow!("missing HTTP request line"))?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts
        .next()
        .ok_or_else(|| anyhow!("missing HTTP method"))?
        .to_string();
    let path = request_parts
        .next()
        .ok_or_else(|| anyhow!("missing HTTP path"))?
        .to_string();

    let mut headers = Vec::new();
    for line in lines {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        headers.push((key.trim().to_string(), value.trim().to_string()));
    }

    let content_length = headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    let body_start = header_end + 4;
    while bytes.len() < body_start + content_length {
        let mut buf = [0; 1024];
        let read = stream.read(&mut buf)?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buf[..read]);
    }
    let body_bytes = bytes
        .get(body_start..body_start + content_length)
        .unwrap_or_default();
    let body = String::from_utf8(body_bytes.to_vec())?;

    Ok(HttpRequest {
        method,
        path,
        headers,
        body,
    })
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

fn write_http_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &str,
) -> Result<()> {
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()?;
    Ok(())
}

enum WebSocketMessage {
    Text(String),
    Ping(Vec<u8>),
    Close,
}

fn websocket_accept_key(key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    BASE64_STANDARD.encode(hasher.finalize())
}

fn read_websocket_text(stream: &mut TcpStream) -> Result<WebSocketMessage> {
    let mut header = [0; 2];
    stream.read_exact(&mut header)?;
    let opcode = header[0] & 0x0f;
    let masked = header[1] & 0x80 != 0;
    let mut len = (header[1] & 0x7f) as u64;
    if len == 126 {
        let mut extended = [0; 2];
        stream.read_exact(&mut extended)?;
        len = u16::from_be_bytes(extended) as u64;
    } else if len == 127 {
        let mut extended = [0; 8];
        stream.read_exact(&mut extended)?;
        len = u64::from_be_bytes(extended);
    }

    let mut mask = [0; 4];
    if masked {
        stream.read_exact(&mut mask)?;
    }

    let mut payload = vec![0; len as usize];
    stream.read_exact(&mut payload)?;
    if masked {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
    }

    match opcode {
        0x1 => Ok(WebSocketMessage::Text(String::from_utf8(payload)?)),
        0x8 => Ok(WebSocketMessage::Close),
        0x9 => Ok(WebSocketMessage::Ping(payload)),
        other => Err(anyhow!("unsupported websocket opcode {other}")),
    }
}

fn write_websocket_text(stream: &mut TcpStream, text: &str) -> Result<()> {
    write_websocket_frame(stream, 0x1, text.as_bytes())
}

fn write_websocket_pong(stream: &mut TcpStream, payload: &[u8]) -> Result<()> {
    write_websocket_frame(stream, 0xa, payload)
}

fn write_websocket_frame(stream: &mut TcpStream, opcode: u8, payload: &[u8]) -> Result<()> {
    let mut frame = Vec::with_capacity(payload.len() + 10);
    frame.push(0x80 | opcode);
    if payload.len() < 126 {
        frame.push(payload.len() as u8);
    } else if u16::try_from(payload.len()).is_ok() {
        frame.push(126);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        frame.push(127);
        frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }
    frame.extend_from_slice(payload);
    stream.write_all(&frame)?;
    stream.flush()?;
    Ok(())
}
