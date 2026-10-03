use crate::MAX_MESSAGE_BYTES;
use crate::protocol::*;
use crate::schema::*;
use crate::security::{
    AgentCapability, RPC_AUTH_ERROR, RPC_INTERNAL_ERROR, RPC_INVALID_PARAMS, RPC_INVALID_REQUEST,
    RPC_METHOD_NOT_FOUND, RPC_PARSE_ERROR, RemoteSecurity, capability_for_observation_mode,
    write_bundle_exclusive,
};
use crate::{MAX_CAPTURE_TIMEOUT_FRAMES, MAX_TICKS_PER_REQUEST};
use anyhow::{Context, Result, anyhow};
use bevy_agent_core::{
    AgentAction, AgentActionCatalog, AgentObservationCatalog, ControlMode, EnvironmentMetadata,
    ObservationConfig, ObservationMode,
};
use bevy_agent_runner::{
    AgentApp, AgentEnvironment, ReplayBundle, ResetOptions, VisualCaptureOptions,
};
use bevy_agent_snapshot::SnapshotStore;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{io::Read, time::Duration};
const PNG_MAGIC: [u8; 8] = [137, 80, 78, 71, 13, 10, 26, 10];

/// Stops serialization at the transport budget rather than allocating an
/// arbitrarily large encoded response and checking it afterward.
pub(crate) fn serialize_json_bounded(value: &impl Serialize, pretty: bool) -> Result<Vec<u8>> {
    serialize_json_with_limit(value, pretty, MAX_MESSAGE_BYTES)
}

fn serialize_json_with_limit(
    value: &impl Serialize,
    pretty: bool,
    limit: usize,
) -> Result<Vec<u8>> {
    struct MessageWriter {
        bytes: Vec<u8>,
        limit: usize,
    }

    impl std::io::Write for MessageWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len()
                > self
                    .limit
                    .saturating_sub(1)
                    .saturating_sub(self.bytes.len())
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("JSON exceeds message limit of {} bytes", self.limit),
                ));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = MessageWriter {
        bytes: Vec::new(),
        limit,
    };
    if pretty {
        serde_json::to_writer_pretty(&mut writer, value)?;
    } else {
        serde_json::to_writer(&mut writer, value)?;
    }
    Ok(writer.bytes)
}

pub(crate) fn serialize_response(response: &JsonRpcResponse) -> String {
    serialize_response_with_limit(response, MAX_MESSAGE_BYTES)
}

/// Reserve response-envelope space when an execution result is nested in status.
/// Internal callers supply at least 256 bytes for a correlated error envelope.
pub(crate) fn serialize_response_with_limit(response: &JsonRpcResponse, limit: usize) -> String {
    let encoded = serialize_json_with_limit(response, false, limit)
        .or_else(|error| {
            let id = match response {
                JsonRpcResponse::Result { id, .. } | JsonRpcResponse::Error { id, .. } => {
                    id.clone()
                }
            };
            serialize_json_with_limit(
                &JsonRpcResponse::Error {
                    jsonrpc: "2.0",
                    id,
                    error: into_internal(error),
                },
                false,
                limit,
            )
        })
        .unwrap_or_else(|_| {
            serde_json::to_vec(&JsonRpcResponse::Error {
                jsonrpc: "2.0",
                id: Value::Null,
                error: into_internal("response and request identifier exceed message limit"),
            })
            .expect("bounded error is serializable")
        });
    String::from_utf8(encoded).expect("JSON serialization produces UTF-8")
}

#[derive(Clone, Debug, Default)]
pub struct JsonRpcBridge {
    pub security: RemoteSecurity,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 128), regex(pattern = "^[A-Za-z0-9_.:-]+$"))]
    pub retry_key: Option<String>,
    #[serde(default = "empty_params", deserialize_with = "deserialize_unique_json")]
    #[schemars(with = "serde_json::Map<String, Value>")]
    pub params: Value,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(untagged, deny_unknown_fields)]
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

/// A bounded, typed, authenticated request ready for its simulation owner.
/// Parameter ownership is retained so executing it does not parse the payload again.
pub struct PreparedRequest {
    pub(crate) id: Value,
    pub(crate) command: RpcCommand,
    pub(crate) retry_key: Option<String>,
    pub(crate) admitted: bool,
}

