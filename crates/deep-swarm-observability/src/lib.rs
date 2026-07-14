use std::{
    error::Error,
    sync::OnceLock,
    time::Duration,
};

use opentelemetry::{
    KeyValue,
    metrics::{Counter, Histogram, MeterProvider as _, UpDownCounter},
};
use opentelemetry_sdk::metrics::SdkMeterProvider;
use prometheus::{Encoder, Registry, TextEncoder};
use tracing::Span;
use tracing_subscriber::EnvFilter;

pub fn init_json_logging() -> Result<(), Box<dyn Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .try_init()?;
    Ok(())
}

pub fn agent_span(session_id: u64, agent_id: u64) -> Span {
    tracing::info_span!(
        "agent",
        session_id,
        agent_id,
        trace_id = %format_args!("agent-{session_id}-{agent_id}")
    )
}

pub fn tool_span(
    run_id: &str,
    session_id: &str,
    agent_id: &str,
    trace_id: &str,
    call_id: &str,
    tool: &str,
) -> Span {
    tracing::info_span!(
        "tool_call",
        run_id,
        session_id,
        agent_id,
        trace_id,
        call_id,
        tool
    )
}

pub struct Observability {
    _provider: SdkMeterProvider,
    registry: Registry,
    active_agents: UpDownCounter<i64>,
    tool_calls: Counter<u64>,
    tool_latency: Histogram<f64>,
}

impl Observability {
    pub fn new() -> Result<Self, Box<dyn Error + Send + Sync>> {
        let registry = Registry::new();
        let exporter = opentelemetry_prometheus::exporter()
            .with_registry(registry.clone())
            .build()?;
        let provider = SdkMeterProvider::builder().with_reader(exporter).build();
        let meter = provider.meter("deep-swarm");
        let active_agents = meter
            .i64_up_down_counter("deepswarm_agents_active")
            .with_description("Current DeepSwarm agent actor count")
            .build();
        let tool_calls = meter
            .u64_counter("deepswarm_tool_calls")
            .with_description("DeepSwarm tool call outcomes")
            .build();
        let tool_latency = meter
            .f64_histogram("deepswarm_tool_call_duration")
            .with_description("DeepSwarm tool call duration")
            .with_unit("s")
            .build();
        Ok(Self {
            _provider: provider,
            registry,
            active_agents,
            tool_calls,
            tool_latency,
        })
    }

    pub fn agent_guard(&self) -> ActiveAgentGuard<'_> {
        self.active_agents.add(1, &[]);
        ActiveAgentGuard { metrics: self }
    }

    pub fn record_tool_call(&self, tool: &str, succeeded: bool, elapsed: Duration) {
        let attributes = [
            KeyValue::new("tool", tool.to_owned()),
            KeyValue::new("outcome", if succeeded { "success" } else { "failure" }),
        ];
        self.tool_calls.add(1, &attributes);
        self.tool_latency.record(elapsed.as_secs_f64(), &attributes);
    }

    pub fn prometheus_text(&self) -> Result<String, Box<dyn Error + Send + Sync>> {
        let families = self.registry.gather();
        let mut bytes = Vec::new();
        TextEncoder::new().encode(&families, &mut bytes)?;
        Ok(String::from_utf8(bytes)?)
    }
}

pub struct ActiveAgentGuard<'a> {
    metrics: &'a Observability,
}

impl Drop for ActiveAgentGuard<'_> {
    fn drop(&mut self) {
        self.metrics.active_agents.add(-1, &[]);
    }
}

pub fn global() -> &'static Observability {
    static GLOBAL: OnceLock<Observability> = OnceLock::new();
    GLOBAL.get_or_init(|| Observability::new().expect("observability initialization must succeed"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exports_agent_count_tool_outcomes_and_latency() {
        let metrics = Observability::new().unwrap();
        let guard = metrics.agent_guard();
        metrics.record_tool_call("read_file", true, Duration::from_millis(4));
        metrics.record_tool_call("run_shell", false, Duration::from_millis(8));

        let active = metrics.prometheus_text().unwrap();
        assert!(active.contains("deepswarm_agents_active"), "missing agents_active metric");
        assert!(active.contains("deepswarm_tool_calls_total"), "missing tool_calls metric");
        assert!(active.contains(r#"outcome="success""#), "missing success outcome");
        assert!(active.contains(r#"outcome="failure""#), "missing failure outcome");
        assert!(active.contains("deepswarm_tool_call_duration_seconds_bucket"), "missing duration metric");

        drop(guard);
        let after_drop = metrics.prometheus_text().unwrap();
        assert!(after_drop.contains("deepswarm_agents_active"), "agents should still exist after drop");
    }
}
