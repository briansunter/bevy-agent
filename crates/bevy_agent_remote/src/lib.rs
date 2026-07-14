//! JSON-RPC, HTTP, WebSocket, and stdio remote-control bridge for agent apps.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use bevy_agent_core::{AgentAction, AgentActionCatalog, ControlMode, ObservationMode, SnapshotId};
use bevy_agent_replay::{ReplayLog, ReplayRecorder, start_recording, stop_recording};
use bevy_agent_runner::{AgentApp, AgentEnvironment, ResetOptions, VisualCaptureOptions};
use bevy_agent_snapshot::SnapshotStore;
use bitflags::bitflags;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};

const MAX_HTTP_HEADER_BYTES: usize = 32 * 1024;
const MAX_HTTP_BODY_BYTES: usize = 8 * 1024 * 1024;
const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(30);

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
        const VISUAL_CAPTURE = 1 << 8;
        /// Mutating control of run state: pause/resume/set_mode.
        const CONTROL = 1 << 9;
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
            | Self::VISUAL_CAPTURE
            | Self::CONTROL
    }
}

#[derive(Clone, Debug, Default)]
pub struct RemoteSecurity {
    pub session_token: Option<String>,
    pub capabilities: AgentCapability,
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

#[derive(Clone, Debug, Deserialize)]
struct VisualCaptureParams {
    #[serde(default)]
    pub output_dir: Option<std::path::PathBuf>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub timeout_frames: Option<u32>,
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
    #[must_use]
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
        if let Err(error) = validate_jsonrpc_version(&request) {
            return JsonRpcResponse::Error {
                jsonrpc: "2.0",
                id,
                error: JsonRpcError {
                    code: -32600,
                    message: error.to_string(),
                },
            };
        }
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
            "agent.info" => {
                self.authorize(None, &request.params)?;
                Ok(json!({
                    "name": "bevy_agent_control",
                    "version": env!("CARGO_PKG_VERSION"),
                    "bevy_version": "0.18.1",
                    "tick": env.current_tick(),
                    "capabilities": self.security.capabilities.bits(),
                }))
            }
            "agent.action_space" => {
                self.authorize(None, &request.params)?;
                let catalog = env.world().get_resource::<AgentActionCatalog>();
                Ok(json!({
                    "type": "json_schema",
                    "actions": ["Noop", "Move", "Look", "Jump", "Crouch", "Sprint", "Interact", "Attack", "UseItem", "Dodge", "Custom"],
                    "schema": agent_action_schema_with_custom_actions(catalog),
                    "custom_actions": custom_action_schema_map(catalog),
                }))
            }
            "agent.observation_space" => {
                self.authorize(None, &request.params)?;
                Ok(json!({
                    "modes": ["PlayerKnowledge", "FullDebugState", "DiffSinceLastTick", "PixelFrame", "Hybrid"],
                    "default": "Hybrid",
                    "schema": observation_schema()
                }))
            }
            "agent.schema" => {
                self.authorize(None, &request.params)?;
                let catalog = env.world().get_resource::<AgentActionCatalog>();
                Ok(json!({
                    "action": agent_action_schema_with_custom_actions(catalog),
                    "custom_actions": custom_action_schema_map(catalog),
                    "observation": observation_schema(),
                    "step_response": step_response_schema(),
                    "visual_capture": visual_capture_schema(),
                }))
            }
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
            "agent.visual.capture" => {
                self.require_capability(AgentCapability::VISUAL_CAPTURE)?;
                let params: VisualCaptureParams = serde_json::from_value(request.params)?;
                self.check_token(params.session_token.as_deref())?;
                let mut options = VisualCaptureOptions::default();
                if let Some(output_dir) = params.output_dir {
                    options.output_dir = output_dir;
                }
                if params.label.is_some() {
                    options.label = params.label;
                }
                if let Some(timeout_frames) = params.timeout_frames {
                    options.timeout_frames = timeout_frames;
                }
                Ok(serde_json::to_value(env.capture_visual(options)?)?)
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
                let mut snapshots = store.snapshots.values().collect::<Vec<_>>();
                snapshots.sort_by(|a, b| {
                    a.manifest
                        .tick
                        .cmp(&b.manifest.tick)
                        .then_with(|| a.manifest.snapshot_id.cmp(&b.manifest.snapshot_id))
                });
                let snapshots = snapshots
                    .into_iter()
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
            "agent.timeline.current" => {
                self.authorize(None, &request.params)?;
                Ok(json!({
                    "tick": env.current_tick(),
                    "timeline_id": env.world().resource::<bevy_agent_core::AgentControlState>().timeline_id,
                    "branch_id": env.world().resource::<bevy_agent_core::AgentControlState>().branch_id,
                }))
            }
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
                self.require_capability(AgentCapability::CONTROL)?;
                let params: ControlModeParams = serde_json::from_value(request.params)?;
                self.check_token(params.session_token.as_deref())?;
                env.world_mut()
                    .resource_mut::<bevy_agent_core::AgentControlState>()
                    .mode = params.mode;
                Ok(Value::Null)
            }
            "agent.control.pause" => {
                self.require_capability(AgentCapability::CONTROL)?;
                self.check_token(token_from_params(&request.params).as_deref())?;
                env.world_mut()
                    .resource_mut::<bevy_agent_core::AgentControlState>()
                    .mode = ControlMode::Paused;
                Ok(Value::Null)
            }
            "agent.control.resume" => {
                self.require_capability(AgentCapability::CONTROL)?;
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
        if let Some(expected) = &self.security.session_token {
            let provided = token.unwrap_or("");
            if !constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
                return Err(anyhow!("invalid or missing session token"));
            }
        }
        Ok(())
    }

