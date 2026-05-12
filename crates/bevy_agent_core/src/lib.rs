use std::collections::VecDeque;
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
    #[error("agent control error: {0}")]
    Message(String),
}

pub type ControlResult<T> = Result<T, AgentControlError>;

#[derive(ScheduleLabel, Clone, Debug, PartialEq, Eq, Hash)]
pub struct AgentReset;

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

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct SnapshotId(pub Uuid);

impl SnapshotId {
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
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for BranchId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Resource, Clone, Debug, Serialize, Deserialize)]
pub struct SimClock {
    pub tick: u64,
    pub dt_seconds: f32,
    pub elapsed_seconds: f64,
}

impl SimClock {
    pub fn new(tick_hz: u32) -> Self {
        Self {
            tick: 0,
            dt_seconds: 1.0 / tick_hz as f32,
            elapsed_seconds: 0.0,
        }
    }

    pub fn advance_one_tick(&mut self) {
        self.tick += 1;
        self.elapsed_seconds += self.dt_seconds as f64;
    }
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
    Error {
        message: String,
    },
}

impl Observation {
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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct StateChecksum {
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
}

#[derive(Clone, Debug)]
pub enum AgentPluginMode {
    Deterministic,
    VisualDebug,
    Remote,
}

#[derive(Clone, Debug)]
pub struct AgentControlPlugin {
    pub mode: AgentPluginMode,
    pub tick_hz: u32,
    pub snapshot_interval: Option<u64>,
}

impl AgentControlPlugin {
    pub fn deterministic() -> Self {
        Self {
            mode: AgentPluginMode::Deterministic,
            tick_hz: 60,
            snapshot_interval: Some(120),
        }
    }

    pub fn visual_debug() -> Self {
        Self {
            mode: AgentPluginMode::VisualDebug,
            tick_hz: 60,
            snapshot_interval: Some(120),
        }
    }

    pub fn remote() -> Self {
        Self {
            mode: AgentPluginMode::Remote,
            tick_hz: 60,
            snapshot_interval: Some(120),
        }
    }
}

impl Plugin for AgentControlPlugin {
    fn build(&self, app: &mut App) {
        app.init_schedule(AgentReset)
            .init_schedule(AgentPreTick)
            .init_schedule(AgentTick)
            .init_schedule(AgentPostTick)
            .insert_resource(SimClock::new(self.tick_hz))
            .init_resource::<StableIdAllocator>()
            .init_resource::<DeterministicRng>()
            .init_resource::<AgentControlState>()
            .init_resource::<AgentActionQueue>()
            .init_resource::<CurrentInputFrame>()
            .init_resource::<ObservationConfig>()
            .init_resource::<RewardState>()
            .init_resource::<EpisodeState>()
            .init_resource::<LastStepResponse>()
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
) {
    input.tick = clock.tick;
    input.actions.clear();
    input.sources.clear();

    let mut remaining = VecDeque::new();
    while let Some(next) = queue.pending.pop_front() {
        if next.tick == clock.tick {
            input.sources.push(next.source);
            input.actions.push(next.action);
        } else if next.tick > clock.tick {
            remaining.push_back(next);
        }
    }

    control.last_action_count = input.actions.len();
    queue.pending = remaining;
}

pub fn collect_observation(world: &mut World) {
    let clock = world.resource::<SimClock>().clone();
    let mode = world.resource::<ObservationConfig>().mode.clone();
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

pub fn default_checksum(world: &World) -> StateChecksum {
    let clock = world.resource::<SimClock>();
    let reward = world.resource::<RewardState>();
    let episode = world.resource::<EpisodeState>();
    let input = world.resource::<CurrentInputFrame>();

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    clock.tick.hash(&mut hasher);
    clock.dt_seconds.to_bits().hash(&mut hasher);
    clock.elapsed_seconds.to_bits().hash(&mut hasher);
    reward.current_reward.to_bits().hash(&mut hasher);
    reward.cumulative_reward.to_bits().hash(&mut hasher);
    episode.done.hash(&mut hasher);
    episode.truncated.hash(&mut hasher);
    episode.reason.hash(&mut hasher);
    input.tick.hash(&mut hasher);
    input.actions.len().hash(&mut hasher);

    StateChecksum {
        tick: clock.tick,
        hash: hasher.finish(),
    }
}
