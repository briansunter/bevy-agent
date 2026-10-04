#![doc = include_str!("../README.md")]

/// Maximum complete JSON message size, including a stdio line delimiter.
pub const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;

/// Maximum number of actions accepted in a single `agent.step_many` call.
pub const MAX_ACTIONS_PER_REQUEST: usize = 1024;
/// Maximum number of ticks accepted in a single `agent.fast_forward` call.
pub const MAX_TICKS_PER_REQUEST: u64 = 10_000;
/// Upper bound on asynchronous capture deadlines and watchdog lifetimes.
pub const MAX_CAPTURE_TIMEOUT_FRAMES: u32 = 1_800;
mod http;
mod main_thread;
mod operations;
mod protocol;
mod rpc;
mod schema;
mod security;
mod server;
mod transport;
mod websocket;

pub use main_thread::BevyRemoteControlPlugin;
pub use operations::{OperationState, OperationStatus, OperationStatusParams};
pub use protocol::{RpcMethod, method_params_schema, rpc_method_schema};
pub use rpc::{
    JsonRpcBridge, JsonRpcError, JsonRpcRequest, JsonRpcResponse, PreparedRequest,
    timeout_frames_to_duration,
};
pub use schema::*;
pub use security::{
    AgentCapability, RPC_AUTH_ERROR, RPC_INTERNAL_ERROR, RPC_INVALID_PARAMS, RPC_INVALID_REQUEST,
    RPC_METHOD_NOT_FOUND, RPC_PARSE_ERROR, RPC_REQUEST_TIMEOUT, RPC_SERVER_BUSY, RemoteSecurity,
    capability_for_observation_mode,
};
pub use server::HttpRemoteServer;
pub use transport::{RemoteServerLimits, RemoteServerStatus};

#[cfg(test)]
mod tests;
