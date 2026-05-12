use anyhow::{Result, anyhow};
use bevy_agent_core::{AgentAction, ControlMode, ObservationMode, SnapshotId};
use bevy_agent_runner::{AgentApp, AgentEnvironment, ResetOptions};
use bevy_agent_snapshot::SnapshotStore;
use bitflags::bitflags;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

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
                "type": "enum",
                "actions": ["Noop", "Move", "Look", "Jump", "Crouch", "Sprint", "Interact", "Attack", "UseItem", "Dodge", "Custom"]
            })),
            "agent.observation_space" => Ok(json!({
                "modes": ["PlayerKnowledge", "FullDebugState", "DiffSinceLastTick", "PixelFrame", "Hybrid"],
                "default": "Hybrid"
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