impl PreparedRequest {
    #[must_use]
    pub const fn id(&self) -> &Value {
        &self.id
    }
    #[must_use]
    pub fn method(&self) -> RpcMethod {
        self.command.method()
    }
    pub(crate) fn retry_identity(&self) -> Option<std::sync::Arc<str>> {
        self.retry_key.as_ref().map(|_| {
            let mut params = self.command.serialized_params();
            params
                .as_object_mut()
                .expect("object params")
                .remove("session_token");
            std::sync::Arc::from(params.to_string())
        })
    }
}

impl std::fmt::Debug for PreparedRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedRequest")
            .field("id", &self.id)
            .field("method", &self.method())
            .finish_non_exhaustive()
    }
}

fn empty_params() -> Value {
    json!({})
}

/// Preserve object uniqueness while building the one owned parameter tree.
/// A plain Value decoder silently overwrites duplicate fields before DTO decoding.
fn deserialize_unique_json<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Value, D::Error> {
    struct UniqueValue(Value);
    impl<'de> Deserialize<'de> for UniqueValue {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            deserialize_unique_json(deserializer).map(Self)
        }
    }
    struct Visitor;
    impl<'de> serde::de::Visitor<'de> for Visitor {
        type Value = Value;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("JSON with unique object fields")
        }
        fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Value, E> {
            Ok(Value::Bool(value))
        }
        fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Value, E> {
            Ok(json!(value))
        }
        fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Value, E> {
            Ok(json!(value))
        }
        fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Value, E> {
            serde_json::Number::from_f64(value)
                .map(Value::Number)
                .ok_or_else(|| E::custom("nonfinite JSON number"))
        }
        fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Value, E> {
            Ok(Value::String(value.to_owned()))
        }
        fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Value, E> {
            Ok(Value::String(value))
        }
        fn visit_unit<E: serde::de::Error>(self) -> Result<Value, E> {
            Ok(Value::Null)
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut sequence: A,
        ) -> Result<Value, A::Error> {
            let mut values = Vec::new();
            while let Some(value) = sequence.next_element::<UniqueValue>()? {
                values.push(value.0);
            }
            Ok(Value::Array(values))
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
            let mut values = serde_json::Map::new();
            while let Some(key) = map.next_key::<String>()? {
                if values.contains_key(&key) {
                    return Err(serde::de::Error::custom("duplicate JSON object field"));
                }
                values.insert(key, map.next_value::<UniqueValue>()?.0);
            }
            Ok(Value::Object(values))
        }
    }
    deserializer.deserialize_any(Visitor)
}

fn error_response(id: Value, error: JsonRpcError) -> JsonRpcResponse {
    JsonRpcResponse::Error {
        jsonrpc: "2.0",
        id,
        error,
    }
}

/// JSON-RPC bridge executing remote calls against a single simulation owner.
///
/// Supported subset: single JSON-RPC 2.0 calls only. Batch requests and
/// notifications are NOT supported; each input must be one object with
/// `method`, `id`, and optional `params`.
impl JsonRpcBridge {
    /// Construct a bridge with validated authentication and CORS configuration.
    pub fn new(security: RemoteSecurity) -> Result<Self> {
        security.validate()?;
        Ok(Self { security })
    }

    /// Decode, bound, and authenticate before admitting a command to its owner.
    pub fn prepare_request(&self, input: &str) -> Result<PreparedRequest, JsonRpcResponse> {
        if input.len() > MAX_MESSAGE_BYTES {
            return Err(error_response(
                Value::Null,
                invalid_params(format!(
                    "request exceeds message limit of {MAX_MESSAGE_BYTES} bytes"
                )),
            ));
        }
        let request = parse_jsonrpc_request(input)?;
        self.prepare_decoded_request(request)
    }