    fn require_capability(&self, capability: AgentCapability) -> Result<()> {
        if !self.security.capabilities.contains(capability) {
            return Err(anyhow!("missing remote capability {capability:?}"));
        }
        Ok(())
    }

    /// Authorize a request against the configured capability set and session
    /// token. `capability` is `None` for read-only inspectors that require no
    /// capability gate but must still present the session token when one is
    /// configured. The token is read with the established `session_token`
    /// params convention, so no token is required when none is configured.
    fn authorize(&self, capability: Option<AgentCapability>, params: &Value) -> Result<()> {
        if let Some(capability) = capability {
            self.require_capability(capability)?;
        }
        self.check_token(token_from_params(params).as_deref())
    }
}

/// Validate the JSON-RPC version field. A present `jsonrpc` member must be
/// exactly `"2.0"`; an absent member is tolerated so existing clients that omit
/// it keep working.
fn validate_jsonrpc_version(request: &JsonRpcRequest) -> Result<()> {
    if let Some(version) = request.jsonrpc.as_deref()
        && version != "2.0"
    {
        return Err(anyhow!("invalid Request: jsonrpc must be \"2.0\""));
    }
    Ok(())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn token_from_params(params: &Value) -> Option<String> {
    params
        .get("session_token")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

#[must_use]
pub fn agent_action_schema() -> Value {
    agent_action_schema_with_custom_actions(None)
}

#[must_use]
pub fn agent_action_schema_with_custom_actions(catalog: Option<&AgentActionCatalog>) -> Value {
    let custom_value_schema = custom_action_value_schema(catalog);
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
            { "type": "object", "required": ["type", "value"], "properties": { "type": { "const": "Custom" }, "value": custom_value_schema }, "additionalProperties": false }
        ]
    })
}

#[must_use]
pub fn custom_action_schema_map(catalog: Option<&AgentActionCatalog>) -> Value {
    let mut map = serde_json::Map::new();
    if let Some(catalog) = catalog {
        for (name, schema) in &catalog.custom_actions {
            map.insert(name.clone(), schema.schema.clone());
        }
    }
    Value::Object(map)
}

fn custom_action_value_schema(catalog: Option<&AgentActionCatalog>) -> Value {
    let Some(catalog) = catalog else {
        return Value::Bool(true);
    };
    if catalog.custom_actions.is_empty() {
        return Value::Bool(true);
    }
    json!({
        "oneOf": catalog
            .custom_actions
            .values()
            .map(|schema| schema.schema.clone())
            .collect::<Vec<_>>()
    })
}

