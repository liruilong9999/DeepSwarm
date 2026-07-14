use crate::{
    error::{Error, Result},
    models::ChatCompletionChunk,
};

#[derive(Debug, Default)]
pub struct SseParser {
    buffer: Vec<u8>,
    data_lines: Vec<String>,
    done: bool,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<ChatCompletionChunk>> {
        self.buffer.extend_from_slice(bytes);
        let mut chunks = Vec::new();
        while let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let mut line = self.buffer.drain(..=newline).collect::<Vec<_>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = std::str::from_utf8(&line)
                .map_err(|error| Error::Protocol(format!("SSE is not UTF-8: {error}")))?;
            self.consume_line(line, &mut chunks)?;
        }
        Ok(chunks)
    }

    pub fn finish(&mut self) -> Result<()> {
        if self.buffer.iter().any(|byte| !byte.is_ascii_whitespace()) || !self.data_lines.is_empty()
        {
            return Err(Error::Protocol(
                "connection ended during an SSE event".into(),
            ));
        }
        if !self.done {
            return Err(Error::Protocol("connection ended before [DONE]".into()));
        }
        Ok(())
    }

    pub fn is_done(&self) -> bool {
        self.done
    }

    fn consume_line(&mut self, line: &str, chunks: &mut Vec<ChatCompletionChunk>) -> Result<()> {
        if self.done {
            if line.trim().is_empty() || line.starts_with(':') {
                return Ok(());
            }
            return Err(Error::Protocol("received SSE event after [DONE]".into()));
        }
        if line.is_empty() {
            return self.dispatch(chunks);
        }
        if line.starts_with(':') {
            return Ok(());
        }
        if let Some(data) = line.strip_prefix("data:") {
            self.data_lines
                .push(data.strip_prefix(' ').unwrap_or(data).to_owned());
        }
        Ok(())
    }

    fn dispatch(&mut self, chunks: &mut Vec<ChatCompletionChunk>) -> Result<()> {
        if self.data_lines.is_empty() {
            return Ok(());
        }
        let data = self.data_lines.join("\n");
        self.data_lines.clear();
        if data == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        chunks.push(
            serde_json::from_str(&data)
                .map_err(|error| Error::Protocol(format!("invalid SSE JSON payload: {error}")))?,
        );
        Ok(())
    }
}

pub fn parse_sse(input: &[u8]) -> Result<Vec<ChatCompletionChunk>> {
    let mut parser = SseParser::new();
    let chunks = parser.push(input)?;
    parser.finish()?;
    Ok(chunks)
}

#[cfg(test)]
mod tests {
    use super::{SseParser, parse_sse};
    use crate::Error;

    const FIXTURE: &[u8] = include_bytes!("../../../tests/fixtures/protocol/chat_stream.sse");

    #[test]
    fn parses_fragmented_keep_alive_stream_in_order() {
        let mut parser = SseParser::new();
        let mut parsed = Vec::new();
        for fragment in FIXTURE.chunks(7) {
            parsed.extend(parser.push(fragment).unwrap());
        }
        parser.finish().unwrap();

        assert_eq!(parsed.len(), 3);
        assert_eq!(
            parsed[0].choices[0].delta.reasoning_content.as_deref(),
            Some("先思考")
        );
        assert_eq!(parsed[1].choices[0].delta.content.as_deref(), Some("答案"));
        assert_eq!(
            parsed[2].choices[0].delta.tool_calls.as_ref().unwrap()[0]
                .function
                .as_ref()
                .unwrap()
                .name
                .as_deref(),
            Some("lookup")
        );
        assert!(parser.is_done());
    }

    #[test]
    fn rejects_invalid_json_and_early_end() {
        assert!(matches!(
            parse_sse(b"data: nope\n\ndata: [DONE]\n\n"),
            Err(Error::Protocol(_))
        ));
        assert!(matches!(
            parse_sse(b"data: {}\n\n"),
            Err(Error::Protocol(_))
        ));
    }

    #[test]
    fn rejects_data_after_done() {
        assert!(matches!(
            parse_sse(b"data: [DONE]\n\ndata: {}\n\n"),
            Err(Error::Protocol(_))
        ));
    }
}
