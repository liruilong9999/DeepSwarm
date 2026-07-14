use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use deep_swarm_core::{
    Coordinator, ErrorKind, Lease, ResultEnvelope, RunState, SubmitOutcome, TaskResult,
};
use tokio::sync::Mutex;
use tonic::{Request, Response, Status, transport::Server};
use uuid::Uuid;

pub mod proto {
    tonic::include_proto!("deepswarm.distributed.v1");
}

use proto::{
    AcquireLeaseReply, Empty, RenewLeaseReply, RenewLeaseRequest, RunStatus, StateReply,
    SubmitDisposition, SubmitResultReply, SubmitResultRequest,
    coordinator_rpc_server::{CoordinatorRpc, CoordinatorRpcServer},
};

pub trait Clock: Send + Sync + 'static {
    fn now_seconds(&self) -> u64;
}

#[derive(Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_seconds(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }
}

#[derive(Default)]
pub struct FixedClock(AtomicU64);

impl FixedClock {
    pub fn new(now_seconds: u64) -> Self {
        Self(AtomicU64::new(now_seconds))
    }

    pub fn set(&self, now_seconds: u64) {
        self.0.store(now_seconds, Ordering::Release);
    }
}

impl Clock for FixedClock {
    fn now_seconds(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
}

pub struct CoordinatorService<C = SystemClock> {
    coordinator: Arc<Mutex<Coordinator>>,
    clock: Arc<C>,
}

impl<C> Clone for CoordinatorService<C> {
    fn clone(&self) -> Self {
        Self {
            coordinator: self.coordinator.clone(),
            clock: self.clock.clone(),
        }
    }
}

impl CoordinatorService<SystemClock> {
    pub fn new(coordinator: Coordinator) -> Self {
        Self::with_clock(coordinator, SystemClock)
    }
}

impl<C: Clock> CoordinatorService<C> {
    pub fn with_clock(coordinator: Coordinator, clock: C) -> Self {
        Self {
            coordinator: Arc::new(Mutex::new(coordinator)),
            clock: Arc::new(clock),
        }
    }