    fn prepare_decoded_request(
        &self,
        request: JsonRpcRequest,
    ) -> Result<PreparedRequest, JsonRpcResponse> {
        let id = match &request.id {
            id if valid_identifier(id) => request.id.clone(),
            _ => Value::Null,
        };
        let retry_key = request.retry_key.clone();
        let prepare = || -> Result<RpcCommand, JsonRpcError> {
            validate_jsonrpc_version(&request).map_err(|error| JsonRpcError {
                code: RPC_INVALID_REQUEST,
                message: error.to_string(),
                data: None,
            })?;
            let method = RpcMethod::from_name(&request.method)
                .ok_or_else(|| method_not_found(format!("unknown method {}", request.method)))?;
            let command =
                RpcCommand::decode(method, request.params).map_err(into_invalid_params)?;
            command.validate_limits().map_err(invalid_params)?;
            self.check_token(command.token()).map_err(into_auth)?;
            if let Some(key) = &retry_key {
                crate::operations::validate_retry_key(key).map_err(invalid_params)?;
            }
            if retry_key.is_some() && command.method() == RpcMethod::OperationStatus {
                return Err(invalid_params(
                    "status requests cannot have an envelope retry_key",
                ));
            }
            Ok(command)
        };
        prepare()
            .map(|command| PreparedRequest {
                id: id.clone(),
                command,
                retry_key,
                admitted: false,
            })
            .map_err(|error| error_response(id, error))
    }

    pub fn handle_json(&self, env: &mut AgentApp, input: &str) -> String {
        let response = match self.prepare_request(input) {
            Ok(request) => self.handle_prepared(env, request),
            Err(response) => response,
        };
        serialize_response(&response)
    }

    pub fn handle_request(&self, env: &mut AgentApp, request: JsonRpcRequest) -> JsonRpcResponse {
        match self.prepare_decoded_request(request) {
            Ok(request) => self.handle_prepared(env, request),
            Err(response) => response,
        }
    }

    pub fn handle_prepared(&self, env: &mut AgentApp, request: PreparedRequest) -> JsonRpcResponse {
        if request.retry_key.is_some() && !request.admitted {
            return error_response(
                request.id,
                invalid_params("retry keys require an HTTP or WebSocket service ledger"),
            );
        }
        match self.dispatch(env, request.command) {
            Ok(result) => JsonRpcResponse::Result {
                jsonrpc: "2.0",
                id: request.id,
                result,
            },
            Err(error) => error_response(request.id, error),
        }
    }

    pub fn serve_stdio(&self, env: &mut AgentApp) -> Result<()> {
        use std::io::{self, BufRead, Write};

        let stdin = io::stdin();
        let mut input = stdin.lock();
        let mut stdout = io::stdout();
        loop {
            let mut line = Vec::new();
            let read = input
                .by_ref()
                .take((MAX_MESSAGE_BYTES + 1) as u64)
                .read_until(b'\n', &mut line)?;
            if read == 0 {
                break;
            }
            if line.len() > MAX_MESSAGE_BYTES {
                return Err(anyhow!("stdio request exceeds {MAX_MESSAGE_BYTES} bytes"));
            }
            let response = self.handle_json(env, std::str::from_utf8(&line)?);
            writeln!(stdout, "{response}")?;
            stdout.flush()?;
        }
        Ok(())
    }

