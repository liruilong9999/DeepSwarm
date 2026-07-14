use std::{
    collections::VecDeque,
    io,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use deep_swarm_client::models::{
    ApiErrorBody, ApiErrorResponse, BalanceInfo, BalanceResponse, ChatChoice, ChatCompletionChunk,
    ChatCompletionRequest, ChatCompletionResponse, ChatMessage, ChatMessageDelta, ChunkChoice,
    CompletionChoice, CompletionRequest, CompletionResponse, ModelInfo, ModelsResponse, Usage,
};
use futures_util::stream;
use tokio::{net::TcpListener, sync::Mutex, task::JoinHandle};

#[derive(Clone, Debug)]
pub struct MockReply {
    pub delay: Duration,
    pub response: MockResponse,
}

impl MockReply {
    pub fn immediate(response: MockResponse) -> Self {
        Self {
            delay: Duration::ZERO,
            response,
        }
    }

    pub fn delayed(delay: Duration, response: MockResponse) -> Self {
        Self { delay, response }
    }
}

#[derive(Clone, Debug)]
pub enum MockResponse {
    Chat(ChatCompletionResponse),
    Stream(Vec<StreamEvent>),
    Completion(CompletionResponse),
    Models(ModelsResponse),
    Balance(BalanceResponse),
    Error { status: u16, message: String },
}

#[derive(Clone, Debug)]
pub enum StreamEvent {
    KeepAlive,
    EmptyLine,
    Data(ChatCompletionChunk),
    Done,
    Raw(Vec<u8>),
    Disconnect,
}

#[derive(Clone)]
pub struct MockState {
    inner: Arc<MockStateInner>,
}

struct MockStateInner {
    api_key: String,
    replies: Mutex<VecDeque<MockReply>>,
    request_count: AtomicUsize,
}

impl MockState {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(MockStateInner {
                api_key: api_key.into(),
                replies: Mutex::new(VecDeque::new()),
                request_count: AtomicUsize::new(0),
            }),
        }
    }

    pub fn with_replies(
        api_key: impl Into<String>,
        replies: impl IntoIterator<Item = MockReply>,
    ) -> Self {
        Self {
            inner: Arc::new(MockStateInner {
                api_key: api_key.into(),
                replies: Mutex::new(replies.into_iter().collect()),
                request_count: AtomicUsize::new(0),
            }),
        }
    }

    pub async fn push(&self, reply: MockReply) {
        self.inner.replies.lock().await.push_back(reply);
    }

    pub fn request_count(&self) -> usize {
        self.inner.request_count.load(Ordering::Relaxed)
    }

    fn authorize(&self, headers: &HeaderMap) -> bool {
        headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value == format!("Bearer {}", self.inner.api_key))
    }

    async fn reply(&self, fallback: MockResponse) -> MockReply {
        self.inner.request_count.fetch_add(1, Ordering::Relaxed);
        self.inner
            .replies
            .lock()
            .await
            .pop_front()
            .unwrap_or_else(|| MockReply::immediate(fallback))
    }
}

pub struct MockServer {
    address: SocketAddr,
    state: MockState,
    task: JoinHandle<()>,
}

impl MockServer {
    pub async fn start(state: MockState) -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server_state = state.clone();
        let task = tokio::spawn(async move {
            let _ = serve(listener, server_state).await;
        });
        Ok(Self {
            address,
            state,
            task,
        })
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    pub fn state(&self) -> &MockState {
        &self.state
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub async fn serve(listener: TcpListener, state: MockState) -> io::Result<()> {
    let app = Router::new()
        .route("/chat/completions", post(chat))
        .route("/beta/completions", post(completion))
        .route("/models", get(models))
        .route("/user/balance", get(balance))
        .with_state(state);
    axum::serve(listener, app).await
}

async fn chat(State(state): State<MockState>, headers: HeaderMap, body: Bytes) -> Response {
    if !state.authorize(&headers) {
        return error_response(StatusCode::UNAUTHORIZED, "invalid API key");
    }
    let request: ChatCompletionRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => {
            return error_response(StatusCode::BAD_REQUEST, format!("invalid request: {error}"));
        }
    };
    let fallback = if request.stream == Some(true) {
        default_stream()
    } else {
        MockResponse::Chat(default_chat())
    };
    respond(state.reply(fallback).await).await
}

async fn completion(State(state): State<MockState>, headers: HeaderMap, body: Bytes) -> Response {
    if !state.authorize(&headers) {
        return error_response(StatusCode::UNAUTHORIZED, "invalid API key");
    }
    if let Err(error) = serde_json::from_slice::<CompletionRequest>(&body) {
        return error_response(StatusCode::BAD_REQUEST, format!("invalid request: {error}"));
    }
    respond(
        state
            .reply(MockResponse::Completion(default_completion()))
            .await,
    )
    .await
}

async fn models(State(state): State<MockState>, headers: HeaderMap) -> Response {
    if !state.authorize(&headers) {
        return error_response(StatusCode::UNAUTHORIZED, "invalid API key");
    }
    respond(state.reply(MockResponse::Models(default_models())).await).await
}

async fn balance(State(state): State<MockState>, headers: HeaderMap) -> Response {
    if !state.authorize(&headers) {
        return error_response(StatusCode::UNAUTHORIZED, "invalid API key");
    }
    respond(state.reply(MockResponse::Balance(default_balance())).await).await
}

async fn respond(reply: MockReply) -> Response {
    if !reply.delay.is_zero() {
        tokio::time::sleep(reply.delay).await;
    }
    match reply.response {
        MockResponse::Chat(value) => json_response(StatusCode::OK, &value),
        MockResponse::Stream(events) => stream_response(events),
        MockResponse::Completion(value) => json_response(StatusCode::OK, &value),
        MockResponse::Models(value) => json_response(StatusCode::OK, &value),
        MockResponse::Balance(value) => json_response(StatusCode::OK, &value),
        MockResponse::Error { status, message } => error_response(
            StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            message,
        ),
    }
}

fn json_response(status: StatusCode, value: &impl serde::Serialize) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_vec(value).expect("shared protocol models serialize"),
    )
        .into_response()
}

