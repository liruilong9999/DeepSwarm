#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use deep_swarm_client::{
    models::{ChatCompletionChunk, ChatMessageDelta, ChunkChoice},
    streaming::parse_sse,
};
use deep_swarm_fuzzer::{MAX_ARRAY_ITEMS, bounded_bytes, bounded_str, sse_within_limits};
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
struct SseInput {
    mode: u8,
    contents: Vec<String>,
}

fuzz_target!(|data: &[u8]| {
    let data = bounded_bytes(data);
    if sse_within_limits(data) {
        let _ = parse_sse(data);
    }

    let mut unstructured = Unstructured::new(data);
    let Ok(generated) = SseInput::arbitrary(&mut unstructured) else {
        return;
    };
    let contents = if generated.contents.is_empty() {
        vec![String::new()]
    } else {
        generated
            .contents
            .into_iter()
            .take(MAX_ARRAY_ITEMS)
            .collect()
    };
    let mut stream = Vec::new();
    for (index, content) in contents.iter().enumerate() {
        let chunk = ChatCompletionChunk {
            id: "fuzz".into(),
            choices: vec![ChunkChoice {
                index: index as u32,
                delta: ChatMessageDelta {
                    content: Some(bounded_str(content).to_owned()),
                    ..ChatMessageDelta::default()
                },
                finish_reason: None,
                logprobs: None,
            }],
            created: 0,
            model: "deepseek-v4-pro".into(),
            system_fingerprint: None,
            object: Some("chat.completion.chunk".into()),
            usage: None,
        };
        stream.extend_from_slice(b"data: ");
        stream.extend_from_slice(&serde_json::to_vec(&chunk).expect("generated chunk serializes"));
        stream.extend_from_slice(b"\n\n");
    }
    match generated.mode % 3 {
        0 => stream.extend_from_slice(b"data: [DONE]\n\n"),
        1 => {}
        _ => stream.extend_from_slice(b"data: {not-json}\n\ndata: [DONE]\n\n"),
    }
    assert_eq!(parse_sse(&stream).is_ok(), generated.mode % 3 == 0);
});
