use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
};

use tokio::sync::{mpsc, oneshot};

use crate::{AgentError, Result};

const WORKSPACE_MAILBOX_CAPACITY: usize = 64;
static NEXT_WORKSPACE_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_LEASE_TOKEN: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSnapshot {
    pub version: u64,
    pub values: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct WorkspaceHandle {
    id: u64,
    tx: mpsc::Sender<WorkspaceCommand>,
}

impl Default for WorkspaceHandle {
    fn default() -> Self {
        Self::new(BTreeMap::new())
    }
}

impl WorkspaceHandle {
    pub fn new(values: BTreeMap<String, String>) -> Self {
        Self::from_snapshot(WorkspaceSnapshot { version: 0, values })
    }

    pub fn from_snapshot(snapshot: WorkspaceSnapshot) -> Self {
        let (tx, rx) = mpsc::channel(WORKSPACE_MAILBOX_CAPACITY);
        tokio::spawn(run_workspace(rx, snapshot));
        Self {
            id: NEXT_WORKSPACE_ID.fetch_add(1, Ordering::Relaxed),
            tx,
        }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub async fn read(&self) -> Result<WorkspaceSnapshot> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(WorkspaceCommand::Read { reply })
            .await
            .map_err(|_| AgentError::AgentClosed)?;
        response.await.map_err(|_| AgentError::AgentClosed)
    }

    pub async fn write(
        &self,
        expected_version: u64,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<u64> {
        self.write_inner(expected_version, key.into(), value.into(), None)
            .await
    }

    pub async fn write_with_lease(
        &self,
        lease: &WriteLease,
        expected_version: u64,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<u64> {
        if lease.workspace_id != self.id {
            return Err(AgentError::InvalidWriteLease);
        }
        self.write_inner(
            expected_version,
            key.into(),
            value.into(),
            Some(lease.token),
        )
        .await
    }

    async fn write_inner(
        &self,
        expected_version: u64,
        key: String,
        value: String,
        lease: Option<u64>,
    ) -> Result<u64> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(WorkspaceCommand::Write {
                expected_version,
                key,
                value,
                lease,
                reply,
            })
            .await
            .map_err(|_| AgentError::AgentClosed)?;
        response.await.map_err(|_| AgentError::AgentClosed)?
    }

    pub async fn acquire_write_lease(&self) -> Result<WriteLease> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(WorkspaceCommand::AcquireLease { reply })
            .await
            .map_err(|_| AgentError::AgentClosed)?;
        let token = response.await.map_err(|_| AgentError::AgentClosed)??;
        Ok(WriteLease {
            workspace_id: self.id,
            token,
            tx: self.tx.clone(),
            released: false,
        })
    }
}

#[derive(Debug)]
pub struct WriteLease {
    workspace_id: u64,
    token: u64,
    tx: mpsc::Sender<WorkspaceCommand>,
    released: bool,
}

impl WriteLease {
    pub async fn release(mut self) -> Result<()> {
        self.release_inner().await
    }

    async fn release_inner(&mut self) -> Result<()> {
        if self.released {
            return Ok(());
        }
        let (reply, response) = oneshot::channel();
        self.tx
            .send(WorkspaceCommand::ReleaseLease {
                token: self.token,
                reply,
            })
            .await
            .map_err(|_| AgentError::AgentClosed)?;
        response.await.map_err(|_| AgentError::AgentClosed)??;
        self.released = true;
        Ok(())
    }
}

impl Drop for WriteLease {
    fn drop(&mut self) {
        if !self.released {
            let command = WorkspaceCommand::ReleaseLeaseNoReply { token: self.token };
            if let Err(mpsc::error::TrySendError::Full(command)) = self.tx.try_send(command)
                && let Ok(runtime) = tokio::runtime::Handle::try_current()
            {
                let tx = self.tx.clone();
                runtime.spawn(async move {
                    let _ = tx.send(command).await;
                });
            }
        }
    }
}

enum WorkspaceCommand {
    Read {
        reply: oneshot::Sender<WorkspaceSnapshot>,
    },
    Write {
        expected_version: u64,
        key: String,
        value: String,
        lease: Option<u64>,
        reply: oneshot::Sender<Result<u64>>,
    },
    AcquireLease {
        reply: oneshot::Sender<Result<u64>>,
    },
    ReleaseLease {
        token: u64,
        reply: oneshot::Sender<Result<()>>,
    },
    ReleaseLeaseNoReply {
        token: u64,
    },
}

async fn run_workspace(mut rx: mpsc::Receiver<WorkspaceCommand>, mut snapshot: WorkspaceSnapshot) {
    let mut active_lease = None;
    while let Some(command) = rx.recv().await {
        match command {
            WorkspaceCommand::Read { reply } => {
                let _ = reply.send(snapshot.clone());
            }
            WorkspaceCommand::Write {
                expected_version,
                key,
                value,
                lease,
                reply,
            } => {
                let result = if expected_version != snapshot.version {
                    Err(AgentError::StateConflict {
                        expected: expected_version,
                        actual: snapshot.version,
                    })
                } else if active_lease.is_some() && active_lease != lease {
                    Err(AgentError::ResourceExhausted(
                        "workspace has an active write lease".into(),
                    ))
                } else {
                    match snapshot.version.checked_add(1) {
                        Some(version) => {
                            snapshot.values.insert(key, value);
                            snapshot.version = version;
                            Ok(version)
                        }
                        None => Err(AgentError::ResourceExhausted(
                            "workspace version is exhausted".into(),
                        )),
                    }
                };
                let _ = reply.send(result);
            }
            WorkspaceCommand::AcquireLease { reply } => {
                let result = if active_lease.is_some() {
                    Err(AgentError::ResourceExhausted(
                        "workspace write lease is already held".into(),
                    ))
                } else {
                    let token = NEXT_LEASE_TOKEN.fetch_add(1, Ordering::Relaxed);
                    active_lease = Some(token);
                    Ok(token)
                };
                let _ = reply.send(result);
            }
            WorkspaceCommand::ReleaseLease { token, reply } => {
                let result = if active_lease == Some(token) {
                    active_lease = None;
                    Ok(())
                } else {
                    Err(AgentError::InvalidWriteLease)
                };
                let _ = reply.send(result);
            }
            WorkspaceCommand::ReleaseLeaseNoReply { token } => {
                if active_lease == Some(token) {
                    active_lease = None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cas_allows_only_one_writer() {
        let workspace = WorkspaceHandle::default();
        let left = workspace.clone();
        let right = workspace.clone();
        let (left, right) = tokio::join!(left.write(0, "left", "1"), right.write(0, "right", "1"));
        assert_eq!(usize::from(left.is_ok()) + usize::from(right.is_ok()), 1);
        let conflict = [left, right].into_iter().find_map(Result::err).unwrap();
        assert_eq!(
            conflict,
            AgentError::StateConflict {
                expected: 0,
                actual: 1
            }
        );
    }

    #[tokio::test]
    async fn write_lease_is_exclusive() {
        let workspace = WorkspaceHandle::default();
        let lease = workspace.acquire_write_lease().await.unwrap();
        assert!(matches!(
            workspace.acquire_write_lease().await,
            Err(AgentError::ResourceExhausted(_))
        ));
        assert!(matches!(
            workspace.write(0, "blocked", "1").await,
            Err(AgentError::ResourceExhausted(_))
        ));
        workspace
            .write_with_lease(&lease, 0, "allowed", "1")
            .await
            .unwrap();
        lease.release().await.unwrap();
        workspace.write(1, "after", "1").await.unwrap();
    }
}
