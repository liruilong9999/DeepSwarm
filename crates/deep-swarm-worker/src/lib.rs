use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    path::PathBuf,
    time::Duration,
};

use tokio_util::sync::CancellationToken;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(not(any(target_os = "linux", windows)))]
mod unsupported;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(not(any(target_os = "linux", windows)))]
use unsupported as platform;
#[cfg(windows)]
use windows as platform;

pub const MIN_CANCELLATION_GRACE: Duration = Duration::from_millis(100);
pub const MAX_CANCELLATION_GRACE: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    WindowsJobObject,
    LinuxCgroupV2,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendInfo {
    pub kind: BackendKind,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HardLimits {
    pub memory_bytes: u64,
    pub cpu_percent: u32,
    pub max_processes: u32,
    pub max_file_descriptors: u64,
    pub max_handles: u32,
    pub timeout: Duration,
    pub cancellation_grace: Duration,
    pub max_output_bytes: usize,
}

impl Default for HardLimits {
    fn default() -> Self {
        Self {
            memory_bytes: 512 * 1024 * 1024,
            cpu_percent: 100,
            max_processes: 32,
            max_file_descriptors: 256,
            max_handles: 4096,
            timeout: Duration::from_secs(120),
            cancellation_grace: Duration::from_secs(5),
            max_output_bytes: 1024 * 1024,
        }
    }
}

impl HardLimits {
    pub fn validate(&self) -> Result<()> {
        if self.memory_bytes == 0
            || self.max_processes == 0
            || self.max_processes > 1024
            || self.max_file_descriptors < 3
            || self.max_handles == 0
            || self.timeout.is_zero()
            || self.max_output_bytes == 0
        {
            return Err(WorkerError::InvalidInput(
                "hard limits must be non-zero, max_processes must not exceed 1024, and max_file_descriptors must be at least 3".into(),
            ));
        }
        if !(1..=100).contains(&self.cpu_percent) {
            return Err(WorkerError::InvalidInput(
                "cpu_percent must be between 1 and 100".into(),
            ));
        }
        if !(MIN_CANCELLATION_GRACE..=MAX_CANCELLATION_GRACE).contains(&self.cancellation_grace) {
            return Err(WorkerError::InvalidInput(
                "cancellation_grace must be between 100 ms and 60 s".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub environment: BTreeMap<OsString, OsString>,
    pub current_dir: Option<PathBuf>,
}

impl CommandSpec {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            environment: BTreeMap::new(),
            current_dir: None,
        }
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args(mut self, args: impl IntoIterator<Item = impl Into<OsString>>) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.environment.insert(key.into(), value.into());
        self
    }

    pub fn current_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(path.into());
        self
    }

    fn validate(&self) -> Result<()> {
        if self.program.as_os_str().is_empty() || contains_nul(self.program.as_os_str()) {
            return Err(WorkerError::InvalidInput(
                "program must be non-empty and contain no NUL".into(),
            ));
        }
        if self.args.iter().any(|arg| contains_nul(arg)) {
            return Err(WorkerError::InvalidInput(
                "arguments must contain no NUL".into(),
            ));
        }
        for (key, value) in &self.environment {
            if key.is_empty()
                || key.to_string_lossy().contains('=')
                || contains_nul(key)
                || contains_nul(value)
            {
                return Err(WorkerError::InvalidInput(
                    "environment keys must be non-empty and contain neither '=' nor NUL; values must contain no NUL".into(),
                ));
            }
        }
        Ok(())
    }
}

fn contains_nul(value: &OsStr) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        value.encode_wide().any(|unit| unit == 0)
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::ffi::OsStrExt;
        value.as_bytes().contains(&0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    Memory,
    ProcessCount,
    FileDescriptors,
    Handles,
    Output,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkerOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerOutcome {
    Succeeded(WorkerOutput),
    Failed {
        output: WorkerOutput,
        reason: String,
    },
    TimedOut(WorkerOutput),
    ResourceExhausted {
        output: WorkerOutput,
        resource: ResourceKind,
    },
    Cancelled(WorkerOutput),
    CleanupFailed {
        output: WorkerOutput,
        reason: String,
    },
}

impl WorkerOutcome {
    pub fn output(&self) -> &WorkerOutput {
        match self {
            Self::Succeeded(output)
            | Self::TimedOut(output)
            | Self::Cancelled(output)
            | Self::Failed { output, .. }
            | Self::ResourceExhausted { output, .. }
            | Self::CleanupFailed { output, .. } => output,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("invalid worker input: {0}")]
    InvalidInput(String),
    #[error("hard isolation backend unavailable: {0}")]
    BackendUnavailable(String),
    #[error("failed to start isolated process: {0}")]
    SpawnFailed(String),
}

pub type Result<T> = std::result::Result<T, WorkerError>;

#[derive(Debug, Default, Clone, Copy)]
pub struct Worker;

impl Worker {
    pub fn probe() -> Result<BackendInfo> {
        platform::probe()
    }

    pub async fn run(
        &self,
        command: CommandSpec,
        limits: HardLimits,
        cancellation: CancellationToken,
    ) -> Result<WorkerOutcome> {
        command.validate()?;
        limits.validate()?;
        platform::run(command, limits, cancellation).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_limits_and_command_parts() {
        let mut limits = HardLimits::default();
        limits.cpu_percent = 0;
        assert!(matches!(
            limits.validate(),
            Err(WorkerError::InvalidInput(_))
        ));

        let command = CommandSpec::new("program").arg(OsString::from("bad\0arg"));
        assert!(matches!(
            command.validate(),
            Err(WorkerError::InvalidInput(_))
        ));
    }
}