    fn dispatch(&self, env: &mut AgentApp, command: RpcCommand) -> Result<Value, JsonRpcError> {
        match command {
            RpcCommand::Info(_) => {
                let metadata = env
                    .world()
                    .get_resource::<EnvironmentMetadata>()
                    .cloned()
                    .unwrap_or_default();
                Ok(json!({
                    "name": metadata.name,
                    "version": metadata.version,
                    "description": metadata.description,
                    "agent_control_version": env!("CARGO_PKG_VERSION"),
                    "bevy_version": "0.18.1",
                    "tick": env.current_tick(),
                    "capabilities": self.security.capabilities.bits(),
                }))
            }
            RpcCommand::ActionSpace(_) => {
                let catalog = env.world().get_resource::<AgentActionCatalog>();
                Ok(json!({
                    "type": "json_schema",
                    "actions": supported_action_names(catalog),
                    "schema": agent_action_schema_with_custom_actions(catalog),
                    "custom_actions": custom_action_schema_map(catalog),
                }))
            }
            RpcCommand::ObservationSpace(_) => {
                let catalog = env.world().get_resource::<AgentObservationCatalog>();
                Ok(json!({
                    "modes": catalog.map(|catalog| catalog.supported_modes()),
                    "default": current_observation_mode(env),
                    "schema": observation_schema_with_catalog(catalog)
                }))
            }
            RpcCommand::Schema(_) => {
                let catalog = env.world().get_resource::<AgentActionCatalog>();
                let observations = env.world().get_resource::<AgentObservationCatalog>();
                Ok(json!({
                    "mutation_failure": mutation_failure_schema(),
                    "request": jsonrpc_request_schema(),
                    "response": jsonrpc_response_schema(),
                    "error": jsonrpc_error_schema(),
                    "methods": rpc_method_schema(catalog, observations),
                    "action": agent_action_schema_with_custom_actions(catalog),
                    "custom_actions": custom_action_schema_map(catalog),
                    "observation": observation_schema_with_catalog(observations),
                    "step_response": step_response_schema_with_catalog(observations),
                    "reset_response": reset_response_schema_with_catalog(observations),
                    "step_many_response": step_many_response_schema_with_catalog(observations),
                    "visual_capture": visual_capture_schema(),
                    "operation_status": operation_status_schema(),
                }))
            }
            RpcCommand::Reset(params) => {
                self.require_capability(AgentCapability::STEP)
                    .map_err(into_auth)?;
                self.authorize_observation_mode(env, &params.options.observation_mode)?;
                let options = params.options;
                env.reset_with_response(options)
                    .map_err(into_internal)
                    .and_then(|v| serde_json::to_value(v).map_err(into_internal))
            }
            RpcCommand::Step(params) => {
                // Request-local mode: read-only snapshot of the requested mode
                // (never mutate the global ObservationConfig here). The same
                // `requested` mode is used for authorization AND for rendering
                // the response after `ensure_initialized_with_mode`, so a
                // first-step implicit reset cannot serve an unauthorized mode.
                let requested = params
                    .observation_mode
                    .clone()
                    .unwrap_or_else(|| current_observation_mode(env));
                self.require_capability(AgentCapability::STEP)
                    .map_err(into_auth)?;
                self.authorize_observation_mode(env, &requested)?;
                env.validate_actions(std::slice::from_ref(&params.action))
                    .map_err(|error| invalid_params(error.to_string()))?;
                step_with_request_mode(env, params.action, &requested).map_err(into_internal)
            }
            RpcCommand::StepMany(params) => {
                let active_mode = current_observation_mode(env);
                self.require_capability(AgentCapability::STEP)
                    .map_err(into_auth)?;
                self.authorize_observation_mode(env, &active_mode)?;
                env.validate_actions(&params.actions)
                    .map_err(|error| invalid_params(error.to_string()))?;
                ensure_initialized_with_mode(env, &active_mode).map_err(into_internal)?;
                let mut response = env
                    .step_many_with_response(
                        params.actions,
                        params.return_observations == ObservationReturn::All,
                    )
                    .map_err(into_internal)?;
                if params.return_observations == ObservationReturn::None {
                    response.observation = None;
                }
                serde_json::to_value(response).map_err(into_internal)
            }
            RpcCommand::FastForward(params) => {
                let active_mode = current_observation_mode(env);
                self.require_capability(AgentCapability::STEP)
                    .map_err(into_auth)?;
                self.authorize_observation_mode(env, &active_mode)?;
                env.validate_actions(&[AgentAction::Noop])
                    .map_err(|error| invalid_params(error.to_string()))?;
                ensure_initialized_with_mode(env, &active_mode).map_err(into_internal)?;
                env.fast_forward(params.ticks)
                    .map_err(into_internal)
                    .and_then(|v| serde_json::to_value(v).map_err(into_internal))
            }
            RpcCommand::Observe(params) => {
                // Request-local: authorize the SAME post-init mode that
                // `observe_with_request_mode` renders, without mutating the
                // global ObservationConfig.
                let requested = params
                    .observation_mode
                    .unwrap_or_else(|| current_observation_mode(env));
                self.authorize_observation_mode(env, &requested)?;
                observe_with_request_mode(env, &requested).map_err(into_internal)
            }
            RpcCommand::VisualCapture(params) => {
                let options = self.prepare_visual_capture(&params)?;
                env.capture_visual(options)
                    .map_err(into_internal)
                    .and_then(|result| {
                        verify_visual_capture_file(&result.path).map_err(into_internal)?;
                        serde_json::to_value(result).map_err(into_internal)
                    })
            }
            RpcCommand::SnapshotCreate(_) => {
                self.require_capability(AgentCapability::SNAPSHOT)
                    .map_err(into_auth)?;
                env.snapshot()
                    .map_err(into_internal)
                    .and_then(|v| serde_json::to_value(v).map_err(into_internal))
            }
            RpcCommand::SnapshotRestore(params) => {
                self.require_capability(AgentCapability::RESTORE)
                    .map_err(into_auth)?;
                env.restore(params.snapshot_id).map_err(into_internal)?;
                Ok(Value::Null)
            }
            RpcCommand::SnapshotList(_) => {
                self.require_capability(AgentCapability::SNAPSHOT)
                    .map_err(into_auth)?;
                let store = env.world().resource::<SnapshotStore>();
                let mut snapshots = store
                    .iter()
                    .map(|(_, snapshot)| snapshot)
                    .collect::<Vec<_>>();
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
            RpcCommand::SnapshotDelete(params) => {
                self.require_capability(AgentCapability::SNAPSHOT)
                    .map_err(into_auth)?;
                env.delete_snapshot(params.snapshot_id)
                    .map_err(into_internal)?;
                Ok(Value::Null)
            }
            RpcCommand::TimelineCurrent(_) => Ok(json!({
                "tick": env.current_tick(),
                "timeline_id": env.world().resource::<bevy_agent_core::AgentControlState>().timeline_id,
                "branch_id": env.world().resource::<bevy_agent_core::AgentControlState>().branch_id,
            })),
            RpcCommand::TimelineBranch(params) => {
                self.require_capability(AgentCapability::BRANCH)
                    .map_err(into_auth)?;
                validate_history_target(env, params.from_tick, "branch")
                    .map_err(|e| invalid_params(e.to_string()))?;
                let branch_id = env
                    .branch(params.from_tick, params.label)
                    .map_err(into_internal)?;
                Ok(json!({
                    "timeline_id": env.world().resource::<bevy_agent_core::AgentControlState>().timeline_id,
                    "branch_id": branch_id,
                    "current_tick": env.current_tick(),
                }))
            }
            RpcCommand::TimelineRestoreTick(params) => {
                self.require_capability(AgentCapability::RESTORE)
                    .map_err(into_auth)?;
                validate_history_target(env, params.tick, "restore_tick")
                    .map_err(|e| invalid_params(e.to_string()))?;
                env.restore_tick(params.tick).map_err(into_internal)?;
                Ok(json!({ "current_tick": env.current_tick() }))
            }
            RpcCommand::ControlSetMode(params) => {
                self.require_capability(AgentCapability::CONTROL)
                    .map_err(into_auth)?;
                env.world_mut()
                    .resource_mut::<bevy_agent_core::AgentControlState>()
                    .mode = params.mode;
                Ok(Value::Null)
            }
            RpcCommand::ControlPause(_) => {
                self.require_capability(AgentCapability::CONTROL)
                    .map_err(into_auth)?;
                env.world_mut()
                    .resource_mut::<bevy_agent_core::AgentControlState>()
                    .mode = ControlMode::Paused;
                Ok(Value::Null)
            }
            RpcCommand::ControlResume(_) => {
                self.require_capability(AgentCapability::CONTROL)
                    .map_err(into_auth)?;
                env.world_mut()
                    .resource_mut::<bevy_agent_core::AgentControlState>()
                    .mode = ControlMode::Agent;
                Ok(Value::Null)
            }
            RpcCommand::ReplayStart(_) => {
                self.require_capability(AgentCapability::STEP)
                    .map_err(into_auth)?;
                // Route through `AgentApp::start_recording` (baseline snapshot
                // capture + timeline pinning), not the free recording fn.
                // Ensure initialization first so the baseline tick is valid.
                // Always pass `None` so a fresh baseline is captured for the
                // current tick; reusing a stale `initial_snapshot` from a
                // previous recording would pin the new log to an old baseline.
                let init_mode = current_observation_mode(env);
                ensure_initialized_with_mode(env, &init_mode).map_err(into_internal)?;
                env.start_recording(None).map_err(into_internal)?;
                Ok(json!({ "recording": true }))
            }
            RpcCommand::ReplayStop(_) => {
                self.require_capability(AgentCapability::STEP)
                    .map_err(into_auth)?;
                let records = env.stop_recording().map_err(into_internal)?;
                Ok(json!({
                    "recording": false,
                    "records": records,
                }))
            }
            RpcCommand::ReplayExport(params) => {
                self.require_capability(
                    AgentCapability::SNAPSHOT_EXPORT
                        | (if params.path.is_some() {
                            AgentCapability::FILESYSTEM
                        } else {
                            AgentCapability::empty()
                        }),
                )
                .map_err(into_auth)?;
                let bundle = env.export_replay_bundle().map_err(into_internal)?;
                let records = bundle.log.records.len();
                let checkpoints = bundle.log.branch_checkpoints.len();
                if let Some(path) = &params.path {
                    let encoded = serialize_json_bounded(&bundle, true).map_err(into_internal)?;
                    let resolved = self
                        .security
                        .resolve_artifact_path(path)
                        .map_err(|error| invalid_params(error.to_string()))?;
                    if let Some(parent) = resolved.parent() {
                        std::fs::create_dir_all(parent).map_err(into_internal)?;
                    }
                    write_bundle_exclusive(&resolved, &encoded).map_err(into_internal)?;
                    Ok(json!({ "path": resolved, "records": records, "checkpoints": checkpoints }))
                } else {
                    Ok(json!({ "records": records, "checkpoints": checkpoints, "bundle": bundle }))
                }
            }
            RpcCommand::ReplayLoad(params) => {
                self.require_capability(
                    AgentCapability::RESTORE
                        | (if params.path.is_some() {
                            AgentCapability::FILESYSTEM
                        } else {
                            AgentCapability::empty()
                        }),
                )
                .map_err(into_auth)?;
                let bundle = if let Some(bundle) = params.bundle {
                    bundle
                } else if let Some(path) = params.path {
                    let resolved = self
                        .security
                        .resolve_artifact_path(&path)
                        .map_err(|error| invalid_params(error.to_string()))?;
                    let bytes = read_replay_file(&resolved)
                        .with_context(|| {
                            format!("reading replay bundle from {}", resolved.display())
                        })
                        .map_err(into_internal)?;
                    serde_json::from_str::<ReplayBundle>(&bytes).map_err(into_invalid_params)?
                } else {
                    return Err(invalid_params(
                        "agent.replay.load requires one of: path, bundle",
                    ));
                };
                let records = bundle.log.records.len();
                let checkpoints = bundle.log.branch_checkpoints.len();
                // Validate the branch graph BEFORE installing anything: the
                // runner install path writes snapshots/store state, so a
                // malformed topology must be rejected up front.
                env.validate_replay_bundle(&bundle)
                    .map_err(|error| invalid_params(error.to_string()))?;
                env.load_replay_bundle(bundle).map_err(into_internal)?;
                Ok(json!({
                    "records": records,
                    "checkpoints": checkpoints,
                }))
            }
            RpcCommand::OperationStatus(_) => Err(into_internal(
                "agent.operations.status requires a network RemoteService",
            )),
        }
    }

    pub(crate) fn prepare_visual_capture(
        &self,
        params: &VisualCaptureParams,
    ) -> Result<VisualCaptureOptions, JsonRpcError> {
        self.check_token(params.session_token.as_deref())
            .map_err(into_auth)?;
        self.require_capability(AgentCapability::FILESYSTEM | AgentCapability::VISUAL_CAPTURE)
            .map_err(into_auth)?;
        let mut options = VisualCaptureOptions::default();
        options.timeout_frames = params.timeout_frames.unwrap_or(options.timeout_frames);
        if !(1..=MAX_CAPTURE_TIMEOUT_FRAMES).contains(&options.timeout_frames) {
            return Err(invalid_params(format!(
                "timeout_frames must be between 1 and {MAX_CAPTURE_TIMEOUT_FRAMES}"
            )));
        }
        options.output_dir = match params.output_dir.as_deref() {
            Some(directory) => self.security.resolve_output_dir(directory),
            None => self.security.resolve_artifact_path("."),
        }
        .map_err(|error| invalid_params(error.to_string()))?;
        options.label = params.label.clone();
        options.source = params.source.unwrap_or_default();
        Ok(options)
    }

    pub(crate) fn check_token(&self, token: Option<&str>) -> Result<()> {
        self.security.validate()?;
        if let Some(expected) = &self.security.session_token {
            let provided = token.unwrap_or("");
            if !constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
                return Err(anyhow!("invalid or missing session token"));
            }
        }
        Ok(())
    }

