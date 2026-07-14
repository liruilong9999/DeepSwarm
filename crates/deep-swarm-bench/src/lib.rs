use std::{
    collections::HashMap,
    process::Command,
    sync::{Arc, Mutex},
    time::Duration,
};

use deep_swarm_agent::{AgentSpec, AgentState, SessionManager, SessionManagerConfig};
use deep_swarm_client::{
    DeepSeekClient, RetryPolicy,
    models::{ChatChoice, ChatCompletionRequest, ChatCompletionResponse, ChatMessage, Usage},
};
use deep_swarm_core::nearest_rank_p95;
use deep_swarm_mock_server::{MockReply, MockResponse};
use deep_swarm_tools::{ToolContext, ToolPolicySnapshot, ToolResult, execute_tool, mock_registry};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::{
    sync::{Semaphore, mpsc},
    task::JoinHandle,
    time::Instant,
};
use tokio_util::sync::CancellationToken;

const API_KEY: &str = "benchmark-key";
const RESPONSE_BYTES: usize = 1024;

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Scenario {
    A,
    B,
}

impl Scenario {
    pub fn dimensions(self) -> (usize, usize) {
        match self {
            Self::A => (1, 500),
            Self::B => (100, 5),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::A => "A",
            Self::B => "B",
        }
    }
}

#[derive(Clone, Debug)]
pub struct BenchmarkConfig {
    pub scenario: Scenario,
    pub warmup: Duration,
    pub steady: Duration,
    pub burst: Duration,
    pub post_load: Duration,
    pub steady_rate: f64,
    pub burst_rate: f64,
    pub drain_timeout: Duration,
    pub mock_isolated: bool,
}

impl BenchmarkConfig {
    pub fn for_scenario(scenario: Scenario) -> Self {
        Self {
            scenario,
            warmup: Duration::from_secs(5 * 60),
            steady: Duration::from_secs(30 * 60),
            burst: Duration::from_secs(5 * 60),
            post_load: Duration::from_secs(5 * 60),
            steady_rate: 200.0,
            burst_rate: 250.0,
            drain_timeout: Duration::from_secs(60),
            mock_isolated: true,
        }
    }

    pub fn planned_steps(&self) -> usize {
        phase_steps(self.warmup, self.steady_rate)
            + phase_steps(self.steady, self.steady_rate)
            + phase_steps(self.burst, self.burst_rate)
    }

