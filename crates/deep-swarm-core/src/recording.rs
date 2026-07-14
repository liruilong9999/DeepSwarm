use std::{fs, path::Path};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{CoreError, ErrorKind, canonical_sha256};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Recording {
    pub format_version: u64,
    pub scenario_hash: String,
    pub config_hash: String,
    pub random_seed: u64,
    pub events: Vec<RecordedEvent>,
}

impl Recording {
    pub fn new(
        scenario: &Value,
        config: &Value,
        random_seed: u64,
        mut events: Vec<RecordedEvent>,
    ) -> Result<Self, CoreError> {
        for (index, event) in events.iter_mut().enumerate() {
            event.sequence = index as u64;
            event.event_hash = event.compute_hash()?;
        }
        Ok(Self {
            format_version: 1,
            scenario_hash: canonical_sha256(scenario)?,
            config_hash: canonical_sha256(config)?,
            random_seed,
            events,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RecordedEvent {
    pub sequence: u64,
    pub kind: String,
    pub name: String,
    pub parameters: Value,
    pub result: Option<Value>,
    pub error: Option<Value>,
    pub duration_ms: u64,
    pub event_hash: String,
}

impl RecordedEvent {
    fn compute_hash(&self) -> Result<String, CoreError> {
        canonical_sha256(&serde_json::json!({
            "sequence": self.sequence,
            "kind": self.kind,
            "name": self.name,
            "parameters": self.parameters,
            "result": self.result,
            "error": self.error,
            "duration_ms": self.duration_ms,
        }))
    }
}

pub struct ReplayCall<'a> {
    pub kind: &'a str,
    pub name: &'a str,
    pub parameters: Value,
    pub schema: Option<&'a Value>,
}

pub struct Replayer {
    recording: Recording,
    cursor: usize,
}

impl Replayer {
    pub fn new(recording: Recording, scenario: &Value, config: &Value) -> Result<Self, CoreError> {
        if recording.format_version != 1 {
            return Err(mismatch(0, "未知 format_version"));
        }
        if recording.scenario_hash != canonical_sha256(scenario)? {
            return Err(mismatch(0, "场景哈希不一致"));
        }
        if recording.config_hash != canonical_sha256(config)? {
            return Err(mismatch(0, "配置哈希不一致"));
        }
        Ok(Self {
            recording,
            cursor: 0,
        })
    }

    pub fn next(&mut self, mut call: ReplayCall<'_>) -> Result<RecordedEvent, CoreError> {
        let index = self.cursor;
        let event = self
            .recording
            .events
            .get(index)
            .ok_or_else(|| mismatch(index, "录制事件已耗尽"))?;
        if event.sequence != index as u64 {
            return Err(mismatch(index, "事件序号不连续"));
        }
        if event.compute_hash()? != event.event_hash {
            return Err(mismatch(index, "事件哈希不一致"));
        }
        if event.kind != call.kind {
            return Err(mismatch(index, "调用类型不一致"));
        }
        if event.name != call.name {
            return Err(mismatch(index, "调用名称不一致"));
        }
        let mut recorded = event.parameters.clone();
        if let Some(schema) = call.schema {
            apply_defaults(&mut recorded, schema);
            apply_defaults(&mut call.parameters, schema);
        }
        if canonical_sha256(&recorded)? != canonical_sha256(&call.parameters)? {
            return Err(mismatch(index, "规范化参数不一致"));
        }
        self.cursor += 1;
        Ok(event.clone())
    }

    pub fn finish(self) -> Result<(), CoreError> {
        if self.cursor == self.recording.events.len() {
            Ok(())
        } else {
            Err(mismatch(self.cursor, "存在未消费录制事件"))
        }
    }
}

pub fn recording_hash(recording: &Recording) -> Result<String, CoreError> {
    let value = serde_json::to_value(recording)
        .map_err(|error| CoreError::invalid(format!("录制序列化失败: {error}")))?;
    canonical_sha256(&value)
}

pub fn write_recording(
    path: &Path,
    recording: &Recording,
    sensitive_schema: Option<&Value>,
) -> Result<(), CoreError> {
    let value = serde_json::to_value(recording)
        .map_err(|error| CoreError::invalid(format!("录制序列化失败: {error}")))?;
    ensure_recording_safe(&value, sensitive_schema)?;
    let partial = path.with_extension("partial");
    let bytes = serde_json::to_vec_pretty(recording)
        .map_err(|error| CoreError::invalid(format!("录制序列化失败: {error}")))?;
    if let Err(error) = fs::write(&partial, bytes).and_then(|_| fs::rename(&partial, path)) {
        let _ = fs::remove_file(&partial);
        return Err(error.into());
    }
    Ok(())
}

fn ensure_recording_safe(value: &Value, schema: Option<&Value>) -> Result<(), CoreError> {
    if schema.is_some_and(|schema| has_sensitive_schema_value(value, schema))
        || contains_secret(value)
    {
        return Err(CoreError::new(
            ErrorKind::RecordingUnsafe,
            "录制内容包含敏感字段、密钥或授权句柄",
        ));
    }
    Ok(())
}

fn has_sensitive_schema_value(value: &Value, schema: &Value) -> bool {
    if schema.get("x-deepswarm-sensitive").and_then(Value::as_bool) == Some(true)
        && !value.is_null()
    {
        return true;
    }
    match (value, schema.get("properties")) {
        (Value::Object(value), Some(Value::Object(properties))) => {
            properties.iter().any(|(key, schema)| {
                value
                    .get(key)
                    .is_some_and(|value| has_sensitive_schema_value(value, schema))
            })
        }
        _ => false,
    }
}

fn contains_secret(value: &Value) -> bool {
    const SECRET_KEYS: &[&str] = &[
        "authorization",
        "proxy-authorization",
        "cookie",
        "set-cookie",
        "x-api-key",
        "api_key",
        "token",
        "access_token",
        "refresh_token",
        "password",
        "secret",
    ];
    match value {
        Value::String(value) => {
            let key = Regex::new(r"(?i)\bsk-[A-Za-z0-9_-]{8,}\b").expect("固定正则有效");
            let pem = Regex::new(
                r"(?s)-----BEGIN [^-]+ PRIVATE KEY-----.*?-----END [^-]+ PRIVATE KEY-----",
            )
            .expect("固定正则有效");
            key.is_match(value)
                || pem.is_match(value)
                || value.starts_with("secret://")
                || value.contains("SecretHandle(")
        }
        Value::Array(values) => values.iter().any(contains_secret),
        Value::Object(values) => values.iter().any(|(key, value)| {
            SECRET_KEYS.contains(&key.to_ascii_lowercase().as_str()) || contains_secret(value)
        }),
        _ => false,
    }
}

fn apply_defaults(value: &mut Value, schema: &Value) {
    if let (Value::Object(value), Some(Value::Object(properties))) =
        (value, schema.get("properties"))
    {
        for (key, property) in properties {
            if !value.contains_key(key)
                && let Some(default) = property.get("default")
            {
                value.insert(key.clone(), default.clone());
            }
            if let Some(child) = value.get_mut(key) {
                apply_defaults(child, property);
            }
        }
    }
}

fn mismatch(index: usize, message: &str) -> CoreError {
    CoreError::new(
        ErrorKind::ReplayMismatch,
        format!("事件 {index} 首个差异: {message}"),
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn event(parameters: Value) -> RecordedEvent {
        RecordedEvent {
            sequence: 0,
            kind: "tool".into(),
            name: "diagnostics".into(),
            parameters,
            result: Some(json!({"ok": true})),
            error: None,
            duration_ms: 2,
            event_hash: String::new(),
        }
    }

    #[test]
    fn replay_matches_defaults_and_finds_first_difference() {
        let scenario = json!({"name": "case"});
        let config = json!({"offline": true});
        let recording = Recording::new(&scenario, &config, 7, vec![event(json!({}))]).unwrap();
        let schema = json!({"properties": {"limit": {"default": 10}}});
        let mut replay = Replayer::new(recording, &scenario, &config).unwrap();
        replay
            .next(ReplayCall {
                kind: "tool",
                name: "diagnostics",
                parameters: json!({"limit": 10}),
                schema: Some(&schema),
            })
            .unwrap();
        replay.finish().unwrap();
    }

    #[test]
    fn refuses_secret_before_writing() {
        let directory = tempfile::tempdir().unwrap();
        let recording = Recording::new(
            &json!({}),
            &json!({}),
            1,
            vec![event(json!({"api_key": "sk-12345678"}))],
        )
        .unwrap();
        let path = directory.path().join("recording.json");
        let error = write_recording(&path, &recording, None).unwrap_err();
        assert_eq!(error.kind, ErrorKind::RecordingUnsafe);
        assert!(!path.exists());
    }
}
