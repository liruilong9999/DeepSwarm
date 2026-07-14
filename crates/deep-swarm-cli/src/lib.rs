use std::{
    env, fs,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use clap::{Args, Parser, Subcommand};
use deep_swarm_core::{
    ActionExecutor, ActionOutput, CaseReport, CaseReportStatus, CaseStatus, CoreError, ErrorKind,
    FixtureRoot, InputFormat, Metrics, RecordedEvent, Recording, ReplayCall, Replayer, Report,
    ReportStatus, ReportSummary, SimilarityRegistry, SystemClock, prepare, prune_reports,
    recording_hash, render_html, render_json, render_junit, run, write_recording,
};
use deep_swarm_mock_server::{MockState, serve};
use deep_swarm_tools::{
    RunToolRegistry, ToolContext, ToolErrorCategory, ToolPolicySnapshot, ToolResult, execute_tool,
    mock_registry,
};
use serde_json::{Map, Value, json};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

const RECORDING_CONFIG: &str = "builtin-mock-v1";

#[derive(Debug, Parser)]
#[command(name = "deep-swarm", version, about = "DeepSwarm 测试与评估命令行")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Run(RunArgs),
    Mock(MockArgs),
    Record(RecordArgs),
    Replay(ReplayArgs),
}

#[derive(Clone, Debug, Args)]
pub struct RunArgs {
    pub scenario: PathBuf,
    #[arg(long, default_value = "tests/fixtures")]
    pub fixtures: PathBuf,
    #[arg(long, default_value = "reports")]
    pub report_dir: PathBuf,
    #[arg(long, default_value_t = 7, value_parser = parse_retention)]
    pub retention_days: u16,
}

#[derive(Clone, Debug, Args)]
pub struct MockArgs {
    #[arg(long, default_value_t = 0)]
    pub port: u16,
    #[arg(long, conflicts_with = "api_key_env")]
    pub api_key: Option<String>,
    #[arg(long, default_value = "DEEP_SWARM_MOCK_API_KEY")]
    pub api_key_env: String,
}

#[derive(Clone, Debug, Args)]
pub struct RecordArgs {
    pub scenario: PathBuf,
    pub output: PathBuf,
    #[arg(long, default_value = "tests/fixtures")]
    pub fixtures: PathBuf,
    #[arg(long, default_value_t = 0)]
    pub seed: u64,
}

#[derive(Clone, Debug, Args)]
pub struct ReplayArgs {
    pub recording: PathBuf,
    pub scenario: PathBuf,
    #[arg(long, default_value = "tests/fixtures")]
    pub fixtures: PathBuf,
}

pub async fn dispatch(cli: Cli) -> Result<u8> {
    match cli.command {
        Command::Run(args) => run_command(args).await,
        Command::Mock(args) => mock_command(args).await,
        Command::Record(args) => record_command(args).await,
        Command::Replay(args) => replay_command(args).await,
    }
}

async fn run_command(args: RunArgs) -> Result<u8> {
    fs::create_dir_all(&args.report_dir)
        .with_context(|| format!("无法创建报告目录 {}", args.report_dir.display()))?;
    prune_reports(&args.report_dir, args.retention_days, &SystemClock)?;
    let source = Scenario::read(&args.scenario)?;
    let fixtures = FixtureRoot::new(&args.fixtures)?;
    let cancellation = CancellationToken::new();
    let executor = ToolExecutor::new(args.fixtures.clone(), cancellation.clone());
    let result = tokio::select! {
        result = execute_scenario(&source, &fixtures, &executor) => result?,
        signal = tokio::signal::ctrl_c() => {
            signal.context("无法监听 Ctrl-C")?;
            cancellation.cancel();
            return Ok(130);
        }
    };
    let report = build_report(result, None);
    write_reports(&args.report_dir, &report)?;
    prune_reports(&args.report_dir, args.retention_days, &SystemClock)?;
    Ok(u8::from(report.status != ReportStatus::Succeeded))
}

async fn mock_command(args: MockArgs) -> Result<u8> {
    let key = match args.api_key {
        Some(key) => key,
        None => env::var(&args.api_key_env).with_context(|| {
            format!(
                "缺少 API key，请使用 --api-key 或设置环境变量 {}",
                args.api_key_env
            )
        })?,
    };
    let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), args.port))
        .await
        .context("无法绑定 Mock 回环端点")?;
    let address = listener.local_addr()?;
    println!("DeepSwarm Mock 正在监听 http://{address}");
    tokio::select! {
        result = serve(listener, MockState::new(key)) => result.context("Mock 服务失败")?,
        signal = tokio::signal::ctrl_c() => signal.context("无法监听 Ctrl-C")?,
    }
    Ok(0)
}

