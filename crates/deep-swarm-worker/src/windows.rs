use std::{
    ffi::OsStr,
    fs::File,
    io::{self, Read},
    mem::{ManuallyDrop, size_of},
    os::windows::{ffi::OsStrExt, io::FromRawHandle},
    ptr::{null, null_mut},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::Instant,
};

use tokio_util::sync::CancellationToken;
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, GENERIC_READ, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
        SetHandleInformation, WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    Security::SECURITY_ATTRIBUTES,
    Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    },
    System::{
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_CPU_RATE_CONTROL_ENABLE,
            JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
            JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_BASIC_PROCESS_ID_LIST,
            JOBOBJECT_CPU_RATE_CONTROL_INFORMATION, JOBOBJECT_CPU_RATE_CONTROL_INFORMATION_0,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOBOBJECT_LIMIT_VIOLATION_INFORMATION_2,
            JobObjectBasicAccountingInformation, JobObjectBasicProcessIdList,
            JobObjectCpuRateControlInformation, JobObjectExtendedLimitInformation,
            JobObjectLimitViolationInformation2, QueryInformationJobObject,
            SetInformationJobObject, TerminateJobObject,
        },
        Pipes::CreatePipe,
        Threading::{
            CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
            GetExitCodeProcess, GetProcessHandleCount, OpenProcess, PROCESS_INFORMATION,
            PROCESS_QUERY_LIMITED_INFORMATION, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOW,
            TerminateProcess, WaitForSingleObject,
        },
    },
};

use crate::{
    BackendInfo, BackendKind, CommandSpec, HardLimits, ResourceKind, Result, WorkerError,
    WorkerOutcome, WorkerOutput,
};

const POLL_MS: u32 = 10;
const TERMINATED_EXIT_CODE: u32 = 0xDEED;

pub fn probe() -> Result<BackendInfo> {
    let job = create_job(&HardLimits::default())?;
    drop(job);
    Ok(BackendInfo {
        kind: BackendKind::WindowsJobObject,
        detail: "Job Object supports suspended assignment, memory/CPU/process limits, kill-on-close, and handle monitoring".into(),
    })
}

pub async fn run(
    command: CommandSpec,
    limits: HardLimits,
    cancellation: CancellationToken,
) -> Result<WorkerOutcome> {
    let process = create_process(&command, &limits)?;
    tokio::task::spawn_blocking(move || supervise(process, limits, cancellation))
        .await
        .map_err(|error| WorkerError::SpawnFailed(format!("supervisor task failed: {error}")))
}

struct OwnedHandle(HANDLE);

unsafe impl Send for OwnedHandle {}

