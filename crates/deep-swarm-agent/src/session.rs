use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use tokio::{
    sync::{Semaphore, broadcast, mpsc, oneshot},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

use crate::{
    AgentError, AgentState, CompletionEvent, Lifecycle, MAX_CANCELLATION_GRACE,
    MIN_CANCELLATION_GRACE, Result, TerminalInfo, WorkspaceHandle,
};

pub type AgentId = u64;
pub type SessionId = u64;
pub type PermissionSet = BTreeSet<String>;

static NEXT_AGENT_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoftBudget {
    pub mailbox_capacity: usize,
    pub max_concurrent_calls: usize,
    pub deadline: Option<Instant>,
    pub max_message_bytes: usize,
    pub max_output_bytes: usize,
    pub cancellation_grace: Duration,
}

impl Default for SoftBudget {
    fn default() -> Self {
        Self {
            mailbox_capacity: 64,
            max_concurrent_calls: 1,
            deadline: None,
            max_message_bytes: 64 * 1024,
            max_output_bytes: 1024 * 1024,
            cancellation_grace: Duration::from_secs(5),
        }
    }
}

impl SoftBudget {
    pub fn validate(&self) -> Result<()> {
        if self.mailbox_capacity == 0
            || self.max_concurrent_calls == 0
            || self.max_message_bytes == 0
            || self.max_output_bytes == 0
        {
            return Err(AgentError::InvalidConfiguration(
                "soft budget limits must be greater than zero".into(),
            ));
        }
        if !(MIN_CANCELLATION_GRACE..=MAX_CANCELLATION_GRACE).contains(&self.cancellation_grace) {
            return Err(AgentError::InvalidConfiguration(
                "cancellation grace must be between 100 ms and 60 s".into(),
            ));
        }
        Ok(())
    }

    pub fn is_narrower_than(&self, parent: &Self) -> bool {
        self.mailbox_capacity <= parent.mailbox_capacity
            && self.max_concurrent_calls <= parent.max_concurrent_calls
            && deadline_is_narrower(self.deadline, parent.deadline)
            && self.max_message_bytes <= parent.max_message_bytes
            && self.max_output_bytes <= parent.max_output_bytes
            && self.cancellation_grace <= parent.cancellation_grace
    }

    pub fn check_output(&self, bytes: usize) -> Result<()> {
        if bytes > self.max_output_bytes {
            Err(AgentError::ResourceExhausted(
                "output exceeds the agent byte budget".into(),
            ))
        } else {
            Ok(())
        }
    }

    fn check_dispatch(&self, message: &str) -> Result<()> {
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(AgentError::ResourceExhausted(
                "agent deadline has elapsed".into(),
            ));
        }
        if message.len() > self.max_message_bytes {
            return Err(AgentError::ResourceExhausted(
                "message exceeds the agent byte budget".into(),
            ));
        }
        Ok(())
    }
}

fn deadline_is_narrower(child: Option<Instant>, parent: Option<Instant>) -> bool {
    match (child, parent) {
        (Some(child), Some(parent)) => child <= parent,
        (Some(_), None) | (None, None) => true,
        (None, Some(_)) => false,
    }
}

#[derive(Debug, Clone, Default)]
pub struct AgentContext {
    pub system_prompt: String,
    pub history: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub credential_handles: Vec<String>,
    pub workspace: WorkspaceHandle,
    pub plan_board: WorkspaceHandle,
}

#[derive(Debug, Clone, Default)]
pub struct AgentSpec {
    pub context: AgentContext,
    pub permissions: PermissionSet,
    pub budget: SoftBudget,
}

