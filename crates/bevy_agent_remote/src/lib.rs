//! JSON-RPC, HTTP, WebSocket, and stdio remote-control bridge for agent apps.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU32, AtomicU64, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use bevy::prelude::*;
use bevy_agent_core::{
    AgentAction, AgentActionCatalog, AgentActionKind, AgentObservationCatalog, ControlMode,
    EnvironmentMetadata, LastStepResponse, ObservationConfig, ObservationMode, SnapshotId,
    collect_observation_with_mode,
};
use bevy_agent_replay::{ReplayLog, Timeline, collect_replay_references, stop_recording};
use bevy_agent_runner::{
    AgentApp, AgentEnvironment, CaptureSource, ReplayBundle, ResetOptions, VisualCaptureOptions,
};
#[cfg(feature = "visual")]
use bevy_agent_runner::{AgentVisualCaptureRenderer, VisualCaptureResult, visual_capture_path};
use bevy_agent_snapshot::{SnapshotStore, delete_snapshot_checked};
use bitflags::bitflags;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};

const MAX_HTTP_HEADER_BYTES: usize = 32 * 1024;
const MAX_HTTP_BODY_BYTES: usize = 8 * 1024 * 1024;
const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(30);
const HTTP_WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// Overall deadline for a single remote request, including main-thread pump.
const HTTP_REQUEST_DEADLINE: Duration = Duration::from_secs(30);
/// Maximum number of actions accepted in a single `agent.step_many` call.
pub const MAX_ACTIONS_PER_REQUEST: usize = 1024;
/// Maximum number of ticks accepted in a single `agent.fast_forward` call.
pub const MAX_TICKS_PER_REQUEST: u64 = 10_000;
/// PNG magic bytes used to verify visual captures after save.
const PNG_MAGIC: [u8; 8] = [137, 80, 78, 71, 13, 10, 26, 10];
/// A larger yaw delta is directionally redundant with a turn inside this range.
const LOOK_YAW_DELTA_LIMIT_RADIANS: f64 = std::f64::consts::PI;
/// This spans the full practical camera pitch range in a single controlled tick.
const LOOK_PITCH_DELTA_LIMIT_RADIANS: f64 = std::f64::consts::FRAC_PI_2;

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
        /// Exporting replay bundles / snapshots to the caller.
        const SNAPSHOT_EXPORT = 1 << 10;
        /// Touching the local filesystem (replay export/load `path`,
        /// visual capture `output_dir`). NOT in the default set so
        /// restricted deployments deny filesystem access by default.
        const FILESYSTEM = 1 << 11;
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
            | Self::SNAPSHOT_EXPORT
    }
}

/// JSON-RPC error codes used by this bridge.
///
/// - `-32700` parse error, `-32600` invalid request, `-32601` method not
///   found, `-32602` invalid params, `-32603` internal error.
/// - `-32001` authentication/authorization failure (server-defined range).
pub const RPC_PARSE_ERROR: i32 = -32700;
pub const RPC_INVALID_REQUEST: i32 = -32600;
pub const RPC_METHOD_NOT_FOUND: i32 = -32601;
pub const RPC_INVALID_PARAMS: i32 = -32602;
pub const RPC_INTERNAL_ERROR: i32 = -32603;
pub const RPC_AUTH_ERROR: i32 = -32001;

#[derive(Clone, Debug, Default)]
pub struct RemoteSecurity {
    pub session_token: Option<String>,
    pub capabilities: AgentCapability,
    /// Root directory confining all filesystem writes/reads (replay export,
    /// replay load from path, visual captures). `None` resolves to the OS
    /// temp dir joined with `bevy-agent-artifacts`.
    pub artifact_root: Option<PathBuf>,
    /// When false (default), absolute paths and `..` traversal are rejected.
    pub allow_absolute_paths: bool,
    /// Optional exact Origin value echoed back as `Access-Control-Allow-Origin`.
    /// When `None` (default) no CORS header is emitted.
    pub allowed_origin: Option<String>,
}

/// Backwards-compatible alias: older specs refer to `BridgeSecurity`.
pub type BridgeSecurity = RemoteSecurity;

impl RemoteSecurity {
    /// Resolved artifact root, defaulting to the OS temp dir.
    #[must_use]
    pub fn artifact_root_resolved(&self) -> PathBuf {
        self.artifact_root
            .clone()
            .unwrap_or_else(|| std::env::temp_dir().join("bevy-agent-artifacts"))
    }

    /// Resolve a caller-supplied relative path against the artifact root,
    /// rejecting absolute paths and `..` traversal unless
    /// `allow_absolute_paths` is set.
    pub fn resolve_artifact_path(&self, raw: &str) -> Result<PathBuf> {
        resolve_confined_path(
            &self.artifact_root_resolved(),
            raw,
            self.allow_absolute_paths,
        )
    }

    /// Resolve a caller-supplied output dir against the artifact root.
    pub fn resolve_output_dir(&self, raw: &std::path::Path) -> Result<PathBuf> {
        let s = raw.to_string_lossy();
        self.resolve_artifact_path(&s)
    }
}

/// Symlink-safe confined path resolution.
///
/// - The artifact `root` is canonicalized once. When the root does not exist
///   yet it is created (`create_dir_all`) so the canonical form covers the
///   real location (symlinked roots resolve to their target).
/// - The destination's nearest existing ancestor is canonicalized, the
///   non-existent remainder is joined back lexically, and the result must
///   stay under the canonical root. `..` components and absolute paths are
///   rejected up front unless `allow_absolute` is set.
/// - With `allow_absolute`, absolute destinations bypass root confinement but
///   still resolve through the nearest-existing-ancestor canonicalization.
///
/// TOCTOU limits: validation and later file creation are not atomic. A
/// concurrent rename/symlink swap between `resolve_*` and file creation can
/// still redirect the final open. Callers writing files must open with
/// exclusive creation (`create_new`, see `write_bundle_exclusive`) and
/// callers with strict integrity needs should additionally open with
/// `O_NOFOLLOW`-style guards / re-validate the parent file id after open.
/// Reads should prefer opening the parent dir and validating before read.
fn resolve_confined_path(root: &PathBuf, raw: &str, allow_absolute: bool) -> Result<PathBuf> {
    use std::path::{Component, Path};
    if raw.is_empty() {
        return Err(anyhow!("empty path is not allowed"));
    }
    let candidate = PathBuf::from(raw);
    if !allow_absolute {
        if raw.split(['/', '\\']).any(|comp| comp == "..") {
            return Err(anyhow!("path traversal (`..`) is not allowed: {raw}"));
        }
        if candidate.is_absolute() {
            return Err(anyhow!("absolute paths are not allowed: {raw}"));
        }
    }
    let joined: PathBuf = if candidate.is_absolute() {
        candidate
    } else {
        root.join(candidate)
    };
    // Reject lexical escape for non-existent paths.
    let mut depth: i32 = 0;
    let mut rooted = false;
    for comp in joined.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => {
                depth = 0;
                rooted = true;
            }
            Component::ParentDir => depth -= 1,
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
        }
        if rooted && depth < 0 {
            return Err(anyhow!("path escapes artifact root: {raw}"));
        }
    }
    // Canonicalize the root once (create it so symlinked roots resolve).
    let root_canon: PathBuf = match root.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            std::fs::create_dir_all(root)
                .with_context(|| format!("creating artifact root {}", root.display()))?;
            root.canonicalize()
                .with_context(|| format!("canonicalizing artifact root {}", root.display()))?
        }
    };
    // Absolute destinations with explicit opt-in bypass confinement.
    if allow_absolute && Path::new(raw).is_absolute() {
        return Ok(canonicalize_nearest_ancestor(&joined).unwrap_or(joined));
    }
    let resolved = canonicalize_nearest_ancestor(&joined).unwrap_or(joined);
    // `starts_with` on canonical prefix covers symlink-redirected ancestors;
    // the lexical depth check above covers non-existent `..` remainder.
    if !resolved.starts_with(&root_canon) {
        return Err(anyhow!("path escapes artifact root: {raw}"));
    }
    Ok(resolved)
}

/// Canonicalize the nearest existing ancestor of `path` and re-append the
/// non-existent remainder lexically. Returns `None` when no ancestor exists
/// (caller falls back to the lexical path).
fn canonicalize_nearest_ancestor(path: &std::path::Path) -> Option<PathBuf> {
    let mut ancestor: &std::path::Path = path;
    let mut remainder: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if ancestor.exists() {
            let canon = ancestor.canonicalize().ok()?;
            let mut out = canon;
            for comp in remainder.iter().rev() {
                out.push(comp);
            }
            return Some(out);
        }
        let parent = ancestor.parent()?;
        if let Some(file_name) = ancestor.file_name() {
            remainder.push(file_name.to_owned());
        }
        // Reached filesystem root without finding an existing ancestor.
        if parent.as_os_str().is_empty() {
            return None;
        }
        ancestor = parent;
    }
}

