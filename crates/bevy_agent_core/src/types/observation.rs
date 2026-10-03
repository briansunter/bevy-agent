use crate::*;

#[derive(
    Clone,
    Debug,
    Default,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
)]
pub enum ObservationMode {
    PlayerKnowledge,
    FullDebugState,
    DiffSinceLastTick,
    PixelFrame,
    #[default]
    Hybrid,
}

#[derive(Resource, Clone, Debug, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ObservationConfig {
    pub mode: ObservationMode,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema, PartialEq)]
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

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema, PartialEq)]
pub struct EntityObservation {
    pub stable_id: Option<StableEntityId>,
    pub kind: String,
    pub position: [f32; 3],
    pub extra: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema, PartialEq)]
pub struct ItemObservation {
    pub id: String,
    pub quantity: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema, PartialEq)]
pub struct ObjectiveObservation {
    pub id: String,
    pub complete: bool,
    pub progress: f32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, schemars::JsonSchema, PartialEq)]
pub struct SymbolicObservation {
    pub tick: u64,
    pub player: PlayerObservation,
    pub visible_entities: Vec<EntityObservation>,
    pub inventory: Vec<ItemObservation>,
    pub objectives: Vec<ObjectiveObservation>,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema, PartialEq)]
pub struct StateDelta {
    pub tick: u64,
    pub changes: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema, PartialEq)]
pub struct PixelObservation {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema, PartialEq)]
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

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
pub struct EnvironmentChecksum {
    pub tick: u64,
    pub hash: u64,
}

/// Checksum of the serialized, registered snapshot representation.
///
/// This is deliberately a distinct type from [`EnvironmentChecksum`]: games
/// may use a custom live-state checksum that covers a different state surface.
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
pub struct SnapshotChecksum {
    pub tick: u64,
    pub hash: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema, PartialEq)]
pub struct StepInfo {
    pub frame: u64,
    pub timeline_id: TimelineId,
    pub branch_id: BranchId,
    pub actions_applied: usize,
    pub snapshot_created: Option<SnapshotId>,
    pub episode_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema, PartialEq)]
pub struct StepResponse<O = Observation> {
    pub tick: u64,
    pub observation: O,
    pub reward: f32,
    pub done: bool,
    pub truncated: bool,
    pub info: StepInfo,
    pub checksum: Option<EnvironmentChecksum>,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema, PartialEq)]
pub struct ResetResponse<O = Observation> {
    pub tick: u64,
    pub observation: O,
    pub checksum: Option<EnvironmentChecksum>,
    pub snapshot_id: Option<SnapshotId>,
    pub timeline_id: TimelineId,
    pub branch_id: BranchId,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema, PartialEq)]
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

#[derive(Resource, Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LastStepResponse<O = Observation>(pub Option<StepResponse<O>>);

impl<O> Default for LastStepResponse<O> {
    fn default() -> Self {
        Self(None)
    }
}