impl AgentSpec {
    fn validate(&self) -> Result<()> {
        self.budget.validate()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkMode {
    Fork,
    Inherit {
        share_workspace: bool,
        share_plan_board: bool,
    },
}

#[derive(Debug, Clone)]
pub struct ChildOptions {
    pub mode: ForkMode,
    pub permissions: Option<PermissionSet>,
    pub budget: Option<SoftBudget>,
}

impl Default for ChildOptions {
    fn default() -> Self {
        Self {
            mode: ForkMode::Fork,
            permissions: None,
            budget: None,
        }
    }
}

#[derive(Debug, Clone)]
struct AgentSnapshot {
    context: AgentContext,
    permissions: PermissionSet,
    budget: SoftBudget,
}

#[derive(Debug, Clone)]
pub struct AgentHandle {
    id: AgentId,
    session_id: SessionId,
    tx: mpsc::Sender<AgentCommand>,
    cancellation: CancellationToken,
    budget: SoftBudget,
    in_flight: Arc<AtomicBool>,
    schedulable: Arc<AtomicBool>,
}

impl AgentHandle {
    pub fn id(&self) -> AgentId {
        self.id
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub fn budget(&self) -> &SoftBudget {
        &self.budget
    }

    pub fn check_output(&self, bytes: usize) -> Result<()> {
        self.budget.check_output(bytes)
    }

    pub async fn state(&self) -> Result<AgentState> {
        let (reply, response) = oneshot::channel();
        self.send_control(AgentCommand::State { reply }).await?;
        response.await.map_err(|_| AgentError::AgentClosed)
    }

    pub async fn terminal(&self) -> Result<Option<TerminalInfo>> {
        let (reply, response) = oneshot::channel();
        self.send_control(AgentCommand::Terminal { reply }).await?;
        response.await.map_err(|_| AgentError::AgentClosed)
    }

    pub async fn transition(&self, target: AgentState, reason: impl Into<String>) -> Result<()> {
        let (reply, response) = oneshot::channel();
        self.send_control(AgentCommand::Transition {
            target,
            reason: reason.into(),
            reply,
        })
        .await?;
        response.await.map_err(|_| AgentError::AgentClosed)?
    }

    pub async fn cancel(&self, reason: impl Into<String>) -> Result<AgentState> {
        self.schedulable.store(false, Ordering::Release);
        self.cancellation.cancel();
        let (reply, response) = oneshot::channel();
        self.send_control(AgentCommand::Cancel {
            reason: reason.into(),
            reply,
        })
        .await?;
        response.await.map_err(|_| AgentError::AgentClosed)?
    }

    pub async fn context(&self) -> Result<AgentContext> {
        Ok(self.snapshot().await?.context)
    }

    pub async fn permissions(&self) -> Result<PermissionSet> {
        Ok(self.snapshot().await?.permissions)
    }

    pub async fn update_environment(
        &self,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<()> {
        let (reply, response) = oneshot::channel();
        self.send_control(AgentCommand::UpdateEnvironment {
            key: key.into(),
            value: value.into(),
            reply,
        })
        .await?;
        response.await.map_err(|_| AgentError::AgentClosed)?
    }

    pub fn try_send_message(&self, message: impl Into<String>) -> Result<()> {
        let message = message.into();
        self.budget.check_dispatch(&message)?;
        self.tx
            .try_send(AgentCommand::Message { message })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => {
                    AgentError::ResourceExhausted("agent mailbox is full".into())
                }
                mpsc::error::TrySendError::Closed(_) => AgentError::AgentClosed,
            })
    }

    async fn snapshot(&self) -> Result<AgentSnapshot> {
        let (reply, response) = oneshot::channel();
        self.send_control(AgentCommand::Snapshot { reply }).await?;
        response.await.map_err(|_| AgentError::AgentClosed)
    }

    async fn send_control(&self, command: AgentCommand) -> Result<()> {
        self.tx
            .send(command)
            .await
            .map_err(|_| AgentError::AgentClosed)
    }

    fn try_dispatch(
        &self,
        message: String,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> std::result::Result<(), tokio::sync::OwnedSemaphorePermit> {
        if !self.schedulable.load(Ordering::Acquire)
            || self.cancellation.is_cancelled()
            || self.budget.check_dispatch(&message).is_err()
            || self
                .in_flight
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return Err(permit);
        }
        match self.tx.try_send(AgentCommand::Dispatch {
            message,
            permit,
            reservation: self.in_flight.clone(),
        }) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(AgentCommand::Dispatch {
                permit,
                reservation,
                ..
            }))
            | Err(mpsc::error::TrySendError::Closed(AgentCommand::Dispatch {
                permit,
                reservation,
                ..
            })) => {
                reservation.store(false, Ordering::Release);
                Err(permit)
            }
            Err(_) => unreachable!(),
        }
    }
}