/// Exclusive file creation (`create_new`): fails when the destination already
/// exists instead of truncating it. Narrows (but does not close) the
/// resolve-vs-open TOCTOU window; callers should still resolve via
/// [`resolve_confined_path`] immediately before calling this.
fn write_bundle_exclusive(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    use std::fs::OpenOptions;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("exclusively creating {}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Centralized per-operation authorization descriptor.
///
/// Each RPC method builds one of these and passes it to
/// [`JsonRpcBridge::authorize_operation`], so capability checks stay in one
/// place instead of scattered `require_capability` calls.
#[derive(Clone, Debug)]
pub struct OperationRequires {
    /// Mutating simulation capability (e.g. `STEP` for step/reset).
    pub mutation: Option<AgentCapability>,
    /// Observation-visibility capability required to read observations
    /// (e.g. `OBSERVE_PLAYER` or `OBSERVE_FULL_STATE`).
    pub observation_visibility: Option<AgentCapability>,
    /// Whether this operation exports snapshot/replay data.
    pub snapshot_export: bool,
    /// Whether this operation imports/restores snapshot/replay data.
    pub restore_import: bool,
    /// Whether this operation touches the filesystem (path/output_dir).
    pub filesystem: bool,
    /// Control-plane capability (e.g. `CONTROL` for pause/resume/set_mode).
    pub control: Option<AgentCapability>,
}

impl OperationRequires {
    #[must_use]
    pub fn none() -> Self {
        Self {
            mutation: None,
            observation_visibility: None,
            snapshot_export: false,
            restore_import: false,
            filesystem: false,
            control: None,
        }
    }
}

/// Capability needed to observe a given mode. `FullDebugState` and `Hybrid`
/// expose full state and require `OBSERVE_FULL_STATE`; every other mode
/// requires only `OBSERVE_PLAYER`.
#[must_use]
pub fn capability_for_observation_mode(mode: &ObservationMode) -> AgentCapability {
    match mode {
        ObservationMode::FullDebugState | ObservationMode::Hybrid => {
            AgentCapability::OBSERVE_FULL_STATE
        }
        ObservationMode::PlayerKnowledge
        | ObservationMode::DiffSinceLastTick
        | ObservationMode::PixelFrame => AgentCapability::OBSERVE_PLAYER,
    }
}

#[derive(Clone, Debug, Default)]
pub struct JsonRpcBridge {
    pub security: RemoteSecurity,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
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
    pub bundle: Option<ReplayBundle>,
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
    pub source: Option<CaptureSource>,
    #[serde(default)]
    pub session_token: Option<String>,
}

fn default_true() -> bool {
    true
}

fn return_all() -> String {
    "all".to_string()
}

/// JSON-RPC bridge executing remote calls against a single simulation owner.
///
/// Supported subset: single JSON-RPC 2.0 calls only. Batch requests and
/// notifications are NOT supported; each input must be one object with
/// `method`, `id`, and optional `params`.
impl JsonRpcBridge {
    #[must_use]
    pub fn new(security: RemoteSecurity) -> Self {
        if let Some(token) = security.session_token.as_deref()
            && token.is_empty()
        {
            panic!("RemoteSecurity::session_token must not be empty; use None for no auth");
        }
        Self { security }
    }

    /// Validating constructor rejecting an empty session token.
    pub fn try_new(security: RemoteSecurity) -> Result<Self> {
        if let Some(token) = security.session_token.as_deref()
            && token.is_empty()
        {
            return Err(anyhow!(
                "RemoteSecurity::session_token must not be empty; use None for no auth"
            ));
        }
        Ok(Self { security })
    }

    /// Alias for [`Self::try_new`] kept for spec compatibility (`with_security`).
    pub fn with_security(security: RemoteSecurity) -> Result<Self> {
        Self::try_new(security)
    }

    pub fn handle_json(&self, env: &mut AgentApp, input: &str) -> String {
        let response = match serde_json::from_str::<JsonRpcRequest>(input) {
            Ok(request) => self.handle_request(env, request),
            Err(error) => JsonRpcResponse::Error {
                jsonrpc: "2.0",
                id: Value::Null,
                error: JsonRpcError {
                    code: RPC_PARSE_ERROR,
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
                    code: RPC_INVALID_REQUEST,
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
                error,
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

    fn dispatch(&self, env: &mut AgentApp, request: JsonRpcRequest) -> Result<Value, JsonRpcError> {
        match request.method.as_str() {
            "agent.info" => {
                self.authorize(None, &request.params).map_err(into_auth)?;
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
            "agent.action_space" => {
                self.authorize(None, &request.params).map_err(into_auth)?;
                let catalog = env.world().get_resource::<AgentActionCatalog>();
                Ok(json!({
                    "type": "json_schema",
                    "actions": supported_action_names(catalog),
                    "schema": agent_action_schema_with_custom_actions(catalog),
                    "custom_actions": custom_action_schema_map(catalog),
                }))
            }
            "agent.observation_space" => {
                self.authorize(None, &request.params).map_err(into_auth)?;
                let catalog = env.world().get_resource::<AgentObservationCatalog>();
                Ok(json!({
                    "modes": ["PlayerKnowledge", "FullDebugState", "DiffSinceLastTick", "PixelFrame", "Hybrid"],
                    "default": "Hybrid",
                    "schema": observation_schema_with_catalog(catalog)
                }))
            }
            "agent.schema" => {
                self.authorize(None, &request.params).map_err(into_auth)?;
                let catalog = env.world().get_resource::<AgentActionCatalog>();
                let observations = env.world().get_resource::<AgentObservationCatalog>();
                Ok(json!({
                    "action": agent_action_schema_with_custom_actions(catalog),
                    "custom_actions": custom_action_schema_map(catalog),
                    "observation": observation_schema_with_catalog(observations),
                    "step_response": step_response_schema_with_catalog(observations),
                    "reset_response": reset_response_schema_with_catalog(observations),
                    "step_many_response": step_many_response_schema_with_catalog(observations),
                    "visual_capture": visual_capture_schema(),
                }))
            }
            "agent.reset" => {
                let params: ResetParams =
                    serde_json::from_value(request.params.clone()).map_err(into_invalid_params)?;
                self.check_token(params.session_token.as_deref())
                    .map_err(into_auth)?;
                self.require_capability(AgentCapability::STEP)
                    .map_err(into_auth)?;
                self.authorize_observation_mode(&params.options.observation_mode)
                    .map_err(into_auth)?;
                let options = params.options.clone();
                env.reset_with_response(options)
                    .map_err(into_internal)
                    .and_then(|v| serde_json::to_value(v).map_err(into_internal))
            }
            "agent.step" => {
                let params: StepParams =
                    serde_json::from_value(request.params.clone()).map_err(into_invalid_params)?;
                self.check_token(params.session_token.as_deref())
                    .map_err(into_auth)?;
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
                self.authorize_observation_mode(&requested)
                    .map_err(into_auth)?;
                ensure_initialized_with_mode(env, &requested).map_err(into_internal)?;
                step_with_request_mode(env, params.action, &requested).map_err(into_internal)
            }
            "agent.step_many" => {
                let params: StepManyParams =
                    serde_json::from_value(request.params.clone()).map_err(into_invalid_params)?;
                self.check_token(params.session_token.as_deref())
                    .map_err(into_auth)?;
                if params.actions.len() > MAX_ACTIONS_PER_REQUEST {
                    return Err(invalid_params(format!(
                        "actions length {} exceeds limit {MAX_ACTIONS_PER_REQUEST}",
                        params.actions.len()
                    )));
                }
                let active_mode = current_observation_mode(env);
                self.require_capability(AgentCapability::STEP)
                    .map_err(into_auth)?;
                self.authorize_observation_mode(&active_mode)
                    .map_err(into_auth)?;
                ensure_initialized_with_mode(env, &active_mode).map_err(into_internal)?;
                if !matches!(params.return_observations.as_str(), "none" | "last" | "all") {
                    return Err(invalid_params(
                        "return_observations must be one of: none, last, all",
                    ));
                }
                let mut response = env
                    .step_many_with_response(
                        params.actions,
                        params.stop_on_done,
                        params.return_observations == "all",
                    )
                    .map_err(into_internal)?;
                if params.return_observations == "none" {
                    response.observation = None;
                }
                serde_json::to_value(response).map_err(into_internal)
            }
            "agent.fast_forward" => {
                let params: FastForwardParams =
                    serde_json::from_value(request.params.clone()).map_err(into_invalid_params)?;
                self.check_token(params.session_token.as_deref())
                    .map_err(into_auth)?;
                if params.ticks > MAX_TICKS_PER_REQUEST {
                    return Err(invalid_params(format!(
                        "ticks {} exceeds limit {MAX_TICKS_PER_REQUEST}",
                        params.ticks
                    )));
                }
                let active_mode = current_observation_mode(env);
                self.require_capability(AgentCapability::STEP)
                    .map_err(into_auth)?;
                self.authorize_observation_mode(&active_mode)
                    .map_err(into_auth)?;
                ensure_initialized_with_mode(env, &active_mode).map_err(into_internal)?;
                env.fast_forward(params.ticks)
                    .map_err(into_internal)
                    .and_then(|v| serde_json::to_value(v).map_err(into_internal))
            }
            "agent.observe" => {
                let params: ObserveParams =
                    serde_json::from_value(request.params.clone()).map_err(into_invalid_params)?;
                self.check_token(params.session_token.as_deref())
                    .map_err(into_auth)?;
                // Request-local: authorize the SAME post-init mode that
                // `observe_with_request_mode` renders, without mutating the
                // global ObservationConfig.
                self.authorize_observation_mode(&params.observation_mode)
                    .map_err(into_auth)?;
                observe_with_request_mode(env, &params.observation_mode).map_err(into_internal)
            }
            "agent.visual.capture" => {
                let params: VisualCaptureParams =
                    serde_json::from_value(request.params.clone()).map_err(into_invalid_params)?;
                self.check_token(params.session_token.as_deref())
                    .map_err(into_auth)?;
                self.authorize_operation(&OperationRequires {
                    mutation: None,
                    observation_visibility: None,
                    snapshot_export: false,
                    restore_import: false,
                    filesystem: true,
                    control: None,
                })
                .map_err(into_auth)?;
                self.require_capability(AgentCapability::VISUAL_CAPTURE)
                    .map_err(into_auth)?;
                let mut options = VisualCaptureOptions::default();
                if let Some(output_dir) = params.output_dir {
                    options.output_dir = self
                        .security
                        .resolve_output_dir(&output_dir)
                        .map_err(into_internal)?;
                } else {
                    options.output_dir = self.security.artifact_root_resolved();
                }
                if params.label.is_some() {
                    options.label = params.label;
                }
                if let Some(timeout_frames) = params.timeout_frames {
                    if timeout_frames == 0 {
                        return Err(invalid_params("timeout_frames must be > 0"));
                    }
                    options.timeout_frames = timeout_frames;
                }
                if let Some(source) = params.source {
                    options.source = source;
                }
                env.capture_visual(options)
                    .map_err(into_internal)
                    .and_then(|result| {
                        verify_visual_capture_file(&result.path).map_err(into_internal)?;
                        serde_json::to_value(result).map_err(into_internal)
                    })
            }
            "agent.snapshot.create" => {
                self.check_token(token_from_params(&request.params).as_deref())
                    .map_err(into_auth)?;
                self.authorize_operation(&OperationRequires {
                    mutation: None,
                    observation_visibility: None,
                    snapshot_export: true,
                    restore_import: false,
                    filesystem: false,
                    control: None,
                })
                .map_err(into_auth)?;
                self.require_capability(AgentCapability::SNAPSHOT)
                    .map_err(into_auth)?;
                env.snapshot()
                    .map_err(into_internal)
                    .and_then(|v| serde_json::to_value(v).map_err(into_internal))
            }
            "agent.snapshot.restore" => {
                let params: SnapshotRestoreParams =
                    serde_json::from_value(request.params.clone()).map_err(into_invalid_params)?;
                self.check_token(params.session_token.as_deref())
                    .map_err(into_auth)?;
                self.authorize_operation(&OperationRequires {
                    mutation: None,
                    observation_visibility: None,
                    snapshot_export: false,
                    restore_import: true,
                    filesystem: false,
                    control: None,
                })
                .map_err(into_auth)?;
                self.require_capability(AgentCapability::RESTORE)
                    .map_err(into_auth)?;
                env.restore(params.snapshot_id).map_err(into_internal)?;
                Ok(Value::Null)
            }
            "agent.snapshot.list" => {
                self.check_token(token_from_params(&request.params).as_deref())
                    .map_err(into_auth)?;
                self.require_capability(AgentCapability::SNAPSHOT)
                    .map_err(into_auth)?;
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
                let params: SnapshotDeleteParams =
                    serde_json::from_value(request.params.clone()).map_err(into_invalid_params)?;
                self.check_token(params.session_token.as_deref())
                    .map_err(into_auth)?;
                self.require_capability(AgentCapability::SNAPSHOT)
                    .map_err(into_auth)?;
                // Checked delete: reject snapshots that are pinned in the
                // store or still referenced by the replay log / live timeline
                // fork snapshots instead of silently dropping them.
                let mut referenced: std::collections::BTreeSet<SnapshotId> = env
                    .replay_log()
                    .map(collect_replay_references)
                    .unwrap_or_default();
                if let Some(timeline) = env.world().get_resource::<Timeline>() {
                    referenced.extend(
                        timeline
                            .branches
                            .values()
                            .filter_map(|branch| branch.fork_snapshot),
                    );
                }
                delete_snapshot_checked(env.world_mut(), params.snapshot_id, &referenced)
                    .map_err(into_internal)?;
                Ok(Value::Null)
            }
            "agent.timeline.current" => {
                self.authorize(None, &request.params).map_err(into_auth)?;
                Ok(json!({
                    "tick": env.current_tick(),
                    "timeline_id": env.world().resource::<bevy_agent_core::AgentControlState>().timeline_id,
                    "branch_id": env.world().resource::<bevy_agent_core::AgentControlState>().branch_id,
                }))
            }
            "agent.timeline.branch" => {
                let params: BranchParams =
                    serde_json::from_value(request.params.clone()).map_err(into_invalid_params)?;
                self.check_token(params.session_token.as_deref())
                    .map_err(into_auth)?;
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
            "agent.timeline.restore_tick" => {
                let params: RestoreTickParams =
                    serde_json::from_value(request.params.clone()).map_err(into_invalid_params)?;
                self.check_token(params.session_token.as_deref())
                    .map_err(into_auth)?;
                self.authorize_operation(&OperationRequires {
                    mutation: None,
                    observation_visibility: None,
                    snapshot_export: false,
                    restore_import: true,
                    filesystem: false,
                    control: None,
                })
                .map_err(into_auth)?;
                self.require_capability(AgentCapability::RESTORE)
                    .map_err(into_auth)?;
                validate_history_target(env, params.tick, "restore_tick")
                    .map_err(|e| invalid_params(e.to_string()))?;
                env.restore_tick(params.tick).map_err(into_internal)?;
                Ok(json!({ "current_tick": env.current_tick() }))
            }
            "agent.control.set_mode" => {
                let params: ControlModeParams =
                    serde_json::from_value(request.params.clone()).map_err(into_invalid_params)?;
                self.check_token(params.session_token.as_deref())
                    .map_err(into_auth)?;
                self.authorize_operation(&OperationRequires {
                    mutation: None,
                    observation_visibility: None,
                    snapshot_export: false,
                    restore_import: false,
                    filesystem: false,
                    control: Some(AgentCapability::CONTROL),
                })
                .map_err(into_auth)?;
                env.world_mut()
                    .resource_mut::<bevy_agent_core::AgentControlState>()
                    .mode = params.mode;
                Ok(Value::Null)
            }
            "agent.control.pause" => {
                self.check_token(token_from_params(&request.params).as_deref())
                    .map_err(into_auth)?;
                self.authorize_operation(&OperationRequires {
                    mutation: None,
                    observation_visibility: None,
                    snapshot_export: false,
                    restore_import: false,
                    filesystem: false,
                    control: Some(AgentCapability::CONTROL),
                })
                .map_err(into_auth)?;
                env.world_mut()
                    .resource_mut::<bevy_agent_core::AgentControlState>()
                    .mode = ControlMode::Paused;
                Ok(Value::Null)
            }
            "agent.control.resume" => {
                self.check_token(token_from_params(&request.params).as_deref())
                    .map_err(into_auth)?;
                self.authorize_operation(&OperationRequires {
                    mutation: None,
                    observation_visibility: None,
                    snapshot_export: false,
                    restore_import: false,
                    filesystem: false,
                    control: Some(AgentCapability::CONTROL),
                })
                .map_err(into_auth)?;
                env.world_mut()
                    .resource_mut::<bevy_agent_core::AgentControlState>()
                    .mode = ControlMode::Agent;
                Ok(Value::Null)
            }
            "agent.replay.start" => {
                self.check_token(token_from_params(&request.params).as_deref())
                    .map_err(into_auth)?;
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
            "agent.replay.stop" => {
                self.check_token(token_from_params(&request.params).as_deref())
                    .map_err(into_auth)?;
                self.require_capability(AgentCapability::STEP)
                    .map_err(into_auth)?;
                let log = stop_recording(env.world_mut());
                Ok(json!({
                    "recording": false,
                    "records": log.records.len(),
                    "log": log,
                }))
            }
            "agent.replay.export" => {
                let params: ReplayExportParams =
                    serde_json::from_value(request.params.clone()).map_err(into_invalid_params)?;
                self.check_token(params.session_token.as_deref())
                    .map_err(into_auth)?;
                self.authorize_operation(&OperationRequires {
                    mutation: None,
                    observation_visibility: None,
                    snapshot_export: true,
                    restore_import: false,
                    filesystem: params.path.is_some(),
                    control: None,
                })
                .map_err(into_auth)?;
                // Export is gated by SNAPSHOT_EXPORT (falling back to SNAPSHOT
                // for backwards compatibility when only SNAPSHOT is granted).
                if !(self
                    .security
                    .capabilities
                    .contains(AgentCapability::SNAPSHOT_EXPORT)
                    || self
                        .security
                        .capabilities
                        .contains(AgentCapability::SNAPSHOT))
                {
                    return Err(auth_error("missing remote capability SNAPSHOT_EXPORT"));
                }
                let bundle = env.export_replay_bundle().map_err(into_internal)?;
                let resolved_path = if let Some(path) = &params.path {
                    let resolved = self
                        .security
                        .resolve_artifact_path(path)
                        .map_err(into_internal)?;
                    let encoded = serde_json::to_string_pretty(&bundle).map_err(into_internal)?;
                    if let Some(parent) = resolved.parent() {
                        std::fs::create_dir_all(parent).map_err(into_internal)?;
                    }
                    write_bundle_exclusive(&resolved, encoded.as_bytes()).map_err(into_internal)?;
                    Some(resolved.to_string_lossy().to_string())
                } else {
                    None
                };
                // Without a path, return base64 bytes inline so callers can
                // fetch the bundle without filesystem access.
                let bundle_bytes = serde_json::to_vec(&bundle).map_err(into_internal)?;
                let bundle_base64 = BASE64_STANDARD.encode(&bundle_bytes);
                Ok(json!({
                    "path": resolved_path,
                    "records": bundle.log.records.len(),
                    "checkpoints": bundle.log.checkpoints.len(),
                    "bundle": bundle,
                    "bundle_base64": bundle_base64,
                }))
            }
            "agent.replay.load" => {
                let params: ReplayLoadParams =
                    serde_json::from_value(request.params.clone()).map_err(into_invalid_params)?;
                self.check_token(params.session_token.as_deref())
                    .map_err(into_auth)?;
                self.authorize_operation(&OperationRequires {
                    mutation: None,
                    observation_visibility: None,
                    snapshot_export: false,
                    restore_import: true,
                    filesystem: params.path.is_some(),
                    control: None,
                })
                .map_err(into_auth)?;
                self.require_capability(AgentCapability::RESTORE)
                    .map_err(into_auth)?;
                let bundle = if let Some(bundle) = params.bundle {
                    bundle
                } else if let Some(log) = params.log {
                    replay_bundle_from_legacy_log(env, log).map_err(into_internal)?
                } else if let Some(path) = params.path {
                    let resolved = self
                        .security
                        .resolve_artifact_path(&path)
                        .map_err(into_internal)?;
                    let bytes = std::fs::read_to_string(&resolved)
                        .with_context(|| {
                            format!("reading replay bundle from {}", resolved.display())
                        })
                        .map_err(into_internal)?;
                    match serde_json::from_str::<ReplayBundle>(&bytes) {
                        Ok(bundle) => bundle,
                        Err(bundle_error) => {
                            let log = serde_json::from_str::<ReplayLog>(&bytes).with_context(|| {
                                format!(
                                    "decoding replay bundle from {} ({bundle_error}); legacy log decode also failed",
                                    resolved.display()
                                )
                            }).map_err(into_internal)?;
                            replay_bundle_from_legacy_log(env, log).map_err(into_internal)?
                        }
                    }
                } else {
                    return Err(invalid_params(
                        "agent.replay.load requires one of: path, bundle, log",
                    ));
                };
                let records = bundle.log.records.len();
                let checkpoints = bundle.log.checkpoints.len();
                // Validate the branch graph BEFORE installing anything: the
                // runner install path writes snapshots/store state, so a
                // malformed topology must be rejected up front.
                validate_replay_bundle_topology(&bundle)
                    .map_err(|error| invalid_params(error.to_string()))?;
                // Self-containment pre-check: every snapshot referenced by the
                // log must be present in the bundle payload. Rejected here
                // with -32602 (invalid params) before delegating to the
                // runner transactional load.
                {
                    let referenced = collect_replay_references(&bundle.log);
                    let provided = bundle
                        .snapshots
                        .iter()
                        .map(|snapshot| snapshot.manifest.snapshot_id)
                        .collect::<std::collections::BTreeSet<_>>();
                    if let Some(missing) = referenced.difference(&provided).next() {
                        return Err(invalid_params(format!(
                            "replay bundle is missing referenced snapshot {missing:?}"
                        )));
                    }
                }
                env.load_replay_bundle(bundle).map_err(into_internal)?;
                Ok(json!({
                    "records": records,
                    "checkpoints": checkpoints,
                }))
            }
            other => Err(method_not_found(format!("unknown method {other}"))),
        }
    }

    pub(crate) fn check_token(&self, token: Option<&str>) -> Result<()> {
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

    /// Central authorization entry point: checks mutation, observation
    /// visibility, snapshot export/import, filesystem, and control gates.
    fn authorize_operation(&self, requires: &OperationRequires) -> Result<()> {
        if let Some(cap) = requires.mutation {
            self.require_capability(cap)?;
        }
        if let Some(cap) = requires.observation_visibility {
            self.require_capability(cap)?;
        }
        if requires.snapshot_export {
            // Caller still needs an explicit export check; the replay.export
            // arm additionally accepts legacy SNAPSHOT. Other exporters
            // require SNAPSHOT_EXPORT/SNAPSHOT via this gate.
            if !(self
                .security
                .capabilities
                .contains(AgentCapability::SNAPSHOT_EXPORT)
                || self
                    .security
                    .capabilities
                    .contains(AgentCapability::SNAPSHOT))
            {
                return Err(anyhow!("missing remote capability SNAPSHOT_EXPORT"));
            }
        }
        if requires.restore_import {
            self.require_capability(AgentCapability::RESTORE)?;
        }
        if requires.filesystem {
            self.require_capability(AgentCapability::FILESYSTEM)?;
        }
        if let Some(cap) = requires.control {
            self.require_capability(cap)?;
        }
        Ok(())
    }

    /// Authorize the observation-visibility gate for a single requested mode.
    /// All response-returning methods (reset/step/step_many/fast_forward/
    /// observe) must authorize through this helper with the SAME post-init
    /// mode that produces the response, so a first-step implicit reset can
    /// never serve a mode the caller was not authorized for.
    fn authorize_observation_mode(&self, mode: &ObservationMode) -> Result<()> {
        self.authorize_operation(&OperationRequires {
            mutation: None,
            observation_visibility: Some(capability_for_observation_mode(mode)),
            snapshot_export: false,
            restore_import: false,
            filesystem: false,
            control: None,
        })
    }
}

/// Read-only view of the current global observation mode (no mutation).
fn current_observation_mode(env: &AgentApp) -> ObservationMode {
    env.world()
        .get_resource::<ObservationConfig>()
        .map(|c| c.mode.clone())
        .unwrap_or_default()
}

/// Ensure the app is initialized, pinning the post-init mode to `mode` when a
/// first-step implicit reset is required. Must run BEFORE authorization-mode
/// selection is finalized by callers: after this returns, the mode that will
/// produce the response is `mode`, so authorization must use `mode`.
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

/// Step request-locally: advance the simulation, then re-render the
/// observation in `mode` via [`collect_observation_with_mode`] without
/// mutating the global [`ObservationConfig`]. Returns the patched response.
fn step_with_request_mode(
    env: &mut AgentApp,
    action: AgentAction,
    mode: &ObservationMode,
) -> Result<Value> {
    let mut response = env.step(action)?;
    collect_observation_with_mode(env.world_mut(), mode.clone());
    let refreshed = env
        .world()
        .get_resource::<LastStepResponse>()
        .and_then(|last| last.0.clone());
    if let Some(mut patched) = refreshed {
        // Preserve step bookkeeping (reward/done/info/checksum/tick) from the
        // just-executed step; only the observation rendering is request-local.
        // `collect_observation_with_mode` rebuilds from identical world state,
        // so these already match, but copy explicitly to guard against
        // checkpoint-patching drift (e.g. terminal snapshots).
        patched.tick = response.tick;
        patched.reward = response.reward;
        patched.done = response.done;
        patched.truncated = response.truncated;
        patched.info = response.info.clone();
        patched.checksum = response.checksum.clone();
        response.observation = patched.observation;
    }
    serde_json::to_value(response).map_err(anyhow::Error::from)
}

/// Observe request-locally without touching the global [`ObservationConfig`].
fn observe_with_request_mode(env: &mut AgentApp, mode: &ObservationMode) -> Result<Value> {
    ensure_initialized_with_mode(env, mode)?;
    collect_observation_with_mode(env.world_mut(), mode.clone());
    let observation = env
        .world()
        .get_resource::<LastStepResponse>()
        .and_then(|last| last.0.clone().map(|r| r.observation))
        .ok_or_else(|| anyhow!("observation produced no response"))?;
    serde_json::to_value(observation).map_err(anyhow::Error::from)
}

/// Validate a restore/branch target tick: it must lie within recorded bounds
/// (at or before the current tick) and the replay interval back to it must
/// fit [`MAX_TICKS_PER_REQUEST`]. `fast_forward` enforces the same tick
/// budget on the forward interval.
/// Maximum branch-graph size accepted on replay import.
pub const MAX_IMPORT_BRANCHES: usize = 10_000;
/// Maximum parent-chain depth followed while validating an imported branch
/// graph. Bounds every lineage walk so a malicious bundle cannot force
/// unbounded traversal.
pub const MAX_IMPORT_LINEAGE_DEPTH: usize = 1024;

fn validate_history_target(env: &AgentApp, target_tick: u64, what: &str) -> Result<()> {
    let current = env.current_tick();
    if target_tick > current {
        return Err(anyhow!(
            "{what} target tick {target_tick} is beyond recorded bounds (current tick {current})"
        ));
    }
    if current.saturating_sub(target_tick) > MAX_TICKS_PER_REQUEST {
        return Err(anyhow!(
            "{what} interval {} exceeds limit {MAX_TICKS_PER_REQUEST}",
            current - target_tick
        ));
    }
    Ok(())
}

fn auth_error(message: impl Into<String>) -> JsonRpcError {
    JsonRpcError {
        code: RPC_AUTH_ERROR,
        message: message.into(),
    }
}

fn invalid_params(message: impl Into<String>) -> JsonRpcError {
    JsonRpcError {
        code: RPC_INVALID_PARAMS,
        message: message.into(),
    }
}

fn method_not_found(message: impl Into<String>) -> JsonRpcError {
    JsonRpcError {
        code: RPC_METHOD_NOT_FOUND,
        message: message.into(),
    }
}

fn into_internal(error: impl std::fmt::Display) -> JsonRpcError {
    JsonRpcError {
        code: RPC_INTERNAL_ERROR,
        message: error.to_string(),
    }
}

fn into_invalid_params(error: serde_json::Error) -> JsonRpcError {
    invalid_params(format!("invalid params: {error}"))
}

fn into_auth(error: anyhow::Error) -> JsonRpcError {
    auth_error(error.to_string())
}

/// Verify a visual capture file exists and starts with the PNG magic bytes.
fn verify_visual_capture_file(path: &std::path::Path) -> Result<()> {
    let bytes =
        std::fs::read(path).with_context(|| format!("reading capture {}", path.display()))?;
    if bytes.len() < PNG_MAGIC.len() || bytes[..PNG_MAGIC.len()] != PNG_MAGIC {
        return Err(anyhow!("capture {} is missing PNG magic", path.display()));
    }
    Ok(())
}

/// Convert frame-count timeouts to a wall-clock deadline (60 fps assumption).
#[must_use]
pub fn timeout_frames_to_duration(timeout_frames: u32) -> Duration {
    Duration::from_secs_f64(f64::from(timeout_frames) / 60.0)
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

/// Validate an imported replay bundle's branch graph BEFORE installing
/// anything into the store/timeline. Rejects oversize graphs, duplicate ids,
/// self-parenting, unknown parents, missing/multiple roots, and parent-chain
/// cycles. Every lineage walk is bounded: each chain carries a visited set
/// (cycle detection) and a [`MAX_IMPORT_LINEAGE_DEPTH`] step cap.
fn validate_replay_bundle_topology(bundle: &ReplayBundle) -> Result<()> {
    use std::collections::{HashMap, HashSet};

    let topology = &bundle.log.timeline_topology;
    // Legacy logs carry no topology; nothing graph-shaped to validate.
    if topology.is_empty() {
        return Ok(());
    }
    if topology.len() > MAX_IMPORT_BRANCHES {
        return Err(anyhow!(
            "replay bundle branch graph of {} exceeds limit of {MAX_IMPORT_BRANCHES}",
            topology.len()
        ));
    }
    let mut parents: HashMap<_, _> = HashMap::with_capacity(topology.len());
    for branch in topology {
        if parents
            .insert(branch.branch_id, branch.parent_branch)
            .is_some()
        {
            return Err(anyhow!(
                "replay bundle has duplicate branch id {:?}",
                branch.branch_id
            ));
        }
        if branch.parent_branch == Some(branch.branch_id) {
            return Err(anyhow!(
                "replay bundle branch {:?} is its own parent",
                branch.branch_id
            ));
        }
    }
    for branch in topology {
        if let Some(parent) = branch.parent_branch
            && !parents.contains_key(&parent)
        {
            return Err(anyhow!(
                "replay bundle branch {:?} has unknown parent {:?}",
                branch.branch_id,
                parent
            ));
        }
    }
    let roots = topology
        .iter()
        .filter(|branch| branch.parent_branch.is_none())
        .count();
    if roots == 0 {
        return Err(anyhow!("replay bundle branch graph has no root"));
    }
    if roots > 1 {
        return Err(anyhow!(
            "replay bundle branch graph has {roots} roots; exactly one is required"
        ));
    }
    // Bounded lineage walk per branch: follow parent links with a per-chain
    // visited set (cycle) and a depth cap (degenerate depth / hidden cycle).
    for branch in topology {
        let mut visited: HashSet<_> = HashSet::new();
        visited.insert(branch.branch_id);
        let mut current = branch.parent_branch;
        let mut depth = 0usize;
        while let Some(id) = current {
            depth += 1;
            if depth > MAX_IMPORT_LINEAGE_DEPTH {
                return Err(anyhow!(
                    "replay bundle branch {:?} lineage exceeds max depth of {MAX_IMPORT_LINEAGE_DEPTH}",
                    branch.branch_id
                ));
            }
            if !visited.insert(id) {
                return Err(anyhow!(
                    "replay bundle branch graph contains a parent-chain cycle at {id:?}"
                ));
            }
            // `None` parents and unknown ids are handled above; unknown here
            // is unreachable, so stop the walk defensively.
            current = parents.get(&id).copied().flatten();
        }
    }
    Ok(())
}

fn replay_bundle_from_legacy_log(env: &AgentApp, log: ReplayLog) -> Result<ReplayBundle> {
    let store = env
        .world()
        .get_resource::<SnapshotStore>()
        .ok_or_else(|| anyhow!("AgentSnapshotPlugin is not installed"))?;
    // Single collector for the payload reference set (initial + legacy
    // checkpoints + branch-tagged checkpoints + topology fork snapshots).
    let referenced = collect_replay_references(&log);
    let snapshots = referenced
        .into_iter()
        .filter_map(|id| store.snapshots.get(&id).cloned())
        .collect();
    Ok(ReplayBundle {
        format_version: ReplayBundle::FORMAT_VERSION,
        log,
        snapshots,
    })
}

#[must_use]
pub fn agent_action_schema() -> Value {
    agent_action_schema_with_custom_actions(None)
}

#[must_use]
pub fn agent_action_schema_with_custom_actions(catalog: Option<&AgentActionCatalog>) -> Value {
    let custom_value_schema = custom_action_value_schema(catalog);
    let mut schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://bevy-agent.rs/schemas/action.json",
        "title": "AgentAction",
        "oneOf": [
            { "type": "object", "required": ["type"], "properties": { "type": { "const": "Noop" } }, "additionalProperties": false },
            { "type": "object", "required": ["type", "x", "y"], "properties": { "type": { "const": "Move" }, "x": { "type": "number", "minimum": -1.0, "maximum": 1.0 }, "y": { "type": "number", "minimum": -1.0, "maximum": 1.0 } }, "additionalProperties": false },
            {
                "type": "object",
                "required": ["type", "yaw_delta", "pitch_delta"],
                "properties": {
                    "type": { "const": "Look" },
                    "yaw_delta": {
                        "type": "number",
                        "minimum": -LOOK_YAW_DELTA_LIMIT_RADIANS,
                        "maximum": LOOK_YAW_DELTA_LIMIT_RADIANS,
                        "description": "Per-tick yaw delta in radians. Bounded to ±π because larger turns are directionally redundant."
                    },
                    "pitch_delta": {
                        "type": "number",
                        "minimum": -LOOK_PITCH_DELTA_LIMIT_RADIANS,
                        "maximum": LOOK_PITCH_DELTA_LIMIT_RADIANS,
                        "description": "Per-tick pitch delta in radians. ±π/2 spans the full practical camera pitch range in one tick."
                    }
                },
                "additionalProperties": false
            },
            { "type": "object", "required": ["type"], "properties": { "type": { "const": "Jump" } }, "additionalProperties": false },
            { "type": "object", "required": ["type"], "properties": { "type": { "const": "Crouch" } }, "additionalProperties": false },
            { "type": "object", "required": ["type"], "properties": { "type": { "const": "Sprint" } }, "additionalProperties": false },
            { "type": "object", "required": ["type"], "properties": { "type": { "const": "Interact" } }, "additionalProperties": false },
            { "type": "object", "required": ["type"], "properties": { "type": { "const": "Attack" }, "target": { "anyOf": [{ "type": "integer", "minimum": 0 }, { "type": "null" }] } }, "additionalProperties": false },
            { "type": "object", "required": ["type", "slot"], "properties": { "type": { "const": "UseItem" }, "slot": { "type": "integer", "minimum": 0, "maximum": 255 } }, "additionalProperties": false },
            { "type": "object", "required": ["type"], "properties": { "type": { "const": "Dodge" } }, "additionalProperties": false },
            { "type": "object", "required": ["type", "value"], "properties": { "type": { "const": "Custom" }, "value": custom_value_schema }, "additionalProperties": false }
        ]
    });
    if let Some(catalog) = catalog
        && catalog.supported_actions.is_some()
        && let Some(variants) = schema.get_mut("oneOf").and_then(Value::as_array_mut)
    {
        variants.retain(|variant| {
            variant
                .pointer("/properties/type/const")
                .and_then(Value::as_str)
                .and_then(action_kind_from_wire_name)
                .is_some_and(|kind| catalog.supports(kind))
        });
    }
    schema
}

#[must_use]
pub fn supported_action_names(catalog: Option<&AgentActionCatalog>) -> Vec<&'static str> {
    const ACTIONS: [(&str, AgentActionKind); 11] = [
        ("Noop", AgentActionKind::Noop),
        ("Move", AgentActionKind::Move),
        ("Look", AgentActionKind::Look),
        ("Jump", AgentActionKind::Jump),
        ("Crouch", AgentActionKind::Crouch),
        ("Sprint", AgentActionKind::Sprint),
        ("Interact", AgentActionKind::Interact),
        ("Attack", AgentActionKind::Attack),
        ("UseItem", AgentActionKind::UseItem),
        ("Dodge", AgentActionKind::Dodge),
        ("Custom", AgentActionKind::Custom),
    ];
    ACTIONS
        .iter()
        .filter(|(_, kind)| catalog.is_none_or(|catalog| catalog.supports(*kind)))
        .map(|(name, _)| *name)
        .collect()
}

fn action_kind_from_wire_name(name: &str) -> Option<AgentActionKind> {
    Some(match name {
        "Noop" => AgentActionKind::Noop,
        "Move" => AgentActionKind::Move,
        "Look" => AgentActionKind::Look,
        "Jump" => AgentActionKind::Jump,
        "Crouch" => AgentActionKind::Crouch,
        "Sprint" => AgentActionKind::Sprint,
        "Interact" => AgentActionKind::Interact,
        "Attack" => AgentActionKind::Attack,
        "UseItem" => AgentActionKind::UseItem,
        "Dodge" => AgentActionKind::Dodge,
        "Custom" => AgentActionKind::Custom,
        _ => return None,
    })
}

#[must_use]
pub fn custom_action_schema_map(catalog: Option<&AgentActionCatalog>) -> Value {
    let mut map = serde_json::Map::new();
    if let Some(catalog) = catalog
        && catalog.supports(AgentActionKind::Custom)
    {
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
    observation_schema_with_catalog(None)
}

#[must_use]
pub fn observation_schema_with_catalog(catalog: Option<&AgentObservationCatalog>) -> Value {
    let domain_value_schema = catalog
        .and_then(|catalog| catalog.schema.clone())
        .unwrap_or(Value::Bool(true));
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://bevy-agent.rs/schemas/observation.json",
        "title": "Observation",
        "oneOf": [
            { "$ref": "#/$defs/symbolicObservationEnvelope" },
            { "$ref": "#/$defs/hybridObservation" },
            { "$ref": "#/$defs/fullStateObservation" },
            { "$ref": "#/$defs/deltaObservation" },
            { "$ref": "#/$defs/pixelObservation" },
            {
                "type": "object",
                "required": ["kind", "tick", "value"],
                "properties": {
                    "kind": { "const": "Domain" },
                    "tick": { "type": "integer", "minimum": 0 },
                    "value": domain_value_schema
                },
                "additionalProperties": false
            },
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
    step_response_schema_with_catalog(None)
}

/// Catalog-aware step response: domain constraints from `catalog` propagate
/// into the embedded observation schema, which carries its own `$id` + `$defs`
/// so `#/$defs/...` refs resolve in the nested observation scope.
#[must_use]
pub fn step_response_schema_with_catalog(catalog: Option<&AgentObservationCatalog>) -> Value {
    let obs = observation_schema_with_catalog(catalog);
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://bevy-agent.rs/schemas/step-response.json",
        "title": "StepResponse",
        "type": "object",
        "required": ["tick", "observation", "reward", "done", "truncated", "info", "checksum"],
        "properties": {
            "tick": { "type": "integer", "minimum": 0 },
            "observation": { "$ref": "#/$defs/observation" },
            "reward": { "type": "number" },
            "done": { "type": "boolean" },
            "truncated": { "type": "boolean" },
            "info": { "type": "object" },
            "checksum": { "anyOf": [{ "type": "object" }, { "type": "null" }] }
        },
        "$defs": {
            "observation": obs
        }
    })
}

#[must_use]
pub fn reset_response_schema() -> Value {
    reset_response_schema_with_catalog(None)
}

#[must_use]
pub fn reset_response_schema_with_catalog(catalog: Option<&AgentObservationCatalog>) -> Value {
    let obs = observation_schema_with_catalog(catalog);
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://bevy-agent.rs/schemas/reset-response.json",
        "title": "ResetResponse",
        "type": "object",
        "required": ["tick", "observation", "checksum", "snapshot_id", "timeline_id", "branch_id"],
        "properties": {
            "tick": { "type": "integer", "minimum": 0 },
            "observation": { "$ref": "#/$defs/observation" },
            "checksum": { "anyOf": [{ "type": "object" }, { "type": "null" }] },
            "snapshot_id": { "anyOf": [{ "type": "string", "format": "uuid" }, { "type": "null" }] },
            "timeline_id": { "type": "string", "format": "uuid" },
            "branch_id": { "type": "string", "format": "uuid" }
        },
        "$defs": {
            "observation": obs
        }
    })
}

#[must_use]
pub fn step_many_response_schema() -> Value {
    step_many_response_schema_with_catalog(None)
}

#[must_use]
pub fn step_many_response_schema_with_catalog(catalog: Option<&AgentObservationCatalog>) -> Value {
    let obs = observation_schema_with_catalog(catalog);
    let step = step_response_schema_with_catalog(catalog);
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://bevy-agent.rs/schemas/step-many-response.json",
        "title": "StepManyResponse",
        "type": "object",
        "required": [
            "start_tick", "end_tick", "steps", "observation", "reward",
            "done", "truncated", "info", "checksum", "responses"
        ],
        "properties": {
            "start_tick": { "type": "integer", "minimum": 0 },
            "end_tick": { "type": "integer", "minimum": 0 },
            "steps": { "type": "integer", "minimum": 0 },
            "observation": { "anyOf": [{ "$ref": "#/$defs/observation" }, { "type": "null" }] },
            "reward": { "type": "number" },
            "done": { "type": "boolean" },
            "truncated": { "type": "boolean" },
            "info": { "anyOf": [{ "type": "object" }, { "type": "null" }] },
            "checksum": { "anyOf": [{ "type": "object" }, { "type": "null" }] },
            "responses": { "type": "array", "items": step }
        },
        "$defs": {
            "observation": obs
        }
    })
}

#[must_use]
pub fn visual_capture_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://bevy-agent.rs/schemas/visual-capture.json",
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

/// Single-owner simulation over serial connections.
///
/// Only one `AgentApp` owner executes simulation work. Concurrent TCP
/// connections queue on the listener and are served sequentially (no async
/// executor; a full async refactor is out of scope). Every connection is
/// bounded: HTTP reads/writes carry [`HTTP_READ_TIMEOUT`] /
/// [`HTTP_WRITE_TIMEOUT`], each request carries an overall
/// [`HTTP_REQUEST_DEADLINE`], and per-request action/tick budgets
/// ([`MAX_ACTIONS_PER_REQUEST`] / [`MAX_TICKS_PER_REQUEST`], including
/// `restore_tick`/`branch` history intervals) cap main-thread work so one
/// connection cannot starve the pump.
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
            let mut stream = stream?;
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
        Ok(())
    }

    fn handle_connection(&self, env: &mut AgentApp, stream: &mut TcpStream) -> Result<()> {
        let deadline = Instant::now() + HTTP_REQUEST_DEADLINE;
        let request = read_http_request(stream)?;
        if request.method == "OPTIONS" {
            return write_preflight_response(stream, &request, &self.bridge.security);
        }
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
            validate_http_rpc(&request, &self.bridge.security)?;
            let response = self.bridge.handle_json(env, &request.body);
            check_deadline(deadline)?;
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

/// Atomic lifecycle for a main-thread remote request.
///
/// `Queued(0) -> Running(1) -> Done(2)`, with `Cancelled(3)` reachable only
/// from `Queued`. The pump claims `Queued -> Running` via `compare_exchange`;
/// the HTTP side cancels `Queued -> Cancelled` on timeout. A timeout racing a
/// claimed (`Running`) request reports `Unknown` execution with the operation
/// id instead of `Cancelled`, because the mutation may still execute.
pub const REQ_QUEUED: u32 = 0;
pub const REQ_RUNNING: u32 = 1;
pub const REQ_DONE: u32 = 2;
pub const REQ_CANCELLED: u32 = 3;

struct MainThreadRemoteRequest {
    op_id: u64,
    body: String,
    reply: mpsc::Sender<String>,
    state: Arc<AtomicU32>,
}

#[derive(Resource)]
struct MainThreadRemoteQueue {
    receiver: Mutex<mpsc::Receiver<MainThreadRemoteRequest>>,
}

#[derive(Resource)]
struct MainThreadRemoteState {
    bridge: JsonRpcBridge,
    reset_once: bool,
}

/// Runs remote control inside a normal Bevy app.
///
/// Networking happens on a background thread, while every JSON-RPC operation
/// that touches Bevy state is pumped from `Update` on Bevy's main thread. This
/// keeps the winit/render runner alive and makes real primary-window captures
/// work in remote visual sessions.
///
/// Single-owner + serial semantics (same as [`HttpRemoteServer`]): one
/// simulation owner, connections served sequentially with per-connection
/// deadlines ([`HTTP_REQUEST_DEADLINE`]) and tick budgets. No async refactor.
pub struct BevyRemoteControlPlugin {
    listener: Arc<TcpListener>,
    bridge: JsonRpcBridge,
}

impl BevyRemoteControlPlugin {
    pub fn bind(bind_addr: impl Into<String>, bridge: JsonRpcBridge) -> Result<Self> {
        let bind_addr = bind_addr.into();
        let listener =
            TcpListener::bind(&bind_addr).with_context(|| format!("binding {bind_addr}"))?;
        let local_addr = listener
            .local_addr()
            .with_context(|| format!("reading bound address for {bind_addr}"))?;
        require_safe_bind(local_addr, &bridge.security)?;
        Ok(Self {
            listener: Arc::new(listener),
            bridge,
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.listener.local_addr()?)
    }
}

impl Plugin for BevyRemoteControlPlugin {
    fn build(&self, app: &mut App) {
        let (sender, receiver) = mpsc::channel();
        let listener = Arc::clone(&self.listener);
        let security = self.bridge.security.clone();
        let local_addr = listener
            .local_addr()
            .map(|address| address.to_string())
            .unwrap_or_else(|_| "<unknown>".to_string());
        std::thread::Builder::new()
            .name("bevy-agent-remote".to_string())
            .spawn(move || {
                eprintln!("bevy_agent_remote listening on http://{local_addr}/rpc");
                if let Err(error) = serve_main_thread_remote(listener, sender, security) {
                    eprintln!("bevy_agent_remote server stopped: {error:#}");
                }
            })
            .expect("failed to spawn bevy-agent remote server thread");

        app.insert_resource(MainThreadRemoteQueue {
            receiver: Mutex::new(receiver),
        })
        .insert_resource(MainThreadRemoteState {
            bridge: self.bridge.clone(),
            reset_once: false,
        })
        .add_systems(Update, pump_main_thread_remote);
    }
}

fn serve_main_thread_remote(
    listener: Arc<TcpListener>,
    sender: mpsc::Sender<MainThreadRemoteRequest>,
    security: RemoteSecurity,
) -> Result<()> {
    for stream in listener.incoming() {
        let mut stream = stream?;
        if let Err(error) = handle_main_thread_connection(&sender, &security, &mut stream) {
            let _ = write_http_response(
                &mut stream,
                500,
                "Internal Server Error",
                "application/json",
                &json!({ "error": error.to_string() }).to_string(),
            );
        }
    }
    Ok(())
}

fn handle_main_thread_connection(
    sender: &mpsc::Sender<MainThreadRemoteRequest>,
    security: &RemoteSecurity,
    stream: &mut TcpStream,
) -> Result<()> {
    // Bounded per-connection deadline: the whole connection (read + pump +
    // write) must fit inside HTTP_REQUEST_DEADLINE.
    let deadline = Instant::now() + HTTP_REQUEST_DEADLINE;
    let request = read_http_request(stream)?;
    check_deadline(deadline)?;
    if request.method == "OPTIONS" {
        return write_preflight_response(stream, &request, security);
    }
    if request.method == "GET" && request.path == "/health" {
        return write_http_response(
            stream,
            200,
            "OK",
            "application/json",
            &json!({ "ok": true }).to_string(),
        );
    }
    if request.method == "GET" && request.path == "/ws" {
        validate_websocket_handshake(&request, security)?;
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
                WebSocketMessage::Text(body) => {
                    let response = request_on_main_thread(sender, body)?;
                    write_websocket_text(stream, &response)?;
                }
                WebSocketMessage::Ping(payload) => write_websocket_pong(stream, &payload)?,
                WebSocketMessage::Close => return Ok(()),
            }
        }
    }
    if request.method == "POST" && request.path == "/rpc" {
        validate_http_rpc(&request, security)?;
        let response = request_on_main_thread(sender, request.body)?;
        check_deadline(deadline)?;
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

/// Explicit lifecycle status for a main-thread remote request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MainThreadRequestStatus {
    Completed,
    Rejected,
    Cancelled,
    Unknown,
}

static MAIN_THREAD_SEQ: AtomicU64 = AtomicU64::new(1);

fn request_on_main_thread(
    sender: &mpsc::Sender<MainThreadRemoteRequest>,
    body: String,
) -> Result<String> {
    let (_, result) = request_on_main_thread_with_status(sender, body)?;
    result
}

fn request_on_main_thread_with_status(
    sender: &mpsc::Sender<MainThreadRemoteRequest>,
    body: String,
) -> Result<(MainThreadRequestStatus, Result<String>)> {
    let (reply, receiver) = mpsc::channel();
    let op_id = MAIN_THREAD_SEQ.fetch_add(1, Ordering::Relaxed);
    let state = Arc::new(AtomicU32::new(REQ_QUEUED));
    sender
        .send(MainThreadRemoteRequest {
            op_id,
            body,
            reply,
            state: Arc::clone(&state),
        })
        .map_err(|_| anyhow!("Bevy main-thread remote pump has stopped"))?;
    match receiver.recv_timeout(HTTP_REQUEST_DEADLINE) {
        Ok(response) => Ok((MainThreadRequestStatus::Completed, Ok(response))),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // Try to cancel before execution. Success => the pump will skip
            // it (CancelledBeforeExecution). Failure => the pump already
            // claimed Running, so execution outcome is unknown: report
            // Unknown with the op id, NOT Cancelled.
            match state.compare_exchange(
                REQ_QUEUED,
                REQ_CANCELLED,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => Ok((
                    MainThreadRequestStatus::Cancelled,
                    Err(anyhow!(
                        "timed out waiting for Bevy main thread (op {op_id} cancelled before execution)"
                    )),
                )),
                Err(current) if current == REQ_RUNNING => Ok((
                    MainThreadRequestStatus::Unknown,
                    Err(anyhow!(
                        "timed out waiting for Bevy main thread; execution state unknown for operation {op_id}"
                    )),
                )),
                Err(current) if current == REQ_DONE => {
                    // Finished racing the timeout; try a final non-blocking
                    // receive before reporting.
                    match receiver.try_recv() {
                        Ok(response) => Ok((MainThreadRequestStatus::Completed, Ok(response))),
                        Err(_) => Ok((
                            MainThreadRequestStatus::Unknown,
                            Err(anyhow!(
                                "request finished racing timeout; execution state unknown for operation {op_id}"
                            )),
                        )),
                    }
                }
                Err(_) => Ok((
                    MainThreadRequestStatus::Cancelled,
                    Err(anyhow!(
                        "timed out waiting for Bevy main thread (op {op_id})"
                    )),
                )),
            }
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => Ok((
            MainThreadRequestStatus::Rejected,
            Err(anyhow!("Bevy main-thread remote pump has stopped")),
        )),
    }
}

fn pump_main_thread_remote(world: &mut World) {
    let requests = {
        let queue = world.resource::<MainThreadRemoteQueue>();
        let receiver = queue
            .receiver
            .lock()
            .expect("main-thread remote queue mutex poisoned");
        receiver.try_iter().take(64).collect::<Vec<_>>()
    };

    for request in requests {
        // Atomic claim Queued -> Running: if CAS fails the HTTP side already
        // cancelled (timeout), so skip execution (CancelledBeforeExecution)
        // and report instead of running the mutation late.
        if request
            .state
            .compare_exchange(REQ_QUEUED, REQ_RUNNING, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            let _ = request.reply.send(error_json_rpc_response_fallback(
                request.op_id,
                "cancelled before execution",
            ));
            continue;
        }
        #[cfg(feature = "visual")]
        if try_schedule_primary_window_capture(world, &request) {
            request.state.store(REQ_DONE, Ordering::SeqCst);
            continue;
        }
        // Re-check after the (possibly expensive) visual-capture scheduling
        // probe: a concurrent timeout-cancel can only have happened from
        // Queued, but the claim above already moved us to Running, so a
        // Cancelled here is impossible; keep the Running guard explicit.
        if request.state.load(Ordering::SeqCst) == REQ_CANCELLED {
            continue;
        }
        let response = dispatch_json_on_world(world, &request.body);
        request.state.store(REQ_DONE, Ordering::SeqCst);
        let _ = request.reply.send(response);
    }
}

fn dispatch_json_on_world(world: &mut World, body: &str) -> String {
    let (bridge, reset_once) = {
        let state = world.resource::<MainThreadRemoteState>();
        (state.bridge.clone(), state.reset_once)
    };
    let owned_world = std::mem::replace(world, World::new());
    let mut app = App::empty();
    *app.world_mut() = owned_world;
    let mut env = AgentApp::from_running_app(app, reset_once);
    let response = bridge.handle_json(&mut env, body);
    let reset_once = env.has_reset();
    let mut app = env.into_app();
    *world = std::mem::replace(app.world_mut(), World::new());
    world.resource_mut::<MainThreadRemoteState>().reset_once = reset_once;
    response
}

#[cfg(feature = "visual")]
fn try_schedule_primary_window_capture(
    world: &mut World,
    request: &MainThreadRemoteRequest,
) -> bool {
    use bevy::render::view::screenshot::{Screenshot, ScreenshotCaptured, save_to_disk};
    use bevy::window::{PrimaryWindow, Window};

    let Ok(rpc) = serde_json::from_str::<JsonRpcRequest>(&request.body) else {
        return false;
    };
    if rpc.method != "agent.visual.capture" {
        return false;
    }
    // Unify through the validated command path: strict version check first.
    if let Err(error) = validate_jsonrpc_version(&rpc) {
        let _ = request.reply.send(error_json_rpc_response(
            rpc.id,
            RPC_INVALID_REQUEST,
            error.to_string(),
        ));
        return true;
    }
    let params = match serde_json::from_value::<VisualCaptureParams>(rpc.params.clone()) {
        Ok(params) => params,
        Err(error) => {
            let _ = request.reply.send(error_json_rpc_response(
                rpc.id,
                RPC_INVALID_PARAMS,
                format!("invalid params: {error}"),
            ));
            return true;
        }
    };
    let source = params.source.unwrap_or_default();
    if source == CaptureSource::Software
        || (source == CaptureSource::Auto
            && world.contains_resource::<AgentVisualCaptureRenderer>())
    {
        return false;
    }

    let bridge = world.resource::<MainThreadRemoteState>().bridge.clone();
    let authorization = bridge
        .require_capability(AgentCapability::VISUAL_CAPTURE)
        .and_then(|()| bridge.require_capability(AgentCapability::FILESYSTEM))
        .and_then(|()| bridge.check_token(params.session_token.as_deref()));
    if let Err(error) = authorization {
        let _ = request.reply.send(error_json_rpc_response(
            rpc.id,
            RPC_AUTH_ERROR,
            error.to_string(),
        ));
        return true;
    }

    // Filesystem confinement for the validated command path.
    let output_dir = match params.output_dir.clone() {
        Some(dir) => match bridge.security.resolve_output_dir(&dir) {
            Ok(resolved) => resolved,
            Err(error) => {
                let _ = request.reply.send(error_json_rpc_response(
                    rpc.id,
                    RPC_INTERNAL_ERROR,
                    error.to_string(),
                ));
                return true;
            }
        },
        None => bridge.security.artifact_root_resolved(),
    };
    let timeout_frames = params
        .timeout_frames
        .unwrap_or_else(|| VisualCaptureOptions::default().timeout_frames);
    if timeout_frames == 0 {
        let _ = request.reply.send(error_json_rpc_response(
            rpc.id,
            RPC_INVALID_PARAMS,
            "timeout_frames must be > 0".to_string(),
        ));
        return true;
    }
    let deadline = timeout_frames_to_duration(timeout_frames);
    let started = Instant::now();

    if !world.resource::<MainThreadRemoteState>().reset_once {
        let reset_request = serde_json::to_string(&JsonRpcRequest {
            jsonrpc: Some("2.0".to_string()),
            id: Value::Null,
            method: "agent.reset".to_string(),
            params: json!({
                "options": ResetOptions::default(),
                "session_token": params.session_token,
            }),
        })
        .expect("reset request is serializable");
        let reset_response = dispatch_json_on_world(world, &reset_request);
        if serde_json::from_str::<Value>(&reset_response)
            .ok()
            .and_then(|value| value.get("error").cloned())
            .is_some()
        {
            let _ = request.reply.send(reset_response);
            return true;
        }
    }

    let (tick, frame, width, height) = {
        let tick = world.resource::<bevy_agent_core::SimClock>().tick;
        let frame = world.resource::<bevy_agent_core::AgentControlState>().frame;
        let mut windows = world.query_filtered::<&Window, With<PrimaryWindow>>();
        let Some(window) = windows.iter(world).next() else {
            let _ = request.reply.send(error_json_rpc_response(
                rpc.id,
                RPC_INTERNAL_ERROR,
                "visual capture requires a primary window".to_string(),
            ));
            return true;
        };
        (
            tick,
            frame,
            window.physical_width(),
            window.physical_height(),
        )
    };
    let options = VisualCaptureOptions {
        output_dir,
        label: params.label,
        timeout_frames,
        source,
    };
    let path = match visual_capture_path(&options, tick, frame) {
        Ok(path) => path,
        Err(error) => {
            let _ = request.reply.send(error_json_rpc_response(
                rpc.id,
                RPC_INTERNAL_ERROR,
                error.to_string(),
            ));
            return true;
        }
    };
    let reply = request.reply.clone();
    let req_state = Arc::clone(&request.state);
    let id = rpc.id;
    let result_path = path.clone();
    let mut save = save_to_disk(path.clone());
    // Watchdog: fail with a typed error if the frame deadline expires.
    {
        let reply = reply.clone();
        let id = id.clone();
        let result_path = result_path.clone();
        std::thread::spawn(move || {
            std::thread::sleep(deadline + Duration::from_millis(100));
            // If the capture file never materialized/validated, report expiry.
            if verify_visual_capture_file(&result_path).is_err()
                && started.elapsed() >= deadline
                && req_state.load(Ordering::SeqCst) != REQ_CANCELLED
            {
                let _ = reply.send(error_json_rpc_response(
                    id,
                    RPC_INTERNAL_ERROR,
                    format!("primary-window capture timed out after {timeout_frames} frames"),
                ));
            }
        });
    }
    world
        .spawn(Screenshot::primary_window())
        .observe(move |captured: On<ScreenshotCaptured>| {
            save(captured);
            // Check file exists + PNG magic after save; typed error on failure.
            if let Err(error) = verify_visual_capture_file(&result_path) {
                let _ = reply.send(error_json_rpc_response(
                    id.clone(),
                    RPC_INTERNAL_ERROR,
                    format!("capture failed verification: {error}"),
                ));
                return;
            }
            let result = VisualCaptureResult {
                tick,
                frame,
                path: result_path.clone(),
                width,
                height,
                format: "png".to_string(),
            };
            let response = JsonRpcResponse::Result {
                jsonrpc: "2.0",
                id: id.clone(),
                result: serde_json::to_value(result).expect("capture result is serializable"),
            };
            let _ = reply
                .send(serde_json::to_string(&response).expect("JSON-RPC response is serializable"));
        });
    true
}

fn error_json_rpc_response(id: Value, code: i32, message: String) -> String {
    serde_json::to_string(&JsonRpcResponse::Error {
        jsonrpc: "2.0",
        id,
        error: JsonRpcError { code, message },
    })
    .expect("JSON-RPC error response is serializable")
}

/// Fallback error payload for pump-side cancellation when no request id is
/// available (uses the operation id as the JSON-RPC id).
fn error_json_rpc_response_fallback(op_id: u64, message: &str) -> String {
    error_json_rpc_response(Value::from(op_id), RPC_INTERNAL_ERROR, message.to_string())
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

fn check_deadline(deadline: Instant) -> Result<()> {
    if Instant::now() > deadline {
        return Err(anyhow!("request deadline exceeded"));
    }
    Ok(())
}

/// Validate HTTP RPC preconditions: browser `Origin` gating (mirrors the
/// WebSocket rule: tokenless servers reject browser-originated traffic) and
/// mandatory `Content-Type: application/json`.
fn validate_http_rpc(request: &HttpRequest, security: &RemoteSecurity) -> Result<()> {
    validate_http_origin(request, security)?;
    let content_type = request.header("content-type").unwrap_or("");
    if !content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .eq_ignore_ascii_case("application/json")
    {
        return Err(anyhow!("POST /rpc requires Content-Type: application/json"));
    }
    Ok(())
}

fn validate_http_origin(request: &HttpRequest, security: &RemoteSecurity) -> Result<()> {
    if security.session_token.is_none() && request.header("origin").is_some() {
        return Err(anyhow!(
            "HTTP requests with an Origin header require a session token"
        ));
    }
    Ok(())
}

/// CORS header value: only echo an explicitly allowed origin, otherwise none.
fn cors_allow_origin(request: &HttpRequest, security: &RemoteSecurity) -> Option<String> {
    let allowed = security.allowed_origin.as_deref()?;
    let origin = request.header("origin")?;
    if origin == allowed {
        Some(allowed.to_string())
    } else {
        None
    }
}

fn write_preflight_response(
    stream: &mut TcpStream,
    request: &HttpRequest,
    security: &RemoteSecurity,
) -> Result<()> {
    let _ = stream.set_write_timeout(Some(HTTP_WRITE_TIMEOUT));
    let mut response =
        String::from("HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n");
    if let Some(origin) = cors_allow_origin(request, security) {
        response.push_str(&format!("Access-Control-Allow-Origin: {origin}\r\n"));
        response.push_str("Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n");
        response.push_str("Access-Control-Allow-Headers: Content-Type\r\n");
    }
    response.push_str("\r\n");
    stream.write_all(response.as_bytes())?;
    stream.flush()?;
    Ok(())
}

fn write_http_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &str,
) -> Result<()> {
    write_http_response_with_security(stream, status, reason, content_type, body, None, None)
}

fn write_http_response_with_security(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &str,
    request: Option<&HttpRequest>,
    security: Option<&RemoteSecurity>,
) -> Result<()> {
    // No wildcard CORS by default. Only echo an explicitly configured
    // `allowed_origin`, and only when it matches the request Origin.
    let cors = match (request, security) {
        (Some(req), Some(sec)) => cors_allow_origin(req, sec),
        _ => None,
    };
    let _ = stream.set_write_timeout(Some(HTTP_WRITE_TIMEOUT));
    let mut response = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n",
        body.len()
    );
    if let Some(origin) = cors {
        response.push_str(&format!("Access-Control-Allow-Origin: {origin}\r\n"));
    }
    response.push_str("\r\n");
    response.push_str(body);
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
    let _ = stream.set_write_timeout(Some(HTTP_WRITE_TIMEOUT));
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
            -LOOK_YAW_DELTA_LIMIT_RADIANS
        );
        assert_eq!(
            look_action["properties"]["yaw_delta"]["maximum"],
            LOOK_YAW_DELTA_LIMIT_RADIANS
        );
        assert_eq!(
            look_action["properties"]["pitch_delta"]["minimum"],
            -LOOK_PITCH_DELTA_LIMIT_RADIANS
        );
        assert_eq!(
            look_action["properties"]["pitch_delta"]["maximum"],
            LOOK_PITCH_DELTA_LIMIT_RADIANS
        );
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

        let observations = AgentObservationCatalog {
            schema: Some(json!({
                "type": "object",
                "required": ["phase"],
                "properties": { "phase": { "type": "string" } }
            })),
        };
        let observation = observation_schema_with_catalog(Some(&observations));
        let domain = observation["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .find(|variant| variant["properties"]["kind"]["const"] == "Domain")
            .unwrap();
        assert_eq!(domain["properties"]["value"]["required"], json!(["phase"]));
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
            ..Default::default()
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

    #[test]
    fn empty_session_token_is_invalid_config() {
        assert!(
            JsonRpcBridge::try_new(RemoteSecurity {
                session_token: Some(String::new()),
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            JsonRpcBridge::with_security(RemoteSecurity {
                session_token: Some(String::new()),
                ..Default::default()
            })
            .is_err()
        );
    }

    #[test]
    #[should_panic(expected = "must not be empty")]
    fn new_panics_on_empty_session_token() {
        let _ = JsonRpcBridge::new(RemoteSecurity {
            session_token: Some(String::new()),
            ..Default::default()
        });
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
        });
        assert!(
            player_only
                .authorize_operation(&OperationRequires {
                    mutation: None,
                    observation_visibility: Some(capability_for_observation_mode(
                        &ObservationMode::PlayerKnowledge
                    )),
                    snapshot_export: false,
                    restore_import: false,
                    filesystem: false,
                    control: None,
                })
                .is_ok()
        );
        assert!(
            player_only
                .authorize_operation(&OperationRequires {
                    mutation: None,
                    observation_visibility: Some(capability_for_observation_mode(
                        &ObservationMode::Hybrid
                    )),
                    snapshot_export: false,
                    restore_import: false,
                    filesystem: false,
                    control: None,
                })
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
        assert_eq!(method_not_found("x").code, -32601);
        assert_eq!(invalid_params("x").code, -32602);
        assert_eq!(auth_error("x").code, -32001);
    }

    #[test]
    fn request_budgets_are_enforced_as_constants() {
        assert_eq!(MAX_ACTIONS_PER_REQUEST, 1024);
        assert_eq!(MAX_TICKS_PER_REQUEST, 10_000);
        assert!(!timeout_frames_to_duration(8).is_zero());
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
        let observations = AgentObservationCatalog {
            schema: Some(json!({
                "type": "object",
                "required": ["phase"],
                "properties": { "phase": { "type": "string" } }
            })),
        };
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

    #[test]
    fn replay_topology_validation_accepts_legacy_and_single_root() {
        // Legacy logs carry no topology.
        let bundle = ReplayBundle {
            format_version: ReplayBundle::FORMAT_VERSION,
            log: ReplayLog::default(),
            snapshots: Vec::new(),
        };
        assert!(validate_replay_bundle_topology(&bundle).is_ok());

        // Single root with a linear child chain.
        let root = bevy_agent_core::BranchId::new();
        let child = bevy_agent_core::BranchId::new();
        let log = ReplayLog {
            timeline_topology: vec![
                bevy_agent_replay::TimelineBranch {
                    branch_id: root,
                    parent_branch: None,
                    fork_tick: 0,
                    fork_snapshot: None,
                    label: None,
                    actions: Vec::new(),
                },
                bevy_agent_replay::TimelineBranch {
                    branch_id: child,
                    parent_branch: Some(root),
                    fork_tick: 5,
                    fork_snapshot: None,
                    label: None,
                    actions: Vec::new(),
                },
            ],
            ..Default::default()
        };
        let bundle = ReplayBundle {
            format_version: ReplayBundle::FORMAT_VERSION,
            log,
            snapshots: Vec::new(),
        };
        assert!(validate_replay_bundle_topology(&bundle).is_ok());
    }

    #[test]
    fn replay_topology_validation_rejects_malformed_graphs() {
        fn bundle_with(branches: Vec<bevy_agent_replay::TimelineBranch>) -> ReplayBundle {
            let log = ReplayLog {
                timeline_topology: branches,
                ..Default::default()
            };
            ReplayBundle {
                format_version: ReplayBundle::FORMAT_VERSION,
                log,
                snapshots: Vec::new(),
            }
        }
        fn branch(
            id: bevy_agent_core::BranchId,
            parent: Option<bevy_agent_core::BranchId>,
        ) -> bevy_agent_replay::TimelineBranch {
            bevy_agent_replay::TimelineBranch {
                branch_id: id,
                parent_branch: parent,
                fork_tick: 0,
                fork_snapshot: None,
                label: None,
                actions: Vec::new(),
            }
        }

        // Self-parenting.
        let id = bevy_agent_core::BranchId::new();
        let err =
            validate_replay_bundle_topology(&bundle_with(vec![branch(id, Some(id))])).unwrap_err();
        assert!(err.to_string().contains("own parent"), "{err}");

        // Two-node cycle also leaves the graph without a root.
        let a = bevy_agent_core::BranchId::new();
        let b = bevy_agent_core::BranchId::new();
        let err = validate_replay_bundle_topology(&bundle_with(vec![
            branch(a, Some(b)),
            branch(b, Some(a)),
        ]))
        .unwrap_err();
        assert!(
            err.to_string().contains("no root") || err.to_string().contains("cycle"),
            "{err}"
        );

        // Longer cycle behind a valid root.
        let root = bevy_agent_core::BranchId::new();
        let x = bevy_agent_core::BranchId::new();
        let y = bevy_agent_core::BranchId::new();
        let err = validate_replay_bundle_topology(&bundle_with(vec![
            branch(root, None),
            branch(x, Some(y)),
            branch(y, Some(x)),
        ]))
        .unwrap_err();
        assert!(err.to_string().contains("cycle"), "{err}");

        // Unknown parent.
        let orphan = bevy_agent_core::BranchId::new();
        let missing = bevy_agent_core::BranchId::new();
        let err =
            validate_replay_bundle_topology(&bundle_with(vec![branch(orphan, Some(missing))]))
                .unwrap_err();
        assert!(err.to_string().contains("unknown parent"), "{err}");

        // Multiple roots.
        let r1 = bevy_agent_core::BranchId::new();
        let r2 = bevy_agent_core::BranchId::new();
        let err =
            validate_replay_bundle_topology(&bundle_with(vec![branch(r1, None), branch(r2, None)]))
                .unwrap_err();
        assert!(err.to_string().contains("roots"), "{err}");

        // Oversize graph.
        let big: Vec<_> = (0..MAX_IMPORT_BRANCHES + 1)
            .map(|_| {
                let fresh = bevy_agent_core::BranchId::new();
                branch(fresh, None)
            })
            .collect();
        let err = validate_replay_bundle_topology(&bundle_with(big)).unwrap_err();
        assert!(err.to_string().contains("exceeds limit"), "{err}");
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