    pub fn self_check(scenario: Scenario) -> Self {
        Self {
            scenario,
            warmup: Duration::from_millis(20),
            steady: Duration::from_millis(50),
            burst: Duration::from_millis(20),
            post_load: Duration::ZERO,
            steady_rate: 100.0,
            burst_rate: 100.0,
            drain_timeout: Duration::from_secs(2),
            mock_isolated: false,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct BenchmarkReport {
    pub scenario: String,
    pub sessions: usize,
    pub agents: usize,
    pub planned_steps: usize,
    pub terminal_steps: usize,
    pub duration_seconds: f64,
    pub steady: PhaseReport,
    pub burst: PhaseReport,
    pub completion_rate: f64,
    pub error_rate: f64,
    pub peak_rss_bytes: u64,
    pub steady_rss_median_bytes: u64,
    pub post_load_rss_bytes: u64,
    pub drain_seconds: f64,
    pub environment: EnvironmentReport,
    pub thresholds_met: Option<bool>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct PhaseReport {
    pub planned_steps: usize,
    pub terminal_steps: usize,
    pub errors: usize,
    pub throughput_steps_per_sec: f64,
    pub latency_p95_ms: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct EnvironmentReport {
    pub compliant: bool,
    pub status: String,
    pub rustc: String,
    pub cpu: String,
    pub kernel: String,
    pub commit: String,
    pub build_parameters: String,
}

#[derive(Clone, Debug)]
struct StepResult {
    phase: &'static str,
    latency_ms: f64,
    failed: bool,
}

#[derive(Default)]
struct Accumulator {
    planned: usize,
    terminal: usize,
    errors: usize,
    latency_ms: Vec<f64>,
}

#[derive(Clone)]
struct StepRuntime {
    client: DeepSeekClient,
    request: ChatCompletionRequest,
    registry: deep_swarm_tools::RunToolRegistry,
    gates: Arc<HashMap<u64, Arc<Semaphore>>>,
}

pub async fn run_scenario(
    config: BenchmarkConfig,
    mock_base_url: impl Into<String>,
) -> Result<BenchmarkReport, String> {
    validate_config(&config)?;
    let (sessions, agents_per_session) = config.scenario.dimensions();
    let manager = SessionManager::new(SessionManagerConfig {
        max_sessions: sessions,
        max_agents_per_session: agents_per_session,
        global_concurrency: 200,
        completion_channel_capacity: 1024,
    })
    .map_err(|error| error.to_string())?;
    let mut gates = HashMap::with_capacity(sessions * agents_per_session);
    for _ in 0..sessions {
        let (session_id, first) = manager
            .create_session(AgentSpec::default())
            .map_err(|error| error.to_string())?;
        prepare_agent(&first).await?;
        gates.insert(first.id(), Arc::new(Semaphore::new(1)));
        for _ in 1..agents_per_session {
            let agent = manager
                .create_agent(session_id, AgentSpec::default())
                .map_err(|error| error.to_string())?;
            prepare_agent(&agent).await?;
            gates.insert(agent.id(), Arc::new(Semaphore::new(1)));
        }
    }

    let runtime = StepRuntime {
        client: DeepSeekClient::with_base_url(API_KEY, mock_base_url)
            .map_err(|error| error.to_string())?
            .with_retry_policy(RetryPolicy::no_delay()),
        request: ChatCompletionRequest::new(
            "deepseek-v4-pro",
            vec![ChatMessage::user("x".repeat(RESPONSE_BYTES))],
        ),
        registry: mock_registry(),
        gates: Arc::new(gates),
    };
    let dispatcher = manager.dispatcher();
    let (results_tx, mut results_rx) = mpsc::unbounded_channel();
    let samples = Arc::new(Mutex::new(Vec::new()));
    let sample_stop = CancellationToken::new();
    let sampler = spawn_rss_sampler(samples.clone(), sample_stop.clone());
    let started = Instant::now();

    schedule_phase(
        "warmup",
        config.warmup,
        config.steady_rate,
        &dispatcher,
        &runtime,
        &results_tx,
    )
    .await;
    let steady_start = started.elapsed();
    schedule_phase(
        "steady",
        config.steady,
        config.steady_rate,
        &dispatcher,
        &runtime,
        &results_tx,
    )
    .await;
    let steady_end = started.elapsed();
    schedule_phase(
        "burst",
        config.burst,
        config.burst_rate,
        &dispatcher,
        &runtime,
        &results_tx,
    )
    .await;
    drop(results_tx);

    let planned = config.planned_steps();
    let drain_started = Instant::now();
    let deadline = drain_started + config.drain_timeout;
    let mut by_phase: HashMap<&'static str, Accumulator> = HashMap::from([
        (
            "warmup",
            Accumulator {
                planned: phase_steps(config.warmup, config.steady_rate),
                ..Accumulator::default()
            },
        ),
        (
            "steady",
            Accumulator {
                planned: phase_steps(config.steady, config.steady_rate),
                ..Accumulator::default()
            },
        ),
        (
            "burst",
            Accumulator {
                planned: phase_steps(config.burst, config.burst_rate),
                ..Accumulator::default()
            },
        ),
    ]);
    let mut terminal = 0;
    while terminal < planned {
        let Some(result) = tokio::time::timeout_at(deadline, results_rx.recv())
            .await
            .map_err(|_| "backlog did not drain before the configured deadline".to_owned())?
        else {
            break;
        };
        terminal += 1;
        let phase = by_phase.get_mut(result.phase).expect("known phase");
        phase.terminal += 1;
        phase.errors += usize::from(result.failed);
        phase.latency_ms.push(result.latency_ms);
    }
    let drain_seconds = drain_started.elapsed().as_secs_f64();
    let load_duration = started.elapsed();
    if !config.post_load.is_zero() {
        tokio::time::sleep(config.post_load).await;
    }
    sample_stop.cancel();
    let _ = sampler.await;
    let mut rss = samples.lock().expect("RSS samples lock").clone();
    rss.push((started.elapsed(), current_rss_bytes()));
    manager
        .cancel_all("benchmark complete")
        .await
        .map_err(|error| error.to_string())?;

    let errors = by_phase.values().map(|phase| phase.errors).sum::<usize>();
    let peak_rss = rss.iter().map(|(_, value)| *value).max().unwrap_or(0);
    let stable_baseline_start = steady_end.saturating_sub(Duration::from_secs(5 * 60));
    let steady_rss = rss
        .iter()
        .filter(|(at, _)| *at >= steady_start.max(stable_baseline_start) && *at <= steady_end)
        .map(|(_, value)| *value)
        .collect::<Vec<_>>();
    let steady_rss_median = median(&steady_rss);
    let post_load_rss = rss.last().map(|(_, value)| *value).unwrap_or(0);
    let environment = environment_report(&config);
    let steady = summarize(by_phase.get("steady").expect("steady phase"), config.steady);
    let burst = summarize(by_phase.get("burst").expect("burst phase"), config.burst);
    let completion_rate = ratio(terminal, planned);
    let error_rate = ratio(errors, planned);
    let thresholds_met = environment.compliant.then(|| {
        steady.throughput_steps_per_sec >= 195.0
            && steady.latency_p95_ms <= 200.0
            && peak_rss <= 4 * 1024 * 1024 * 1024
            && error_rate <= 0.001
            && completion_rate >= 0.999
            && drain_seconds <= 60.0
            && (steady_rss_median == 0 || post_load_rss as f64 <= steady_rss_median as f64 * 1.10)
    });

    Ok(BenchmarkReport {
        scenario: config.scenario.name().into(),
        sessions,
        agents: sessions * agents_per_session,
        planned_steps: planned,
        terminal_steps: terminal,
        duration_seconds: load_duration.as_secs_f64(),
        steady,
        burst,
        completion_rate,
        error_rate,
        peak_rss_bytes: peak_rss,
        steady_rss_median_bytes: steady_rss_median,
        post_load_rss_bytes: post_load_rss,
        drain_seconds,
        environment,
        thresholds_met,
    })
}

async fn prepare_agent(agent: &deep_swarm_agent::AgentHandle) -> Result<(), String> {
    agent
        .transition(AgentState::Ready, "benchmark ready")
        .await
        .map_err(|error| error.to_string())?;
    agent
        .transition(AgentState::Running, "benchmark running")
        .await
        .map_err(|error| error.to_string())
}

async fn schedule_phase(
    phase: &'static str,
    duration: Duration,
    rate: f64,
    dispatcher: &deep_swarm_agent::FairDispatcher,
    runtime: &StepRuntime,
    results: &mpsc::UnboundedSender<StepResult>,
) {
    let steps = phase_steps(duration, rate);
    let phase_started = Instant::now();
    for index in 0..steps {
        let scheduled = phase_started + Duration::from_secs_f64(index as f64 / rate);
        tokio::time::sleep_until(scheduled).await;
        let Ok(agent_id) = dispatcher.try_dispatch(format!("{phase}-{index}")) else {
            let _ = results.send(StepResult {
                phase,
                latency_ms: scheduled.elapsed().as_secs_f64() * 1000.0,
                failed: true,
            });
            continue;
        };
        let runtime = runtime.clone();
        let results = results.clone();
        tokio::spawn(async move {
            let failed = run_step(agent_id, index, &runtime).await.is_err();
            let _ = results.send(StepResult {
                phase,
                latency_ms: scheduled.elapsed().as_secs_f64() * 1000.0,
                failed,
            });
        });
    }
    tokio::time::sleep_until(phase_started + duration).await;
}

async fn run_step(agent_id: u64, index: usize, runtime: &StepRuntime) -> Result<(), String> {
    let gate = runtime
        .gates
        .get(&agent_id)
        .ok_or_else(|| "dispatcher returned an unknown agent".to_owned())?
        .clone();
    let _permit = gate
        .acquire_owned()
        .await
        .map_err(|_| "agent gate closed".to_owned())?;

    let response = runtime
        .client
        .chat_completion(&runtime.request)
        .await
        .map_err(|error| error.to_string())?;
    let content = response
        .choices
        .first()
        .and_then(|choice| choice.message.content.as_deref())
        .ok_or_else(|| "mock response did not contain content".to_owned())?;
    if content.len() != RESPONSE_BYTES {
        return Err(format!("mock response was {} bytes", content.len()));
    }

    let context = ToolContext {
        run_id: "benchmark".into(),
        session_id: "benchmark".into(),
        agent_id: agent_id.to_string(),
        call_id: format!("call-{index}"),
        workspace_root: std::env::temp_dir(),
        policy: ToolPolicySnapshot::mock_only(["diagnostics"]),
        deadline: tokio::time::Instant::now() + Duration::from_secs(5),
        cancellation: CancellationToken::new(),
        max_output_bytes: 1024,
        secret_handles: Vec::new(),
    };
    match execute_tool(
        &runtime.registry,
        "diagnostics",
        json!({"subject_id": agent_id.to_string()}),
        &context,
        None,
    )
    .await
    {
        ToolResult::Success { .. } => Ok(()),
        result => Err(format!("mock tool failed: {result:?}")),
    }
}

pub fn benchmark_mock_reply() -> MockReply {
    MockReply::immediate(MockResponse::Chat(ChatCompletionResponse {
        id: "chatcmpl-benchmark".into(),
        choices: vec![ChatChoice {
            index: 0,
            message: ChatMessage::assistant("x".repeat(RESPONSE_BYTES)),
            finish_reason: Some("stop".into()),
            logprobs: None,
        }],
        created: 0,
        model: "deepseek-v4-pro".into(),
        system_fingerprint: Some("benchmark".into()),
        object: Some("chat.completion".into()),
        usage: Some(Usage {
            prompt_tokens: 1,
            completion_tokens: 1,
            total_tokens: 2,
            ..Usage::default()
        }),
    }))
}

fn phase_steps(duration: Duration, rate: f64) -> usize {
    (duration.as_secs_f64() * rate).round() as usize
}

fn validate_config(config: &BenchmarkConfig) -> Result<(), String> {
    if !config.steady_rate.is_finite()
        || !config.burst_rate.is_finite()
        || config.steady_rate <= 0.0
        || config.burst_rate <= 0.0
        || config.drain_timeout.is_zero()
    {
        return Err("rates and drain timeout must be positive".into());
    }
    Ok(())
}

fn summarize(stats: &Accumulator, duration: Duration) -> PhaseReport {
    PhaseReport {
        planned_steps: stats.planned,
        terminal_steps: stats.terminal,
        errors: stats.errors,
        throughput_steps_per_sec: stats.terminal as f64 / duration.as_secs_f64(),
        latency_p95_ms: nearest_rank_p95(&stats.latency_ms).unwrap_or(0.0),
    }
}

fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        1.0
    } else {
        numerator as f64 / denominator as f64
    }
}

fn median(values: &[u64]) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let mut values = values.to_vec();
    values.sort_unstable();
    values[values.len() / 2]
}

