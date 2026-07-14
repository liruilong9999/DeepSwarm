use std::{
    ffi::CString,
    fs, io,
    os::unix::{ffi::OsStrExt, process::CommandExt},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use tokio::{io::AsyncReadExt, process::Command, time::Instant};
use tokio_util::sync::CancellationToken;

use crate::{
    BackendInfo, BackendKind, CommandSpec, HardLimits, ResourceKind, Result, WorkerError,
    WorkerOutcome, WorkerOutput,
};

static NEXT_CGROUP: AtomicU64 = AtomicU64::new(1);
const POLL_INTERVAL: Duration = Duration::from_millis(10);

pub fn probe() -> Result<BackendInfo> {
    let mut cgroup = Cgroup::create(&HardLimits::default())?;
    cgroup.cleanup().map_err(|error| {
        WorkerError::BackendUnavailable(format!("cgroup cleanup failed: {error}"))
    })?;
    Ok(BackendInfo {
        kind: BackendKind::LinuxCgroupV2,
        detail: "cgroup v2 supports memory/CPU/process limits and cgroup.kill; RLIMIT_NOFILE is applied before exec".into(),
    })
}

pub async fn run(
    command: CommandSpec,
    limits: HardLimits,
    cancellation: CancellationToken,
) -> Result<WorkerOutcome> {
    let mut cgroup = Cgroup::create(&limits)?;
    let mut process = Command::new(&command.program);
    process
        .args(&command.args)
        .env_clear()
        .envs(&command.environment)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(current_dir) = &command.current_dir {
        process.current_dir(current_dir);
    }
    install_pre_exec(&mut process, &cgroup.path, limits.max_file_descriptors)?;
    let mut child = process
        .spawn()
        .map_err(|error| WorkerError::SpawnFailed(error.to_string()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| WorkerError::SpawnFailed("stdout pipe unavailable".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| WorkerError::SpawnFailed("stderr pipe unavailable".into()))?;
    let capture = Capture::new(limits.max_output_bytes);
    let stdout_reader = capture.reader(stdout, capture.stdout.clone());
    let stderr_reader = capture.reader(stderr, capture.stderr.clone());
    let started = Instant::now();
    let mut cancelled_at = None;
    let mut pending = None;
    let status = loop {
        tokio::select! {
            status = child.wait() => {
                break status.map_err(|error| WorkerError::SpawnFailed(error.to_string()))?;
            }
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
        }
        if capture.exceeded.load(Ordering::Acquire) {
            pending = Some(PendingOutcome::Resource(ResourceKind::Output));
            cgroup
                .kill()
                .map_err(|error| WorkerError::SpawnFailed(error.to_string()))?;
        } else if cancellation.is_cancelled() {
            pending.get_or_insert(PendingOutcome::Cancelled);
            let cancelled_at = cancelled_at.get_or_insert_with(Instant::now);
            if cancelled_at.elapsed() >= limits.cancellation_grace {
                cgroup
                    .kill()
                    .map_err(|error| WorkerError::SpawnFailed(error.to_string()))?;
            }
        } else if started.elapsed() >= limits.timeout {
            pending = Some(PendingOutcome::TimedOut);
            cgroup
                .kill()
                .map_err(|error| WorkerError::SpawnFailed(error.to_string()))?;
        }
    };

    let violation = cgroup.violation();
    let cleanup = cgroup.cleanup();
    let _ = stdout_reader.await;
    let _ = stderr_reader.await;
    if pending.is_none() && capture.exceeded.load(Ordering::Acquire) {
        pending = Some(PendingOutcome::Resource(ResourceKind::Output));
    }
    let output = capture.output(status.code());
    if let Err(error) = cleanup {
        return Ok(WorkerOutcome::CleanupFailed {
            output,
            reason: error.to_string(),
        });
    }
    Ok(match pending {
        Some(PendingOutcome::TimedOut) => WorkerOutcome::TimedOut(output),
        Some(PendingOutcome::Resource(resource)) => {
            WorkerOutcome::ResourceExhausted { output, resource }
        }
        Some(PendingOutcome::Cancelled) => WorkerOutcome::Cancelled(output),
        None if violation.is_some() => WorkerOutcome::ResourceExhausted {
            output,
            resource: violation.unwrap_or(ResourceKind::Memory),
        },
        None if status.success() => WorkerOutcome::Succeeded(output),
        None => WorkerOutcome::Failed {
            reason: format!("process exited with status {status}"),
            output,
        },
    })
}

#[derive(Clone, Copy)]
enum PendingOutcome {
    TimedOut,
    Resource(ResourceKind),
    Cancelled,
}

struct Cgroup {
    path: PathBuf,
    cleaned: bool,
}

impl Cgroup {
    fn create(limits: &HardLimits) -> Result<Self> {
        let root = std::env::var_os("DEEP_SWARM_CGROUP_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/sys/fs/cgroup"));
        let controllers = fs::read_to_string(root.join("cgroup.controllers")).map_err(|error| {
            WorkerError::BackendUnavailable(format!("cgroup v2 is unavailable: {error}"))
        })?;
        for required in ["memory", "cpu", "pids"] {
            if !controllers
                .split_whitespace()
                .any(|value| value == required)
            {
                return Err(WorkerError::BackendUnavailable(format!(
                    "cgroup v2 controller '{required}' is unavailable"
                )));
            }
        }
        let id = NEXT_CGROUP.fetch_add(1, Ordering::Relaxed);
        let path = root.join(format!("deep-swarm-{}-{id}", std::process::id()));
        fs::create_dir(&path).map_err(|error| {
            WorkerError::BackendUnavailable(format!("cannot create delegated cgroup: {error}"))
        })?;
        let configured = (|| -> io::Result<()> {
            fs::write(path.join("memory.max"), limits.memory_bytes.to_string())?;
            fs::write(
                path.join("cpu.max"),
                format!("{} 100000", limits.cpu_percent * 1000),
            )?;
            fs::write(path.join("pids.max"), limits.max_processes.to_string())?;
            if !path.join("cgroup.kill").is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "cgroup.kill is unavailable",
                ));
            }
            Ok(())
        })();
        if let Err(error) = configured {
            let _ = fs::remove_dir(&path);
            return Err(WorkerError::BackendUnavailable(format!(
                "cannot configure delegated cgroup: {error}"
            )));
        }
        Ok(Self {
            path,
            cleaned: false,
        })
    }

    fn kill(&self) -> io::Result<()> {
        fs::write(self.path.join("cgroup.kill"), "1")
    }

    fn violation(&self) -> Option<ResourceKind> {
        if event_count(&self.path.join("memory.events"), "oom_kill") > 0
            || event_count(&self.path.join("memory.events"), "max") > 0
        {
            Some(ResourceKind::Memory)
        } else if event_count(&self.path.join("pids.events"), "max") > 0 {
            Some(ResourceKind::ProcessCount)
        } else {
            None
        }
    }

    fn cleanup(&mut self) -> io::Result<()> {
        if self.cleaned {
            return Ok(());
        }
        self.kill()?;
        for _ in 0..20 {
            if fs::read_to_string(self.path.join("cgroup.procs"))?
                .trim()
                .is_empty()
            {
                fs::remove_dir(&self.path)?;
                self.cleaned = true;
                return Ok(());
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "cgroup still contains processes after termination",
        ))
    }
}

