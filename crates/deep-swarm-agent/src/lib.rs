mod lifecycle;
mod session;
mod workspace;

pub use lifecycle::{AgentState, CompletionEvent, Lifecycle, TerminalInfo};
pub use session::{
    AgentContext, AgentHandle, AgentId, AgentSpec, ChildOptions, FairDispatcher, ForkMode,
    PermissionSet, SessionId, SessionManager, SessionManagerConfig, SoftBudget,
};
pub use workspace::{WorkspaceHandle, WorkspaceSnapshot, WriteLease};

use std::time::Duration;

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum AgentError {
    #[error("invalid agent state transition from {from:?} to {to:?}")]
    InvalidState { from: AgentState, to: AgentState },
    #[error("resource exhausted: {0}")]
    ResourceExhausted(String),
    #[error("state conflict: expected version {expected}, actual version {actual}")]
    StateConflict { expected: u64, actual: u64 },
    #[error("invalid workspace write lease")]
    InvalidWriteLease,
    #[error("permission escalation is not allowed")]
    PermissionEscalation,
    #[error("budget escalation is not allowed")]
    BudgetEscalation,
    #[error("session not found")]
    SessionNotFound,
    #[error("agent not found")]
    AgentNotFound,
    #[error("agent task is closed")]
    AgentClosed,
    #[error("invalid configuration: {0}")]
    InvalidConfiguration(String),
}

pub type Result<T> = std::result::Result<T, AgentError>;

pub const MIN_CANCELLATION_GRACE: Duration = Duration::from_millis(100);
pub const MAX_CANCELLATION_GRACE: Duration = Duration::from_secs(60);
