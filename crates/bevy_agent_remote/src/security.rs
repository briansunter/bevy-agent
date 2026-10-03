use anyhow::{Context, Result, anyhow};
use bevy_agent_core::ObservationMode;
use bitflags::bitflags;
use serde::{Deserialize, Serialize};
use std::{io::Write, net::SocketAddr, path::PathBuf};

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct AgentCapability: u32 {
        const STEP = 1 << 0;
        const OBSERVE_PLAYER = 1 << 1;
        const OBSERVE_FULL_STATE = 1 << 2;
        const SNAPSHOT = 1 << 3;
        const RESTORE = 1 << 4;
        const BRANCH = 1 << 5;
        const VISUAL_CAPTURE = 1 << 6;
        /// Mutating control of run state: pause/resume/set_mode.
        const CONTROL = 1 << 7;
        /// Exporting replay bundles / snapshots to the caller.
        const SNAPSHOT_EXPORT = 1 << 8;
        /// Touching the local filesystem (replay export/load `path`,
        /// visual capture `output_dir`). NOT in the default set so
        /// restricted deployments deny filesystem access by default.
        const FILESYSTEM = 1 << 9;
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
/// Deadline expiry or disconnected simulation owner, with operation correlation.
pub const RPC_REQUEST_TIMEOUT: i32 = -32002;
/// Bounded connection, command, or operation admission is saturated.
pub const RPC_SERVER_BUSY: i32 = -32003;

#[derive(Clone, Debug, Default)]
pub struct RemoteSecurity {
    pub session_token: Option<String>,
    pub capabilities: AgentCapability,
    /// Root directory confining all filesystem writes/reads (replay export,
    /// replay load from path, visual captures). `None` resolves to the OS
    /// temp dir joined with `bevy-agent-artifacts`.
    pub artifact_root: Option<PathBuf>,
    /// Optional exact Origin value echoed back as `Access-Control-Allow-Origin`.
    /// When `None` (default) no CORS header is emitted.
    pub allowed_origin: Option<String>,
}

impl RemoteSecurity {
    /// Validate configuration at each network/authentication boundary, including
    /// bridges constructed directly through their public fields.
    pub(crate) fn validate(&self) -> Result<()> {
        if self.session_token.as_deref() == Some("") {
            return Err(anyhow!(
                "RemoteSecurity::session_token must not be empty; use None for no auth"
            ));
        }
        if self
            .allowed_origin
            .as_deref()
            .is_some_and(|origin| origin.contains(['\r', '\n']))
        {
            return Err(anyhow!("allowed_origin must not contain HTTP line breaks"));
        }
        Ok(())
    }

    /// Resolved artifact root, defaulting to the OS temp dir.
    #[must_use]
    pub fn artifact_root_resolved(&self) -> PathBuf {
        self.artifact_root
            .clone()
            .unwrap_or_else(|| std::env::temp_dir().join("bevy-agent-artifacts"))
    }

    /// Resolve a relative artifact path, rejecting traversal and symlink escape.
    pub fn resolve_artifact_path(&self, raw: &str) -> Result<PathBuf> {
        resolve_confined_path(&self.artifact_root_resolved(), raw)
    }

    pub fn resolve_output_dir(&self, raw: &std::path::Path) -> Result<PathBuf> {
        self.resolve_artifact_path(
            raw.to_str()
                .ok_or_else(|| anyhow!("artifact paths must be valid UTF-8"))?,
        )
    }
}

/// Resolve the nearest existing ancestor before appending new components.
/// Concurrent ancestor replacement between validation and open remains a
/// filesystem TOCTOU boundary; exclusive final-file creation prevents truncation.
fn resolve_confined_path(root: &std::path::Path, raw: &str) -> Result<PathBuf> {
    let candidate = std::path::Path::new(raw);
    if raw.is_empty()
        || candidate.is_absolute()
        || raw.split(['/', '\\']).any(|component| component == "..")
    {
        return Err(anyhow!(
            "artifact path must be relative and cannot contain parent traversal: {raw}"
        ));
    }
    std::fs::create_dir_all(root)
        .with_context(|| format!("creating artifact root {}", root.display()))?;
    let root = root
        .canonicalize()
        .with_context(|| format!("canonicalizing artifact root {}", root.display()))?;
    let joined = root.join(candidate);
    let mut ancestor = joined.as_path();
    let mut remainder = Vec::new();
    let resolved = loop {
        match ancestor.symlink_metadata() {
            Ok(_) => {
                // Unlike `exists`, symlink_metadata sees dangling links: a
                // failed canonicalization is rejected rather than falling back.
                let mut canonical = ancestor
                    .canonicalize()
                    .with_context(|| format!("resolving artifact path {raw}"))?;
                for component in remainder.iter().rev() {
                    canonical.push(component);
                }
                break canonical;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                remainder.push(
                    ancestor
                        .file_name()
                        .ok_or_else(|| anyhow!("invalid artifact path {raw}"))?
                        .to_owned(),
                );
                ancestor = ancestor
                    .parent()
                    .ok_or_else(|| anyhow!("invalid artifact path {raw}"))?;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("resolving artifact path {raw}"));
            }
        }
    };
    if !resolved.starts_with(&root) {
        return Err(anyhow!("path escapes artifact root: {raw}"));
    }
    Ok(resolved)
}

/// Exclusive file creation (`create_new`): fails when the destination already
/// exists instead of truncating it. Narrows (but does not close) the
/// resolve-vs-open TOCTOU window; callers should still resolve via
/// [`resolve_confined_path`] immediately before calling this.
pub(crate) fn write_bundle_exclusive(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
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

pub(crate) fn require_safe_bind(local_addr: SocketAddr, security: &RemoteSecurity) -> Result<()> {
    security.validate()?;
    if security.session_token.is_none() && !local_addr.ip().is_loopback() {
        return Err(anyhow!(
            "refusing unauthenticated remote control on non-loopback bind {}; set a session token",
            local_addr
        ));
    }
    Ok(())
}