enum AgentCommand {
    State {
        reply: oneshot::Sender<AgentState>,
    },
    Terminal {
        reply: oneshot::Sender<Option<TerminalInfo>>,
    },
    Snapshot {
        reply: oneshot::Sender<AgentSnapshot>,
    },
    Transition {
        target: AgentState,
        reason: String,
        reply: oneshot::Sender<Result<()>>,
    },
    Cancel {
        reason: String,
        reply: oneshot::Sender<Result<AgentState>>,
    },
    UpdateEnvironment {
        key: String,
        value: String,
        reply: oneshot::Sender<Result<()>>,
    },
    Message {
        message: String,
    },
    Dispatch {
        message: String,
        permit: tokio::sync::OwnedSemaphorePermit,
        reservation: Arc<AtomicBool>,
    },
}

fn spawn_agent(
    session_id: SessionId,
    session_cancellation: &CancellationToken,
    spec: AgentSpec,
    completions: broadcast::Sender<CompletionEvent>,
) -> AgentHandle {
    let id = NEXT_AGENT_ID.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = mpsc::channel(spec.budget.mailbox_capacity);
    let cancellation = session_cancellation.child_token();
    let handle = AgentHandle {
        id,
        session_id,
        tx,
        cancellation: cancellation.clone(),
        budget: spec.budget.clone(),
        in_flight: Arc::new(AtomicBool::new(false)),
        schedulable: Arc::new(AtomicBool::new(false)),
    };
    tokio::spawn(run_agent(
        id,
        rx,
        session_cancellation.clone(),
        spec,
        completions,
        handle.schedulable.clone(),
    ));
    handle
}

async fn run_agent(
    id: AgentId,
    mut rx: mpsc::Receiver<AgentCommand>,
    session_cancellation: CancellationToken,
    mut spec: AgentSpec,
    completions: broadcast::Sender<CompletionEvent>,
    schedulable: Arc<AtomicBool>,
) {
    let mut lifecycle = Lifecycle::default();
    let mut cancellation_handled = false;
    loop {
        tokio::select! {
            _ = session_cancellation.cancelled(), if !cancellation_handled => {
                cancellation_handled = true;
                schedulable.store(false, Ordering::Release);
                let _ = lifecycle.cancel("cancelled by parent scope");
                emit_completion(id, &mut lifecycle, &completions);
            }
            command = rx.recv() => {
                let Some(command) = command else { break };
                match command {
                    AgentCommand::State { reply } => { let _ = reply.send(lifecycle.state()); }
                    AgentCommand::Terminal { reply } => {
                        let _ = reply.send(lifecycle.terminal().cloned());
                    }
                    AgentCommand::Snapshot { reply } => {
                        let _ = reply.send(AgentSnapshot {
                            context: spec.context.clone(),
                            permissions: spec.permissions.clone(),
                            budget: spec.budget.clone(),
                        });
                    }
                    AgentCommand::Transition { target, reason, reply } => {
                        let result = lifecycle.transition(target, reason);
                        if result.is_ok() {
                            schedulable.store(target == AgentState::Running, Ordering::Release);
                        }
                        emit_completion(id, &mut lifecycle, &completions);
                        let _ = reply.send(result);
                    }
                    AgentCommand::Cancel { reason, reply } => {
                        schedulable.store(false, Ordering::Release);
                        let result = lifecycle.cancel(reason).map(|_| lifecycle.state());
                        emit_completion(id, &mut lifecycle, &completions);
                        let _ = reply.send(result);
                    }
                    AgentCommand::UpdateEnvironment { key, value, reply } => {
                        let result = if lifecycle.state().is_terminal() {
                            Err(AgentError::InvalidState {
                                from: lifecycle.state(),
                                to: lifecycle.state(),
                            })
                        } else {
                            spec.context.environment.insert(key, value);
                            Ok(())
                        };
                        let _ = reply.send(result);
                    }
                    AgentCommand::Message { message } => {
                        if !lifecycle.state().is_terminal() {
                            spec.context.history.push(message);
                        }
                    }
                    AgentCommand::Dispatch { message, permit, reservation } => {
                        if lifecycle.state() == AgentState::Running {
                            spec.context.history.push(message);
                        }
                        reservation.store(false, Ordering::Release);
                        drop(permit);
                    }
                }
            }
        }
    }
}

