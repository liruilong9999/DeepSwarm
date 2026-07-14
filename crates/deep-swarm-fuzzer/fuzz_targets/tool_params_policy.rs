#![no_main]

use std::{sync::OnceLock, time::Duration};

use arbitrary::{Arbitrary, Unstructured};
use deep_swarm_fuzzer::{bounded_bytes, bounded_str, json_depth_within, value_within_limits};
use deep_swarm_tools::{ToolContext, ToolPolicySnapshot, ToolResult, execute_tool, mock_registry};
use libfuzzer_sys::fuzz_target;
use serde_json::{Value, json};
use tokio::runtime::Runtime;
use tokio_util::sync::CancellationToken;

#[derive(Arbitrary, Debug)]
struct PolicyInput {
    known_tool: bool,
    valid_params: bool,
    allowed: bool,
    subject: String,
}

fuzz_target!(|data: &[u8]| {
    let data = bounded_bytes(data);
    if json_depth_within(data) {
        if let Ok(value) = serde_json::from_slice::<Value>(data) {
            if value_within_limits(&value, 0) {
                let first = invoke("diagnostics", value.clone(), false);
                let second = invoke("diagnostics", value, false);
                assert_eq!(first, second);
                assert!(!matches!(first, ToolResult::Success { .. }));
            }
        }
    }

    let mut unstructured = Unstructured::new(data);
    let Ok(generated) = PolicyInput::arbitrary(&mut unstructured) else {
        return;
    };
    let name = if generated.known_tool {
        "diagnostics"
    } else {
        "unknown"
    };
    let params = if generated.valid_params {
        json!({"subject_id": bounded_str(&generated.subject)})
    } else {
        json!({"unknown": bounded_str(&generated.subject)})
    };
    let first = invoke(name, params.clone(), generated.allowed);
    let second = invoke(name, params, generated.allowed);
    assert_eq!(first, second);
    assert_eq!(
        matches!(first, ToolResult::Success { .. }),
        generated.known_tool && generated.valid_params && generated.allowed
    );
});

fn invoke(name: &str, params: Value, allowed: bool) -> ToolResult {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    let policy = if allowed {
        ToolPolicySnapshot::mock_only(["diagnostics"])
    } else {
        ToolPolicySnapshot::default()
    };
    let context = ToolContext {
        run_id: "fuzz".into(),
        session_id: "fuzz".into(),
        agent_id: "fuzz".into(),
        call_id: "fuzz".into(),
        workspace_root: std::env::temp_dir(),
        policy,
        deadline: tokio::time::Instant::now() + Duration::from_secs(60),
        cancellation: CancellationToken::new(),
        max_output_bytes: 64 * 1024,
        secret_handles: Vec::new(),
    };
    RUNTIME
        .get_or_init(|| Runtime::new().expect("fuzz runtime"))
        .block_on(execute_tool(&mock_registry(), name, params, &context, None))
}