async fn record_command(args: RecordArgs) -> Result<u8> {
    let source = Scenario::read(&args.scenario)?;
    let fixtures = FixtureRoot::new(&args.fixtures)?;
    let cancellation = CancellationToken::new();
    let executor = RecordingExecutor::new(args.fixtures, cancellation.clone());
    let result = tokio::select! {
        result = execute_scenario(&source, &fixtures, &executor) => result?,
        signal = tokio::signal::ctrl_c() => {
            signal.context("无法监听 Ctrl-C")?;
            cancellation.cancel();
            return Ok(130);
        }
    };
    let events = executor.take_events()?;
    let recording = Recording::new(&source.value, &recording_config(), args.seed, events)?;
    write_recording(&args.output, &recording, None)?;
    println!(
        "录制完成: events={} hash={}",
        recording.events.len(),
        recording_hash(&recording)?
    );
    Ok(u8::from(execution_failed(&result)))
}

async fn replay_command(args: ReplayArgs) -> Result<u8> {
    let source = Scenario::read(&args.scenario)?;
    let fixtures = FixtureRoot::new(&args.fixtures)?;
    let bytes = fs::read(&args.recording)
        .with_context(|| format!("无法读取录制 {}", args.recording.display()))?;
    let recording: Recording = serde_json::from_slice(&bytes).context("录制文件结构无效")?;
    let event_count = recording.events.len();
    let executor = ReplayExecutor::new(Replayer::new(
        recording,
        &source.value,
        &recording_config(),
    )?);
    let result = execute_scenario(&source, &fixtures, &executor).await?;
    executor.finish()?;
    if execution_failed(&result) {
        bail!("回放断言或用例失败");
    }
    println!(
        "回放验证通过: events={event_count} cases={}",
        result.cases.len()
    );
    Ok(0)
}

struct Scenario {
    bytes: Vec<u8>,
    format: InputFormat,
    value: Value,
}

impl Scenario {
    fn read(path: &Path) -> Result<Self> {
        let bytes = fs::read(path).with_context(|| format!("无法读取场景 {}", path.display()))?;
        let format = match path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("json") => InputFormat::Json,
            Some("yaml" | "yml") => InputFormat::Yaml,
            _ => bail!("场景扩展名必须为 .json、.yaml 或 .yml"),
        };
        let value = match format {
            InputFormat::Json => serde_json::from_slice(&bytes).context("场景 JSON 无效")?,
            InputFormat::Yaml => serde_yaml::from_slice(&bytes).context("场景 YAML 无效")?,
        };
        Ok(Self {
            bytes,
            format,
            value,
        })
    }
}

async fn execute_scenario(
    source: &Scenario,
    fixtures: &FixtureRoot,
    executor: &dyn ActionExecutor,
) -> Result<deep_swarm_core::ExecutionResult> {
    let evaluators = SimilarityRegistry::default();
    let prepared = prepare(&source.bytes, source.format, fixtures, &evaluators)?;
    Ok(run(
        &prepared,
        fixtures,
        executor,
        &evaluators,
        &Metrics::default(),
    )
    .await?)
}

struct ToolExecutor {
    registry: RunToolRegistry,
    workspace_root: PathBuf,
    policy: ToolPolicySnapshot,
    cancellation: CancellationToken,
    calls: AtomicU64,
}

impl ToolExecutor {
    fn new(workspace_root: PathBuf, cancellation: CancellationToken) -> Self {
        let registry = mock_registry();
        let policy = ToolPolicySnapshot::mock_only(registry.names());
        Self {
            registry,
            workspace_root,
            policy,
            cancellation,
            calls: AtomicU64::new(0),
        }
    }

    async fn execute_raw(&self, tool: &str, parameters: Value) -> ToolResult {
        let call = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
        execute_tool(
            &self.registry,
            tool,
            parameters,
            &ToolContext {
                run_id: "cli-run".into(),
                session_id: "cli-session".into(),
                agent_id: "cli-agent".into(),
                call_id: format!("call-{call}"),
                workspace_root: self.workspace_root.clone(),
                policy: self.policy.clone(),
                deadline: tokio::time::Instant::now() + Duration::from_secs(120),
                cancellation: self.cancellation.clone(),
                max_output_bytes: 64 * 1024,
                secret_handles: Vec::new(),
            },
            None,
        )
        .await
    }
}

