//! Stable identities for framework-owned snapshot types.

use crate::SnapshotType;
use bevy::prelude::Transform;
use bevy_agent_core::{
    AgentActionQueue, CurrentInputFrame, DeterministicRng, EpisodeState, ObservationConfig,
    RewardState, SimClock, StableEntityId, StableIdAllocator,
};

macro_rules! identity {
    ($ty:ty, $id:literal) => {
        impl SnapshotType for $ty {
            const TYPE_ID: &'static str = $id;
            const SCHEMA_VERSION: u32 = 1;
        }
    };
}

identity!(StableEntityId, "bevy_agent_core.stable_entity_id");
identity!(SimClock, "bevy_agent_core.sim_clock");
identity!(AgentActionQueue, "bevy_agent_core.action_queue");
identity!(StableIdAllocator, "bevy_agent_core.stable_id_allocator");
identity!(DeterministicRng, "bevy_agent_core.deterministic_rng");
identity!(CurrentInputFrame, "bevy_agent_core.input_frame");
identity!(ObservationConfig, "bevy_agent_core.observation_config");
identity!(RewardState, "bevy_agent_core.reward_state");
identity!(EpisodeState, "bevy_agent_core.episode_state");
identity!(Transform, "bevy.transform");