impl Drop for Cgroup {
    fn drop(&mut self) {
        if !self.cleaned {
            let _ = self.kill();
            let _ = fs::remove_dir(&self.path);
        }
    }
}

fn event_count(path: &Path, name: &str) -> u64 {
    fs::read_to_string(path)
        .ok()
        .and_then(|events| {
            events.lines().find_map(|line| {
                let (key, value) = line.split_once(' ')?;
                (key == name).then(|| value.parse().ok()).flatten()
            })
        })
        .unwrap_or(0)
}

fn install_pre_exec(process: &mut Command, cgroup: &Path, max_fds: u64) -> Result<()> {
    let cgroup_procs = CString::new(cgroup.join("cgroup.procs").as_os_str().as_bytes())
        .map_err(|_| WorkerError::InvalidInput("cgroup path contains NUL".into()))?;
    let max_fds = libc::rlim_t::try_from(max_fds).map_err(|_| {
        WorkerError::InvalidInput("max_file_descriptors does not fit this platform".into())
    })?;
    // SAFETY: the closure calls only async-signal-safe libc functions between fork and exec.
    unsafe {
        process.pre_exec(move || {
            let limit = libc::rlimit {
                rlim_cur: max_fds,
                rlim_max: max_fds,
            };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                return Err(io::Error::last_os_error());
            }
            let fd = libc::open(cgroup_procs.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let mut buffer = [0u8; 32];
            let written = format_pid(libc::getpid() as u32, &mut buffer);
            let result = libc::write(fd, buffer.as_ptr().cast(), written);
            let write_error = io::Error::last_os_error();
            libc::close(fd);
            if result != written as isize {
                return Err(write_error);
            }
            Ok(())
        });
    }
    Ok(())
}

fn format_pid(mut pid: u32, buffer: &mut [u8; 32]) -> usize {
    let mut cursor = buffer.len() - 1;
    buffer[cursor] = b'\n';
    if pid == 0 {
        cursor -= 1;
        buffer[cursor] = b'0';
    } else {
        while pid > 0 {
            cursor -= 1;
            buffer[cursor] = b'0' + (pid % 10) as u8;
            pid /= 10;
        }
    }
    let length = buffer.len() - cursor;
    buffer.copy_within(cursor.., 0);
    length
}

struct Capture {
    stdout: Arc<Mutex<Vec<u8>>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    total: Arc<AtomicUsize>,
    exceeded: Arc<AtomicBool>,
    limit: usize,
}

impl Capture {
    fn new(limit: usize) -> Self {
        Self {
            stdout: Arc::new(Mutex::new(Vec::new())),
            stderr: Arc::new(Mutex::new(Vec::new())),
            total: Arc::new(AtomicUsize::new(0)),
            exceeded: Arc::new(AtomicBool::new(false)),
            limit,
        }
    }

    fn reader<R>(
        &self,
        mut reader: R,
        destination: Arc<Mutex<Vec<u8>>>,
    ) -> tokio::task::JoinHandle<()>
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
    {
        let total = self.total.clone();
        let exceeded = self.exceeded.clone();
        let limit = self.limit;
        tokio::spawn(async move {
            let mut chunk = [0; 8192];
            loop {
                let Ok(read) = reader.read(&mut chunk).await else {
                    break;
                };
                if read == 0 {
                    break;
                }
                let before = total.fetch_add(read, Ordering::AcqRel);
                let keep = limit.saturating_sub(before).min(read);
                destination
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .extend_from_slice(&chunk[..keep]);
                if keep < read {
                    exceeded.store(true, Ordering::Release);
                    break;
                }
            }
        })
    }

    fn output(&self, exit_code: Option<i32>) -> WorkerOutput {
        WorkerOutput {
            stdout: self
                .stdout
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
            stderr: self
                .stderr
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
            exit_code,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_pid_without_allocating_after_fork() {
        let mut buffer = [0; 32];
        let length = format_pid(12345, &mut buffer);
        assert_eq!(&buffer[..length], b"12345\n");
    }
}