#[async_trait]
impl ActionExecutor for ToolExecutor {
    fn parameters_schema(&self, tool: &str) -> Option<Value> {
        self.registry.get(tool).map(|tool| tool.parameters_schema())
    }

    async fn execute(&self, tool: &str, parameters: Value) -> Result<ActionOutput, CoreError> {
        action_output(self.execute_raw(tool, parameters).await)
    }
}

struct RecordingExecutor {
    tools: ToolExecutor,
    events: Mutex<Vec<RecordedEvent>>,
}

impl RecordingExecutor {
    fn new(workspace_root: PathBuf, cancellation: CancellationToken) -> Self {
        Self {
            tools: ToolExecutor::new(workspace_root, cancellation),
            events: Mutex::new(Vec::new()),
        }
    }

    fn take_events(&self) -> Result<Vec<RecordedEvent>> {
        Ok(std::mem::take(
            &mut *self.events.lock().map_err(|_| anyhow!("录制事件锁损坏"))?,
        ))
    }
}

#[async_trait]
impl ActionExecutor for RecordingExecutor {
    fn parameters_schema(&self, tool: &str) -> Option<Value> {
        self.tools.parameters_schema(tool)
    }

    async fn execute(&self, tool: &str, parameters: Value) -> Result<ActionOutput, CoreError> {
        let started = Instant::now();
        let raw = self.tools.execute_raw(tool, parameters.clone()).await;
        let (result, error) = match &raw {
            ToolResult::Success { value, metadata } => {
                (Some(json!({"value": value, "metadata": metadata})), None)
            }
            ToolResult::Failure {
                category,
                message,
                retryable,
            } => (
                None,
                Some(json!({
                    "category": category,
                    "message": message,
                    "retryable": retryable,
                })),
            ),
        };
        self.events
            .lock()
            .map_err(|_| CoreError::new(ErrorKind::Io, "录制事件锁损坏"))?
            .push(RecordedEvent {
                sequence: 0,
                kind: "tool".into(),
                name: tool.to_owned(),
                parameters,
                result,
                error,
                duration_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
                event_hash: String::new(),
            });
        action_output(raw)
    }
}

struct ReplayExecutor(Mutex<Option<Replayer>>);

impl ReplayExecutor {
    fn new(replayer: Replayer) -> Self {
        Self(Mutex::new(Some(replayer)))
    }

    fn finish(&self) -> Result<()> {
        self.0
            .lock()
            .map_err(|_| anyhow!("回放状态锁损坏"))?
            .take()
            .ok_or_else(|| anyhow!("回放已结束"))?
            .finish()?;
        Ok(())
    }
}

#[async_trait]
impl ActionExecutor for ReplayExecutor {
    async fn execute(&self, tool: &str, parameters: Value) -> Result<ActionOutput, CoreError> {
        let event = self
            .0
            .lock()
            .map_err(|_| CoreError::new(ErrorKind::Io, "回放状态锁损坏"))?
            .as_mut()
            .ok_or_else(|| CoreError::new(ErrorKind::ReplayMismatch, "回放已结束"))?
            .next(ReplayCall {
                kind: "tool",
                name: tool,
                parameters,
                schema: None,
            })?;
        if let Some(error) = event.error {
            return Err(CoreError::new(
                ErrorKind::InvalidInput,
                format!("录制工具错误: {error}"),
            ));
        }
        let result = event
            .result
            .ok_or_else(|| CoreError::new(ErrorKind::ReplayMismatch, "录制事件缺少结果和错误"))?;
        Ok(ActionOutput {
            value: result
                .get("value")
                .cloned()
                .ok_or_else(|| CoreError::new(ErrorKind::ReplayMismatch, "录制结果缺少 value"))?,
            metadata: result.get("metadata").cloned().unwrap_or(Value::Null),
        })
    }
}

fn action_output(result: ToolResult) -> Result<ActionOutput, CoreError> {
    match result {
        ToolResult::Success { value, metadata } => Ok(ActionOutput {
            value,
            metadata: serde_json::to_value(metadata)
                .map_err(|error| CoreError::new(ErrorKind::InvalidInput, error.to_string()))?,
        }),
        ToolResult::Failure {
            category,
            message,
            retryable,
        } => Err(CoreError::new(
            match category {
                ToolErrorCategory::InvalidInput => ErrorKind::InvalidInput,
                _ => ErrorKind::Io,
            },
            format!("工具失败 {category:?} (retryable={retryable}): {message}"),
        )),
    }
}