fn spawn_rss_sampler(
    samples: Arc<Mutex<Vec<(Duration, u64)>>>,
    stop: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let started = Instant::now();
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = stop.cancelled() => break,
                _ = interval.tick() => {
                    samples
                        .lock()
                        .expect("RSS samples lock")
                        .push((started.elapsed(), current_rss_bytes()));
                }
            }
        }
    })
}

fn current_rss_bytes() -> u64 {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|status| {
                status
                    .lines()
                    .find(|line| line.starts_with("VmRSS:"))
                    .and_then(|line| line.split_whitespace().nth(1))
                    .and_then(|value| value.parse::<u64>().ok())
            })
            .unwrap_or(0)
            * 1024
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

fn environment_report(config: &BenchmarkConfig) -> EnvironmentReport {
    let compliant = cfg!(target_os = "linux")
        && !cfg!(debug_assertions)
        && config.mock_isolated
        && std::env::var("DEEP_SWARM_PERF_RUNNER").as_deref() == Ok("1");
    EnvironmentReport {
        compliant,
        status: if compliant {
            "compliant".into()
        } else {
            "environment_mismatch".into()
        },
        rustc: command_output("rustc", &["-Vv"]),
        cpu: cpu_name(),
        kernel: if cfg!(target_os = "windows") {
            std::env::var("OS").unwrap_or_else(|_| "unknown".into())
        } else {
            command_output("uname", &["-sr"])
        },
        commit: command_output("git", &["rev-parse", "HEAD"]),
        build_parameters: format!(
            "release={}, scenario={}, warmup={:?}, steady={:?}, burst={:?}, steady_rate={}, burst_rate={}",
            !cfg!(debug_assertions),
            config.scenario.name(),
            config.warmup,
            config.steady,
            config.burst,
            config.steady_rate,
            config.burst_rate
        ),
    }
}

