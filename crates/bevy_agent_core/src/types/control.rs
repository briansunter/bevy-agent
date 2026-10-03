use crate::*;

/// Execution context for the simulation.
///
/// The runner installs `Reconstructing` while rebuilding history. Input
/// resolution, replay recording, and snapshot creation share this gate.
#[derive(
    Resource,
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
pub enum ExecutionContext {
    #[default]
    Live,
    Reconstructing,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
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

#[derive(Resource, Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
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

#[derive(Resource, Clone, Debug, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RewardState {
    pub current_reward: f32,
    pub cumulative_reward: f32,
}

#[derive(Resource, Clone, Debug, Default, Serialize, Deserialize, schemars::JsonSchema)]
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
