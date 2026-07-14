use std::{
    env, fs,
    net::{SocketAddr, TcpListener as StdTcpListener, TcpStream},
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
    time::Duration,
};

use deep_swarm_bench::{BenchmarkConfig, Scenario, benchmark_mock_reply, run_scenario};
use deep_swarm_mock_server::{MockState, serve};
use tokio::net::TcpListener;

const API_KEY: &str = "benchmark-key";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    if args.first().is_some_and(|arg| arg == "--mock-child") {
        return mock_child(&args[1..]).await;
    }
    let options = Options::parse(&args)?;
    let mut reports = Vec::new();
    for scenario in options.scenarios.clone() {
        let mut config = if options.self_check {
            BenchmarkConfig::self_check(scenario)
        } else {
            BenchmarkConfig::for_scenario(scenario)
        };
        options.apply(&mut config);
        config.mock_isolated = true;
        let (mut child, base_url) = spawn_mock(config.planned_steps() + 8)?;
        let result = run_scenario(config, base_url).await;
        let _ = child.kill();
        let _ = child.wait();
        reports.push(result?);
    }
    let json = serde_json::to_string_pretty(&reports)?;
    if let Some(path) = options.output {
        fs::write(path, &json)?;
    }
    println!("{json}");
    if reports
        .iter()
        .any(|report| report.thresholds_met == Some(false))
    {
        std::process::exit(2);
    }
    Ok(())
}

async fn mock_child(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let address = value(args, "--addr")?.parse::<SocketAddr>()?;
    let responses = value(args, "--responses")?.parse::<usize>()?;
    // ponytail: O(steps * response size) queue; use a programmable fallback once MockState exposes one.
    let replies = std::iter::repeat_with(benchmark_mock_reply).take(responses);
    let state = MockState::with_replies(API_KEY, replies);
    serve(TcpListener::bind(address).await?, state).await?;
    Ok(())
}

fn spawn_mock(responses: usize) -> Result<(Child, String), Box<dyn std::error::Error>> {
    let address = {
        let listener = StdTcpListener::bind("127.0.0.1:0")?;
        listener.local_addr()?
    };
    let child = Command::new(env::current_exe()?)
        .arg("--mock-child")
        .arg("--addr")
        .arg(address.to_string())
        .arg("--responses")
        .arg(responses.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()?;
    for _ in 0..100 {
        if TcpStream::connect_timeout(&address, Duration::from_millis(20)).is_ok() {
            return Ok((child, format!("http://{address}")));
        }
        thread::sleep(Duration::from_millis(20));
    }
    Err("mock child did not start".into())
}

struct Options {
    scenarios: Vec<Scenario>,
    warmup: Option<Duration>,
    steady: Option<Duration>,
    burst: Option<Duration>,
    post_load: Option<Duration>,
    steady_rate: Option<f64>,
    burst_rate: Option<f64>,
    output: Option<PathBuf>,
    self_check: bool,
}

impl Options {
    fn parse(args: &[String]) -> Result<Self, Box<dyn std::error::Error>> {
        let scenario = optional_value(args, "--scenario").unwrap_or("all");
        let scenarios = match scenario {
            "a" | "A" => vec![Scenario::A],
            "b" | "B" => vec![Scenario::B],
            "all" => vec![Scenario::A, Scenario::B],
            _ => return Err("--scenario must be a, b, or all".into()),
        };
        Ok(Self {
            scenarios,
            warmup: seconds(args, "--warmup-secs")?,
            steady: seconds(args, "--steady-secs")?,
            burst: seconds(args, "--burst-secs")?,
            post_load: seconds(args, "--post-load-secs")?,
            steady_rate: number(args, "--steady-rate")?,
            burst_rate: number(args, "--burst-rate")?,
            output: optional_value(args, "--output").map(PathBuf::from),
            self_check: args.iter().any(|arg| arg == "--self-check"),
        })
    }

    fn apply(&self, config: &mut BenchmarkConfig) {
        if let Some(value) = self.warmup {
            config.warmup = value;
        }
        if let Some(value) = self.steady {
            config.steady = value;
        }
        if let Some(value) = self.burst {
            config.burst = value;
        }
        if let Some(value) = self.post_load {
            config.post_load = value;
        }
        if let Some(value) = self.steady_rate {
            config.steady_rate = value;
        }
        if let Some(value) = self.burst_rate {
            config.burst_rate = value;
        }
    }
}

fn seconds(args: &[String], name: &str) -> Result<Option<Duration>, Box<dyn std::error::Error>> {
    let value = number(args, name)?;
    if value.is_some_and(|value| !value.is_finite() || value < 0.0) {
        return Err(format!("{name} must be a finite non-negative number").into());
    }
    Ok(value.map(Duration::from_secs_f64))
}

fn number(args: &[String], name: &str) -> Result<Option<f64>, Box<dyn std::error::Error>> {
    optional_value(args, name)
        .map(str::parse)
        .transpose()
        .map_err(Into::into)
}

fn optional_value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|arg| arg == name)
        .and_then(|index| args.get(index + 1))
        .map(String::as_str)
}

fn value<'a>(args: &'a [String], name: &str) -> Result<&'a str, Box<dyn std::error::Error>> {
    optional_value(args, name).ok_or_else(|| format!("missing {name}").into())
}