fn command_output(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|output| !output.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

fn cpu_name() -> String {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/cpuinfo")
            .ok()
            .and_then(|info| {
                info.lines()
                    .find(|line| line.starts_with("model name"))
                    .and_then(|line| line.split_once(':'))
                    .map(|(_, value)| value.trim().to_owned())
            })
            .unwrap_or_else(|| "unknown".into())
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_else(|_| "unknown".into())
    }
}

#[cfg(test)]
mod tests {
    use deep_swarm_mock_server::{MockServer, MockState};

    use super::*;

    #[tokio::test]
    async fn short_self_check_runs_both_scenarios_without_false_failure() {
        for scenario in [Scenario::A, Scenario::B] {
            let mut config = BenchmarkConfig::self_check(scenario);
            let replies =
                std::iter::repeat_with(benchmark_mock_reply).take(config.planned_steps() + 8);
            let server = MockServer::start(MockState::with_replies(API_KEY, replies))
                .await
                .unwrap();
            config.mock_isolated = false;
            let report = run_scenario(config, server.base_url()).await.unwrap();
            assert_eq!(report.sessions * (report.agents / report.sessions), 500);
            assert_eq!(report.planned_steps, report.terminal_steps);
            assert_eq!(report.thresholds_met, None);
            assert_eq!(report.environment.status, "environment_mismatch");
        }
    }
}