    pub(crate) fn require_capability(&self, capability: AgentCapability) -> Result<()> {
        if !self.security.capabilities.contains(capability) {
            return Err(anyhow!("missing remote capability {capability:?}"));
        }
        Ok(())
    }

    /// Authorize the observation-visibility gate for a single requested mode.
    /// All response-returning methods (reset/step/step_many/fast_forward/
    /// observe) must authorize through this helper with the SAME post-init
    /// mode that produces the response, so a first-step implicit reset can
    /// never serve a mode the caller was not authorized for.
    fn authorize_observation_mode(
        &self,
        env: &AgentApp,
        mode: &ObservationMode,
    ) -> Result<(), JsonRpcError> {
        let catalog = env
            .world()
            .get_resource::<AgentObservationCatalog>()
            .ok_or_else(|| into_internal("missing observation catalog"))?;
        catalog
            .validate_mode(mode)
            .map_err(|error| invalid_params(error.to_string()))?;
        self.require_capability(capability_for_observation_mode(mode))
            .map_err(into_auth)
    }
}

/// Read-only view of the current global observation mode (no mutation).
fn current_observation_mode(env: &AgentApp) -> ObservationMode {
    env.world()
        .get_resource::<ObservationConfig>()
        .map(|c| c.mode.clone())
        .unwrap_or_default()
}

