use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{CoreError, ErrorKind, canonical_json, canonical_sha256};

pub const LEASE_SECONDS: u64 = 30;
pub const RENEWAL_SECONDS: u64 = 10;
pub const MAX_ATTEMPTS: u8 = 3;
const MAX_RESULT_BYTES: usize = 1024 * 1024;
const MAX_ARTIFACTS: usize = 1000;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TerminalStatus {
    Succeeded,
    Failed,
    Timeout,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ResultEvent {
    pub kind: String,
    pub name: String,
    pub status: String,
    pub duration_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ResultAssertion {
    pub id: String,
    pub passed: bool,
    pub message: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub logical_name: String,
    pub content_hash: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TaskResult {
    pub status: TerminalStatus,
    pub events: Vec<ResultEvent>,
    pub assertions: Vec<ResultAssertion>,
    pub metrics: BTreeMap<String, Value>,
    pub artifacts: Vec<Artifact>,
}

impl TaskResult {
    pub fn output_hash(&self) -> Result<String, CoreError> {
        canonical_sha256(&normalized_result(self)?)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    pub run_id: String,
    pub task_id: String,
    pub input_hash: String,
    pub attempt: u8,
    pub lease_id: Uuid,
    pub random_seed: u64,
    pub expires_at: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResultEnvelope {
    pub run_id: String,
    pub task_id: String,
    pub attempt: u8,
    pub lease_id: Uuid,
    pub result_json: TaskResult,
    pub output_hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubmitOutcome {
    Accepted,
    DroppedOldLease,
    DroppedDuplicate,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunState {
    Running,
    Succeeded,
    Failed { tasks: Vec<String> },
}

#[derive(Clone, Debug)]
struct Task {
    input_hash: String,
    attempt: u8,
    lease: Option<Lease>,
    result: Option<TaskResult>,
    exhausted: bool,
}

pub struct Coordinator {
    run_id: String,
    tasks: BTreeMap<String, Task>,
    coordinator_failed: bool,
}

impl Coordinator {
    pub fn new(
        run_id: impl Into<String>,
        tasks: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Self, CoreError> {
        let run_id = run_id.into();
        let mut values = BTreeMap::new();
        for (task_id, input_hash) in tasks {
            if values
                .insert(
                    task_id.clone(),
                    Task {
                        input_hash,
                        attempt: 0,
                        lease: None,
                        result: None,
                        exhausted: false,
                    },
                )
                .is_some()
            {
                return Err(CoreError::invalid(format!("重复 task_id: {task_id}")));
            }
        }
        Ok(Self {
            run_id,
            tasks: values,
            coordinator_failed: false,
        })
    }

    pub fn issue_lease(&mut self, now: u64) -> Option<Lease> {
        for (task_id, task) in &mut self.tasks {
            if task.result.is_some() || task.exhausted {
                continue;
            }
            if task
                .lease
                .as_ref()
                .is_some_and(|lease| lease.expires_at > now)
            {
                continue;
            }
            if task.attempt >= MAX_ATTEMPTS {
                task.exhausted = true;
                task.lease = None;
                continue;
            }
            task.attempt += 1;
            let lease = Lease {
                run_id: self.run_id.clone(),
                task_id: task_id.clone(),
                input_hash: task.input_hash.clone(),
                attempt: task.attempt,
                lease_id: Uuid::new_v4(),
                random_seed: stable_seed(&self.run_id, task_id, task.attempt),
                expires_at: now + LEASE_SECONDS,
            };
            task.lease = Some(lease.clone());
            return Some(lease);
        }
        None
    }

    pub fn renew(&mut self, lease: &Lease, now: u64) -> bool {
        let Some(task) = self.tasks.get_mut(&lease.task_id) else {
            return false;
        };
        let Some(current) = task.lease.as_mut() else {
            return false;
        };
        if current.run_id != lease.run_id
            || current.attempt != lease.attempt
            || current.lease_id != lease.lease_id
            || current.expires_at <= now
            || task.result.is_some()
        {
            return false;
        }
        current.expires_at = now + LEASE_SECONDS;
        true
    }

    pub fn submit(
        &mut self,
        envelope: ResultEnvelope,
        now: u64,
    ) -> Result<SubmitOutcome, CoreError> {
        let Some(task) = self.tasks.get_mut(&envelope.task_id) else {
            return Ok(SubmitOutcome::DroppedOldLease);
        };
        if task.result.is_some() {
            return Ok(SubmitOutcome::DroppedDuplicate);
        }
        let Some(lease) = &task.lease else {
            return Ok(SubmitOutcome::DroppedOldLease);
        };
        if envelope.run_id != self.run_id
            || envelope.attempt != lease.attempt
            || envelope.lease_id != lease.lease_id
            || lease.expires_at <= now
        {
            return Ok(SubmitOutcome::DroppedOldLease);
        }
        if envelope.result_json.artifacts.len() > MAX_ARTIFACTS {
            return Err(CoreError::new(
                ErrorKind::ResultTooLarge,
                "制品清单超过 1000 项",
            ));
        }
        validate_artifacts(&envelope.result_json.artifacts)?;
        let normalized = normalized_result(&envelope.result_json)?;
        if canonical_json(&normalized)?.len() > MAX_RESULT_BYTES {
            return Err(CoreError::new(
                ErrorKind::ResultTooLarge,
                "result_json 超过 1 MiB",
            ));
        }
        if canonical_sha256(&normalized)? != envelope.output_hash {
            return Err(CoreError::new(
                ErrorKind::ResultCorrupted,
                "output_hash 与规范化结果不一致",
            ));
        }
        task.result = Some(envelope.result_json);
        task.lease = None;
        Ok(SubmitOutcome::Accepted)
    }

    pub fn expire(&mut self, now: u64) {
        for task in self.tasks.values_mut() {
            if task.result.is_none()
                && task
                    .lease
                    .as_ref()
                    .is_some_and(|lease| lease.expires_at <= now)
            {
                task.lease = None;
                if task.attempt >= MAX_ATTEMPTS {
                    task.exhausted = true;
                }
            }
        }
    }

    pub fn coordinator_exited(&mut self) {
        self.coordinator_failed = true;
    }

    pub fn state(&self) -> RunState {
        if self.coordinator_failed {
            return RunState::Failed {
                tasks: vec!["coordinator".to_owned()],
            };
        }
        let mut failed = Vec::new();
        let mut complete = true;
        for (task_id, task) in &self.tasks {
            match task.result.as_ref().map(|result| &result.status) {
                Some(TerminalStatus::Succeeded) => {}
                Some(_) => failed.push(task_id.clone()),
                None if task.exhausted => failed.push(task_id.clone()),
                None => complete = false,
            }
        }
        if !complete {
            RunState::Running
        } else if failed.is_empty() {
            RunState::Succeeded
        } else {
            RunState::Failed { tasks: failed }
        }
    }
}

fn normalized_result(result: &TaskResult) -> Result<Value, CoreError> {
    let mut value = serde_json::to_value(result)
        .map_err(|error| CoreError::invalid(format!("结果序列化失败: {error}")))?;
    value["artifacts"]
        .as_array_mut()
        .expect("TaskResult.artifacts 始终为数组")
        .sort_by(|left, right| {
            left["logical_name"]
                .as_str()
                .cmp(&right["logical_name"].as_str())
        });
    Ok(value)
}

fn validate_artifacts(artifacts: &[Artifact]) -> Result<(), CoreError> {
    let mut names = BTreeSet::new();
    for artifact in artifacts {
        if artifact.logical_name.is_empty() || !names.insert(&artifact.logical_name) {
            return Err(CoreError::invalid("制品逻辑名称为空或重复"));
        }
        if artifact.content_hash.len() != 64
            || !artifact
                .content_hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(CoreError::invalid("制品内容哈希必须为 SHA-256 十六进制"));
        }
    }
    Ok(())
}

fn stable_seed(run_id: &str, task_id: &str, attempt: u8) -> u64 {
    let digest = Sha256::digest(format!("{run_id}\0{task_id}\0{attempt}").as_bytes());
    u64::from_be_bytes(digest[..8].try_into().expect("SHA-256 至少 8 字节"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(status: TerminalStatus) -> TaskResult {
        TaskResult {
            status,
            events: Vec::new(),
            assertions: Vec::new(),
            metrics: BTreeMap::new(),
            artifacts: Vec::new(),
        }
    }

    fn envelope(lease: &Lease, result_json: TaskResult) -> ResultEnvelope {
        ResultEnvelope {
            run_id: lease.run_id.clone(),
            task_id: lease.task_id.clone(),
            attempt: lease.attempt,
            lease_id: lease.lease_id,
            output_hash: result_json.output_hash().unwrap(),
            result_json,
        }
    }

    #[test]
    fn expired_lease_changes_id_and_old_result_is_ignored() {
        let mut coordinator =
            Coordinator::new("run", [("task".to_owned(), "input".to_owned())]).unwrap();
        let first = coordinator.issue_lease(0).unwrap();
        let second = coordinator.issue_lease(30).unwrap();
        assert_eq!(second.attempt, 2);
        assert_ne!(second.lease_id, first.lease_id);
        assert_eq!(
            coordinator
                .submit(envelope(&first, result(TerminalStatus::Succeeded)), 30)
                .unwrap(),
            SubmitOutcome::DroppedOldLease
        );
        assert_eq!(coordinator.state(), RunState::Running);
        assert_eq!(
            coordinator
                .submit(envelope(&second, result(TerminalStatus::Succeeded)), 31)
                .unwrap(),
            SubmitOutcome::Accepted
        );
        assert_eq!(
            coordinator
                .submit(envelope(&second, result(TerminalStatus::Succeeded)), 31)
                .unwrap(),
            SubmitOutcome::DroppedDuplicate
        );
        assert_eq!(coordinator.state(), RunState::Succeeded);
    }

    #[test]
    fn corrupt_result_and_third_expiry_fail() {
        let mut coordinator =
            Coordinator::new("run", [("task".to_owned(), "input".to_owned())]).unwrap();
        let lease = coordinator.issue_lease(0).unwrap();
        let mut corrupt = envelope(&lease, result(TerminalStatus::Succeeded));
        corrupt.output_hash = "0".repeat(64);
        assert_eq!(
            coordinator.submit(corrupt, 1).unwrap_err().kind,
            ErrorKind::ResultCorrupted
        );
        coordinator.issue_lease(30).unwrap();
        coordinator.issue_lease(60).unwrap();
        coordinator.expire(90);
        assert_eq!(
            coordinator.state(),
            RunState::Failed {
                tasks: vec!["task".to_owned()]
            }
        );
    }

    #[test]
    fn result_and_artifact_limits_are_enforced() {
        let mut coordinator =
            Coordinator::new("run", [("task".to_owned(), "input".to_owned())]).unwrap();
        let lease = coordinator.issue_lease(0).unwrap();
        let mut oversized = result(TerminalStatus::Succeeded);
        oversized.events.push(ResultEvent {
            kind: "tool".into(),
            name: "x".repeat(MAX_RESULT_BYTES),
            status: "succeeded".into(),
            duration_ms: 1,
        });
        assert_eq!(
            coordinator
                .submit(envelope(&lease, oversized), 1)
                .unwrap_err()
                .kind,
            ErrorKind::ResultTooLarge
        );

        let mut too_many_artifacts = result(TerminalStatus::Succeeded);
        too_many_artifacts.artifacts = (0..=MAX_ARTIFACTS)
            .map(|index| Artifact {
                logical_name: index.to_string(),
                content_hash: "0".repeat(64),
            })
            .collect();
        assert_eq!(
            coordinator
                .submit(envelope(&lease, too_many_artifacts), 1)
                .unwrap_err()
                .kind,
            ErrorKind::ResultTooLarge
        );
    }
}
