//! Replay logs and timeline branches for deterministic agent-controlled games.

mod log;
mod model;
mod owner;
mod recording;
mod timeline;

pub use log::{MAX_RECONSTRUCTION_TICKS, collect_replay_references};
pub use model::*;
pub use owner::BranchSavepoint;
pub use recording::{AgentReplayPlugin, record_replay_step, start_recording, stop_recording};
pub use timeline::*;

#[cfg(test)]
mod tests;