/// Initialize an authorized batch/fast-forward response in its selected mode.
fn ensure_initialized_with_mode(env: &mut AgentApp, mode: &ObservationMode) -> Result<()> {
    if env.has_reset() {
        return Ok(());
    }
    env.reset_with_response(ResetOptions {
        observation_mode: mode.clone(),
        ..ResetOptions::default()
    })?;
    Ok(())
}

/// Step through the runner with one request-local observation extraction.
fn step_with_request_mode(
    env: &mut AgentApp,
    action: AgentAction,
    mode: &ObservationMode,
) -> Result<Value> {
    env.step_with_observation_mode(action, mode.clone())
        .and_then(|response| serde_json::to_value(response).map_err(anyhow::Error::from))
}

/// Delegate observation extraction and request-local mode handling to the runner.
fn observe_with_request_mode(env: &mut AgentApp, mode: &ObservationMode) -> Result<Value> {
    env.observe(mode.clone())
        .and_then(|observation| serde_json::to_value(observation).map_err(anyhow::Error::from))
}

/// Apply the remote budget to the runner's actual reconstruction plan.
fn validate_history_target(env: &AgentApp, target_tick: u64, what: &str) -> Result<()> {
    env.validate_history_navigation(target_tick, MAX_TICKS_PER_REQUEST)
        .with_context(|| format!("invalid {what} target tick {target_tick}"))
}