    pub fn into_server(self) -> CoordinatorRpcServer<Self> {
        CoordinatorRpcServer::new(self)
    }
}

pub async fn serve<C: Clock>(
    address: SocketAddr,
    service: CoordinatorService<C>,
) -> Result<(), tonic::transport::Error> {
    Server::builder()
        .add_service(service.into_server())
        .serve(address)
        .await
}

#[tonic::async_trait]
impl<C: Clock> CoordinatorRpc for CoordinatorService<C> {
    async fn acquire_lease(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<AcquireLeaseReply>, Status> {
        let lease = self
            .coordinator
            .lock()
            .await
            .issue_lease(self.clock.now_seconds())
            .map(lease_to_proto);
        Ok(Response::new(AcquireLeaseReply { lease }))
    }

    async fn renew_lease(
        &self,
        request: Request<RenewLeaseRequest>,
    ) -> Result<Response<RenewLeaseReply>, Status> {
        let lease = request
            .into_inner()
            .lease
            .ok_or_else(|| Status::invalid_argument("lease is required"))?;
        let lease = lease_from_proto(lease)?;
        let renewed = self
            .coordinator
            .lock()
            .await
            .renew(&lease, self.clock.now_seconds());
        Ok(Response::new(RenewLeaseReply {
            renewed,
            expires_at: if renewed {
                self.clock.now_seconds() + deep_swarm_core::LEASE_SECONDS
            } else {
                0
            },
        }))
    }

    async fn submit_result(
        &self,
        request: Request<SubmitResultRequest>,
    ) -> Result<Response<SubmitResultReply>, Status> {
        let request = request.into_inner();
        if request.result_json.len() > 1024 * 1024 {
            return Err(Status::resource_exhausted("result_json exceeds 1 MiB"));
        }
        let result_json: TaskResult = serde_json::from_slice(&request.result_json)
            .map_err(|error| Status::invalid_argument(format!("invalid result_json: {error}")))?;
        let envelope = ResultEnvelope {
            run_id: request.run_id,
            task_id: request.task_id,
            attempt: u8::try_from(request.attempt)
                .map_err(|_| Status::invalid_argument("attempt exceeds u8"))?,
            lease_id: Uuid::parse_str(&request.lease_id)
                .map_err(|_| Status::invalid_argument("invalid lease_id"))?,
            result_json,
            output_hash: request.output_hash,
        };
        let outcome = self
            .coordinator
            .lock()
            .await
            .submit(envelope, self.clock.now_seconds())
            .map_err(core_status)?;
        let disposition = match outcome {
            SubmitOutcome::Accepted => SubmitDisposition::Accepted,
            SubmitOutcome::DroppedOldLease => SubmitDisposition::DroppedOldLease,
            SubmitOutcome::DroppedDuplicate => SubmitDisposition::DroppedDuplicate,
        };
        Ok(Response::new(SubmitResultReply {
            disposition: disposition.into(),
        }))
    }

    async fn get_state(&self, _request: Request<Empty>) -> Result<Response<StateReply>, Status> {
        let (status, failed_tasks) = match self.coordinator.lock().await.state() {
            RunState::Running => (RunStatus::Running, Vec::new()),
            RunState::Succeeded => (RunStatus::Succeeded, Vec::new()),
            RunState::Failed { tasks } => (RunStatus::Failed, tasks),
        };
        Ok(Response::new(StateReply {
            status: status.into(),
            failed_tasks,
        }))
    }
}

fn lease_to_proto(lease: Lease) -> proto::Lease {
    proto::Lease {
        run_id: lease.run_id,
        task_id: lease.task_id,
        input_hash: lease.input_hash,
        attempt: u32::from(lease.attempt),
        lease_id: lease.lease_id.to_string(),
        random_seed: lease.random_seed,
        expires_at: lease.expires_at,
    }
}

fn lease_from_proto(lease: proto::Lease) -> Result<Lease, Status> {
    Ok(Lease {
        run_id: lease.run_id,
        task_id: lease.task_id,
        input_hash: lease.input_hash,
        attempt: u8::try_from(lease.attempt)
            .map_err(|_| Status::invalid_argument("attempt exceeds u8"))?,
        lease_id: Uuid::parse_str(&lease.lease_id)
            .map_err(|_| Status::invalid_argument("invalid lease_id"))?,
        random_seed: lease.random_seed,
        expires_at: lease.expires_at,
    })
}

fn core_status(error: deep_swarm_core::CoreError) -> Status {
    match error.kind {
        ErrorKind::ResultTooLarge => Status::resource_exhausted(error.to_string()),
        ErrorKind::ResultCorrupted | ErrorKind::InvalidInput => {
            Status::invalid_argument(error.to_string())
        }
        _ => Status::internal(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use deep_swarm_core::{Artifact, TerminalStatus};
    use tokio::net::TcpListener;
    use tokio_stream::wrappers::TcpListenerStream;

    use super::*;
    use proto::coordinator_rpc_client::CoordinatorRpcClient;

    fn successful_result() -> TaskResult {
        TaskResult {
            status: TerminalStatus::Succeeded,
            events: Vec::new(),
            assertions: Vec::new(),
            metrics: BTreeMap::new(),
            artifacts: vec![Artifact {
                logical_name: "report".into(),
                content_hash: "a".repeat(64),
            }],
        }
    }

    #[tokio::test]
    async fn grpc_round_trip_preserves_lease_and_deduplicates_result() {
        let clock = FixedClock::new(100);
        let service = CoordinatorService::with_clock(
            Coordinator::new("run", [("task".into(), "input".into())]).unwrap(),
            clock,
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(
            Server::builder()
                .add_service(service.into_server())
                .serve_with_incoming(TcpListenerStream::new(listener)),
        );
        let mut client = CoordinatorRpcClient::connect(format!("http://{address}"))
            .await
            .unwrap();

        let lease = client
            .acquire_lease(Empty {})
            .await
            .unwrap()
            .into_inner()
            .lease
            .unwrap();
        assert_eq!(lease.attempt, 1);
        assert!(
            client
                .renew_lease(RenewLeaseRequest {
                    lease: Some(lease.clone()),
                })
                .await
                .unwrap()
                .into_inner()
                .renewed
        );

        let result = successful_result();
        let request = SubmitResultRequest {
            run_id: lease.run_id,
            task_id: lease.task_id,
            attempt: lease.attempt,
            lease_id: lease.lease_id,
            result_json: serde_json::to_vec(&result).unwrap(),
            output_hash: result.output_hash().unwrap(),
        };
        let first = client
            .submit_result(request.clone())
            .await
            .unwrap()
            .into_inner();
        assert_eq!(first.disposition, SubmitDisposition::Accepted as i32);
        let duplicate = client.submit_result(request).await.unwrap().into_inner();
        assert_eq!(
            duplicate.disposition,
            SubmitDisposition::DroppedDuplicate as i32
        );
        let state = client.get_state(Empty {}).await.unwrap().into_inner();
        assert_eq!(state.status, RunStatus::Succeeded as i32);
        server.abort();
    }

    #[tokio::test]
    async fn rejects_corrupt_hash_and_oversized_result() {
        let service = CoordinatorService::with_clock(
            Coordinator::new("run", [("task".into(), "input".into())]).unwrap(),
            FixedClock::new(0),
        );
        let lease = service
            .acquire_lease(Request::new(Empty {}))
            .await
            .unwrap()
            .into_inner()
            .lease
            .unwrap();
        let result = successful_result();
        let error = service
            .submit_result(Request::new(SubmitResultRequest {
                run_id: lease.run_id.clone(),
                task_id: lease.task_id.clone(),
                attempt: lease.attempt,
                lease_id: lease.lease_id.clone(),
                result_json: serde_json::to_vec(&result).unwrap(),
                output_hash: "0".repeat(64),
            }))
            .await
            .unwrap_err();
        assert_eq!(error.code(), tonic::Code::InvalidArgument);

        let error = service
            .submit_result(Request::new(SubmitResultRequest {
                run_id: lease.run_id,
                task_id: lease.task_id,
                attempt: lease.attempt,
                lease_id: lease.lease_id,
                result_json: vec![b'x'; 1024 * 1024 + 1],
                output_hash: String::new(),
            }))
            .await
            .unwrap_err();
        assert_eq!(error.code(), tonic::Code::ResourceExhausted);
    }
}
