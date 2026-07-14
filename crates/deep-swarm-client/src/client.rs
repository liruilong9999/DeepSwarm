use std::{collections::VecDeque, pin::Pin, time::Duration};

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use rand::Rng;
use reqwest::{Method, Response};
use serde::{Serialize, de::DeserializeOwned};

use crate::{
    error::{Error, Result},
    models::{
        ApiErrorResponse, BalanceResponse, ChatCompletionChunk, ChatCompletionRequest,
        ChatCompletionResponse, CompletionRequest, CompletionResponse, ModelsResponse,
    },
    streaming::SseParser,
};

const MAX_ATTEMPTS: usize = 3;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

pub type ChatCompletionStream =
    Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk>> + Send + 'static>>;

#[derive(Clone, Copy, Debug)]
pub struct RetryPolicy {
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl RetryPolicy {
    pub const fn no_delay() -> Self {
        Self {
            base_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
        }
    }

    async fn wait(&self, failed_attempt: usize) {
        if self.base_delay.is_zero() {
            return;
        }
        let factor = 1_u32 << failed_attempt.saturating_sub(1).min(31);
        let exponential = self.base_delay.saturating_mul(factor).min(self.max_delay);
        let max_jitter_ms = (exponential.as_millis() / 2).min(u64::MAX as u128) as u64;
        let jitter = rand::rng().random_range(0..=max_jitter_ms);
        tokio::time::sleep(exponential.saturating_add(Duration::from_millis(jitter))).await;
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            base_delay: Duration::from_millis(200),
            max_delay: Duration::from_secs(2),
        }
    }
}

#[derive(Clone, Debug)]
pub struct DeepSeekClient {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
    beta_base_url: String,
    retry: RetryPolicy,
}

impl DeepSeekClient {
    pub fn new(api_key: impl Into<String>) -> Result<Self> {
        Self::with_base_url(api_key, "https://api.deepseek.com")
    }

    pub fn with_base_url(api_key: impl Into<String>, base_url: impl Into<String>) -> Result<Self> {
        let base_url = base_url.into().trim_end_matches('/').to_owned();
        let http = build_http(DEFAULT_TIMEOUT)?;
        Ok(Self {
            http,
            api_key: api_key.into(),
            beta_base_url: format!("{base_url}/beta"),
            base_url,
            retry: RetryPolicy::default(),
        })
    }

    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Result<Self> {
        self.http = build_http(timeout)?;
        Ok(self)
    }

    pub async fn chat_completion(
        &self,
        request: &ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse> {
        let mut request = request.clone();
        request.stream = Some(false);
        self.post_json(format!("{}/chat/completions", self.base_url), &request)
            .await
    }

    pub async fn chat_completion_stream(
        &self,
        request: &ChatCompletionRequest,
    ) -> Result<ChatCompletionStream> {
        let mut request = request.clone();
        request.stream = Some(true);
        let body = serde_json::to_vec(&request)
            .map_err(|error| Error::Protocol(format!("failed to encode request: {error}")))?;
        let response = self
            .send_with_retry(
                Method::POST,
                format!("{}/chat/completions", self.base_url),
                Some(&body),
            )
            .await?;
        let state = StreamState {
            body: Box::pin(response.bytes_stream()),
            parser: SseParser::new(),
            pending: VecDeque::new(),
        };
        Ok(Box::pin(futures_util::stream::try_unfold(
            state,
            |mut state| async move {
                loop {
                    if let Some(chunk) = state.pending.pop_front() {
                        return Ok(Some((chunk, state)));
                    }
                    match state.body.next().await {
                        Some(Ok(bytes)) => state.pending.extend(state.parser.push(&bytes)?),
                        Some(Err(error)) => return Err(Error::from_reqwest(error)),
                        None => {
                            state.parser.finish()?;
                            return Ok(None);
                        }
                    }
                }
            },
        )))
    }

    pub async fn completion(&self, request: &CompletionRequest) -> Result<CompletionResponse> {
        self.post_json(format!("{}/completions", self.beta_base_url), request)
            .await
    }

    pub async fn list_models(&self) -> Result<ModelsResponse> {
        self.get_json(format!("{}/models", self.base_url)).await
    }

    pub async fn balance(&self) -> Result<BalanceResponse> {
        self.get_json(format!("{}/user/balance", self.base_url))
            .await
    }

    async fn post_json<T: DeserializeOwned>(
        &self,
        url: String,
        request: &impl Serialize,
    ) -> Result<T> {
        let body = serde_json::to_vec(request)
            .map_err(|error| Error::Protocol(format!("failed to encode request: {error}")))?;
        self.request_json(Method::POST, url, Some(&body)).await
    }

    async fn get_json<T: DeserializeOwned>(&self, url: String) -> Result<T> {
        self.request_json(Method::GET, url, None).await
    }

    async fn request_json<T: DeserializeOwned>(
        &self,
        method: Method,
        url: String,
        body: Option<&[u8]>,
    ) -> Result<T> {
        for attempt in 1..=MAX_ATTEMPTS {
            let result = async {
                let response = self.send_once(method.clone(), &url, body).await?;
                let bytes = response.bytes().await.map_err(Error::from_reqwest)?;
                serde_json::from_slice(&bytes)
                    .map_err(|error| Error::Protocol(format!("invalid JSON response: {error}")))
            }
            .await;
            match result {
                Err(error) if error.is_retryable() && attempt < MAX_ATTEMPTS => {
                    self.retry.wait(attempt).await;
                }
                result => return result,
            }
        }
        unreachable!("the fixed retry loop always returns")
    }

    async fn send_with_retry(
        &self,
        method: Method,
        url: String,
        body: Option<&[u8]>,
    ) -> Result<Response> {
        for attempt in 1..=MAX_ATTEMPTS {
            match self.send_once(method.clone(), &url, body).await {
                Err(error) if error.is_retryable() && attempt < MAX_ATTEMPTS => {
                    self.retry.wait(attempt).await;
                }
                result => return result,
            }
        }
        unreachable!("the fixed retry loop always returns")
    }

    async fn send_once(&self, method: Method, url: &str, body: Option<&[u8]>) -> Result<Response> {
        let mut request = self.http.request(method, url).bearer_auth(&self.api_key);
        if let Some(body) = body {
            request = request
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.to_vec());
        }
        let response = request.send().await.map_err(Error::from_reqwest)?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        let bytes = response.bytes().await.map_err(Error::from_reqwest)?;
        let message = serde_json::from_slice::<ApiErrorResponse>(&bytes)
            .map(|body| body.error.message)
            .unwrap_or_else(|_| String::from_utf8_lossy(&bytes).into_owned());
        Err(Error::from_status(status, message))
    }
}

struct StreamState {
    body: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>,
    parser: SseParser,
    pending: VecDeque<ChatCompletionChunk>,
}

fn build_http(timeout: Duration) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .build()
        .map_err(Error::from_reqwest)
}