fn emit_completion(
    id: AgentId,
    lifecycle: &mut Lifecycle,
    completions: &broadcast::Sender<CompletionEvent>,
) {
    if let Some(event) = lifecycle.take_completion(id) {
        let _ = completions.send(event);
    }
}

#[derive(Debug, Clone)]
pub struct SessionManagerConfig {
    pub max_sessions: usize,
    pub max_agents_per_session: usize,
    pub global_concurrency: usize,
    pub completion_channel_capacity: usize,
}

impl Default for SessionManagerConfig {
    fn default() -> Self {
        Self {
            max_sessions: 100,
            max_agents_per_session: 500,
            global_concurrency: 200,
            completion_channel_capacity: 1024,
        }
    }
}

impl SessionManagerConfig {
    fn validate(&self) -> Result<()> {
        if self.max_sessions == 0
            || self.max_agents_per_session == 0
            || self.max_agents_per_session > 500
            || self.global_concurrency == 0
            || self.completion_channel_capacity == 0
        {
            return Err(AgentError::InvalidConfiguration(
                "session, agent, concurrency, and completion limits must be non-zero; agents must not exceed 500 per session".into(),
            ));
        }
        Ok(())
    }
}

struct SessionRecord {
    cancellation: CancellationToken,
    agents: RwLock<HashMap<AgentId, AgentHandle>>,
}

#[derive(Clone)]
pub struct SessionManager {
    config: SessionManagerConfig,
    sessions: Arc<RwLock<HashMap<SessionId, Arc<SessionRecord>>>>,
    dispatcher: FairDispatcher,
    completions: broadcast::Sender<CompletionEvent>,
}

impl SessionManager {
    pub fn new(config: SessionManagerConfig) -> Result<Self> {
        config.validate()?;
        let (completions, _) = broadcast::channel(config.completion_channel_capacity);
        Ok(Self {
            dispatcher: FairDispatcher::new(config.global_concurrency),
            sessions: Arc::new(RwLock::new(HashMap::new())),
            config,
            completions,
        })
    }

    pub fn subscribe_completions(&self) -> broadcast::Receiver<CompletionEvent> {
        self.completions.subscribe()
    }

    pub fn dispatcher(&self) -> FairDispatcher {
        self.dispatcher.clone()
    }

    pub fn create_session(&self, initial: AgentSpec) -> Result<(SessionId, AgentHandle)> {
        initial.validate()?;
        let mut sessions = self.sessions.write().expect("session lock poisoned");
        if sessions.len() >= self.config.max_sessions {
            return Err(AgentError::ResourceExhausted(
                "maximum session count reached".into(),
            ));
        }
        let session_id = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
        let cancellation = CancellationToken::new();
        let agent = spawn_agent(session_id, &cancellation, initial, self.completions.clone());
        let mut agents = HashMap::new();
        agents.insert(agent.id(), agent.clone());
        sessions.insert(
            session_id,
            Arc::new(SessionRecord {
                cancellation,
                agents: RwLock::new(agents),
            }),
        );
        drop(sessions);
        self.dispatcher.register(agent.clone());
        Ok((session_id, agent))
    }

