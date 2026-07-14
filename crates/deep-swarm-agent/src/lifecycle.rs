use std::time::SystemTime;

use crate::{AgentError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentState {
    Created,
    Ready,
    Running,
    Paused,
    Cancelling,
    Succeeded,
    Failed,
    Cancelled,
}

impl AgentState {
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }

    pub const fn can_transition_to(self, target: Self) -> bool {
        matches!(
            (self, target),
            (Self::Created, Self::Ready | Self::Cancelled)
                | (Self::Ready, Self::Running | Self::Cancelled)
                | (
                    Self::Running,
                    Self::Paused | Self::Cancelling | Self::Succeeded | Self::Failed
                )
                | (Self::Paused, Self::Running | Self::Cancelling)
                | (Self::Cancelling, Self::Cancelled | Self::Failed)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalInfo {
    pub state: AgentState,
    pub finished_at: SystemTime,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionEvent {
    pub agent_id: u64,
    pub terminal: TerminalInfo,
}

#[derive(Debug, Clone)]
pub struct Lifecycle {
    state: AgentState,
    terminal: Option<TerminalInfo>,
    completion_emitted: bool,
}

impl Default for Lifecycle {
    fn default() -> Self {
        Self {
            state: AgentState::Created,
            terminal: None,
            completion_emitted: false,
        }
    }
}

impl Lifecycle {
    pub fn state(&self) -> AgentState {
        self.state
    }

    pub fn terminal(&self) -> Option<&TerminalInfo> {
        self.terminal.as_ref()
    }

    pub fn transition(&mut self, target: AgentState, reason: impl Into<String>) -> Result<()> {
        if !self.state.can_transition_to(target) {
            return Err(AgentError::InvalidState {
                from: self.state,
                to: target,
            });
        }
        self.state = target;
        if target.is_terminal() {
            self.terminal = Some(TerminalInfo {
                state: target,
                finished_at: SystemTime::now(),
                reason: reason.into(),
            });
        }
        Ok(())
    }

    pub fn cancel(&mut self, reason: impl Into<String>) -> Result<bool> {
        if self.state.is_terminal() {
            return Ok(false);
        }
        let reason = reason.into();
        match self.state {
            AgentState::Created | AgentState::Ready => {
                self.transition(AgentState::Cancelled, reason)?
            }
            AgentState::Running | AgentState::Paused => {
                self.transition(AgentState::Cancelling, "cancellation requested")?;
                self.transition(AgentState::Cancelled, reason)?;
            }
            AgentState::Cancelling => self.transition(AgentState::Cancelled, reason)?,
            AgentState::Succeeded | AgentState::Failed | AgentState::Cancelled => unreachable!(),
        }
        Ok(true)
    }

    pub(crate) fn take_completion(&mut self, agent_id: u64) -> Option<CompletionEvent> {
        if self.completion_emitted {
            return None;
        }
        let terminal = self.terminal.clone()?;
        self.completion_emitted = true;
        Some(CompletionEvent { agent_id, terminal })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_every_state_pair() {
        let states = [
            AgentState::Created,
            AgentState::Ready,
            AgentState::Running,
            AgentState::Paused,
            AgentState::Cancelling,
            AgentState::Succeeded,
            AgentState::Failed,
            AgentState::Cancelled,
        ];
        let legal = [
            (AgentState::Created, AgentState::Ready),
            (AgentState::Created, AgentState::Cancelled),
            (AgentState::Ready, AgentState::Running),
            (AgentState::Ready, AgentState::Cancelled),
            (AgentState::Running, AgentState::Paused),
            (AgentState::Running, AgentState::Cancelling),
            (AgentState::Running, AgentState::Succeeded),
            (AgentState::Running, AgentState::Failed),
            (AgentState::Paused, AgentState::Running),
            (AgentState::Paused, AgentState::Cancelling),
            (AgentState::Cancelling, AgentState::Cancelled),
            (AgentState::Cancelling, AgentState::Failed),
        ];

        for from in states {
            for to in states {
                assert_eq!(from.can_transition_to(to), legal.contains(&(from, to)));
                let mut lifecycle = Lifecycle {
                    state: from,
                    terminal: None,
                    completion_emitted: false,
                };
                let result = lifecycle.transition(to, "test");
                assert_eq!(result.is_ok(), legal.contains(&(from, to)));
                if !legal.contains(&(from, to)) {
                    assert_eq!(
                        result,
                        Err(AgentError::InvalidState { from, to }),
                        "{from:?} -> {to:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn completion_and_cancel_are_idempotent() {
        let mut lifecycle = Lifecycle::default();
        assert!(lifecycle.cancel("stopped").unwrap());
        assert!(!lifecycle.cancel("again").unwrap());
        assert_eq!(lifecycle.terminal().unwrap().reason, "stopped");
        assert!(lifecycle.take_completion(0).is_some());
        assert!(lifecycle.take_completion(0).is_none());
    }

    #[test]
    fn cancellation_reaches_a_terminal_state_from_every_non_terminal_state() {
        for state in [
            AgentState::Created,
            AgentState::Ready,
            AgentState::Running,
            AgentState::Paused,
            AgentState::Cancelling,
        ] {
            let mut lifecycle = Lifecycle {
                state,
                terminal: None,
                completion_emitted: false,
            };
            assert!(lifecycle.cancel("session stopped").unwrap());
            assert_eq!(lifecycle.state(), AgentState::Cancelled);
            assert_eq!(lifecycle.terminal().unwrap().reason, "session stopped");
            assert!(!lifecycle.cancel("again").unwrap());
            assert_eq!(lifecycle.state(), AgentState::Cancelled);
        }
    }
}
