use crate::*;

#[derive(Resource, Clone, Debug, Serialize, Deserialize)]
pub struct SimClock {
    pub tick: u64,
    pub dt_seconds: f32,
    pub elapsed_seconds: f64,
}

impl SimClock {
    /// Creates a clock running at `tick_hz` ticks per second.
    ///
    /// # Panics
    ///
    /// Panics when `tick_hz` is zero, because the fixed timestep
    /// `1.0 / tick_hz` would be infinite.
    #[must_use]
    pub fn new(tick_hz: u32) -> Self {
        Self::try_new(tick_hz).unwrap_or_else(|error| panic!("{error}"))
    }

    /// Fallible constructor used when the tick rate comes from untrusted
    /// input (configs, snapshots, network params).
    pub fn try_new(tick_hz: u32) -> ControlResult<Self> {
        if tick_hz == 0 {
            return Err(AgentControlError::Message(
                "SimClock tick rate must be > 0 Hz".to_string(),
            ));
        }
        Ok(Self {
            tick: 0,
            dt_seconds: 1.0 / tick_hz as f32,
            elapsed_seconds: 0.0,
        })
    }

    /// Validates an imported/deserialized clock (snapshot restore, replay).
    /// Rejects zero/NaN/infinite timesteps and absurd tick values that would
    /// poison physics (`dt * velocity`) or terminal checks.
    pub fn validate(&self) -> ControlResult<()> {
        if !self.dt_seconds.is_finite() || self.dt_seconds <= 0.0 {
            return Err(AgentControlError::Message(format!(
                "invalid SimClock dt_seconds {}: must be finite and > 0",
                self.dt_seconds
            )));
        }
        if !self.elapsed_seconds.is_finite() || self.elapsed_seconds < 0.0 {
            return Err(AgentControlError::Message(format!(
                "invalid SimClock elapsed_seconds {}: must be finite and >= 0",
                self.elapsed_seconds
            )));
        }
        validate_clock_tick(self.tick)?;
        Ok(())
    }

    /// Sets the tick after validating it (snapshot/timeline restores).
    pub fn set_tick_checked(&mut self, tick: u64) -> ControlResult<()> {
        validate_clock_tick(tick)?;
        self.tick = tick;
        Ok(())
    }

    pub fn advance_one_tick(&mut self) {
        self.tick += 1;
        self.elapsed_seconds += self.dt_seconds as f64;
    }
}

/// Upper bound guard for imported tick values. Real episodes are thousands
/// of ticks; anything above u32::MAX almost certainly indicates corrupt or
/// adversarial snapshot/replay data.
pub fn validate_clock_tick(tick: u64) -> ControlResult<()> {
    const MAX_REASONABLE_TICK: u64 = u32::MAX as u64;
    if tick > MAX_REASONABLE_TICK {
        return Err(AgentControlError::Message(format!(
            "invalid SimClock tick {tick}: exceeds maximum {MAX_REASONABLE_TICK}"
        )));
    }
    Ok(())
}

impl Default for SimClock {
    fn default() -> Self {
        Self::new(60)
    }
}

#[derive(Resource, Clone, Debug, Serialize, Deserialize)]
pub struct DeterministicRng {
    pub seed: u64,
    pub rng: ChaCha8Rng,
}

impl DeterministicRng {
    #[must_use]
    pub fn seeded(seed: u64) -> Self {
        Self {
            seed,
            rng: ChaCha8Rng::seed_from_u64(seed),
        }
    }
}

impl Default for DeterministicRng {
    fn default() -> Self {
        Self::seeded(0)
    }
}
