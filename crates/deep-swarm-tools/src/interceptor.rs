use serde_json::Value;

use crate::{RunToolRegistry, ToolContext, ToolErrorCategory, ToolResult};

#[derive(Clone, Debug)]
pub struct FaultInjection {
    pub result: ToolResult,
}

pub async fn execute_tool(
    registry: &RunToolRegistry,
    name: &str,
    params: Value,
    ctx: &ToolContext,
    fault: Option<&FaultInjection>,
) -> ToolResult {
    let Some(tool) = registry.get(name) else {
        return ToolResult::failure(
            ToolErrorCategory::NotFound,
            format!("unknown tool `{name}`"),
        );
    };

    let validator = match jsonschema::validator_for(&tool.parameters_schema()) {
        Ok(validator) => validator,
        Err(error) => {
            return ToolResult::failure(
                ToolErrorCategory::Internal,
                format!("invalid registered schema: {error}"),
            );
        }
    };
    if let Err(error) = validator.validate(&params) {
        return ToolResult::failure(ToolErrorCategory::InvalidInput, error.to_string());
    }
    if !ctx.policy.permits(name, tool.is_mock()) {
        return ToolResult::failure(
            ToolErrorCategory::Denied,
            format!("tool `{name}` is denied"),
        );
    }
    if ctx.cancellation.is_cancelled() {
        return ToolResult::failure(ToolErrorCategory::Cancelled, "tool call was cancelled");
    }
    if tokio::time::Instant::now() >= ctx.deadline {
        return ToolResult::failure(ToolErrorCategory::Timeout, "tool deadline elapsed");
    }
    if let Some(fault) = fault {
        return fault.result.clone();
    }

    tokio::select! {
        () = ctx.cancellation.cancelled() => {
            ToolResult::failure(ToolErrorCategory::Cancelled, "tool call was cancelled")
        }
        result = tokio::time::timeout_at(ctx.deadline, tool.execute(params, ctx)) => {
            match result {
                Ok(ToolResult::Success { value, .. })
                    if serde_json::to_vec(&value).map_or(true, |bytes| bytes.len() > ctx.max_output_bytes) =>
                {
                    ToolResult::failure(
                        ToolErrorCategory::ResourceExhausted,
                        format!("tool output exceeds {} bytes", ctx.max_output_bytes),
                    )
                }
                Ok(result) => result,
                Err(_) => ToolResult::failure(ToolErrorCategory::Timeout, "tool deadline elapsed"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, time::Duration};

    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use super::execute_tool;
    use crate::{ToolContext, ToolErrorCategory, ToolPolicySnapshot, ToolResult, mock_registry};

    fn context(policy: ToolPolicySnapshot) -> ToolContext {
        ToolContext {
            run_id: "run".into(),
            session_id: "session".into(),
            agent_id: "agent".into(),
            call_id: "call".into(),
            workspace_root: PathBuf::from("."),
            policy,
            deadline: tokio::time::Instant::now() + Duration::from_secs(1),
            cancellation: CancellationToken::new(),
            max_output_bytes: 1024,
            secret_handles: Vec::new(),
        }
    }

    #[tokio::test]
    async fn validates_parameters_before_policy() {
        let result = execute_tool(
            &mock_registry(),
            "read_file",
            json!({"unknown": true}),
            &context(ToolPolicySnapshot::default()),
            None,
        )
        .await;
        assert!(matches!(
            result,
            ToolResult::Failure {
                category: ToolErrorCategory::InvalidInput,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn denies_unlisted_tools() {
        let result = execute_tool(
            &mock_registry(),
            "git_status",
            json!({}),
            &context(ToolPolicySnapshot::default()),
            None,
        )
        .await;
        assert!(matches!(
            result,
            ToolResult::Failure {
                category: ToolErrorCategory::Denied,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn enforces_output_budget_in_one_place() {
        let mut ctx = context(ToolPolicySnapshot::mock_only(["git_status"]));
        ctx.max_output_bytes = 1;
        let result = execute_tool(&mock_registry(), "git_status", json!({}), &ctx, None).await;
        assert!(matches!(
            result,
            ToolResult::Failure {
                category: ToolErrorCategory::ResourceExhausted,
                ..
            }
        ));
    }
}
