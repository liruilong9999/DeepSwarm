use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{RunBuilder, RunToolRegistry, Tool, ToolContext, ToolMetadata, ToolResult};

type Responder = fn(&Value) -> Value;

struct MockTool {
    name: &'static str,
    description: &'static str,
    schema: Value,
    respond: Responder,
}

#[async_trait]
impl Tool for MockTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        self.description
    }

    fn parameters_schema(&self) -> Value {
        self.schema.clone()
    }

    fn is_mock(&self) -> bool {
        true
    }

    async fn execute(&self, params: Value, _ctx: &ToolContext) -> ToolResult {
        let value = (self.respond)(&params);
        ToolResult::Success {
            metadata: ToolMetadata {
                output_bytes: value.to_string().len(),
                mock: true,
            },
            value,
        }
    }
}

fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

fn tool(
    name: &'static str,
    description: &'static str,
    properties: Value,
    required: &[&str],
    respond: Responder,
) -> Arc<dyn Tool> {
    Arc::new(MockTool {
        name,
        description,
        schema: object_schema(properties, required),
        respond,
    })
}

macro_rules! static_mock {
    ($module:ident, $name:literal, $description:literal, $value:expr) => {
        mod $module {
            use super::*;

            fn respond(_: &Value) -> Value {
                $value
            }

            pub fn create() -> Arc<dyn Tool> {
                tool($name, $description, json!({}), &[], respond)
            }
        }
    };
}

mod read_file {
    use super::*;

    fn respond(params: &Value) -> Value {
        json!({"path": params["path"], "content": "", "bytes": 0})
    }

    pub fn create() -> Arc<dyn Tool> {
        tool(
            "read_file",
            "Return a deterministic mock file",
            json!({"path": {"type": "string", "minLength": 1}}),
            &["path"],
            respond,
        )
    }
}

mod grep_files {
    use super::*;

    fn respond(params: &Value) -> Value {
        json!({"pattern": params["pattern"], "matches": []})
    }

    pub fn create() -> Arc<dyn Tool> {
        tool(
            "grep_files",
            "Return deterministic mock search results",
            json!({"pattern": {"type": "string"}}),
            &["pattern"],
            respond,
        )
    }
}

mod run_shell {
    use super::*;

    fn respond(params: &Value) -> Value {
        json!({"program": params["program"], "exit_code": 0, "stdout": "", "stderr": ""})
    }

    pub fn create() -> Arc<dyn Tool> {
        tool(
            "run_shell",
            "Return a successful mock process result",
            json!({
                "program": {"type": "string", "minLength": 1},
                "args": {"type": "array", "items": {"type": "string"}, "default": []}
            }),
            &["program"],
            respond,
        )
    }
}

static_mock!(
    git_status,
    "git_status",
    "Return a clean mock repository",
    json!({"clean": true, "branch": "main"})
);

mod task_create {
    use super::*;

    fn respond(params: &Value) -> Value {
        let digest = Sha256::digest(params.to_string().as_bytes());
        json!({"id": format!("task-{:x}", digest)[..21].to_owned(), "name": params["name"]})
    }

    pub fn create() -> Arc<dyn Tool> {
        tool(
            "task_create",
            "Create a deterministic mock task",
            json!({"name": {"type": "string", "minLength": 1}}),
            &["name"],
            respond,
        )
    }
}

static_mock!(
    pr_attempt_record,
    "pr_attempt_record",
    "Record a mock pull request attempt",
    json!({"recorded": true})
);
static_mock!(
    github_issue_context,
    "github_issue_context",
    "Return empty mock issue context",
    json!({"issue": null})
);
static_mock!(
    automation_list,
    "automation_list",
    "Return mock automations",
    json!({"automations": []})
);
static_mock!(
    update_plan,
    "update_plan",
    "Update a mock plan",
    json!({"updated": true})
);
static_mock!(
    agent_open,
    "agent_open",
    "Open a mock child agent",
    json!({"agent_id": "mock-agent"})
);
static_mock!(
    rlm_open,
    "rlm_open",
    "Open a mock RLM session",
    json!({"rlm_id": "mock-rlm"})
);

mod diagnostics {
    use super::*;

    fn respond(params: &Value) -> Value {
        let subject = params
            .get("subject_id")
            .and_then(Value::as_str)
            .unwrap_or("system");
        json!({"summary": format!("{subject}: ok")})
    }

    pub fn create() -> Arc<dyn Tool> {
        tool(
            "diagnostics",
            "Return deterministic mock diagnostics",
            json!({"subject_id": {"type": "string"}}),
            &[],
            respond,
        )
    }
}

pub fn builtin_mock_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        read_file::create(),
        grep_files::create(),
        run_shell::create(),
        git_status::create(),
        task_create::create(),
        pr_attempt_record::create(),
        github_issue_context::create(),
        automation_list::create(),
        update_plan::create(),
        agent_open::create(),
        rlm_open::create(),
        diagnostics::create(),
    ]
}

pub fn mock_registry() -> RunToolRegistry {
    RunBuilder::with_tools(builtin_mock_tools())
        .expect("built-in schemas are valid")
        .build()
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, path::PathBuf, time::Duration};

    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{ToolContext, ToolPolicySnapshot, execute_tool};

    #[test]
    fn exposes_exactly_the_twelve_v1_tools() {
        let names: BTreeSet<_> = builtin_mock_tools()
            .into_iter()
            .map(|tool| tool.name().to_owned())
            .collect();
        assert_eq!(names.len(), 12);
        assert!(names.contains("read_file"));
        assert!(names.contains("diagnostics"));
    }

    #[tokio::test]
    async fn mock_task_ids_are_stable() {
        let policy = ToolPolicySnapshot::mock_only(["task_create"]);
        let context = ToolContext {
            run_id: "run".into(),
            session_id: "session".into(),
            agent_id: "agent".into(),
            call_id: "call".into(),
            workspace_root: PathBuf::from("."),
            policy,
            deadline: tokio::time::Instant::now() + Duration::from_secs(1),
            cancellation: CancellationToken::new(),
            secret_handles: Vec::new(),
        };
        let first = execute_tool(
            &mock_registry(),
            "task_create",
            json!({"name": "x"}),
            &context,
            None,
        )
        .await;
        let second = execute_tool(
            &mock_registry(),
            "task_create",
            json!({"name": "x"}),
            &context,
            None,
        )
        .await;
        assert_eq!(first, second);
    }
}