fn auth_error(message: impl Into<String>) -> JsonRpcError {
    JsonRpcError {
        code: RPC_AUTH_ERROR,
        message: message.into(),
        data: None,
    }
}

fn invalid_params(message: impl Into<String>) -> JsonRpcError {
    JsonRpcError {
        code: RPC_INVALID_PARAMS,
        message: message.into(),
        data: None,
    }
}

fn method_not_found(message: impl Into<String>) -> JsonRpcError {
    JsonRpcError {
        code: RPC_METHOD_NOT_FOUND,
        message: message.into(),
        data: None,
    }
}

fn into_internal(error: impl std::fmt::Display + 'static) -> JsonRpcError {
    let data = (&error as &dyn std::any::Any)
        .downcast_ref::<anyhow::Error>()
        .and_then(|error| error.downcast_ref::<bevy_agent_runner::MutationFailure>())
        .map(|failure| serde_json::to_value(failure).expect("mutation outcome is serializable"));
    JsonRpcError {
        code: RPC_INTERNAL_ERROR,
        message: error.to_string(),
        data,
    }
}

fn into_invalid_params(error: serde_json::Error) -> JsonRpcError {
    invalid_params(format!("invalid params: {error}"))
}

fn into_auth(error: anyhow::Error) -> JsonRpcError {
    auth_error(error.to_string())
}