impl OwnedHandle {
    fn new(handle: HANDLE) -> io::Result<Self> {
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(handle))
        }
    }

    fn into_file(self) -> File {
        let this = ManuallyDrop::new(self);
        // SAFETY: ownership of the valid handle moves from OwnedHandle to File exactly once.
        unsafe { File::from_raw_handle(this.0.cast()) }
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: OwnedHandle is created only for valid, uniquely owned handles.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

struct WindowsProcess {
    job: OwnedHandle,
    process: OwnedHandle,
    stdout: File,
    stderr: File,
}

fn create_job(limits: &HardLimits) -> Result<OwnedHandle> {
    // SAFETY: null security/name pointers request an unnamed job with default security.
    let job = OwnedHandle::new(unsafe { CreateJobObjectW(null(), null()) })
        .map_err(|error| backend_error("CreateJobObjectW", error))?;
    let mut extended = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    extended.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        | JOB_OBJECT_LIMIT_JOB_MEMORY
        | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
    extended.BasicLimitInformation.ActiveProcessLimit = limits.max_processes;
    extended.JobMemoryLimit = usize::try_from(limits.memory_bytes).map_err(|_| {
        WorkerError::InvalidInput("memory_bytes does not fit the current platform".into())
    })?;
    set_job_information(job.0, JobObjectExtendedLimitInformation, &extended)
        .map_err(|error| backend_error("setting Job Object memory/process limits", error))?;

    let cpu = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION {
        ControlFlags: JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
        Anonymous: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION_0 {
            CpuRate: limits.cpu_percent * 100,
        },
    };
    set_job_information(job.0, JobObjectCpuRateControlInformation, &cpu)
        .map_err(|error| backend_error("setting Job Object CPU limit", error))?;
    Ok(job)
}

fn set_job_information<T>(job: HANDLE, class: i32, value: &T) -> io::Result<()> {
    // SAFETY: value points to the structure required by class for its exact byte length.
    if unsafe {
        SetInformationJobObject(
            job,
            class,
            (value as *const T).cast(),
            size_of::<T>() as u32,
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn create_process(command: &CommandSpec, limits: &HardLimits) -> Result<WindowsProcess> {
    let job = create_job(limits)?;
    let (stdout_read, stdout_write) = pipe().map_err(spawn_io("creating stdout pipe"))?;
    let (stderr_read, stderr_write) = pipe().map_err(spawn_io("creating stderr pipe"))?;
    let stdin = null_input().map_err(spawn_io("opening NUL for stdin"))?;
    let application = wide_null(command.program.as_os_str());
    let mut command_line = command_line(command);
    let environment = environment_block(command);
    let current_dir = command
        .current_dir
        .as_ref()
        .map(|path| wide_null(path.as_os_str()));
    let current_dir_ptr = current_dir.as_ref().map_or(null(), |path| path.as_ptr());
    let startup = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        dwFlags: STARTF_USESTDHANDLES,
        hStdInput: stdin.0,
        hStdOutput: stdout_write.0,
        hStdError: stderr_write.0,
        ..Default::default()
    };
    let mut information = PROCESS_INFORMATION::default();

    // SAFETY: all pointers reference live, NUL-terminated buffers; inherited handles are valid.
    let created = unsafe {
        CreateProcessW(
            application.as_ptr(),
            command_line.as_mut_ptr(),
            null(),
            null(),
            1,
            CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
            environment.as_ptr().cast(),
            current_dir_ptr,
            &startup,
            &mut information,
        )
    };
    if created == 0 {
        return Err(WorkerError::SpawnFailed(format!(
            "CreateProcessW failed: {}",
            io::Error::last_os_error()
        )));
    }
    let process = OwnedHandle::new(information.hProcess)
        .map_err(|error| WorkerError::SpawnFailed(error.to_string()))?;
    let thread = OwnedHandle::new(information.hThread)
        .map_err(|error| WorkerError::SpawnFailed(error.to_string()))?;

    // SAFETY: the process is suspended, and both handles are valid.
    if unsafe { AssignProcessToJobObject(job.0, process.0) } == 0 {
        // SAFETY: this is the still-suspended process created above.
        unsafe {
            TerminateProcess(process.0, TERMINATED_EXIT_CODE);
        }
        return Err(backend_error(
            "AssignProcessToJobObject",
            io::Error::last_os_error(),
        ));
    }
    // SAFETY: this is the primary thread returned by CreateProcessW and remains suspended.
    if unsafe { ResumeThread(thread.0) } == u32::MAX {
        // SAFETY: terminating the job closes the only process tree started here.
        unsafe {
            TerminateJobObject(job.0, TERMINATED_EXIT_CODE);
        }
        return Err(WorkerError::SpawnFailed(format!(
            "ResumeThread failed: {}",
            io::Error::last_os_error()
        )));
    }
    drop(thread);
    drop(stdin);
    drop(stdout_write);
    drop(stderr_write);
    Ok(WindowsProcess {
        job,
        process,
        stdout: stdout_read.into_file(),
        stderr: stderr_read.into_file(),
    })
}

fn pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let mut read = null_mut();
    let mut write = null_mut();
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 1,
    };
    // SAFETY: output pointers and security attributes are valid for this call.
    if unsafe { CreatePipe(&mut read, &mut write, &attributes, 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let read = OwnedHandle::new(read)?;
    let write = OwnedHandle::new(write)?;
    // SAFETY: read is valid; clearing inheritance keeps only the child write end inheritable.
    if unsafe { SetHandleInformation(read.0, HANDLE_FLAG_INHERIT, 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((read, write))
}

fn null_input() -> io::Result<OwnedHandle> {
    let path = wide_null(OsStr::new("NUL"));
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 1,
    };
    // SAFETY: path and security attributes are valid; no template handle is used.
    OwnedHandle::new(unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &attributes,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    })
}

fn wide_null(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain([0]).collect()
}

fn command_line(command: &CommandSpec) -> Vec<u16> {
    let mut line = quote_argument(command.program.as_os_str());
    for argument in &command.args {
        line.push(b' ' as u16);
        line.extend(quote_argument(argument));
    }
    line.push(0);
    line
}

fn quote_argument(argument: &OsStr) -> Vec<u16> {
    let units: Vec<_> = argument.encode_wide().collect();
    if !units.is_empty()
        && !units
            .iter()
            .any(|unit| *unit == b' ' as u16 || *unit == b'\t' as u16 || *unit == b'"' as u16)
    {
        return units;
    }
    let mut quoted = vec![b'"' as u16];
    let mut slashes = 0;
    for unit in units {
        if unit == b'\\' as u16 {
            slashes += 1;
        } else if unit == b'"' as u16 {
            quoted.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2 + 1));
            quoted.push(unit);
            slashes = 0;
        } else {
            quoted.extend(std::iter::repeat_n(b'\\' as u16, slashes));
            quoted.push(unit);
            slashes = 0;
        }
    }
    quoted.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
    quoted.push(b'"' as u16);
    quoted
}

fn environment_block(command: &CommandSpec) -> Vec<u16> {
    let mut block = Vec::new();
    for (key, value) in &command.environment {
        block.extend(key.encode_wide());
        block.push(b'=' as u16);
        block.extend(value.encode_wide());
        block.push(0);
    }
    block.push(0);
    if block.len() == 1 {
        block.push(0);
    }
    block
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

    fn reader(&self, mut file: File, destination: Arc<Mutex<Vec<u8>>>) -> thread::JoinHandle<()> {
        let total = self.total.clone();
        let exceeded = self.exceeded.clone();
        let limit = self.limit;
        thread::spawn(move || {
            let mut chunk = [0; 8192];
            while let Ok(read) = file.read(&mut chunk) {
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

#[derive(Clone, Copy)]
enum PendingOutcome {
    TimedOut,
    Resource(ResourceKind),
    Cancelled,
    Failed,
}

fn supervise(
    process: WindowsProcess,
    limits: HardLimits,
    cancellation: CancellationToken,
) -> WorkerOutcome {
    let capture = Capture::new(limits.max_output_bytes);
    let stdout_reader = capture.reader(process.stdout, capture.stdout.clone());
    let stderr_reader = capture.reader(process.stderr, capture.stderr.clone());
    let started = Instant::now();
    let mut cancellation_started = None;
    let mut pending = None;
    let exit_code = loop {
        // SAFETY: process handle remains valid for the duration of supervision.
        match unsafe { WaitForSingleObject(process.process.0, POLL_MS) } {
            WAIT_OBJECT_0 => break process_exit_code(process.process.0),
            WAIT_TIMEOUT => {}
            _ => {
                pending = Some(PendingOutcome::Failed);
                terminate_job(process.job.0);
                break process_exit_code(process.process.0);
            }
        }
        if capture.exceeded.load(Ordering::Acquire) {
            pending = Some(PendingOutcome::Resource(ResourceKind::Output));
            terminate_job(process.job.0);
        } else {
            match job_handle_count(process.job.0, limits.max_processes) {
                Err(_) => {
                    pending = Some(PendingOutcome::Failed);
                    terminate_job(process.job.0);
                }
                Ok(count) if count > limits.max_handles as u64 => {
                    pending = Some(PendingOutcome::Resource(ResourceKind::Handles));
                    terminate_job(process.job.0);
                }
                Ok(_) if cancellation.is_cancelled() => {
                    pending.get_or_insert(PendingOutcome::Cancelled);
                    let cancelled_at = cancellation_started.get_or_insert_with(Instant::now);
                    if cancelled_at.elapsed() >= limits.cancellation_grace {
                        terminate_job(process.job.0);
                    }
                }
                Ok(_) if started.elapsed() >= limits.timeout => {
                    pending = Some(PendingOutcome::TimedOut);
                    terminate_job(process.job.0);
                }
                Ok(_) => {}
            }
        }
    };

    let violation = job_violation(process.job.0);
    let cleanup_error = if terminate_job(process.job.0) {
        wait_for_empty_job(process.job.0).err()
    } else {
        Some(io::Error::last_os_error())
    };
    let _ = stdout_reader.join();
    let _ = stderr_reader.join();
    if pending.is_none() && capture.exceeded.load(Ordering::Acquire) {
        pending = Some(PendingOutcome::Resource(ResourceKind::Output));
    }
    let output = capture.output(exit_code);
    if let Some(error) = cleanup_error {
        return WorkerOutcome::CleanupFailed {
            output,
            reason: error.to_string(),
        };
    }
    match pending {
        Some(PendingOutcome::TimedOut) => WorkerOutcome::TimedOut(output),
        Some(PendingOutcome::Resource(resource)) => {
            WorkerOutcome::ResourceExhausted { output, resource }
        }
        Some(PendingOutcome::Cancelled) => WorkerOutcome::Cancelled(output),
        Some(PendingOutcome::Failed) => WorkerOutcome::Failed {
            output,
            reason: "WaitForSingleObject failed".into(),
        },
        None if violation.is_some() => WorkerOutcome::ResourceExhausted {
            output,
            resource: violation.unwrap_or(ResourceKind::Memory),
        },
        None if exit_code == Some(0) => WorkerOutcome::Succeeded(output),
        None => WorkerOutcome::Failed {
            reason: format!("process exited with code {exit_code:?}"),
            output,
        },
    }
}

fn terminate_job(job: HANDLE) -> bool {
    // SAFETY: job is a valid Job Object owned by the supervisor.
    unsafe { TerminateJobObject(job, TERMINATED_EXIT_CODE) != 0 }
}

fn wait_for_empty_job(job: HANDLE) -> io::Result<()> {
    for _ in 0..100 {
        let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        // SAFETY: accounting has the exact layout and size requested by the information class.
        if unsafe {
            QueryInformationJobObject(
                job,
                JobObjectBasicAccountingInformation,
                (&mut accounting as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if accounting.ActiveProcesses == 0 {
            return Ok(());
        }
        thread::sleep(std::time::Duration::from_millis(POLL_MS.into()));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "Job Object still contains active processes",
    ))
}

fn process_exit_code(process: HANDLE) -> Option<i32> {
    let mut code = 0;
    // SAFETY: process is valid and code points to writable storage.
    if unsafe { GetExitCodeProcess(process, &mut code) } == 0 {
        None
    } else {
        Some(code as i32)
    }
}

fn job_violation(job: HANDLE) -> Option<ResourceKind> {
    let mut violation = JOBOBJECT_LIMIT_VIOLATION_INFORMATION_2::default();
    // SAFETY: violation has the exact layout and size requested by the information class.
    if unsafe {
        QueryInformationJobObject(
            job,
            JobObjectLimitViolationInformation2,
            (&mut violation as *mut JOBOBJECT_LIMIT_VIOLATION_INFORMATION_2).cast(),
            size_of::<JOBOBJECT_LIMIT_VIOLATION_INFORMATION_2>() as u32,
            null_mut(),
        )
    } == 0
    {
        None
    } else if violation.ViolationLimitFlags & JOB_OBJECT_LIMIT_JOB_MEMORY != 0 {
        Some(ResourceKind::Memory)
    } else if violation.ViolationLimitFlags & JOB_OBJECT_LIMIT_ACTIVE_PROCESS != 0 {
        Some(ResourceKind::ProcessCount)
    } else {
        None
    }
}

fn job_handle_count(job: HANDLE, max_processes: u32) -> io::Result<u64> {
    let words = 2 + max_processes as usize;
    let mut storage = vec![0usize; words];
    // SAFETY: storage is writable and large enough for the bounded process list.
    if unsafe {
        QueryInformationJobObject(
            job,
            JobObjectBasicProcessIdList,
            storage.as_mut_ptr().cast(),
            (storage.len() * size_of::<usize>()) as u32,
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful query initialized the header and reported list entries.
    let list = unsafe { &*(storage.as_ptr().cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>()) };
    let mut total = 0u64;
    for index in 0..list.NumberOfProcessIdsInList as usize {
        // SAFETY: the query cannot return more entries than the supplied buffer.
        let process_id = unsafe { *list.ProcessIdList.as_ptr().add(index) } as u32;
        // SAFETY: querying a process from this Job; false prevents handle inheritance.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
        let Ok(handle) = OwnedHandle::new(handle) else {
            continue;
        };
        let mut count = 0;
        // SAFETY: handle and output pointer are valid.
        if unsafe { GetProcessHandleCount(handle.0, &mut count) } != 0 {
            total += u64::from(count);
        }
    }
    Ok(total)
}

fn backend_error(operation: &str, error: io::Error) -> WorkerError {
    WorkerError::BackendUnavailable(format!("{operation} failed: {error}"))
}

fn spawn_io(operation: &'static str) -> impl FnOnce(io::Error) -> WorkerError {
    move |error| WorkerError::SpawnFailed(format!("{operation}: {error}"))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::Write,
        process::Command,
        time::{Duration, SystemTime},
    };

    use windows_sys::Win32::{
        Foundation::WAIT_OBJECT_0,
        Storage::FileSystem::SYNCHRONIZE,
        System::Threading::{OpenProcess, WaitForSingleObject},
    };

    use super::*;
    use crate::{CommandSpec, HardLimits, Worker};

    const HELPER_ENV: &str = "DEEP_SWARM_WORKER_HELPER";
    const PID_FILE_ENV: &str = "DEEP_SWARM_WORKER_PID_FILE";
    const HELPER_TEST: &str = "windows::tests::worker_helper";

    fn helper(mode: &str) -> CommandSpec {
        CommandSpec::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg(HELPER_TEST)
            .arg("--nocapture")
            .env(HELPER_ENV, mode)
    }

    fn limits() -> HardLimits {
        HardLimits {
            memory_bytes: 512 * 1024 * 1024,
            max_processes: 8,
            timeout: Duration::from_secs(5),
            cancellation_grace: Duration::from_millis(100),
            ..HardLimits::default()
        }
    }

    #[test]
    fn worker_helper() {
        match std::env::var(HELPER_ENV).as_deref() {
            Ok("success") => print!("worker-success"),
            Ok("sleep") | Ok("grandchild") => std::thread::sleep(Duration::from_secs(30)),
            Ok("output") => std::io::stdout().write_all(&vec![b'x'; 64 * 1024]).unwrap(),
            Ok("tree") => {
                let mut child = Command::new(std::env::current_exe().unwrap());
                child
                    .args(["--exact", HELPER_TEST, "--nocapture"])
                    .env_clear()
                    .env(HELPER_ENV, "grandchild");
                let child = child.spawn().unwrap();
                fs::write(std::env::var(PID_FILE_ENV).unwrap(), child.id().to_string()).unwrap();
                std::thread::sleep(Duration::from_secs(30));
            }
            _ => {}
        }
    }

    #[tokio::test]
    async fn probes_and_runs_in_a_job() {
        assert_eq!(Worker::probe().unwrap().kind, BackendKind::WindowsJobObject);
        let outcome = Worker
            .run(helper("success"), limits(), CancellationToken::new())
            .await
            .unwrap();
        assert!(matches!(outcome, WorkerOutcome::Succeeded(_)));
        assert!(
            outcome
                .output()
                .stdout
                .windows(14)
                .any(|part| part == b"worker-success")
        );
    }

    #[tokio::test]
    async fn timeout_terminates_the_job() {
        let mut limits = limits();
        limits.timeout = Duration::from_millis(150);
        let outcome = Worker
            .run(helper("sleep"), limits, CancellationToken::new())
            .await
            .unwrap();
        assert!(matches!(outcome, WorkerOutcome::TimedOut(_)));
    }

    #[tokio::test]
    async fn output_limit_terminates_the_job() {
        let mut limits = limits();
        limits.max_output_bytes = 1024;
        let outcome = Worker
            .run(helper("output"), limits, CancellationToken::new())
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            WorkerOutcome::ResourceExhausted {
                resource: ResourceKind::Output,
                ..
            }
        ));
        assert!(outcome.output().stdout.len() + outcome.output().stderr.len() <= 1024);
    }

    #[tokio::test]
    async fn cancellation_observes_the_grace_period() {
        let cancellation = CancellationToken::new();
        let trigger = cancellation.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            trigger.cancel();
        });
        let outcome = Worker
            .run(helper("sleep"), limits(), cancellation)
            .await
            .unwrap();
        assert!(matches!(outcome, WorkerOutcome::Cancelled(_)));
    }

    #[tokio::test]
    async fn timeout_cleans_up_descendants() {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let pid_file = std::env::temp_dir().join(format!("deep-swarm-worker-{unique}.pid"));
        let mut limits = limits();
        limits.timeout = Duration::from_millis(500);
        let outcome = Worker
            .run(
                helper("tree").env(PID_FILE_ENV, pid_file.as_os_str()),
                limits,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(matches!(outcome, WorkerOutcome::TimedOut(_)));
        let pid: u32 = fs::read_to_string(&pid_file).unwrap().parse().unwrap();
        let _ = fs::remove_file(pid_file);
        // SAFETY: query-only handle; if PID still exists it must already be signaled.
        let process = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
        if let Ok(process) = OwnedHandle::new(process) {
            // SAFETY: process is a valid synchronization handle.
            assert_eq!(unsafe { WaitForSingleObject(process.0, 0) }, WAIT_OBJECT_0);
        }
    }

    #[test]
    fn quotes_windows_arguments_without_shell_parsing() {
        assert_eq!(
            String::from_utf16(&quote_argument(OsStr::new("a b\\\"c"))).unwrap(),
            "\"a b\\\\\\\"c\""
        );
    }
}

