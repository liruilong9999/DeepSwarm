use tokio_util::sync::CancellationToken;

use crate::{BackendInfo, CommandSpec, HardLimits, Result, WorkerError, WorkerOutcome};

pub fn probe() -> Result<BackendInfo> {
    Err(WorkerError::BackendUnavailable(
        "hard isolation is supported only on Windows and Linux".into(),
    ))
}

pub async fn run(
    _command: CommandSpec,
    _limits: HardLimits,
    _cancellation: CancellationToken,
) -> Result<WorkerOutcome> {
    Err(WorkerError::BackendUnavailable(
        "hard isolation is supported only on Windows and Linux".into(),
    ))
}
