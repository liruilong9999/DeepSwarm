mod assertion;
mod data;
mod distributed;
mod dsl;
mod error;
mod recording;
mod report;
mod runner;
mod scheduler;
mod value;

pub use assertion::{
    AssertionFailure, MetricSample, Metrics, SimilarityEvaluator, SimilarityRegistry,
    nearest_rank_p95,
};
pub use data::{DataSet, FixtureRoot};
pub use distributed::{
    Artifact, Coordinator, LEASE_SECONDS, Lease, MAX_ATTEMPTS, RENEWAL_SECONDS, ResultAssertion,
    ResultEnvelope, ResultEvent, RunState, SubmitOutcome, TaskResult, TerminalStatus,
};
pub use dsl::{
    Assertion, Case, DataFormat, DataSource, Document, InputFormat, Load, LoadPhase, PreparedRun,
    Step, Suite, prepare,
};
pub use error::{CoreError, ErrorKind};
pub use recording::{
    RecordedEvent, Recording, ReplayCall, Replayer, recording_hash, write_recording,
};
pub use report::{
    CaseReport, CaseReportStatus, Clock, Report, ReportStatus, ReportSummary, SystemClock,
    UncertainOperation, prune_reports, render_html, render_json, render_junit, sanitize_report,
};
pub use runner::{ActionExecutor, ActionOutput, CaseOutcome, CaseStatus, ExecutionResult, run};
pub use scheduler::{ScheduledRate, rate_at, weighted_allocations};
pub use value::{canonical_json, canonical_sha256};
