use std::{collections::BTreeSet, path::PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretHandle(String);

impl SecretHandle {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn id(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolPolicySnapshot {
    allowed_tools: BTreeSet<String>,
    pub allow_real_tools: bool,
}

impl ToolPolicySnapshot {
    pub fn mock_only(tools: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            allowed_tools: tools.into_iter().map(Into::into).collect(),
            allow_real_tools: false,
        }
    }

    pub fn permits(&self, name: &str, is_mock: bool) -> bool {
        self.allowed_tools.contains(name) && (is_mock || self.allow_real_tools)
    }
}

#[derive(Clone, Debug)]
pub struct ToolContext {
    pub run_id: String,
    pub session_id: String,
    pub agent_id: String,
    pub call_id: String,
    pub workspace_root: PathBuf,
    pub policy: ToolPolicySnapshot,
    pub deadline: Instant,
    pub cancellation: CancellationToken,
    pub max_output_bytes: usize,
    pub secret_handles: Vec<SecretHandle>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolMetadata {
    pub output_bytes: usize,
    pub mock: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolErrorCategory {
    InvalidInput,
    Denied,
    NotFound,
    Timeout,
    Cancelled,
    RateLimited,
    ResourceExhausted,
    CleanupFailed,
    OutcomeUnknown,
    External,
    Internal,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ToolResult {
    Success {
        value: Value,
        metadata: ToolMetadata,
    },
    Failure {
        category: ToolErrorCategory,
        message: String,
        retryable: bool,
    },
}

impl ToolResult {
    pub fn failure(category: ToolErrorCategory, message: impl Into<String>) -> Self {
        Self::Failure {
            category,
            message: message.into(),
            retryable: false,
        }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> Value;
    fn is_mock(&self) -> bool;
    async fn execute(&self, params: Value, ctx: &ToolContext) -> ToolResult;
}
