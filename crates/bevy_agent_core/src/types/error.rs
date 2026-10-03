use crate::*;

#[derive(Error, Debug, Clone)]
pub enum AgentControlError {
    #[error("agent tick produced no step response")]
    MissingStepResponse,
    #[error("requested resource is not installed: {0}")]
    MissingResource(&'static str),
    #[error("required schedule is not installed: {0}")]
    MissingSchedule(&'static str),
    #[error("invalid game integration: {0}")]
    InvalidIntegration(String),
    #[error("invalid JSON schema: {0}")]
    InvalidSchema(String),
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
