use regex::Regex;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::{CoreError, ErrorKind};

pub fn canonical_json(value: &Value) -> Result<Vec<u8>, CoreError> {
    let mut output = Vec::new();
    write_canonical(value, &mut output)?;
    Ok(output)
}

pub fn canonical_sha256(value: &Value) -> Result<String, CoreError> {
    Ok(format!("{:x}", Sha256::digest(canonical_json(value)?)))
}

fn write_canonical(value: &Value, output: &mut Vec<u8>) -> Result<(), CoreError> {
    match value {
        Value::Null => output.extend_from_slice(b"null"),
        Value::Bool(value) => output.extend_from_slice(value.to_string().as_bytes()),
        Value::Number(value) => {
            let text = if value.as_f64() == Some(0.0) {
                "0".to_owned()
            } else if let Some(number) = value.as_f64().filter(|number| {
                number.fract() == 0.0 && number.abs() >= 1e-6 && number.abs() < 1e21
            }) {
                format!("{number:.0}")
            } else {
                value.to_string()
            };
            output.extend_from_slice(text.as_bytes());
        }
        Value::String(value) => output.extend_from_slice(
            serde_json::to_string(value)
                .map_err(|error| CoreError::invalid(error.to_string()))?
                .as_bytes(),
        ),
        Value::Array(values) => {
            output.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                write_canonical(value, output)?;
            }
            output.push(b']');
        }
        Value::Object(values) => {
            output.push(b'{');
            let mut entries = values.iter().collect::<Vec<_>>();
            entries.sort_by(|(left, _), (right, _)| left.encode_utf16().cmp(right.encode_utf16()));
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                output.extend_from_slice(
                    serde_json::to_string(key)
                        .map_err(|error| CoreError::invalid(error.to_string()))?
                        .as_bytes(),
                );
                output.push(b':');
                write_canonical(value, output)?;
            }
            output.push(b'}');
        }
    }
    Ok(())
}

pub(crate) fn interpolate(value: &Value, context: &Value) -> Result<Value, CoreError> {
    match value {
        Value::String(text) => interpolate_string(text, context),
        Value::Array(values) => values
            .iter()
            .map(|value| interpolate(value, context))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        Value::Object(values) => values
            .iter()
            .map(|(key, value)| Ok((key.clone(), interpolate(value, context)?)))
            .collect::<Result<serde_json::Map<_, _>, CoreError>>()
            .map(Value::Object),
        _ => Ok(value.clone()),
    }
}

fn interpolate_string(text: &str, context: &Value) -> Result<Value, CoreError> {
    let regex = Regex::new(r"\$\{([^{}]+)\}").expect("固定正则有效");
    if let Some(capture) = regex.captures(text)
        && capture.get(0).is_some_and(|value| value.as_str() == text)
    {
        return lookup_path(context, &capture[1]).cloned().ok_or_else(|| {
            CoreError::new(
                ErrorKind::UnresolvedOutput,
                format!("无法解析变量: {}", &capture[1]),
            )
        });
    }
    let mut output = String::new();
    let mut offset = 0;
    for capture in regex.captures_iter(text) {
        let matched = capture.get(0).expect("完整匹配存在");
        output.push_str(&text[offset..matched.start()]);
        let value = lookup_path(context, &capture[1]).ok_or_else(|| {
            CoreError::new(
                ErrorKind::UnresolvedOutput,
                format!("无法解析变量: {}", &capture[1]),
            )
        })?;
        output.push_str(value.as_str().ok_or_else(|| {
            CoreError::invalid(format!("字符串插值只接受字符串: {}", &capture[1]))
        })?);
        offset = matched.end();
    }
    output.push_str(&text[offset..]);
    Ok(Value::String(output))
}

pub(crate) fn lookup_path<'a>(context: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.')
        .try_fold(context, |current, segment| match current {
            Value::Object(values) => values.get(segment),
            Value::Array(values) => segment
                .parse::<usize>()
                .ok()
                .and_then(|index| values.get(index)),
            _ => None,
        })
}

pub(crate) fn resolve_static_scopes(
    scopes: &[&Map<String, Value>],
) -> Result<Map<String, Value>, CoreError> {
    let mut values = Map::new();
    for scope in scopes {
        values.extend(
            scope
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
    }
    for _ in 0..=values.len() {
        let mut changed = false;
        let context = Value::Object(values.clone());
        for value in values.values_mut() {
            let resolved =
                interpolate(value, &context).map_err(|error| CoreError::invalid(error.message))?;
            changed |= resolved != *value;
            *value = resolved;
        }
        if !changed {
            if values.values().any(contains_placeholder) {
                return Err(CoreError::invalid("静态变量存在循环引用"));
            }
            return Ok(values);
        }
    }
    Err(CoreError::invalid("静态变量存在循环引用"))
}

fn contains_placeholder(value: &Value) -> bool {
    match value {
        Value::String(value) => value.contains("${"),
        Value::Array(values) => values.iter().any(contains_placeholder),
        Value::Object(values) => values.values().any(contains_placeholder),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn canonicalizes_object_keys_and_numbers() {
        assert_eq!(
            canonical_json(&json!({"z": 1.0, "a": [true, null]})).unwrap(),
            br#"{"a":[true,null],"z":1}"#
        );
    }
}
