//! Core Bevy plugin primitives for controllable agent-driven simulations.
//!
//! This crate owns the deterministic tick schedules, action queue, input frame,
//! simulation clock, observation types, reward/episode state, and extension
//! traits used by the runner, snapshot, replay, and remote crates.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::hash::{Hash, Hasher};

use bevy::ecs::schedule::ScheduleLabel;
use bevy::prelude::*;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

#[derive(Error, Debug)]
pub enum AgentControlError {
    #[error("agent tick produced no step response")]
    MissingStepResponse,
    #[error("requested resource is not installed: {0}")]
    MissingResource(&'static str),
    #[error("invalid action: {0}")]
    InvalidAction(String),
    #[error("unsupported observation mode: {0}")]
    UnsupportedObservationMode(String),
    #[error("episode is terminal ({reason}); reset before stepping")]
    TerminalStepRejected { reason: String },
    #[error("agent control error: {0}")]
    Message(String),
}

pub type ControlResult<T> = Result<T, AgentControlError>;

#[derive(ScheduleLabel, Clone, Debug, PartialEq, Eq, Hash)]
pub struct AgentReset;

/// Runs once immediately before each controlled simulation tick.
///
/// Agent policies should enqueue actions for the upcoming tick from this
/// schedule. Keeping decisions in their own schedule makes it impossible for
/// policy code to accidentally run once per render frame.
#[derive(ScheduleLabel, Clone, Debug, PartialEq, Eq, Hash)]
pub struct AgentDecision;

#[derive(ScheduleLabel, Clone, Debug, PartialEq, Eq, Hash)]
pub struct AgentPreTick;

#[derive(ScheduleLabel, Clone, Debug, PartialEq, Eq, Hash)]
pub struct AgentTick;

#[derive(ScheduleLabel, Clone, Debug, PartialEq, Eq, Hash)]
pub struct AgentPostTick;

#[derive(SystemSet, Clone, Debug, PartialEq, Eq, Hash)]
pub enum AgentResetSet {
    Core,
    Game,
    Observation,
}

#[derive(SystemSet, Clone, Debug, PartialEq, Eq, Hash)]
pub enum AgentSet {
    BeginTick,
    DrainActions,
    ApplyInput,
    Simulation,
    TerminalCheck,
    Observation,
    ReplayRecord,
    Snapshot,
    EndTick,
}

#[derive(Component, Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct StableEntityId(pub u128);

impl StableEntityId {
    #[must_use]
    pub const fn from_u64(value: u64) -> Self {
        Self(value as u128)
    }
}

#[derive(Component, Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct SnapshotEntity;

#[derive(Resource, Clone, Debug, Serialize, Deserialize)]
pub struct StableIdAllocator {
    pub next: u128,
}

impl Default for StableIdAllocator {
    fn default() -> Self {
        Self { next: 1 }
    }
}

impl StableIdAllocator {
    pub fn allocate(&mut self) -> StableEntityId {
        let id = StableEntityId(self.next);
        self.next += 1;
        id
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SnapshotId(pub Uuid);

impl SnapshotId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SnapshotId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct TimelineId(pub Uuid);

impl TimelineId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for TimelineId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct BranchId(pub Uuid);

impl BranchId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for BranchId {
    fn default() -> Self {
        Self::new()
    }
}

/// Execution context for the simulation.
///
/// Moved into `bevy_agent_core` (from the replay crate) so that
/// `drain_agent_actions` can gate on a world resource instead of scanning
/// the action queue for a caller-forgeable marker. The runner installs
/// `Reconstructing` for the duration of history replay; policy systems and
/// snapshot bookkeeping stay quiet while recorded ticks are rebuilt.
/// The replay crate keeps a mirror enum for its own recording gate; the
/// runner sets both during reconstruction.
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionContext {
    #[default]
    Live,
    Reconstructing,
}

/// Prefix reserved for internal custom-action markers.
///
/// [`validate_action_against_catalog`] rejects any `Custom` value containing
/// this prefix so callers cannot forge internal privilege signals (the old
/// `__bevy_agent_reconstructing__` queue sentinel is gone; reconstruction
/// is signaled via the [`ExecutionContext`] resource instead).
pub const RESERVED_CUSTOM_PREFIX: &str = "__bevy_agent_";

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

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum AgentAction {
    #[default]
    Noop,
    Move {
        x: f32,
        y: f32,
    },
    Look {
        yaw_delta: f32,
        pitch_delta: f32,
    },
    Jump,
    Crouch,
    Sprint,
    Interact,
    Attack {
        target: Option<StableEntityId>,
    },
    UseItem {
        slot: u8,
    },
    Dodge,
    Custom {
        value: serde_json::Value,
    },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum AgentActionKind {
    Noop,
    Move,
    Look,
    Jump,
    Crouch,
    Sprint,
    Interact,
    Attack,
    UseItem,
    Dodge,
    Custom,
}

impl AgentAction {
    #[must_use]
    pub const fn kind(&self) -> AgentActionKind {
        match self {
            Self::Noop => AgentActionKind::Noop,
            Self::Move { .. } => AgentActionKind::Move,
            Self::Look { .. } => AgentActionKind::Look,
            Self::Jump => AgentActionKind::Jump,
            Self::Crouch => AgentActionKind::Crouch,
            Self::Sprint => AgentActionKind::Sprint,
            Self::Interact => AgentActionKind::Interact,
            Self::Attack { .. } => AgentActionKind::Attack,
            Self::UseItem { .. } => AgentActionKind::UseItem,
            Self::Dodge => AgentActionKind::Dodge,
            Self::Custom { .. } => AgentActionKind::Custom,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CustomActionSchema {
    pub name: String,
    pub schema: serde_json::Value,
}

#[derive(Resource, Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct AgentActionCatalog {
    /// `None` preserves the original behavior and exposes every built-in
    /// action. Games can opt into accurate discovery with
    /// [`AgentControlAppExt::set_supported_actions`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supported_actions: Option<BTreeSet<AgentActionKind>>,
    pub custom_actions: BTreeMap<String, CustomActionSchema>,
}

impl AgentActionCatalog {
    pub fn register_custom_action_schema(
        &mut self,
        name: impl Into<String>,
        schema: serde_json::Value,
    ) {
        let name = name.into();
        self.custom_actions
            .insert(name.clone(), CustomActionSchema { name, schema });
    }

    pub fn set_supported_actions(&mut self, actions: impl IntoIterator<Item = AgentActionKind>) {
        self.supported_actions = Some(actions.into_iter().collect());
    }

    #[must_use]
    pub fn supports(&self, action: AgentActionKind) -> bool {
        self.supported_actions
            .as_ref()
            .is_none_or(|supported| supported.contains(&action))
    }

    /// Validates an action against the catalog at the controller boundary.
    /// Rejects unsupported kinds and out-of-bounds/invalid payloads with
    /// [`AgentControlError::InvalidAction`].
    pub fn validate_action(&self, action: &AgentAction) -> ControlResult<()> {
        validate_action_against_catalog(self, action)
    }
}

/// Controller-boundary validation shared by in-process (`AgentApp::step`)
/// and remote (JSON-RPC) stepping paths.
///
/// * Unsupported [`AgentActionKind`]s (per [`AgentActionCatalog`]) are
///   rejected.
/// * Bounded payloads are rejected when non-finite or out of range:
///   `Move.x/y` must be finite in `[-1, 1]`, `Look` deltas must be finite.
/// * `Custom` values containing the reserved [`RESERVED_CUSTOM_PREFIX`]
///   (`__bevy_agent_`) are rejected: that prefix is reserved for internal
///   markers and must never be caller-forgeable.
pub fn validate_action_against_catalog(
    catalog: &AgentActionCatalog,
    action: &AgentAction,
) -> ControlResult<()> {
    let kind = action.kind();
    if !catalog.supports(kind) {
        return Err(AgentControlError::InvalidAction(format!(
            "unsupported action kind {kind:?}"
        )));
    }
    if let AgentAction::Custom { value } = action
        && custom_value_is_reserved(value)
    {
        return Err(AgentControlError::InvalidAction(format!(
            "Custom action value uses reserved prefix {RESERVED_CUSTOM_PREFIX:?}"
        )));
    }
    match action {
        AgentAction::Move { x, y } => {
            if !x.is_finite() || !y.is_finite() {
                return Err(AgentControlError::InvalidAction(format!(
                    "Move{{x: {x}, y: {y}}} must be finite"
                )));
            }
            if x.abs() > 1.0 || y.abs() > 1.0 {
                return Err(AgentControlError::InvalidAction(format!(
                    "Move{{x: {x}, y: {y}}} out of bounds: expected [-1, 1]"
                )));
            }
        }
        AgentAction::Look {
            yaw_delta,
            pitch_delta,
        } if !yaw_delta.is_finite() || !pitch_delta.is_finite() => {
            return Err(AgentControlError::InvalidAction(format!(
                "Look{{yaw_delta: {yaw_delta}, pitch_delta: {pitch_delta}}} must be finite"
            )));
        }
        _ => {}
    }
    Ok(())
}

/// Returns `true` when a `Custom` action value touches the reserved
/// [`RESERVED_CUSTOM_PREFIX`] namespace (object keys or string payloads,
/// searched recursively). Reserved keys can never be set by callers.
fn custom_value_is_reserved(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(map) => map.iter().any(|(key, nested)| {
            key.starts_with(RESERVED_CUSTOM_PREFIX) || custom_value_is_reserved(nested)
        }),
        serde_json::Value::Array(items) => items.iter().any(custom_value_is_reserved),
        serde_json::Value::String(text) => text.starts_with(RESERVED_CUSTOM_PREFIX),
        _ => false,
    }
}

/// Legacy reconstruction sentinel shape (pre-`ExecutionContext`).
///
/// The queue-marker mechanism is removed: reconstruction is signaled via the
/// [`ExecutionContext`] resource. This helper only lets `drain_agent_actions`
/// drop stale sentinels still present in old queues/snapshots; it never
/// grants privilege.
fn is_legacy_reconstructing_sentinel(action: &AgentAction) -> bool {
    if let AgentAction::Custom { value } = action {
        value.get("__bevy_agent_reconstructing__") == Some(&serde_json::Value::Bool(true))
    } else {
        false
    }
}

#[derive(Resource, Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnvironmentMetadata {
    pub name: String,
    pub version: String,
    pub description: Option<String>,
}

impl Default for EnvironmentMetadata {
    fn default() -> Self {
        Self {
            name: "unknown-game".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            description: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ActionSource {
    Agent,
    Human,
    Replay,
    Script,
    Network,
    Test,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ScheduledAction<A = AgentAction> {
    pub tick: u64,
    pub source: ActionSource,
    pub action: A,
}

#[derive(Resource, Clone, Debug, Serialize, Deserialize)]
pub struct AgentActionQueue<A = AgentAction> {
    pub pending: VecDeque<ScheduledAction<A>>,
}

impl<A> Default for AgentActionQueue<A> {
    fn default() -> Self {
        Self {
            pending: VecDeque::new(),
        }
    }
}

impl<A> AgentActionQueue<A> {
    pub fn schedule(&mut self, tick: u64, source: ActionSource, action: A) {
        self.pending.push_back(ScheduledAction {
            tick,
            source,
            action,
        });
    }

    pub fn clear(&mut self) {
        self.pending.clear();
    }
}

/// Policy for reconciling queued future inputs across history navigation.
///
/// `ReplaceFromImport` (replay-bundle load) replaces the live queue with the
/// imported snapshot's `action_queue` and never merges destination futures;
/// `PreserveCurrentFuture` (interactive `restore_tick`/`branch` rewind)
/// preserves caller-enqueued futures beyond the target via a
/// multiplicity-preserving merge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingInputPolicy {
    ReplaceFromImport,
    PreserveCurrentFuture,
}

/// Applies pending-input reconciliation per `policy`.
///
/// * `ReplaceFromImport`: `queue.pending` becomes `imported` verbatim
///   (`before` — pre-existing destination futures — is discarded).
/// * `PreserveCurrentFuture`: multiset-merges `before` into the live queue
///   (an identical entry is appended only while the live queue holds fewer
///   copies), then re-sorts by tick. `imported` is ignored.
pub fn apply_pending_policy<A: Clone + PartialEq>(
    queue: &mut AgentActionQueue<A>,
    before: Vec<ScheduledAction<A>>,
    imported: Vec<ScheduledAction<A>>,
    policy: PendingInputPolicy,
) {
    match policy {
        PendingInputPolicy::ReplaceFromImport => {
            queue.pending = imported.into();
        }
        PendingInputPolicy::PreserveCurrentFuture => {
            for (index, scheduled) in before.iter().enumerate() {
                let needed = before[..=index]
                    .iter()
                    .filter(|candidate| *candidate == scheduled)
                    .count();
                let present = queue
                    .pending
                    .iter()
                    .filter(|candidate| *candidate == scheduled)
                    .count();
                if present < needed {
                    queue.pending.push_back(scheduled.clone());
                }
            }
            let mut pending = std::mem::take(&mut queue.pending);
            pending
                .make_contiguous()
                .sort_by_key(|scheduled| scheduled.tick);
            queue.pending = pending;
        }
    }
}

#[derive(Resource, Clone, Debug, Serialize, Deserialize)]
pub struct CurrentInputFrame<A = AgentAction> {
    pub tick: u64,
    pub actions: Vec<A>,
    pub sources: Vec<ActionSource>,
}

impl<A> Default for CurrentInputFrame<A> {
    fn default() -> Self {
        Self {
            tick: 0,
            actions: Vec::new(),
            sources: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub enum ObservationMode {
    PlayerKnowledge,
    FullDebugState,
    DiffSinceLastTick,
    PixelFrame,
    #[default]
    Hybrid,
}

impl ObservationMode {
    /// Modes with a first-class extractor in the sample games.
    ///
    /// `Symbolic` observations are produced from `PlayerKnowledge` requests
    /// and `Hybrid` adds an optional debug payload on top of the same
    /// symbolic base. Discovery (`agent.observation_space`) should advertise
    /// only these; `PixelFrame`/`DiffSinceLastTick`/`FullDebugState` fall back
    /// to symbolic (`Hybrid` keeps its debug block) unless a game installs a
    /// dedicated renderer/delta encoder.
    #[must_use]
    pub const fn implemented_modes() -> &'static str {
        "Symbolic, Hybrid (Hybrid carries symbolic + optional debug; no pixel capture by default)"
    }

    /// Returns `true` for modes with a dedicated extractor. Everything else
    /// uses the symbolic fallback documented in
    /// [`ObservationMode::implemented_modes`].
    #[must_use]
    pub const fn is_implemented(&self) -> bool {
        match self {
            Self::PlayerKnowledge | Self::Hybrid => true,
            Self::FullDebugState | Self::DiffSinceLastTick | Self::PixelFrame => false,
        }
    }

    /// Validates a requested mode. Implemented modes pass through;
    /// unimplemented modes return [`AgentControlError::UnsupportedObservationMode`]
    /// so callers can either surface the error or fall back to symbolic.
    pub fn validate_supported(&self) -> ControlResult<()> {
        if self.is_implemented() {
            Ok(())
        } else {
            Err(AgentControlError::UnsupportedObservationMode(format!(
                "{self:?} has no dedicated extractor; use Hybrid or PlayerKnowledge (symbolic fallback)"
            )))
        }
    }
}

#[derive(Resource, Clone, Debug, Default, Serialize, Deserialize)]
pub struct ObservationConfig {
    pub mode: ObservationMode,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PlayerObservation {
    pub stable_id: Option<StableEntityId>,
    pub position: [f32; 3],
    pub velocity: [f32; 2],
    pub health: f32,
    pub score: i32,
    pub on_ground: bool,
}

impl Default for PlayerObservation {
    fn default() -> Self {
        Self {
            stable_id: None,
            position: [0.0, 0.0, 0.0],
            velocity: [0.0, 0.0],
            health: 1.0,
            score: 0,
            on_ground: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct EntityObservation {
    pub stable_id: Option<StableEntityId>,
    pub kind: String,
    pub position: [f32; 3],
    pub extra: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ItemObservation {
    pub id: String,
    pub quantity: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ObjectiveObservation {
    pub id: String,
    pub complete: bool,
    pub progress: f32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct SymbolicObservation {
    pub tick: u64,
    pub player: PlayerObservation,
    pub visible_entities: Vec<EntityObservation>,
    pub inventory: Vec<ItemObservation>,
    pub objectives: Vec<ObjectiveObservation>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StateDelta {
    pub tick: u64,
    pub changes: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PixelObservation {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind")]
pub enum Observation {
    Symbolic(SymbolicObservation),
    FullState(serde_json::Value),
    Delta(StateDelta),
    Pixels(PixelObservation),
    Hybrid {
        symbolic: SymbolicObservation,
        pixels: Option<PixelObservation>,
        debug: Option<serde_json::Value>,
    },
    /// A game-defined, domain-shaped observation.
    Domain {
        tick: u64,
        value: serde_json::Value,
    },
    Error {
        message: String,
    },
}

impl Observation {
    #[must_use]
    pub fn default_for_tick(tick: u64) -> Self {
        let symbolic = SymbolicObservation {
            tick,
            ..Default::default()
        };
        Self::Hybrid {
            symbolic,
            pixels: None,
            debug: None,
        }
    }
}

#[derive(Resource, Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct AgentObservationCatalog {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnvironmentChecksum {
    pub tick: u64,
    pub hash: u64,
}

/// Backwards-compatible name for the checksum of live gameplay state.
pub type StateChecksum = EnvironmentChecksum;

/// Checksum of the serialized, registered snapshot representation.
///
/// This is deliberately a distinct type from [`EnvironmentChecksum`]: games
/// may use a custom live-state checksum that covers a different state surface.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotChecksum {
    pub tick: u64,
    pub hash: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StepInfo {
    pub frame: u64,
    pub timeline_id: TimelineId,
    pub branch_id: BranchId,
    pub actions_applied: usize,
    pub snapshot_created: Option<SnapshotId>,
    pub episode_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StepResponse<O = Observation> {
    pub tick: u64,
    pub observation: O,
    pub reward: f32,
    pub done: bool,
    pub truncated: bool,
    pub info: StepInfo,
    pub checksum: Option<StateChecksum>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ResetResponse<O = Observation> {
    pub tick: u64,
    pub observation: O,
    pub checksum: Option<EnvironmentChecksum>,
    pub snapshot_id: Option<SnapshotId>,
    pub timeline_id: TimelineId,
    pub branch_id: BranchId,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StepManyResponse<O = Observation> {
    pub start_tick: u64,
    pub end_tick: u64,
    pub steps: usize,
    pub observation: Option<O>,
    pub reward: f32,
    pub done: bool,
    pub truncated: bool,
    pub info: Option<StepInfo>,
    pub checksum: Option<EnvironmentChecksum>,
    pub responses: Vec<StepResponse<O>>,
}

#[derive(Resource, Clone, Debug, Serialize, Deserialize)]
pub struct LastStepResponse<O = Observation>(pub Option<StepResponse<O>>);

impl<O> Default for LastStepResponse<O> {
    fn default() -> Self {
        Self(None)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum ControlMode {
    Human,
    #[default]
    Agent,
    Hybrid,
    Replay,
    Paused,
    InspectOnly,
}

impl ControlMode {
    /// Source/mode enforcement matrix for input resolution (`drain`).
    ///
    /// | mode        | Agent | Human | Replay | Script | Network | Test |
    /// |-------------|-------|-------|--------|--------|---------|------|
    /// | Agent       | yes   | no    | no     | yes    | no      | yes  |
    /// | Human       | no    | yes   | no     | no     | no      | yes  |
    /// | Hybrid      | yes   | yes   | no     | yes    | yes     | yes  |
    /// | Replay      | no    | no    | yes    | no     | no      | no   |
    /// | Paused      | no    | no    | no     | no     | no      | no   |
    /// | InspectOnly | no    | no    | no     | no     | no      | no   |
    ///
    /// `Paused`/`InspectOnly` never accept steps (the runner rejects them
    /// before enqueueing). `Script`/`Test` are trusted automation sources.
    #[must_use]
    pub const fn accepts_source(&self, source: &ActionSource) -> bool {
        match self {
            Self::Agent => matches!(
                source,
                ActionSource::Agent | ActionSource::Script | ActionSource::Test
            ),
            Self::Human => matches!(source, ActionSource::Human | ActionSource::Test),
            Self::Hybrid => !matches!(source, ActionSource::Replay),
            Self::Replay => matches!(source, ActionSource::Replay),
            Self::Paused | Self::InspectOnly => false,
        }
    }

    /// Returns `true` when external stepping is allowed at all.
    #[must_use]
    pub const fn allows_stepping(&self) -> bool {
        !matches!(self, Self::Paused | Self::InspectOnly)
    }

    /// Filters a drained input frame to the sources this mode accepts.
    /// Returns the rejected count so callers can log/metrics it.
    #[must_use]
    pub fn filter_sources(
        &self,
        actions: &mut Vec<AgentAction>,
        sources: &mut Vec<ActionSource>,
    ) -> usize {
        let mut rejected = 0;
        let mut kept_actions = Vec::with_capacity(actions.len());
        let mut kept_sources = Vec::with_capacity(sources.len());
        for (action, source) in actions.drain(..).zip(sources.drain(..)) {
            if self.accepts_source(&source) {
                kept_actions.push(action);
                kept_sources.push(source);
            } else {
                rejected += 1;
            }
        }
        *actions = kept_actions;
        *sources = kept_sources;
        rejected
    }
}

#[derive(Resource, Clone, Debug, Serialize, Deserialize)]
pub struct AgentControlState {
    pub mode: ControlMode,
    pub frame: u64,
    pub timeline_id: TimelineId,
    pub branch_id: BranchId,
    pub last_action_count: usize,
    pub last_snapshot_created: Option<SnapshotId>,
}

impl Default for AgentControlState {
    fn default() -> Self {
        Self {
            mode: ControlMode::Agent,
            frame: 0,
            timeline_id: TimelineId::new(),
            branch_id: BranchId::new(),
            last_action_count: 0,
            last_snapshot_created: None,
        }
    }
}

#[derive(Resource, Clone, Debug, Default, Serialize, Deserialize)]
pub struct RewardState {
    pub current_reward: f32,
    pub cumulative_reward: f32,
}

#[derive(Resource, Clone, Debug, Default, Serialize, Deserialize)]
pub struct EpisodeState {
    pub done: bool,
    pub truncated: bool,
    pub reason: Option<String>,
}

impl EpisodeState {
    /// Terminal episodes use absorbing semantics: once `done` or `truncated`
    /// is set, the simulation must not advance gameplay or accumulate reward
    /// until a reset. Stepping past terminal without a reset is rejected with
    /// [`AgentControlError::TerminalStepRejected`].
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        self.done || self.truncated
    }

    /// Returns an error when a step is attempted past terminal state.
    pub fn ensure_not_terminal(&self) -> ControlResult<()> {
        if self.is_terminal() {
            Err(AgentControlError::TerminalStepRejected {
                reason: self.reason.clone().unwrap_or_else(|| "unknown".to_string()),
            })
        } else {
            Ok(())
        }
    }

    /// Returns `false` once terminal so games can stop reward accumulation
    /// post-terminal (no-op ticks must not farm shaping rewards).
    #[must_use]
    pub const fn should_accumulate_reward(&self) -> bool {
        !self.is_terminal()
    }
}

type ObservationExtractorFn = dyn Fn(&mut World, ObservationMode) -> Observation + Send + Sync;
type ChecksumExtractorFn = dyn Fn(&mut World) -> StateChecksum + Send + Sync;

pub struct AgentObservationExtractor {
    extract: Box<ObservationExtractorFn>,
}

impl AgentObservationExtractor {
    pub fn new<F>(extract: F) -> Self
    where
        F: Fn(&mut World, ObservationMode) -> Observation + Send + Sync + 'static,
    {
        Self {
            extract: Box::new(extract),
        }
    }

    pub fn extract(&self, world: &mut World, mode: ObservationMode) -> Observation {
        (self.extract)(world, mode)
    }
}

impl Resource for AgentObservationExtractor {}

pub struct AgentChecksumExtractor {
    extract: Box<ChecksumExtractorFn>,
}

impl AgentChecksumExtractor {
    pub fn new<F>(extract: F) -> Self
    where
        F: Fn(&mut World) -> StateChecksum + Send + Sync + 'static,
    {
        Self {
            extract: Box::new(extract),
        }
    }

    pub fn extract(&self, world: &mut World) -> StateChecksum {
        (self.extract)(world)
    }
}

impl Resource for AgentChecksumExtractor {}

pub trait AgentControlAppExt {
    fn insert_observation_extractor<F>(&mut self, extract: F) -> &mut Self
    where
        F: Fn(&mut World, ObservationMode) -> Observation + Send + Sync + 'static;

    fn insert_checksum_extractor<F>(&mut self, extract: F) -> &mut Self
    where
        F: Fn(&mut World) -> StateChecksum + Send + Sync + 'static;

    fn register_custom_action_schema(
        &mut self,
        name: impl Into<String>,
        schema: serde_json::Value,
    ) -> &mut Self;

    fn set_environment_metadata(
        &mut self,
        name: impl Into<String>,
        version: impl Into<String>,
        description: Option<String>,
    ) -> &mut Self;

    fn set_supported_actions(
        &mut self,
        actions: impl IntoIterator<Item = AgentActionKind>,
    ) -> &mut Self;

    fn set_observation_schema(&mut self, schema: serde_json::Value) -> &mut Self;
}

impl AgentControlAppExt for App {
    fn insert_observation_extractor<F>(&mut self, extract: F) -> &mut Self
    where
        F: Fn(&mut World, ObservationMode) -> Observation + Send + Sync + 'static,
    {
        self.insert_resource(AgentObservationExtractor::new(extract))
    }

    fn insert_checksum_extractor<F>(&mut self, extract: F) -> &mut Self
    where
        F: Fn(&mut World) -> StateChecksum + Send + Sync + 'static,
    {
        self.insert_resource(AgentChecksumExtractor::new(extract))
    }

    fn register_custom_action_schema(
        &mut self,
        name: impl Into<String>,
        schema: serde_json::Value,
    ) -> &mut Self {
        if !self.world().contains_resource::<AgentActionCatalog>() {
            self.init_resource::<AgentActionCatalog>();
        }
        self.world_mut()
            .resource_mut::<AgentActionCatalog>()
            .register_custom_action_schema(name, schema);
        self
    }

    fn set_environment_metadata(
        &mut self,
        name: impl Into<String>,
        version: impl Into<String>,
        description: Option<String>,
    ) -> &mut Self {
        self.insert_resource(EnvironmentMetadata {
            name: name.into(),
            version: version.into(),
            description,
        })
    }

    fn set_supported_actions(
        &mut self,
        actions: impl IntoIterator<Item = AgentActionKind>,
    ) -> &mut Self {
        if !self.world().contains_resource::<AgentActionCatalog>() {
            self.init_resource::<AgentActionCatalog>();
        }
        self.world_mut()
            .resource_mut::<AgentActionCatalog>()
            .set_supported_actions(actions);
        self
    }

    fn set_observation_schema(&mut self, schema: serde_json::Value) -> &mut Self {
        self.insert_resource(AgentObservationCatalog {
            schema: Some(schema),
        })
    }
}

/// Runs exactly one agent-controlled simulation tick, including the policy
/// decision and pre/post hooks.
pub fn run_agent_tick(world: &mut World) {
    world.run_schedule(AgentDecision);
    world.run_schedule(AgentPreTick);
    world.run_schedule(AgentTick);
    world.run_schedule(AgentPostTick);
}

#[derive(Clone, Debug)]
pub enum AgentPluginMode {
    Deterministic,
    VisualDebug,
    Remote,
}

/// Marker recording which [`AgentControlPlugin`] preset built the app.
///
/// `Deterministic` disables visual capture and pins a fixed RNG seed;
/// `VisualDebug` enables the visual-capture path (renderers still installed
/// by the game/runner); `Remote` matches deterministic without visuals.
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)]
pub struct PresetVisualCapture(pub bool);

#[derive(Clone, Debug)]
pub struct AgentControlPlugin {
    pub mode: AgentPluginMode,
    pub tick_hz: u32,
}

impl AgentControlPlugin {
    #[must_use]
    pub fn deterministic() -> Self {
        Self {
            mode: AgentPluginMode::Deterministic,
            tick_hz: 60,
        }
    }

    #[must_use]
    pub fn visual_debug() -> Self {
        Self {
            mode: AgentPluginMode::VisualDebug,
            tick_hz: 60,
        }
    }

    #[must_use]
    pub fn remote() -> Self {
        Self {
            mode: AgentPluginMode::Remote,
            tick_hz: 60,
        }
    }
}

impl Plugin for AgentControlPlugin {
    /// Builds the core schedules/resources, inspecting [`AgentPluginMode`]:
    /// `Deterministic` pins `DeterministicRng::seeded(0)` and disables visual
    /// capture; `VisualDebug` keeps the fixed RNG but enables the
    /// visual-capture path; `Remote` matches deterministic. Presets are
    /// otherwise equivalent (same schedules, tick rate, and resources).
    fn build(&self, app: &mut App) {
        let visual_enabled = matches!(self.mode, AgentPluginMode::VisualDebug);
        app.init_schedule(AgentReset)
            .init_schedule(AgentDecision)
            .init_schedule(AgentPreTick)
            .init_schedule(AgentTick)
            .init_schedule(AgentPostTick)
            .insert_resource(SimClock::new(self.tick_hz))
            .init_resource::<StableIdAllocator>()
            .insert_resource(DeterministicRng::seeded(0))
            .insert_resource(PresetVisualCapture(visual_enabled))
            .init_resource::<AgentControlState>()
            .init_resource::<AgentActionQueue>()
            .init_resource::<CurrentInputFrame>()
            .init_resource::<AgentActionCatalog>()
            .init_resource::<AgentObservationCatalog>()
            .init_resource::<EnvironmentMetadata>()
            .init_resource::<ObservationConfig>()
            .init_resource::<RewardState>()
            .init_resource::<EpisodeState>()
            .init_resource::<LastStepResponse>()
            .init_resource::<ExecutionContext>()
            .configure_sets(
                AgentReset,
                (
                    AgentResetSet::Core,
                    AgentResetSet::Game,
                    AgentResetSet::Observation,
                )
                    .chain(),
            )
            .configure_sets(
                AgentTick,
                (
                    AgentSet::BeginTick,
                    AgentSet::DrainActions,
                    AgentSet::ApplyInput,
                    AgentSet::Simulation,
                    AgentSet::TerminalCheck,
                    AgentSet::Observation,
                    AgentSet::ReplayRecord,
                    AgentSet::Snapshot,
                    AgentSet::EndTick,
                )
                    .chain(),
            )
            .add_systems(AgentReset, reset_agent_core.in_set(AgentResetSet::Core))
            .add_systems(
                AgentReset,
                collect_observation.in_set(AgentResetSet::Observation),
            )
            .add_systems(
                AgentTick,
                (
                    begin_tick.in_set(AgentSet::BeginTick),
                    drain_agent_actions.in_set(AgentSet::DrainActions),
                    collect_observation.in_set(AgentSet::Observation),
                    end_tick.in_set(AgentSet::EndTick),
                ),
            );
    }
}

#[allow(clippy::too_many_arguments)]
pub fn reset_agent_core(
    mut clock: ResMut<SimClock>,
    mut ids: ResMut<StableIdAllocator>,
    mut queue: ResMut<AgentActionQueue>,
    mut input: ResMut<CurrentInputFrame>,
    mut reward: ResMut<RewardState>,
    mut episode: ResMut<EpisodeState>,
    mut control: ResMut<AgentControlState>,
    mut last: ResMut<LastStepResponse>,
    mut rng: ResMut<DeterministicRng>,
) {
    let dt_seconds = clock.dt_seconds;
    *clock = SimClock {
        tick: 0,
        dt_seconds,
        elapsed_seconds: 0.0,
    };
    *ids = StableIdAllocator::default();
    queue.clear();
    input.tick = 0;
    input.actions.clear();
    input.sources.clear();
    *reward = RewardState::default();
    *episode = EpisodeState::default();
    control.frame = 0;
    control.last_action_count = 0;
    control.last_snapshot_created = None;
    *last = LastStepResponse(None);
    let seed = rng.seed;
    *rng = DeterministicRng::seeded(seed);
}

pub fn begin_tick(mut clock: ResMut<SimClock>, mut control: ResMut<AgentControlState>) {
    clock.advance_one_tick();
    control.frame += 1;
    control.last_snapshot_created = None;
}

pub fn drain_agent_actions(
    clock: Res<SimClock>,
    mut queue: ResMut<AgentActionQueue>,
    mut input: ResMut<CurrentInputFrame>,
    mut control: ResMut<AgentControlState>,
    context: Option<Res<ExecutionContext>>,
) {
    input.tick = clock.tick;
    input.actions.clear();
    input.sources.clear();

    let mode = control.mode.clone();
    // Reconstruction bypass is gated on the `ExecutionContext` resource
    // (installed by the runner during history replay), never on queue
    // contents: no caller-forgeable marker can escalate privilege. Stale
    // legacy sentinels are dropped as metadata, never treated as input.
    let reconstructing = matches!(context.as_deref(), Some(ExecutionContext::Reconstructing));
    let mut remaining = VecDeque::new();
    while let Some(next) = queue.pending.pop_front() {
        // Drop legacy Reconstructing sentinels; they are metadata, not input.
        if is_legacy_reconstructing_sentinel(&next.action) {
            continue;
        }
        if next.tick == clock.tick {
            // Enforce the source/mode matrix at input resolution: actions
            // from rejected sources are dropped (not applied, not counted).
            // During Reconstructing all recorded sources are accepted.
            if reconstructing || mode.accepts_source(&next.source) {
                input.sources.push(next.source);
                input.actions.push(next.action);
            }
        } else if next.tick > clock.tick {
            remaining.push_back(next);
        }
    }

    control.last_action_count = input.actions.len();
    queue.pending = remaining;
}

pub fn collect_observation(world: &mut World) {
    let mode = world.resource::<ObservationConfig>().mode.clone();
    collect_observation_with_mode(world, mode);
}

/// Request-local observation collection: renders with the explicitly passed
/// `mode` instead of mutating the global [`ObservationConfig`].
///
/// Remote/step paths should prefer this so a per-request
/// `observation_mode` does not leak into subsequent ticks. At minimum,
/// callers that must go through the global resource should snapshot
/// `ObservationConfig.mode` before an implicit reset and reapply it after.
pub fn collect_observation_with_mode(world: &mut World, mode: ObservationMode) {
    let clock = world.resource::<SimClock>().clone();
    let observation = if world.contains_resource::<AgentObservationExtractor>() {
        world.resource_scope(|world, extractor: Mut<AgentObservationExtractor>| {
            extractor.extract(world, mode)
        })
    } else {
        Observation::default_for_tick(clock.tick)
    };

    let checksum = if world.contains_resource::<AgentChecksumExtractor>() {
        Some(
            world.resource_scope(|world, extractor: Mut<AgentChecksumExtractor>| {
                extractor.extract(world)
            }),
        )
    } else {
        Some(default_checksum(world))
    };

    let reward = world.resource::<RewardState>().clone();
    let episode = world.resource::<EpisodeState>().clone();
    let control = world.resource::<AgentControlState>().clone();

    let response = StepResponse {
        tick: clock.tick,
        observation,
        reward: reward.current_reward,
        done: episode.done,
        truncated: episode.truncated,
        info: StepInfo {
            frame: control.frame,
            timeline_id: control.timeline_id,
            branch_id: control.branch_id,
            actions_applied: control.last_action_count,
            snapshot_created: control.last_snapshot_created,
            episode_reason: episode.reason,
        },
        checksum,
    };

    world.resource_mut::<LastStepResponse>().0 = Some(response);
}

pub fn end_tick() {}

/// Version for the canonical checksum serialization.
///
/// Bump this when the set of hashed fields or their encoding changes so
/// mismatched producers/consumers fail loudly instead of colliding silently.
pub const CHECKSUM_VERSION: u32 = 1;

pub fn default_checksum(world: &World) -> StateChecksum {
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
        hasher.write_u64(queue.pending.len() as u64);
        for scheduled in &queue.pending {
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

    StateChecksum {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn app_with_core() -> App {
        let mut app = App::new();
        app.add_plugins(AgentControlPlugin::deterministic());
        app.finish();
        app.cleanup();
        app
    }

    #[test]
    fn stable_id_allocator_allocates_monotonic_ids() {
        let mut allocator = StableIdAllocator::default();

        assert_eq!(allocator.allocate(), StableEntityId(1));
        assert_eq!(allocator.allocate(), StableEntityId(2));
        assert_eq!(allocator.next, 3);
    }

    #[test]
    fn sim_clock_advance_preserves_fixed_dt() {
        let mut clock = SimClock::new(20);

        clock.advance_one_tick();
        clock.advance_one_tick();

        assert_eq!(clock.tick, 2);
        assert_eq!(clock.dt_seconds, 0.05);
        assert!((clock.elapsed_seconds - 0.1).abs() < 1.0e-6);
    }

    #[test]
    fn action_queue_schedules_and_clears_actions() {
        let mut queue = AgentActionQueue::default();

        queue.schedule(7, ActionSource::Test, AgentAction::Jump);
        assert_eq!(queue.pending.len(), 1);

        queue.clear();
        assert!(queue.pending.is_empty());
    }

    #[test]
    fn custom_action_schemas_register_in_catalog() {
        let mut app = App::new();
        app.register_custom_action_schema(
            "Input",
            serde_json::json!({
                "type": "object",
                "required": ["type"],
                "properties": { "type": { "const": "Input" } }
            }),
        );

        let catalog = app.world().resource::<AgentActionCatalog>();
        assert!(catalog.custom_actions.contains_key("Input"));
        assert_eq!(
            catalog.custom_actions["Input"].schema["properties"]["type"]["const"],
            "Input"
        );
    }

    #[test]
    fn stable_json_hash_sorts_object_keys() {
        let a = serde_json::json!({ "b": 2, "a": [true, null] });
        let b = serde_json::json!({ "a": [true, null], "b": 2 });
        let c = serde_json::json!({ "a": [true, null], "b": 3 });

        assert_eq!(stable_hash_json(&a), stable_hash_json(&b));
        assert_ne!(stable_hash_json(&a), stable_hash_json(&c));
    }

    #[test]
    fn core_tick_drains_only_current_actions_and_preserves_future_actions() {
        let mut app = app_with_core();
        app.world_mut().resource_mut::<AgentActionQueue>().schedule(
            1,
            ActionSource::Agent,
            AgentAction::Jump,
        );
        app.world_mut().resource_mut::<AgentActionQueue>().schedule(
            3,
            ActionSource::Script,
            AgentAction::Interact,
        );

        app.world_mut().run_schedule(AgentTick);

        let input = app.world().resource::<CurrentInputFrame>();
        assert_eq!(input.tick, 1);
        assert_eq!(input.actions, vec![AgentAction::Jump]);
        assert_eq!(input.sources, vec![ActionSource::Agent]);
        assert_eq!(app.world().resource::<AgentActionQueue>().pending.len(), 1);
        assert_eq!(
            app.world()
                .resource::<AgentControlState>()
                .last_action_count,
            1
        );
    }

    #[test]
    fn reset_core_clears_episode_reward_input_and_preserves_tick_dt_and_rng_seed() {
        let mut app = app_with_core();
        {
            let world = app.world_mut();
            world.resource_mut::<SimClock>().tick = 99;
            world.resource_mut::<SimClock>().dt_seconds = 0.25;
            world.resource_mut::<RewardState>().current_reward = 10.0;
            world.resource_mut::<EpisodeState>().done = true;
            world
                .resource_mut::<CurrentInputFrame>()
                .actions
                .push(AgentAction::Jump);
            world.resource_mut::<DeterministicRng>().seed = 123;
            world.resource_mut::<AgentActionQueue>().schedule(
                100,
                ActionSource::Agent,
                AgentAction::Noop,
            );
        }

        app.world_mut().run_schedule(AgentReset);

        assert_eq!(app.world().resource::<SimClock>().tick, 0);
        assert_eq!(app.world().resource::<SimClock>().dt_seconds, 0.25);
        assert_eq!(app.world().resource::<RewardState>().current_reward, 0.0);
        assert!(!app.world().resource::<EpisodeState>().done);
        assert!(
            app.world()
                .resource::<CurrentInputFrame>()
                .actions
                .is_empty()
        );
        assert!(
            app.world()
                .resource::<AgentActionQueue>()
                .pending
                .is_empty()
        );
        assert_eq!(app.world().resource::<DeterministicRng>().seed, 123);
    }

    #[test]
    fn observation_and_checksum_extractors_override_defaults() {
        let mut app = App::new();
        app.add_plugins(AgentControlPlugin::deterministic())
            .insert_observation_extractor(|world, mode| {
                assert!(matches!(mode, ObservationMode::FullDebugState));
                Observation::FullState(serde_json::json!({
                    "tick": world.resource::<SimClock>().tick
                }))
            })
            .insert_checksum_extractor(|world| StateChecksum {
                tick: world.resource::<SimClock>().tick,
                hash: 42,
            });
        app.finish();
        app.cleanup();
        app.world_mut().resource_mut::<ObservationConfig>().mode = ObservationMode::FullDebugState;

        app.world_mut().run_schedule(AgentTick);

        let response = app
            .world()
            .resource::<LastStepResponse>()
            .0
            .as_ref()
            .unwrap();
        assert_eq!(
            response.observation,
            Observation::FullState(serde_json::json!({ "tick": 1 }))
        );
        assert_eq!(response.checksum, Some(StateChecksum { tick: 1, hash: 42 }));
    }

    #[test]
    fn default_checksum_detects_mutation_of_each_authoritative_field() {
        use rand::RngCore;

        fn seeded_app() -> App {
            let mut app = app_with_core();
            app.world_mut().resource_mut::<SimClock>().tick = 7;
            app.world_mut().resource_mut::<SimClock>().dt_seconds = 0.05;
            app.world_mut().resource_mut::<SimClock>().elapsed_seconds = 0.35;
            app.world_mut().resource_mut::<RewardState>().current_reward = 1.5;
            app.world_mut()
                .resource_mut::<RewardState>()
                .cumulative_reward = 4.25;
            app.world_mut()
                .resource_mut::<CurrentInputFrame>()
                .actions
                .push(AgentAction::Jump);
            app.world_mut()
                .resource_mut::<CurrentInputFrame>()
                .sources
                .push(ActionSource::Agent);
            app.world_mut().resource_mut::<AgentActionQueue>().schedule(
                9,
                ActionSource::Script,
                AgentAction::Interact,
            );
            app
        }

        let base = default_checksum(seeded_app().world());

        let mut mutated = seeded_app();
        mutated.world_mut().resource_mut::<SimClock>().tick = 8;
        assert_ne!(
            default_checksum(mutated.world()),
            base,
            "tick mutation undetected"
        );

        let mut mutated = seeded_app();
        mutated
            .world_mut()
            .resource_mut::<RewardState>()
            .current_reward = 99.0;
        assert_ne!(
            default_checksum(mutated.world()),
            base,
            "reward mutation undetected"
        );

        let mut mutated = seeded_app();
        mutated.world_mut().resource_mut::<EpisodeState>().done = true;
        assert_ne!(
            default_checksum(mutated.world()),
            base,
            "episode.done mutation undetected"
        );

        let mut mutated = seeded_app();
        mutated
            .world_mut()
            .resource_mut::<CurrentInputFrame>()
            .actions
            .push(AgentAction::Crouch);
        assert_ne!(
            default_checksum(mutated.world()),
            base,
            "input action mutation undetected"
        );

        let mut mutated = seeded_app();
        mutated
            .world_mut()
            .resource_mut::<AgentActionQueue>()
            .schedule(10, ActionSource::Agent, AgentAction::Dodge);
        assert_ne!(
            default_checksum(mutated.world()),
            base,
            "queued input mutation undetected"
        );

        let mut mutated = seeded_app();
        mutated
            .world_mut()
            .resource_mut::<DeterministicRng>()
            .rng
            .next_u32();
        assert_ne!(
            default_checksum(mutated.world()),
            base,
            "rng state mutation undetected"
        );

        // Action contents (not just queue length) must matter.
        let mut mutated = seeded_app();
        if let Some(first) = mutated
            .world_mut()
            .resource_mut::<AgentActionQueue>()
            .pending
            .front_mut()
        {
            first.action = AgentAction::Dodge;
        }
        assert_ne!(
            default_checksum(mutated.world()),
            base,
            "queued action contents mutation undetected"
        );
    }

    #[test]
    fn agent_decision_runs_once_before_each_simulation_tick() {
        #[derive(Resource, Default)]
        struct PolicyCalls(u64);

        fn policy(
            mut calls: ResMut<PolicyCalls>,
            clock: Res<SimClock>,
            mut queue: ResMut<AgentActionQueue>,
        ) {
            calls.0 += 1;
            queue.schedule(clock.tick + 1, ActionSource::Agent, AgentAction::Noop);
        }

        let mut app = App::new();
        app.add_plugins(AgentControlPlugin::deterministic())
            .init_resource::<PolicyCalls>()
            .add_systems(AgentDecision, policy);
        app.finish();
        app.cleanup();

        run_agent_tick(app.world_mut());
        run_agent_tick(app.world_mut());

        assert_eq!(app.world().resource::<PolicyCalls>().0, 2);
        assert_eq!(app.world().resource::<SimClock>().tick, 2);
        assert_eq!(
            app.world()
                .resource::<AgentControlState>()
                .last_action_count,
            1
        );
    }

    #[test]
    fn sim_clock_rejects_zero_tick_rate() {
        assert!(SimClock::try_new(0).is_err());
        assert!(SimClock::try_new(60).is_ok());
    }

    #[test]
    #[should_panic(expected = "tick rate must be > 0")]
    fn sim_clock_new_panics_on_zero() {
        let _ = SimClock::new(0);
    }

    #[test]
    fn sim_clock_validates_imported_values() {
        let valid = SimClock::new(60);
        assert!(valid.validate().is_ok());

        let mut bad_dt = valid.clone();
        bad_dt.dt_seconds = f32::INFINITY;
        assert!(bad_dt.validate().is_err());

        let mut bad_tick = valid.clone();
        bad_tick.tick = u64::MAX;
        assert!(bad_tick.validate().is_err());
        assert!(validate_clock_tick(u64::MAX).is_err());
        assert!(bad_tick.set_tick_checked(5).is_ok());
        assert_eq!(bad_tick.tick, 5);
    }

    #[test]
    fn action_validation_rejects_unsupported_and_out_of_bounds() {
        let mut catalog = AgentActionCatalog::default();
        catalog.set_supported_actions([AgentActionKind::Move, AgentActionKind::Jump]);
        assert!(
            catalog
                .validate_action(&AgentAction::Move { x: 1.5, y: 0.0 })
                .is_err()
        );
        assert!(
            catalog
                .validate_action(&AgentAction::Move {
                    x: f32::NAN,
                    y: 0.0
                })
                .is_err()
        );
        assert!(catalog.validate_action(&AgentAction::Dodge).is_err());
        assert!(
            catalog
                .validate_action(&AgentAction::Move { x: 0.5, y: 0.0 })
                .is_ok()
        );
    }

    #[test]
    fn observation_mode_advertises_implemented_subset() {
        assert!(ObservationMode::Hybrid.is_implemented());
        assert!(ObservationMode::PlayerKnowledge.is_implemented());
        assert!(!ObservationMode::PixelFrame.is_implemented());
        assert!(!ObservationMode::DiffSinceLastTick.is_implemented());
        assert!(ObservationMode::Hybrid.validate_supported().is_ok());
        assert!(ObservationMode::PixelFrame.validate_supported().is_err());
    }

    #[test]
    fn control_mode_enforces_source_matrix() {
        assert!(ControlMode::Agent.accepts_source(&ActionSource::Agent));
        assert!(!ControlMode::Agent.accepts_source(&ActionSource::Human));
        assert!(ControlMode::Human.accepts_source(&ActionSource::Human));
        assert!(!ControlMode::Human.accepts_source(&ActionSource::Agent));
        assert!(ControlMode::Hybrid.accepts_source(&ActionSource::Human));
        assert!(ControlMode::Hybrid.accepts_source(&ActionSource::Agent));
        assert!(!ControlMode::Hybrid.accepts_source(&ActionSource::Replay));
        assert!(ControlMode::Replay.accepts_source(&ActionSource::Replay));
        assert!(!ControlMode::Paused.allows_stepping());
        assert!(!ControlMode::InspectOnly.allows_stepping());
    }

    #[test]
    fn terminal_state_is_absorbing() {
        let live = EpisodeState::default();
        assert!(!live.is_terminal());
        assert!(live.should_accumulate_reward());
        let done = EpisodeState {
            done: true,
            truncated: false,
            reason: Some("goal_reached".to_string()),
        };
        assert!(done.is_terminal());
        assert!(!done.should_accumulate_reward());
        assert!(done.ensure_not_terminal().is_err());
    }

    #[test]
    fn plugin_presets_differ_on_visual_capture() {
        let mut det = App::new();
        det.add_plugins(AgentControlPlugin::deterministic());
        det.finish();
        det.cleanup();
        let mut vis = App::new();
        vis.add_plugins(AgentControlPlugin::visual_debug());
        vis.finish();
        vis.cleanup();
        assert_eq!(
            det.world().resource::<PresetVisualCapture>(),
            &PresetVisualCapture(false)
        );
        assert_eq!(
            vis.world().resource::<PresetVisualCapture>(),
            &PresetVisualCapture(true)
        );
    }

    #[test]
    fn drain_enforces_control_mode_matrix() {
        let mut app = app_with_core();
        app.world_mut().resource_mut::<AgentControlState>().mode = ControlMode::Human;
        app.world_mut().resource_mut::<AgentActionQueue>().schedule(
            1,
            ActionSource::Agent,
            AgentAction::Jump,
        );
        app.world_mut().run_schedule(AgentTick);
        // Agent-sourced action is dropped in Human mode.
        assert!(
            app.world()
                .resource::<CurrentInputFrame>()
                .actions
                .is_empty()
        );
    }
}
