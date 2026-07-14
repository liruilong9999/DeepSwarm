use serde_json::Value;

pub const MAX_DEPTH: usize = 32;
pub const MAX_ARRAY_ITEMS: usize = 1_000;
pub const MAX_STRING_BYTES: usize = 64 * 1024;
pub const MAX_SSE_EVENTS: usize = 10_000;

pub fn bounded_bytes(input: &[u8]) -> &[u8] {
    &input[..input.len().min(MAX_STRING_BYTES)]
}

pub fn bounded_str(input: &str) -> &str {
    if input.len() <= MAX_STRING_BYTES {
        return input;
    }
    let mut end = MAX_STRING_BYTES;
    while !input.is_char_boundary(end) {
        end -= 1;
    }
    &input[..end]
}

pub fn json_depth_within(input: &[u8]) -> bool {
    let mut depth = 0_usize;
    let mut quoted = false;
    let mut escaped = false;
    for byte in input {
        if quoted {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                quoted = false;
            }
        } else {
            match *byte {
                b'"' => quoted = true,
                b'{' | b'[' => {
                    depth += 1;
                    if depth > MAX_DEPTH {
                        return false;
                    }
                }
                b'}' | b']' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
    }
    true
}

pub fn value_within_limits(value: &Value, depth: usize) -> bool {
    if depth > MAX_DEPTH {
        return false;
    }
    match value {
        Value::String(value) => value.len() <= MAX_STRING_BYTES,
        Value::Array(values) => {
            values.len() <= MAX_ARRAY_ITEMS
                && values
                    .iter()
                    .all(|value| value_within_limits(value, depth + 1))
        }
        Value::Object(values) => {
            values.len() <= MAX_ARRAY_ITEMS
                && values.iter().all(|(key, value)| {
                    key.len() <= MAX_STRING_BYTES && value_within_limits(value, depth + 1)
                })
        }
        _ => true,
    }
}

pub fn sse_within_limits(input: &[u8]) -> bool {
    input
        .split(|byte| *byte == b'\n')
        .filter(|line| line.starts_with(b"data:"))
        .take(MAX_SSE_EVENTS + 1)
        .count()
        <= MAX_SSE_EVENTS
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn enforces_all_fuzz_input_limits() {
        assert!(json_depth_within(br#"{"value":[1]}"#));
        assert!(!json_depth_within(
            format!("{}{}", "[".repeat(MAX_DEPTH + 1), "]".repeat(MAX_DEPTH + 1)).as_bytes()
        ));
        assert!(value_within_limits(&json!([1, 2, 3]), 0));
        assert!(!value_within_limits(
            &Value::Array(vec![Value::Null; MAX_ARRAY_ITEMS + 1]),
            0
        ));
        assert_eq!(
            bounded_bytes(&vec![0; MAX_STRING_BYTES + 1]).len(),
            MAX_STRING_BYTES
        );
    }
}