    pub fn create_agent(&self, session_id: SessionId, spec: AgentSpec) -> Result<AgentHandle> {
        spec.validate()?;
        let session = self.session(session_id)?;
        let mut agents = session.agents.write().expect("agent lock poisoned");
        if agents.len() >= self.config.max_agents_per_session {
            return Err(AgentError::ResourceExhausted(
                "maximum agent count reached for session".into(),
            ));
        }
        let agent = spawn_agent(
            session_id,
            &session.cancellation,
            spec,
            self.completions.clone(),
        );
        agents.insert(agent.id(), agent.clone());
        drop(agents);
        self.dispatcher.register(agent.clone());
        Ok(agent)
    }

    pub async fn fork_agent(
        &self,
        parent_id: AgentId,
        options: ChildOptions,
    ) -> Result<AgentHandle> {
        let (session, parent) = self.find_agent(parent_id)?;
        let parent = parent.snapshot().await?;
        let permissions = options
            .permissions
            .unwrap_or_else(|| parent.permissions.clone());
        if !permissions.is_subset(&parent.permissions) {
            return Err(AgentError::PermissionEscalation);
        }
        let budget = options.budget.unwrap_or_else(|| parent.budget.clone());
        budget.validate()?;
        if !budget.is_narrower_than(&parent.budget) {
            return Err(AgentError::BudgetEscalation);
        }

        let workspace = match options.mode {
            ForkMode::Inherit {
                share_workspace: true,
                ..
            } => parent.context.workspace.clone(),
            _ => WorkspaceHandle::from_snapshot(parent.context.workspace.read().await?),
        };
        let plan_board = match options.mode {
            ForkMode::Inherit {
                share_plan_board: true,
                ..
            } => parent.context.plan_board.clone(),
            _ => WorkspaceHandle::from_snapshot(parent.context.plan_board.read().await?),
        };
        let spec = AgentSpec {
            context: AgentContext {
                system_prompt: parent.context.system_prompt,
                history: parent.context.history,
                environment: parent.context.environment,
                credential_handles: Vec::new(),
                workspace,
                plan_board,
            },
            permissions,
            budget,
        };

        let mut agents = session.agents.write().expect("agent lock poisoned");
        if agents.len() >= self.config.max_agents_per_session {
            return Err(AgentError::ResourceExhausted(
                "maximum agent count reached for session".into(),
            ));
        }
        let child = spawn_agent(
            parent_id_session(&agents, parent_id)?,
            &session.cancellation,
            spec,
            self.completions.clone(),
        );
        agents.insert(child.id(), child.clone());
        drop(agents);
        self.dispatcher.register(child.clone());
        Ok(child)
    }

    pub fn get_agent(&self, agent_id: AgentId) -> Result<AgentHandle> {
        self.find_agent(agent_id).map(|(_, agent)| agent)
    }

    pub fn session_count(&self) -> usize {
        self.sessions.read().expect("session lock poisoned").len()
    }

    pub fn agent_count(&self, session_id: SessionId) -> Result<usize> {
        Ok(self
            .session(session_id)?
            .agents
            .read()
            .expect("agent lock poisoned")
            .len())
    }

    pub async fn cancel_session(
        &self,
        session_id: SessionId,
        reason: impl Into<String>,
    ) -> Result<()> {
        let session = self.session(session_id)?;
        session.cancellation.cancel();
        let agents: Vec<_> = session
            .agents
            .read()
            .expect("agent lock poisoned")
            .values()
            .cloned()
            .collect();
        let reason = reason.into();
        for agent in agents {
            agent.cancel(reason.clone()).await?;
        }
        Ok(())
    }