fn build_report(
    result: deep_swarm_core::ExecutionResult,
    recording_hash: Option<String>,
) -> Report {
    let suite_failed = !result.suite_failures.is_empty();
    let mut cases = Vec::with_capacity(result.cases.len());
    for case in result.cases {
        let steps = step_reports(case.steps);
        let status = match case.status {
            CaseStatus::Failed => CaseReportStatus::Failed,
            CaseStatus::Passed
                if !steps.is_empty()
                    && steps
                        .iter()
                        .all(|step| step["status"].as_str() == Some("skipped")) =>
            {
                CaseReportStatus::Skipped
            }
            CaseStatus::Passed => CaseReportStatus::Passed,
        };
        cases.push(CaseReport {
            id: case.id,
            status,
            steps,
            error: case.error.map(|message| json!({"message": message})),
        });
    }
    let passed = cases
        .iter()
        .filter(|case| case.status == CaseReportStatus::Passed)
        .count() as u64;
    let failed = cases
        .iter()
        .filter(|case| case.status == CaseReportStatus::Failed)
        .count() as u64;
    let skipped = cases
        .iter()
        .filter(|case| case.status == CaseReportStatus::Skipped)
        .count() as u64;
    let planned = cases.len() as u64;
    Report {
        schema_version: 1,
        run_id: run_id(),
        status: if failed > 0 || suite_failed {
            ReportStatus::Failed
        } else {
            ReportStatus::Succeeded
        },
        created_at: now_rfc3339(),
        summary: ReportSummary {
            planned,
            completed: planned,
            passed,
            failed,
            skipped,
        },
        cases,
        metrics: suite_failed.then(|| json!({"suite_failures": result.suite_failures})),
        recording_hash,
        uncertain_operations: Vec::new(),
    }
}

fn step_reports(steps: Value) -> Vec<Value> {
    steps
        .as_object()
        .into_iter()
        .flat_map(Map::iter)
        .map(|(id, state)| {
            let mut state = state.as_object().cloned().unwrap_or_default();
            state.insert("id".to_owned(), Value::String(id.clone()));
            Value::Object(state)
        })
        .collect()
}

fn write_reports(directory: &Path, report: &Report) -> Result<()> {
    let base = directory.join(&report.run_id);
    fs::write(base.with_extension("json"), render_json(report, &[])?)?;
    fs::write(base.with_extension("xml"), render_junit(report, &[])?)?;
    fs::write(base.with_extension("html"), render_html(report, &[])?)?;
    Ok(())
}

fn execution_failed(result: &deep_swarm_core::ExecutionResult) -> bool {
    !result.suite_failures.is_empty()
        || result
            .cases
            .iter()
            .any(|case| case.status == CaseStatus::Failed)
}

fn recording_config() -> Value {
    json!({"tool_registry": RECORDING_CONFIG})
}

fn parse_retention(value: &str) -> std::result::Result<u16, String> {
    let days = value
        .parse::<u16>()
        .map_err(|_| "retention_days 必须为 0..=365".to_owned())?;
    if days > 365 {
        Err("retention_days 必须为 0..=365".to_owned())
    } else {
        Ok(days)
    }
}

fn run_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("run-{nanos}")
}

fn now_rfc3339() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = (seconds / 86_400) as i64;
    let day_seconds = seconds % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        day_seconds / 3_600,
        day_seconds % 3_600 / 60,
        day_seconds % 60
    )
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn exposes_only_the_four_v1_commands() {
        let command = Cli::command();
        let names = command
            .get_subcommands()
            .map(|command| command.get_name())
            .collect::<Vec<_>>();
        assert_eq!(names, ["run", "mock", "record", "replay"]);
        assert!(Cli::try_parse_from(["deep-swarm", "run", "case.yaml"]).is_ok());
        assert!(
            Cli::try_parse_from(["deep-swarm", "run", "case.yaml", "--retention-days", "366"])
                .is_err()
        );
    }

    #[test]
    fn utc_formatter_is_schema_compatible() {
        let value = now_rfc3339();
        assert_eq!(value.len(), 20);
        assert!(value.ends_with('Z'));
    }
}
