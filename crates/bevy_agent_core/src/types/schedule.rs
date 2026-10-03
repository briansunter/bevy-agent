use crate::*;

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

/// Collects the completed tick's observation, replay record, and checkpoint
/// after all gameplay hooks have finished.
#[derive(ScheduleLabel, Clone, Debug, PartialEq, Eq, Hash)]
pub struct AgentFinalize;

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