    pub async fn cancel_all(&self, reason: impl Into<String>) -> Result<()> {
        let session_ids: Vec<_> = self
            .sessions
            .read()
            .expect("session lock poisoned")
            .keys()
            .copied()
            .collect();
        let reason = reason.into();
        for session_id in session_ids {
            self.cancel_session(session_id, reason.clone()).await?;
        }
        Ok(())
    }

    fn session(&self, session_id: SessionId) -> Result<Arc<SessionRecord>> {
        self.sessions
            .read()
            .expect("session lock poisoned")
            .get(&session_id)
            .cloned()
            .ok_or(AgentError::SessionNotFound)
    }

    fn find_agent(&self, agent_id: AgentId) -> Result<(Arc<SessionRecord>, AgentHandle)> {
        // ponytail: bounded O(sessions) lookup; add a global index only if profiling shows it matters.
        for session in self
            .sessions
            .read()
            .expect("session lock poisoned")
            .values()
        {
            if let Some(agent) = session
                .agents
                .read()
                .expect("agent lock poisoned")
                .get(&agent_id)
                .cloned()
            {
                return Ok((session.clone(), agent));
            }
        }
        Err(AgentError::AgentNotFound)
    }
}

fn parent_id_session(
    agents: &HashMap<AgentId, AgentHandle>,
    parent_id: AgentId,
) -> Result<SessionId> {
    agents
        .get(&parent_id)
        .map(AgentHandle::session_id)
        .ok_or(AgentError::AgentNotFound)
}

#[derive(Clone)]
pub struct FairDispatcher {
    agents: Arc<RwLock<Vec<AgentHandle>>>,
    cursor: Arc<AtomicUsize>,
    permits: Arc<Semaphore>,
}

impl FairDispatcher {
    fn new(global_concurrency: usize) -> Self {
        Self {
            agents: Arc::new(RwLock::new(Vec::new())),
            cursor: Arc::new(AtomicUsize::new(0)),
            permits: Arc::new(Semaphore::new(global_concurrency)),
        }
    }

    fn register(&self, agent: AgentHandle) {
        self.agents
            .write()
            .expect("dispatcher lock poisoned")
            .push(agent);
    }

