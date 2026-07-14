use std::{ffi::OsString, io::Write, path::PathBuf, process::ExitCode, time::Duration};

use deep_swarm_worker::{CommandSpec, HardLimits, Worker, WorkerOutcome};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => ExitCode::from(code),
        Err(message) => {
            eprintln!("deep-swarm-worker: {message}");
            ExitCode::from(6)
        }
    }
}

async fn run() -> std::result::Result<u8, String> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.as_slice() == [OsString::from("--probe")] {
        let backend = Worker::probe().map_err(|error| error.to_string())?;
        println!("{:?}: {}", backend.kind, backend.detail);
        return Ok(0);
    }

    let (limits, command) = parse(args)?;
    let cancellation = CancellationToken::new();
    let signal = cancellation.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal.cancel();
        }
    });
    let outcome = Worker
        .run(command, limits, cancellation)
        .await
        .map_err(|error| error.to_string())?;
    std::io::stdout()
        .write_all(&outcome.output().stdout)
        .map_err(|error| error.to_string())?;
    std::io::stderr()
        .write_all(&outcome.output().stderr)
        .map_err(|error| error.to_string())?;
    Ok(match outcome {
        WorkerOutcome::Succeeded(_) => 0,
        WorkerOutcome::Failed { reason, .. } => {
            eprintln!("worker failed: {reason}");
            1
        }
        WorkerOutcome::TimedOut(_) => 2,
        WorkerOutcome::ResourceExhausted { resource, .. } => {
            eprintln!("worker resource exhausted: {resource:?}");
            3
        }
        WorkerOutcome::Cancelled(_) => 4,
        WorkerOutcome::CleanupFailed { reason, .. } => {
            eprintln!("worker cleanup failed: {reason}");
            5
        }
    })
}

fn parse(args: Vec<OsString>) -> std::result::Result<(HardLimits, CommandSpec), String> {
    let mut limits = HardLimits::default();
    let mut environment = Vec::new();
    let mut current_dir: Option<PathBuf> = None;
    let separator = args
        .iter()
        .position(|arg| arg == "--")
        .ok_or("missing '--' before program")?;
    let (flags, command) = args.split_at(separator);
    let command = &command[1..];
    let Some(program) = command.first() else {
        return Err("missing program after '--'".into());
    };

    let mut index = 0;
    while index < flags.len() {
        let flag = flags[index].to_string_lossy();
        let value = flags
            .get(index + 1)
            .ok_or_else(|| format!("missing value for {flag}"))?;
        match flag.as_ref() {
            "--memory-bytes" => limits.memory_bytes = number(value, &flag)?,
            "--cpu-percent" => limits.cpu_percent = number(value, &flag)?,
            "--max-processes" => limits.max_processes = number(value, &flag)?,
            "--max-file-descriptors" => limits.max_file_descriptors = number(value, &flag)?,
            "--max-handles" => limits.max_handles = number(value, &flag)?,
            "--timeout-ms" => limits.timeout = Duration::from_millis(number(value, &flag)?),
            "--cancel-grace-ms" => {
                limits.cancellation_grace = Duration::from_millis(number(value, &flag)?)
            }
            "--max-output-bytes" => limits.max_output_bytes = number(value, &flag)?,
            "--env" => environment.push(split_environment(value)?),
            "--current-dir" => current_dir = Some(value.clone().into()),
            _ => return Err(format!("unknown option: {flag}")),
        }
        index += 2;
    }

    let mut spec = CommandSpec::new(program.clone()).args(command.iter().skip(1).cloned());
    for (key, value) in environment {
        spec = spec.env(key, value);
    }
    if let Some(path) = current_dir {
        spec = spec.current_dir(path);
    }
    Ok((limits, spec))
}

fn number<T>(value: &OsString, flag: &str) -> std::result::Result<T, String>
where
    T: std::str::FromStr,
{
    value
        .to_string_lossy()
        .parse()
        .map_err(|_| format!("invalid numeric value for {flag}"))
}

fn split_environment(value: &OsString) -> std::result::Result<(OsString, OsString), String> {
    let value = value.to_string_lossy();
    let (key, value) = value.split_once('=').ok_or("--env requires KEY=VALUE")?;
    Ok((key.into(), value.into()))
}