fn error_response(status: StatusCode, message: impl Into<String>) -> Response {
    json_response(
        status,
        &ApiErrorResponse {
            error: ApiErrorBody {
                message: message.into(),
                kind: Some("mock_error".into()),
                param: None,
                code: Some(status.as_u16().into()),
            },
        },
    )
}

fn stream_response(events: Vec<StreamEvent>) -> Response {
    let body = Body::from_stream(stream::iter(events.into_iter().map(|event| {
        match event {
            StreamEvent::KeepAlive => Ok(Bytes::from_static(b": keep-alive\n\n")),
            StreamEvent::EmptyLine => Ok(Bytes::from_static(b"\n")),
            StreamEvent::Data(chunk) => serde_json::to_vec(&chunk)
                .map(|json| Bytes::from([b"data: ".as_slice(), &json, b"\n\n"].concat()))
                .map_err(io::Error::other),
            StreamEvent::Done => Ok(Bytes::from_static(b"data: [DONE]\n\n")),
            StreamEvent::Raw(bytes) => Ok(Bytes::from(bytes)),
            StreamEvent::Disconnect => Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "scripted disconnect",
            )),
        }
    })));
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(body)
        .expect("static streaming response")
}

fn default_chat() -> ChatCompletionResponse {
    ChatCompletionResponse {
        id: "chatcmpl-mock".into(),
        choices: vec![ChatChoice {
            index: 0,
            message: ChatMessage::assistant("mock response"),
            finish_reason: Some("stop".into()),
            logprobs: None,
        }],
        created: 1_720_000_000,
        model: "deepseek-v4-pro".into(),
        system_fingerprint: Some("fp_mock".into()),
        object: Some("chat.completion".into()),
        usage: Some(Usage {
            prompt_tokens: 1,
            completion_tokens: 2,
            total_tokens: 3,
            ..Usage::default()
        }),
    }
}

fn default_stream() -> MockResponse {
    MockResponse::Stream(vec![
        StreamEvent::KeepAlive,
        StreamEvent::Data(ChatCompletionChunk {
            id: "chatcmpl-mock".into(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChatMessageDelta {
                    content: Some("mock response".into()),
                    ..ChatMessageDelta::default()
                },
                finish_reason: Some("stop".into()),
                logprobs: None,
            }],
            created: 1_720_000_000,
            model: "deepseek-v4-pro".into(),
            system_fingerprint: Some("fp_mock".into()),
            object: Some("chat.completion.chunk".into()),
            usage: None,
        }),
        StreamEvent::Done,
    ])
}

fn default_completion() -> CompletionResponse {
    CompletionResponse {
        id: "cmpl-mock".into(),
        choices: vec![CompletionChoice {
            index: 0,
            text: "mock completion".into(),
            finish_reason: Some("stop".into()),
            logprobs: None,
        }],
        created: 1_720_000_000,
        model: "deepseek-v4-pro".into(),
        object: Some("text_completion".into()),
        usage: Some(Usage {
            prompt_tokens: 1,
            completion_tokens: 2,
            total_tokens: 3,
            ..Usage::default()
        }),
    }
}

fn default_models() -> ModelsResponse {
    ModelsResponse {
        object: "list".into(),
        data: vec![ModelInfo {
            id: "deepseek-v4-pro".into(),
            object: "model".into(),
            owned_by: "deepseek".into(),
        }],
    }
}

fn default_balance() -> BalanceResponse {
    BalanceResponse {
        is_available: true,
        balance_infos: vec![BalanceInfo {
            currency: "CNY".into(),
            total_balance: "100.00".into(),
            granted_balance: "0.00".into(),
            topped_up_balance: "100.00".into(),
        }],
    }
}
