use crate::*;

pub const LOOK_YAW_DELTA_LIMIT_RADIANS: f32 = std::f32::consts::PI;
pub const LOOK_PITCH_DELTA_LIMIT_RADIANS: f32 = std::f32::consts::FRAC_PI_2;

#[derive(Clone, Debug, Default, Serialize, Deserialize, schemars::JsonSchema, PartialEq)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum AgentAction {
    #[default]
    #[serde(deserialize_with = "deserialize_empty_action")]
    Noop,
    Move {
        #[serde(deserialize_with = "deserialize_move_axis")]
        #[schemars(range(min = -1.0, max = 1.0))]
        x: f32,
        #[serde(deserialize_with = "deserialize_move_axis")]
        #[schemars(range(min = -1.0, max = 1.0))]
        y: f32,
    },
    Look {
        #[serde(deserialize_with = "deserialize_yaw_delta")]
        #[schemars(range(min = -LOOK_YAW_DELTA_LIMIT_RADIANS, max = LOOK_YAW_DELTA_LIMIT_RADIANS))]
        yaw_delta: f32,
        #[serde(deserialize_with = "deserialize_pitch_delta")]
        #[schemars(range(min = -LOOK_PITCH_DELTA_LIMIT_RADIANS, max = LOOK_PITCH_DELTA_LIMIT_RADIANS))]
        pitch_delta: f32,
    },
    #[serde(deserialize_with = "deserialize_empty_action")]
    Jump,
    #[serde(deserialize_with = "deserialize_empty_action")]
    Crouch,
    #[serde(deserialize_with = "deserialize_empty_action")]
    Sprint,
    #[serde(deserialize_with = "deserialize_empty_action")]
    Interact,
    Attack {
        target: Option<StableEntityId>,
    },
    UseItem {
        slot: u8,
    },
    #[serde(deserialize_with = "deserialize_empty_action")]
    Dodge,
    Custom {
        value: serde_json::Value,
    },
}

// Serde's internally tagged unit visitor ignores additional fields even with
// deny_unknown_fields. Parse its remaining envelope as a strict empty object.
fn deserialize_empty_action<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<(), D::Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct EmptyAction {}
    EmptyAction::deserialize(deserializer).map(|_| ())
}

fn deserialize_bounded_f32<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
    limit: f32,
) -> Result<f32, D::Error> {
    let value = f64::deserialize(deserializer)?;
    if !value.is_finite() || value.abs() > f64::from(limit) {
        return Err(serde::de::Error::custom(format!(
            "value must be finite in [-{limit}, {limit}]"
        )));
    }
    Ok(value as f32)
}

fn deserialize_move_axis<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<f32, D::Error> {
    deserialize_bounded_f32(deserializer, 1.0)
}
fn deserialize_yaw_delta<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<f32, D::Error> {
    deserialize_bounded_f32(deserializer, LOOK_YAW_DELTA_LIMIT_RADIANS)
}
fn deserialize_pitch_delta<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<f32, D::Error> {
    deserialize_bounded_f32(deserializer, LOOK_PITCH_DELTA_LIMIT_RADIANS)
}

#[derive(
    Clone,
    Copy,
    Debug,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
)]
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

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
pub enum ActionSource {
    Agent,
    Human,
    Replay,
    Script,
    Network,
    Test,
}

#[derive(Resource, Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
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