    pub fn try_dispatch(&self, message: impl Into<String>) -> Result<AgentId> {
        let agents = self
            .agents
            .read()
            .expect("dispatcher lock poisoned")
            .clone();
        if agents.is_empty() {
            return Err(AgentError::AgentNotFound);
        }
        let mut permit = self.permits.clone().try_acquire_owned().map_err(|_| {
            AgentError::ResourceExhausted("global concurrency limit reached".into())
        })?;
        let start = self.cursor.fetch_add(1, Ordering::Relaxed) % agents.len();
        let message = message.into();
        for offset in 0..agents.len() {
            let agent = &agents[(start + offset) % agents.len()];
            match agent.try_dispatch(message.clone(), permit) {
                Ok(()) => return Ok(agent.id()),
                Err(returned) => permit = returned,
            }
        }
        Err(AgentError::ResourceExhausted(
            "all agent mailboxes or per-agent slots are busy".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn permissions(values: &[&str]) -> PermissionSet {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn manager() -> SessionManager {
        SessionManager::new(SessionManagerConfig::default()).unwrap()
    }

    async fn running(agent: &AgentHandle) {
        agent.transition(AgentState::Ready, "ready").await.unwrap();
        agent
            .transition(AgentState::Running, "running")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn fork_copies_private_state_and_drops_credentials() {
        let manager = manager();
        let mut values = BTreeMap::new();
        values.insert("file".into(), "parent".into());
        let budget = SoftBudget::default();
        let spec = AgentSpec {
            context: AgentContext {
                system_prompt: "system".into(),
                history: vec!["before".into()],
                environment: BTreeMap::from([("MODE".into(), "parent".into())]),
                credential_handles: vec!["secret-handle".into()],
                workspace: WorkspaceHandle::new(values),
                plan_board: WorkspaceHandle::default(),
            },
            permissions: permissions(&["read", "write"]),
            budget: budget.clone(),
        };
        let (session, parent) = manager.create_session(spec).unwrap();
        let mut child_budget = budget;
        child_budget.max_output_bytes /= 2;
        let child = manager
            .fork_agent(
                parent.id(),
                ChildOptions {
                    mode: ForkMode::Fork,
                    permissions: Some(permissions(&["read"])),
                    budget: Some(child_budget),
                },
            )
            .await
            .unwrap();

        parent.update_environment("MODE", "changed").await.unwrap();
        parent
            .context()
            .await
            .unwrap()
            .workspace
            .write(0, "file", "changed")
            .await
            .unwrap();
        let child_context = child.context().await.unwrap();
        assert_eq!(child_context.environment["MODE"], "parent");
        assert_eq!(
            child_context.workspace.read().await.unwrap().values["file"],
            "parent"
        );
        assert!(child_context.credential_handles.is_empty());
        assert_eq!(child.permissions().await.unwrap(), permissions(&["read"]));
        assert_eq!(manager.agent_count(session).unwrap(), 2);
    }

    #[tokio::test]
    async fn inherit_shares_only_requested_state() {
        let manager = manager();
        let (session, parent) = manager.create_session(AgentSpec::default()).unwrap();
        let child = manager
            .fork_agent(
                parent.id(),
                ChildOptions {
                    mode: ForkMode::Inherit {
                        share_workspace: true,
                        share_plan_board: false,
                    },
                    ..ChildOptions::default()
                },
            )
            .await
            .unwrap();
        let parent_context = parent.context().await.unwrap();
        let child_context = child.context().await.unwrap();
        assert_eq!(parent_context.workspace.id(), child_context.workspace.id());
        assert_ne!(
            parent_context.plan_board.id(),
            child_context.plan_board.id()
        );
        parent_context
            .workspace
            .write(0, "shared", "yes")
            .await
            .unwrap();
        assert_eq!(
            child_context.workspace.read().await.unwrap().values["shared"],
            "yes"
        );
        assert_eq!(manager.agent_count(session).unwrap(), 2);
    }

    #[tokio::test]
    async fn rejects_child_escalation_without_creating_an_agent() {
        let manager = manager();
        let (session, parent) = manager
            .create_session(AgentSpec {
                permissions: permissions(&["read"]),
                ..AgentSpec::default()
            })
            .unwrap();
        assert!(matches!(
            manager
                .fork_agent(
                    parent.id(),
                    ChildOptions {
                        permissions: Some(permissions(&["read", "write"])),
                        ..ChildOptions::default()
                    }
                )
                .await,
            Err(AgentError::PermissionEscalation)
        ));
        let mut larger = SoftBudget::default();
        larger.max_output_bytes += 1;
        assert!(matches!(
            manager
                .fork_agent(
                    parent.id(),
                    ChildOptions {
                        budget: Some(larger),
                        ..ChildOptions::default()
                    }
                )
                .await,
            Err(AgentError::BudgetEscalation)
        ));
        assert_eq!(manager.agent_count(session).unwrap(), 1);
    }

    #[tokio::test]
    async fn parent_and_child_cancel_independently_but_session_cancels_all() {
        let manager = manager();
        let mut completions = manager.subscribe_completions();
        let (session, parent) = manager.create_session(AgentSpec::default()).unwrap();
        let child = manager
            .fork_agent(parent.id(), ChildOptions::default())
            .await
            .unwrap();
        child.cancel("child only").await.unwrap();
        assert_eq!(child.state().await.unwrap(), AgentState::Cancelled);
        assert_eq!(
            child.terminal().await.unwrap().unwrap().reason,
            "child only"
        );
        assert_eq!(parent.state().await.unwrap(), AgentState::Created);
        manager
            .cancel_session(session, "session end")
            .await
            .unwrap();
        assert_eq!(parent.state().await.unwrap(), AgentState::Cancelled);
        assert_eq!(child.state().await.unwrap(), AgentState::Cancelled);

        let first = completions.recv().await.unwrap();
        let second = completions.recv().await.unwrap();
        assert_ne!(first.agent_id, second.agent_id);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), completions.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn bounded_mailbox_reports_resource_exhaustion() {
        let manager = manager();
        let (_, agent) = manager
            .create_session(AgentSpec {
                budget: SoftBudget {
                    mailbox_capacity: 1,
                    ..SoftBudget::default()
                },
                ..AgentSpec::default()
            })
            .unwrap();
        agent.try_send_message("first").unwrap();
        assert!(matches!(
            agent.try_send_message("second"),
            Err(AgentError::ResourceExhausted(_))
        ));
    }

    #[tokio::test]
    async fn global_limit_and_round_robin_are_enforced() {
        let manager = SessionManager::new(SessionManagerConfig {
            global_concurrency: 1,
            ..SessionManagerConfig::default()
        })
        .unwrap();
        let (_, first) = manager.create_session(AgentSpec::default()).unwrap();
        let second = manager
            .create_agent(first.session_id(), AgentSpec::default())
            .unwrap();
        running(&first).await;
        running(&second).await;
        let dispatcher = manager.dispatcher();
        assert_eq!(dispatcher.try_dispatch("one").unwrap(), first.id());
        assert!(matches!(
            dispatcher.try_dispatch("blocked"),
            Err(AgentError::ResourceExhausted(_))
        ));
        tokio::task::yield_now().await;
        assert_eq!(dispatcher.try_dispatch("two").unwrap(), second.id());
        tokio::task::yield_now().await;
        second
            .transition(AgentState::Succeeded, "done")
            .await
            .unwrap();
        assert_eq!(dispatcher.try_dispatch("three").unwrap(), first.id());
    }

    #[tokio::test]
    async fn soft_byte_and_deadline_budgets_are_enforced() {
        let manager = manager();
        let (_, agent) = manager
            .create_session(AgentSpec {
                budget: SoftBudget {
                    deadline: Some(Instant::now()),
                    max_message_bytes: 3,
                    max_output_bytes: 3,
                    ..SoftBudget::default()
                },
                ..AgentSpec::default()
            })
            .unwrap();
        assert!(matches!(
            agent.try_send_message("late"),
            Err(AgentError::ResourceExhausted(_))
        ));
        assert!(agent.check_output(3).is_ok());
        assert!(matches!(
            agent.check_output(4),
            Err(AgentError::ResourceExhausted(_))
        ));
    }

    #[tokio::test]
    async fn lightweight_capacity_checks_cover_both_baselines() {
        let dense = manager();
        let (dense_session, _) = dense.create_session(AgentSpec::default()).unwrap();
        for _ in 1..500 {
            dense
                .create_agent(dense_session, AgentSpec::default())
                .unwrap();
        }
        assert_eq!(dense.agent_count(dense_session).unwrap(), 500);
        assert!(matches!(
            dense.create_agent(dense_session, AgentSpec::default()),
            Err(AgentError::ResourceExhausted(_))
        ));

        let many = manager();
        for _ in 0..100 {
            let (session, _) = many.create_session(AgentSpec::default()).unwrap();
            for _ in 1..5 {
                many.create_agent(session, AgentSpec::default()).unwrap();
            }
        }
        assert_eq!(many.session_count(), 100);
        assert!(matches!(
            many.create_session(AgentSpec::default()),
            Err(AgentError::ResourceExhausted(_))
        ));
        assert_eq!(
            many.sessions
                .read()
                .unwrap()
                .values()
                .map(|session| session.agents.read().unwrap().len())
                .sum::<usize>(),
            500
        );
    }
}
