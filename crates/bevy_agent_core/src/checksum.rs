//! Canonical environment-state and JSON hashing.

use crate::*;
use std::hash::Hasher;

/// Version for the canonical checksum serialization.
///
/// Bump this when the set of hashed fields or their encoding changes so
/// mismatched producers/consumers fail loudly instead of colliding silently.
pub const CHECKSUM_VERSION: u32 = 2;

pub fn default_checksum(world: &World) -> EnvironmentChecksum {
    let clock = world.resource::<SimClock>();
    let reward = world.resource::<RewardState>();
    let episode = world.resource::<EpisodeState>();
    let input = world.resource::<CurrentInputFrame>();

    let mut hasher = StableHasher::new();
    hasher.write_u32(CHECKSUM_VERSION);
    hasher.write_u64(clock.tick);
    hasher.write_u32(clock.dt_seconds.to_bits());
    hasher.write_u64(clock.elapsed_seconds.to_bits());
    hasher.write_u32(reward.current_reward.to_bits());
    hasher.write_u32(reward.cumulative_reward.to_bits());
    hasher.write_bool_value(episode.done);
    hasher.write_bool_value(episode.truncated);
    // `None` vs `Some("")` must hash differently; length-prefix via write_string.
    match &episode.reason {
        Some(reason) => {
            hasher.write_bool_value(true);
            hasher.write_string(reason);
        }
        None => hasher.write_bool_value(false),
    }

    // Current input frame: tick + full action/source contents (not just len).
    hasher.write_u64(input.tick);
    hasher.write_u64(input.actions.len() as u64);
    for action in &input.actions {
        match serde_json::to_value(action) {
            Ok(value) => hasher.write_json(&value),
            Err(_) => hasher.write_string("unserializable-action"),
        }
    }
    hasher.write_u64(input.sources.len() as u64);
    for source in &input.sources {
        match serde_json::to_value(source) {
            Ok(value) => hasher.write_json(&value),
            Err(_) => hasher.write_string("unserializable-source"),
        }
    }

    // Queued (not yet drained) inputs: tick + source + action contents.
    if let Some(queue) = world.get_resource::<AgentActionQueue>() {
        hasher.write_u64(queue.len() as u64);
        for scheduled in queue.iter() {
            hasher.write_u64(scheduled.tick);
            match serde_json::to_value(&scheduled.source) {
                Ok(value) => hasher.write_json(&value),
                Err(_) => hasher.write_string("unserializable-source"),
            }
            match serde_json::to_value(&scheduled.action) {
                Ok(value) => hasher.write_json(&value),
                Err(_) => hasher.write_string("unserializable-action"),
            }
        }
    } else {
        // Explicit partial-checksum marker when the queue is unavailable.
        hasher.write_string("partial:no-action-queue");
    }

    // RNG state, when available. Without it the checksum is partial: two
    // worlds that differ only in future RNG draws would otherwise collide.
    if let Some(rng) = world.get_resource::<DeterministicRng>() {
        hasher.write_u64(rng.seed);
        match serde_json::to_value(&rng.rng) {
            Ok(value) => hasher.write_json(&value),
            Err(_) => hasher.write_string("partial:unserializable-rng"),
        }
    } else {
        hasher.write_string("partial:no-rng");
    }

    EnvironmentChecksum {
        tick: clock.tick,
        hash: hasher.finish_hash(),
    }
}

#[derive(Clone, Debug)]
pub struct StableHasher {
    state: u64,
}

impl StableHasher {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x00000100000001b3;

    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: Self::OFFSET,
        }
    }

    pub fn write_stable_bytes(&mut self, bytes: &[u8]) {
        self.write_u64(bytes.len() as u64);
        self.write(bytes);
    }

    pub fn write_string(&mut self, value: &str) {
        self.write_stable_bytes(value.as_bytes());
    }

    pub fn write_bool_value(&mut self, value: bool) {
        self.write_u8(u8::from(value));
    }

    pub fn write_f32_value(&mut self, value: f32) {
        self.write_u32(value.to_bits());
    }

    pub fn write_f64_value(&mut self, value: f64) {
        self.write_u64(value.to_bits());
    }

    pub fn write_json(&mut self, value: &serde_json::Value) {
        hash_json_into(value, self);
    }

    #[must_use]
    pub const fn finish_hash(&self) -> u64 {
        self.state
    }
}

impl Default for StableHasher {
    fn default() -> Self {
        Self::new()
    }
}

impl Hasher for StableHasher {
    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.state ^= u64::from(*byte);
            self.state = self.state.wrapping_mul(Self::PRIME);
        }
    }

    fn write_u8(&mut self, i: u8) {
        self.write(&[i]);
    }

    fn write_u16(&mut self, i: u16) {
        self.write(&i.to_le_bytes());
    }

    fn write_u32(&mut self, i: u32) {
        self.write(&i.to_le_bytes());
    }

    fn write_u64(&mut self, i: u64) {
        self.write(&i.to_le_bytes());
    }

    fn write_u128(&mut self, i: u128) {
        self.write(&i.to_le_bytes());
    }

    fn write_usize(&mut self, i: usize) {
        // Fixed-width LE regardless of platform (usize is 32/64-bit).
        self.write(&(i as u64).to_le_bytes());
    }

    fn write_i8(&mut self, i: i8) {
        self.write(&[i as u8]);
    }

    fn write_i16(&mut self, i: i16) {
        self.write(&i.to_le_bytes());
    }

    fn write_i32(&mut self, i: i32) {
        self.write(&i.to_le_bytes());
    }

    fn write_i64(&mut self, i: i64) {
        self.write(&i.to_le_bytes());
    }

    fn write_i128(&mut self, i: i128) {
        self.write(&i.to_le_bytes());
    }

    fn write_isize(&mut self, i: isize) {
        self.write(&(i as i64).to_le_bytes());
    }

    fn finish(&self) -> u64 {
        self.finish_hash()
    }
}

#[must_use]
pub fn stable_hash_json(value: &serde_json::Value) -> u64 {
    let mut hasher = StableHasher::new();
    hasher.write_json(value);
    hasher.finish_hash()
}

fn hash_json_into(value: &serde_json::Value, hasher: &mut StableHasher) {
    match value {
        serde_json::Value::Null => hasher.write_u8(0),
        serde_json::Value::Bool(value) => {
            hasher.write_u8(1);
            hasher.write_bool_value(*value);
        }
        serde_json::Value::Number(value) => {
            hasher.write_u8(2);
            hasher.write_string(&value.to_string());
        }
        serde_json::Value::String(value) => {
            hasher.write_u8(3);
            hasher.write_string(value);
        }
        serde_json::Value::Array(values) => {
            hasher.write_u8(4);
            hasher.write_u64(values.len() as u64);
            for value in values {
                hash_json_into(value, hasher);
            }
        }
        serde_json::Value::Object(values) => {
            hasher.write_u8(5);
            hasher.write_u64(values.len() as u64);
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            for key in keys {
                hasher.write_string(key);
                hash_json_into(&values[key], hasher);
            }
        }
    }
}
