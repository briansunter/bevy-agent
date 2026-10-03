//! Explicit recovery information for failures after mutation begins.
use super::*;

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MutationFailure {
    pub operation: String,
    pub tick_before: u64,
    pub tick_after: u64,
    pub tick_committed: bool,
    pub completed_steps: usize,
    pub recovery_required: bool,
    pub message: String,
}
impl std::fmt::Display for MutationFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} failed at tick {} (tick committed: {}, recovery required: {}): {}",
            self.operation,
            self.tick_after,
            self.tick_committed,
            self.recovery_required,
            self.message
        )
    }
}
impl std::error::Error for MutationFailure {}

impl AgentApp {
    pub(super) fn batch_failure(
        &self,
        operation: &str,
        start_tick: u64,
        completed_steps: usize,
        error: anyhow::Error,
    ) -> anyhow::Error {
        if let Some(original) = error.downcast_ref::<MutationFailure>() {
            let mut failure = original.clone();
            failure.operation = operation.into();
            failure.tick_before = start_tick;
            failure.completed_steps = completed_steps;
            failure.tick_committed |= completed_steps != 0;
            return failure.into();
        }
        if completed_steps == 0 {
            return error;
        }
        MutationFailure {
            operation: operation.into(),
            tick_before: start_tick,
            tick_after: self.current_tick(),
            tick_committed: true,
            completed_steps,
            recovery_required: false,
            message: format!("{error:#}"),
        }
        .into()
    }

    pub(super) fn mutation_failed(
        &mut self,
        operation: &str,
        tick_before: u64,
        error: anyhow::Error,
    ) -> anyhow::Error {
        if error.is::<MutationFailure>() {
            return error;
        }
        let failure = MutationFailure {
            operation: operation.to_owned(),
            tick_before,
            tick_after: self.current_tick(),
            tick_committed: self.current_tick() > tick_before,
            completed_steps: 0,
            recovery_required: true,
            message: format!("{error:#}"),
        };
        self.app.world_mut().resource_mut::<LastStepResponse>().0 = None;
        self.app.world_mut().insert_resource(FaultState {
            message: failure.to_string(),
        });
        failure.into()
    }
}
