use std::time::Duration;

use deep_swarm_client::{
    DeepSeekClient, Error, RetryPolicy,
    models::{ChatCompletionRequest, ChatCompletionResponse, ChatMessage, CompletionRequest, Stop},
};
use deep_swarm_mock_server::{MockReply, MockResponse, MockServer, MockState, StreamEvent};
use futures_util::TryStreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const API_KEY: &str = "contract-key";
const CHAT_FIXTURE: &str = include_str!("../../../tests/fixtures/protocol/chat_response.json");
const STREAM_FIXTURE: &[u8] = include_bytes!("../../../tests/fixtures/protocol/chat_stream.sse");

fn client(server: &MockServer) -> DeepSeekClient {
    DeepSeekClient::with_base_url(API_KEY, server.base_url())
        .unwrap()
        .with_retry_policy(RetryPolicy::no_delay())
}

fn chat_request() -> ChatCompletionRequest {
    ChatCompletionRequest::new("deepseek-v4-pro", vec![ChatMessage::user("call the tool")])
}

fn completion_request() -> CompletionRequest {
    CompletionRequest {
        model: "deepseek-v4-pro".into(),
        prompt: "fn main() {".into(),
        suffix: Some("}".into()),
        echo: Some(false),
        logprobs: None,
        max_tokens: Some(32),
        stop: Some(Stop::One("}".into())),
        stream: Some(false),
        stream_options: None,
        temperature: None,
        top_p: None,
    }
}

#[tokio::test]
async fn same_typed_fixtures_pass_model_parser_and_mock_server() {
    let expected: ChatCompletionResponse = serde_json::from_str(CHAT_FIXTURE).unwrap();
    let stream_fragments = STREAM_FIXTURE
        .chunks(11)
        .map(|fragment| StreamEvent::Raw(fragment.to_vec()))
        .collect();
    let state = MockState::with_replies(
        API_KEY,
        [
            MockReply::immediate(MockResponse::Chat(expected.clone())),
            MockReply::immediate(MockResponse::Stream(stream_fragments)),
        ],
    );
    let server = MockServer::start(state).await.unwrap();

    assert_eq!(
        client(&server)
            .chat_completion(&chat_request())
            .await
            .unwrap(),
        expected
    );
    let chunks = client(&server)
        .chat_completion_stream(&chat_request())
        .await
        .unwrap()
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    assert_eq!(chunks.len(), 3);
    assert_eq!(server.state().request_count(), 2);
}

#[tokio::test]
async fn accepts_non_stream_keep_alive_empty_lines() {
    let state = MockState::with_replies(
        API_KEY,
        [MockReply::immediate(MockResponse::Stream(vec![
            StreamEvent::Raw(b"\n\n".to_vec()),
            StreamEvent::Raw(CHAT_FIXTURE.as_bytes().to_vec()),
        ]))],
    );
    let server = MockServer::start(state).await.unwrap();
    let response = client(&server)
        .chat_completion(&chat_request())
        .await
        .unwrap();
    assert_eq!(response.id, "chatcmpl-contract");
}

#[tokio::test]
async fn covers_all_declared_endpoints_and_bearer_authentication() {
    let state = MockState::new(API_KEY);
    let server = MockServer::start(state).await.unwrap();
    let client = client(&server);

    assert_eq!(
        client
            .chat_completion(&chat_request())
            .await
            .unwrap()
            .choices[0]
            .message
            .content
            .as_deref(),
        Some("mock response")
    );
    assert_eq!(client.list_models().await.unwrap().data.len(), 1);
    assert!(client.balance().await.unwrap().is_available);
    assert_eq!(
        client
            .completion(&completion_request())
            .await
            .unwrap()
            .choices[0]
            .text,
        "mock completion"
    );

    let unauthorized = DeepSeekClient::with_base_url("wrong", server.base_url())
        .unwrap()
        .with_retry_policy(RetryPolicy::no_delay())
        .list_models()
        .await;
    assert!(matches!(unauthorized, Err(Error::Authentication(_))));
}

#[tokio::test]
async fn retries_only_temporary_errors_at_most_three_times() {
    for (status, expected_attempts) in [
        (400, 1),
        (401, 1),
        (402, 1),
        (404, 1),
        (422, 1),
        (429, 3),
        (500, 3),
        (502, 1),
        (503, 3),
    ] {
        let replies = (0..3).map(|_| {
            MockReply::immediate(MockResponse::Error {
                status,
                message: format!("status {status}"),
            })
        });
        let state = MockState::with_replies(API_KEY, replies);
        let server = MockServer::start(state).await.unwrap();
        let result = client(&server).list_models().await;
        assert!(result.is_err(), "status {status} unexpectedly succeeded");
        assert_eq!(
            server.state().request_count(),
            expected_attempts,
            "status {status} used the wrong attempt count"
        );
    }
}

#[tokio::test]
async fn classifies_invalid_json_and_missing_done_as_protocol_errors() {
    for events in [
        vec![
            StreamEvent::Raw(b"data: not-json\n\n".to_vec()),
            StreamEvent::Done,
        ],
        vec![StreamEvent::Raw(
            b"data: {\"id\":\"incomplete\"}\n\n".to_vec(),
        )],
    ] {
        let state = MockState::with_replies(
            API_KEY,
            [MockReply::immediate(MockResponse::Stream(events))],
        );
        let server = MockServer::start(state).await.unwrap();
        let result = client(&server)
            .chat_completion_stream(&chat_request())
            .await
            .unwrap()
            .try_collect::<Vec<_>>()
            .await;
        assert!(matches!(result, Err(Error::Protocol(_))));
    }
}

#[tokio::test]
async fn supports_programmable_delay_without_real_wait_in_retry_tests() {
    let expected: ChatCompletionResponse = serde_json::from_str(CHAT_FIXTURE).unwrap();
    let state = MockState::with_replies(
        API_KEY,
        [MockReply::delayed(
            Duration::from_millis(1),
            MockResponse::Chat(expected),
        )],
    );
    let server = MockServer::start(state).await.unwrap();
    client(&server)
        .chat_completion(&chat_request())
        .await
        .unwrap();
}

#[tokio::test]
async fn retries_connection_failures_and_stops_after_success() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for attempt in 1..=3 {
            let (mut socket, _) = listener.accept().await.unwrap();
            if attempt < 3 {
                drop(socket);
                continue;
            }
            let mut request = vec![0; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            let body = r#"{"object":"list","data":[]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
        3
    });
    let client = DeepSeekClient::with_base_url(API_KEY, format!("http://{address}"))
        .unwrap()
        .with_retry_policy(RetryPolicy::no_delay());

    assert!(client.list_models().await.unwrap().data.is_empty());
    assert_eq!(server.await.unwrap(), 3);
}

#[tokio::test]
async fn maps_deadline_to_timeout_without_retrying() {
    let state = MockState::with_replies(
        API_KEY,
        [MockReply::delayed(
            Duration::from_millis(50),
            MockResponse::Chat(serde_json::from_str(CHAT_FIXTURE).unwrap()),
        )],
    );
    let server = MockServer::start(state).await.unwrap();
    let client = client(&server)
        .with_timeout(Duration::from_millis(1))
        .unwrap();

    assert!(matches!(
        client.chat_completion(&chat_request()).await,
        Err(Error::Timeout(_))
    ));
    assert!(server.state().request_count() <= 1);
}