#[must_use]
pub fn observation_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "Observation",
        "oneOf": [
            { "$ref": "#/$defs/symbolicObservationEnvelope" },
            { "$ref": "#/$defs/hybridObservation" },
            { "$ref": "#/$defs/fullStateObservation" },
            { "$ref": "#/$defs/deltaObservation" },
            { "$ref": "#/$defs/pixelObservation" },
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
            "fullStateObservation": {
                "type": "object",
                "required": ["kind"],
                "properties": {
                    "kind": { "const": "FullState" }
                },
                "additionalProperties": true
            },
            "deltaObservation": {
                "type": "object",
                "required": ["kind", "tick", "changes"],
                "properties": {
                    "kind": { "const": "Delta" },
                    "tick": { "type": "integer", "minimum": 0 },
                    "changes": true
                }
            },
            "pixelObservation": {
                "type": "object",
                "required": ["kind", "width", "height", "rgba"],
                "properties": {
                    "kind": { "const": "Pixels" },
                    "width": { "type": "integer", "minimum": 1 },
                    "height": { "type": "integer", "minimum": 1 },
                    "rgba": { "type": "array", "items": { "type": "integer", "minimum": 0, "maximum": 255 } }
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

#[must_use]
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

#[must_use]
pub fn visual_capture_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "VisualCapture",
        "type": "object",
        "required": ["tick", "frame", "path", "width", "height", "format"],
        "properties": {
            "tick": { "type": "integer", "minimum": 0 },
            "frame": { "type": "integer", "minimum": 0 },
            "path": { "type": "string" },
            "width": { "type": "integer", "minimum": 1 },
            "height": { "type": "integer", "minimum": 1 },
            "format": { "const": "png" }
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
        let local_addr = listener
            .local_addr()
            .with_context(|| format!("reading bound address for {}", self.bind_addr))?;
        require_safe_bind(local_addr, &self.bridge.security)?;
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
        validate_websocket_handshake(request, &self.bridge.security)?;
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

fn validate_websocket_handshake(request: &HttpRequest, security: &RemoteSecurity) -> Result<()> {
    let valid_upgrade = request
        .header("upgrade")
        .map(|value| value.trim().eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);
    if !valid_upgrade {
        return Err(anyhow!("missing or invalid WebSocket Upgrade header"));
    }

    let connection_upgrade = request
        .header("connection")
        .map(|value| {
            value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
        })
        .unwrap_or(false);
    if !connection_upgrade {
        return Err(anyhow!("missing WebSocket Connection: Upgrade header"));
    }

    if request.header("sec-websocket-version").map(str::trim) != Some("13") {
        return Err(anyhow!("WebSocket version 13 is required"));
    }

    let has_key = request
        .header("sec-websocket-key")
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    if !has_key {
        return Err(anyhow!("missing Sec-WebSocket-Key"));
    }

    // A browser Origin is a cross-site request signal. Tokenless loopback
    // WebSocket sessions are intended for non-browser local clients; require
    // an explicit session token before accepting browser-originated traffic.
    if security.session_token.is_none() && request.header("origin").is_some() {
        return Err(anyhow!(
            "WebSocket connections with an Origin header require a session token"
        ));
    }

    Ok(())
}

fn require_safe_bind(local_addr: SocketAddr, security: &RemoteSecurity) -> Result<()> {
    if security.session_token.is_none() && !local_addr.ip().is_loopback() {
        return Err(anyhow!(
            "refusing unauthenticated remote control on non-loopback bind {}; set a session token",
            local_addr
        ));
    }
    Ok(())
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
    let _ = stream.set_read_timeout(Some(HTTP_READ_TIMEOUT));

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
        if bytes.len() > MAX_HTTP_HEADER_BYTES {
            return Err(anyhow!(
                "HTTP request headers exceed {MAX_HTTP_HEADER_BYTES} bytes"
            ));
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

    let content_length = match headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
    {
        Some((_, value)) => value
            .parse::<usize>()
            .with_context(|| format!("invalid Content-Length: {value}"))?,
        None => 0,
    };
    if content_length > MAX_HTTP_BODY_BYTES {
        return Err(anyhow!(
            "HTTP request body of {content_length} bytes exceeds limit of {MAX_HTTP_BODY_BYTES}"
        ));
    }
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
        .ok_or_else(|| anyhow!("HTTP body shorter than declared Content-Length"))?;
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

#[derive(Debug)]
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
    let fin = header[0] & 0x80 != 0;
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

    let is_control_frame = matches!(opcode, 0x8..=0xa);

    // Validate the frame before allocating its payload. Client-to-server
    // frames must be masked, this server has no continuation support so
    // fragments are rejected, control frames cannot exceed 125 bytes, and the
    // payload must fit the body limit (including 64-bit extended lengths).
    if !masked {
        return Err(anyhow!("unmasked websocket client frame"));
    }
    if !fin {
        return Err(anyhow!("fragmented websocket frames are not supported"));
    }
    if is_control_frame && len > 125 {
        return Err(anyhow!("control frame payload exceeds 125 bytes"));
    }
    if len > MAX_HTTP_BODY_BYTES as u64 {
        return Err(anyhow!(
            "websocket payload of {len} bytes exceeds limit of {MAX_HTTP_BODY_BYTES}"
        ));
    }

    let mut mask = [0; 4];
    stream.read_exact(&mut mask)?;

    let mut payload = vec![0; len as usize];
    stream.read_exact(&mut payload)?;
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte ^= mask[index % 4];
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

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
        });

        assert!(bridge.check_token(None).is_err());
        assert!(bridge.check_token(Some("wrong")).is_err());
        assert!(bridge.check_token(Some("secret")).is_ok());
    }

    #[test]
    fn capability_check_rejects_missing_capability() {
        let bridge = JsonRpcBridge::new(RemoteSecurity {
            capabilities: AgentCapability::STEP,
            ..Default::default()
        });

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
        assert_eq!(observation["title"], "Observation");
        assert!(observation["$defs"]["player"].is_object());
        assert!(observation["$defs"]["pixelObservation"].is_object());
        assert!(observation["$defs"]["deltaObservation"].is_object());
        assert!(observation["$defs"]["fullStateObservation"].is_object());
        assert_eq!(step["title"], "StepResponse");
        assert_eq!(visual["title"], "VisualCapture");
        assert_eq!(visual["properties"]["format"]["const"], "png");
    }

    #[test]
    fn action_schema_includes_registered_custom_actions() {
        let mut catalog = AgentActionCatalog::default();
        catalog.register_custom_action_schema(
            "Input",
            serde_json::json!({
                "type": "object",
                "required": ["type"],
                "properties": { "type": { "const": "Input" } }
            }),
        );

        let action = agent_action_schema_with_custom_actions(Some(&catalog));
        let custom_actions = custom_action_schema_map(Some(&catalog));

        assert_eq!(
            custom_actions["Input"]["properties"]["type"]["const"],
            "Input"
        );
        assert_eq!(
            action["oneOf"][10]["properties"]["value"]["oneOf"][0]["properties"]["type"]["const"],
            "Input"
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
    fn token_from_params_extracts_string_token_only() {
        assert_eq!(
            token_from_params(&serde_json::json!({ "session_token": "abc" })),
            Some("abc".to_string())
        );
        assert_eq!(
            token_from_params(&serde_json::json!({ "session_token": 123 })),
            None
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
                MAX_HTTP_BODY_BYTES + 1
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
        // A capability that was never granted by default is still absent.
        assert!(!AgentCapability::default().contains(AgentCapability::MUTATE_ECS));
    }

    #[test]
    fn authorize_readonly_enforces_session_token_when_configured() {
        let bridge = JsonRpcBridge::new(RemoteSecurity {
            session_token: Some("secret".to_string()),
            ..Default::default()
        });

        // Read-only inspectors pass no capability; only the session token matters.
        assert!(bridge.authorize(None, &json!({})).is_err());
        assert!(
            bridge
                .authorize(None, &json!({ "session_token": "wrong" }))
                .is_err()
        );
        assert!(
            bridge
                .authorize(None, &json!({ "session_token": "secret" }))
                .is_ok()
        );

        // With no token configured, read-only access stays open.
        assert!(JsonRpcBridge::default().authorize(None, &json!({})).is_ok());
    }

    #[test]
    fn authorize_control_requires_capability_and_token() {
        let bridge = JsonRpcBridge::new(RemoteSecurity {
            session_token: Some("secret".to_string()),
            ..Default::default()
        });

        // CONTROL is granted by default, so a correct token authorizes it.
        assert!(
            bridge
                .authorize(
                    Some(AgentCapability::CONTROL),
                    &json!({ "session_token": "secret" })
                )
                .is_ok()
        );
        assert!(
            bridge
                .authorize(Some(AgentCapability::CONTROL), &json!({}))
                .is_err()
        );
        assert!(
            bridge
                .authorize(
                    Some(AgentCapability::CONTROL),
                    &json!({ "session_token": "wrong" })
                )
                .is_err()
        );

        // Dropping CONTROL rejects even a correct token.
        let mut capabilities = AgentCapability::default();
        capabilities.remove(AgentCapability::CONTROL);
        let no_control = JsonRpcBridge::new(RemoteSecurity {
            session_token: Some("secret".to_string()),
            capabilities,
        });
        assert!(
            no_control
                .authorize(
                    Some(AgentCapability::CONTROL),
                    &json!({ "session_token": "secret" })
                )
                .is_err()
        );
    }

    #[test]
    fn validate_jsonrpc_version_rejects_anything_other_than_two_dot_zero() {
        fn req(version: Option<&str>) -> JsonRpcRequest {
            JsonRpcRequest {
                jsonrpc: version.map(ToOwned::to_owned),
                id: json!(1),
                method: "agent.info".to_string(),
                params: json!({}),
            }
        }

        assert!(validate_jsonrpc_version(&req(Some("2.0"))).is_ok());
        // Absent version is tolerated to keep existing clients working.
        assert!(validate_jsonrpc_version(&req(None)).is_ok());
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
}