/// Verify a visual capture file exists and starts with the PNG magic bytes.
pub(crate) fn verify_visual_capture_file(path: &std::path::Path) -> Result<()> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("reading capture {}", path.display()))?;
    let mut bytes = [0; PNG_MAGIC.len()];
    if file.read_exact(&mut bytes).is_err() || bytes != PNG_MAGIC {
        return Err(anyhow!("capture {} is missing PNG magic", path.display()));
    }
    Ok(())
}

/// Convert frame-count timeouts to a wall-clock deadline (60 fps assumption).
#[must_use]
pub fn timeout_frames_to_duration(timeout_frames: u32) -> Duration {
    Duration::from_secs_f64(f64::from(timeout_frames) / 60.0)
}

fn valid_identifier(id: &Value) -> bool {
    match id {
        Value::String(value) => value.chars().count() <= 256,
        Value::Number(_) | Value::Null => true,
        _ => false,
    }
}

/// Validate the required JSON-RPC 2.0 version and supported identifier shape.
pub(crate) fn validate_jsonrpc_version(request: &JsonRpcRequest) -> Result<()> {
    if !request.params.is_object() {
        return Err(anyhow!("invalid Request: params must be an object"));
    }
    if !valid_identifier(&request.id) {
        return Err(anyhow!(
            "invalid Request: id must be a string of at most 256 characters, number, or null"
        ));
    }
    if request.jsonrpc != "2.0" {
        return Err(anyhow!("invalid Request: jsonrpc must be \"2.0\""));
    }
    Ok(())
}

/// Decode syntax separately from the JSON-RPC envelope so malformed JSON and
/// malformed requests produce their distinct standard error codes.
pub(crate) fn parse_jsonrpc_request(input: &str) -> Result<JsonRpcRequest, JsonRpcResponse> {
    let failure = |id, code, message| JsonRpcResponse::Error {
        jsonrpc: "2.0",
        id,
        error: JsonRpcError {
            code,
            message,
            data: None,
        },
    };
    // The success path parses once. Only invalid envelopes need a best-effort
    // Value decode to recover a valid correlation identifier.
    let request: JsonRpcRequest = serde_json::from_str(input).map_err(|error| {
        if error.is_data() {
            let value = match serde_json::from_str::<Value>(input) {
                Ok(value) => value,
                Err(syntax) => {
                    return failure(
                        Value::Null,
                        RPC_PARSE_ERROR,
                        format!("parse error: {syntax}"),
                    );
                }
            };
            let id = value
                .get("id")
                .cloned()
                .filter(valid_identifier)
                .unwrap_or(Value::Null);
            failure(id, RPC_INVALID_REQUEST, format!("invalid Request: {error}"))
        } else {
            failure(
                Value::Null,
                RPC_PARSE_ERROR,
                format!("parse error: {error}"),
            )
        }
    })?;
    let id = match &request.id {
        id if valid_identifier(id) => request.id.clone(),
        _ => Value::Null,
    };
    validate_jsonrpc_version(&request)
        .map_err(|error| failure(id, RPC_INVALID_REQUEST, error.to_string()))?;
    Ok(request)
}

pub(crate) fn read_replay_file(path: &std::path::Path) -> Result<String> {
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take((MAX_MESSAGE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_MESSAGE_BYTES {
        return Err(anyhow!(
            "replay file exceeds limit of {MAX_MESSAGE_BYTES} bytes"
        ));
    }
    Ok(String::from_utf8(bytes)?)
}

pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}
