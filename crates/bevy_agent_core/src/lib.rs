//! Core Bevy plugin primitives for controllable agent-driven simulations.
//!
//! This crate owns the deterministic tick schedules, action queue, input frame,
//! simulation clock, observation types, reward/episode state, and extension
//! traits used by the runner, snapshot, replay, and remote crates.

use bevy::ecs::schedule::ScheduleLabel;
use bevy::prelude::*;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

#[path = "types/actions.rs"]
mod actions;
#[path = "types/clock.rs"]
mod clock;
#[path = "types/control.rs"]
mod control;
#[path = "types/error.rs"]
mod error;
#[path = "types/identity.rs"]
mod identity;
#[path = "types/observation.rs"]
mod observation;
#[path = "types/schedule.rs"]
mod schedule;

pub use actions::*;
pub use clock::*;
pub use control::*;
pub use error::*;
pub use identity::*;
pub use observation::*;
pub use schedule::*;

mod catalog;
mod checksum;
mod integration;
mod memory;
mod queue;
mod runtime;
mod schema;

pub use catalog::*;
pub use checksum::*;
pub use integration::*;
pub use memory::json_heap_bytes;
pub use queue::*;
pub use runtime::{
    AgentControlPlugin, AgentTickFailure, begin_tick, collect_observation,
    collect_observation_with_mode, drain_agent_actions, reset_agent_core, run_agent_tick,
    validate_next_tick,
};

#[cfg(test)]
mod tests;
