use std::{
    fs,
    path::Path,
    time::{Duration, SystemTime},
};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{CoreError, canonical_json, canonical_sha256};

const REPORT_SCHEMA: &str = include_str!("../../../schemas/report-v1.schema.json");
const STREAM_LIMIT: usize = 4 * 1024;
const CONTEXT_LIMIT: usize = 64 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ReportStatus {
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CaseReportStatus {
    Passed,
    Failed,
    Skipped,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReportSummary {
    pub planned: u64,
    pub completed: u64,
    pub passed: u64,
    pub failed: u64,
    pub skipped: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CaseReport {
    pub id: String,
    pub status: CaseReportStatus,
    pub steps: Vec<Value>,
    pub error: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct UncertainOperation {
    pub call_id: String,
    pub tool: String,
    pub target: String,
    pub started_at: String,
    pub cancelled_at: String,
    pub reason: String,
    pub closed: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Report {
    pub schema_version: u64,
    pub run_id: String,
    pub status: ReportStatus,
    pub created_at: String,
    pub summary: ReportSummary,
    pub cases: Vec<CaseReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<Value>,
    pub recording_hash: Option<String>,
    pub uncertain_operations: Vec<UncertainOperation>,
}

pub fn sanitize_report(
    report: &Report,
    additional_sensitive_paths: &[String],
) -> Result<Report, CoreError> {
    if report.schema_version != 1 {
        return Err(CoreError::invalid("未知报告 schema_version"));
    }
    let mut value = serde_json::to_value(report)
        .map_err(|error| CoreError::invalid(format!("报告序列化失败: {error}")))?;
    for pointer in additional_sensitive_paths {
        if let Some(value) = value.pointer_mut(pointer) {
            *value = Value::String("[REDACTED]".to_owned());
        }
    }
    redact(&mut value, false)?;
    validate_report(&value)?;
    serde_json::from_value(value)
        .map_err(|error| CoreError::invalid(format!("脱敏报告结构无效: {error}")))
}

pub fn render_json(
    report: &Report,
    additional_sensitive_paths: &[String],
) -> Result<Vec<u8>, CoreError> {
    serde_json::to_vec_pretty(&sanitize_report(report, additional_sensitive_paths)?)
        .map_err(|error| CoreError::invalid(format!("JSON 报告生成失败: {error}")))
}

pub fn render_junit(
    report: &Report,
    additional_sensitive_paths: &[String],
) -> Result<String, CoreError> {
    let report = sanitize_report(report, additional_sensitive_paths)?;
    let mut output = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><testsuite name=\"DeepSwarm\" tests=\"{}\" failures=\"{}\" skipped=\"{}\">",
        report.summary.planned, report.summary.failed, report.summary.skipped
    );
    for case in report.cases {
        output.push_str(&format!("<testcase name=\"{}\">", xml_escape(&case.id)));
        match case.status {
            CaseReportStatus::Failed => output.push_str(&format!(
                "<failure message=\"{}\"/>",
                xml_escape(
                    &case
                        .error
                        .as_ref()
                        .map(Value::to_string)
                        .unwrap_or_else(|| "failed".to_owned())
                )
            )),
            CaseReportStatus::Skipped => output.push_str("<skipped/>"),
            CaseReportStatus::Cancelled => output.push_str("<error message=\"cancelled\"/>"),
            CaseReportStatus::Passed => {}
        }
        output.push_str("</testcase>");
    }
    output.push_str("</testsuite>");
    Ok(output)
}

pub fn render_html(
    report: &Report,
    additional_sensitive_paths: &[String],
) -> Result<String, CoreError> {
    let report = sanitize_report(report, additional_sensitive_paths)?;
    let mut rows = String::new();
    for case in report.cases {
        rows.push_str(&format!(
            "<tr><td>{}</td><td>{:?}</td></tr>",
            html_escape(&case.id),
            case.status
        ));
    }
    Ok(format!(
        "<!doctype html><html lang=\"zh-CN\"><meta charset=\"utf-8\"><title>DeepSwarm {}</title><body><h1>DeepSwarm {}</h1><table><thead><tr><th>用例</th><th>状态</th></tr></thead><tbody>{rows}</tbody></table></body></html>",
        html_escape(&report.run_id),
        html_escape(&report.run_id)
    ))
}

pub trait Clock {
    fn now(&self) -> SystemTime;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

pub fn prune_reports(
    directory: &Path,
    retention_days: u16,
    clock: &dyn Clock,
) -> Result<Vec<std::path::PathBuf>, CoreError> {
    if retention_days > 365 {
        return Err(CoreError::invalid("retention_days 必须为 0..=365"));
    }
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let retention = Duration::from_secs(u64::from(retention_days) * 24 * 60 * 60);
    let mut removed = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let created_at = report_created_at(&entry.path()).unwrap_or(entry.metadata()?.modified()?);
        if clock.now().duration_since(created_at).unwrap_or_default() >= retention {
            fs::remove_file(entry.path())?;
            removed.push(entry.path());
        }
    }
    Ok(removed)
}

fn report_created_at(path: &Path) -> Option<SystemTime> {
    let value: Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    parse_utc(value.get("created_at")?.as_str()?)
}

fn parse_utc(value: &str) -> Option<SystemTime> {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || !value.ends_with('Z')
    {
        return None;
    }
    let number = |range: std::ops::Range<usize>| value.get(range)?.parse::<i64>().ok();
    let year = number(0..4)?;
    let month = number(5..7)?;
    let day = number(8..10)?;
    let hour = number(11..13)?;
    let minute = number(14..16)?;
    let second = number(17..19)?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=60).contains(&second)
    {
        return None;
    }
    // Civil-date conversion avoids a date-time dependency for the single retention comparison.
    let adjusted_year = year - i64::from(month <= 2);
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let adjusted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    let seconds = days
        .checked_mul(86_400)?
        .checked_add(hour * 3_600 + minute * 60 + second)?;
    (seconds >= 0).then(|| SystemTime::UNIX_EPOCH + Duration::from_secs(seconds as u64))
}

fn redact(value: &mut Value, free_object: bool) -> Result<(), CoreError> {
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
    const FREE_ALLOWED: &[&str] = &[
        "id",
        "status",
        "tool",
        "value",
        "metadata",
        "error",
        "assertions",
        "assertion_id",
        "message",
        "category",
        "retryable",
        "stdout",
        "stderr",
        "path",
        "bytes",
        "sha256",
        "original_length",
        "truncated",
    ];
    match value {
        Value::String(text) => *text = redact_text(text),
        Value::Array(values) => {
            for value in values {
                redact(value, free_object)?;
            }
        }
        Value::Object(values) => {
            let keys = values.keys().cloned().collect::<Vec<_>>();
            for key in keys {
                let item = values.get_mut(&key).expect("键存在");
                let lower = key.to_ascii_lowercase();
                if SECRET_KEYS.contains(&lower.as_str()) {
                    *item = Value::String("[REDACTED]".to_owned());
                } else if free_object && !FREE_ALLOWED.contains(&lower.as_str()) {
                    *item = json!({"type": type_name(item)});
                } else if matches!(lower.as_str(), "stdout" | "stderr") {
                    if let Some(text) = item.as_str() {
                        *item = Value::String(truncate_utf8(&redact_text(text), STREAM_LIMIT));
                    } else {
                        redact(item, true)?;
                    }
                } else {
                    let child_is_free = free_object
                        || matches!(
                            lower.as_str(),
                            "steps" | "error" | "metrics" | "value" | "metadata"
                        );
                    redact(item, child_is_free)?;
                }
            }
            if free_object && canonical_json(&Value::Object(values.clone()))?.len() > CONTEXT_LIMIT
            {
                let original = Value::Object(values.clone());
                *value = json!({
                    "truncated": true,
                    "original_length": canonical_json(&original)?.len(),
                    "sha256": canonical_sha256(&original)?,
                });
            }
        }
        _ => {}
    }
    Ok(())
}

fn redact_text(text: &str) -> String {
    let key = Regex::new(r"(?i)\bsk-[A-Za-z0-9_-]{8,}\b").expect("固定正则有效");
    let pem =
        Regex::new(r"(?s)-----BEGIN [^-]+ PRIVATE KEY-----.*?-----END [^-]+ PRIVATE KEY-----")
            .expect("固定正则有效");
    pem.replace_all(&key.replace_all(text, "[REDACTED]"), "[REDACTED]")
        .into_owned()
}

fn truncate_utf8(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn validate_report(value: &Value) -> Result<(), CoreError> {
    let schema: Value = serde_json::from_str(REPORT_SCHEMA)
        .map_err(|error| CoreError::invalid(format!("内置报告 Schema 无效: {error}")))?;
    let validator = jsonschema::validator_for(&schema)
        .map_err(|error| CoreError::invalid(format!("内置报告 Schema 无效: {error}")))?;
    validator
        .validate(value)
        .map_err(|error| CoreError::invalid(format!("报告 Schema 校验失败: {error}")))
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn html_escape(text: &str) -> String {
    xml_escape(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(step: Value) -> Report {
        Report {
            schema_version: 1,
            run_id: "run".into(),
            status: ReportStatus::Failed,
            created_at: "2026-07-14T00:00:00Z".into(),
            summary: ReportSummary {
                planned: 1,
                completed: 1,
                passed: 0,
                failed: 1,
                skipped: 0,
            },
            cases: vec![CaseReport {
                id: "case".into(),
                status: CaseReportStatus::Failed,
                steps: vec![step],
                error: None,
            }],
            metrics: None,
            recording_hash: None,
            uncertain_operations: Vec::new(),
        }
    }

    #[test]
    fn every_renderer_uses_the_same_redaction() {
        let report = report(json!({
            "id": "step",
            "stdout": format!("sk-12345678{}", "x".repeat(5000)),
            "unknown": "must-not-leak",
            "authorization": "Bearer secret"
        }));
        for output in [
            String::from_utf8(render_json(&report, &[]).unwrap()).unwrap(),
            render_junit(&report, &[]).unwrap(),
            render_html(&report, &[]).unwrap(),
        ] {
            assert!(!output.contains("sk-12345678"));
            assert!(!output.contains("Bearer secret"));
            assert!(!output.contains("must-not-leak"));
        }
    }

    #[test]
    fn stream_and_context_limits_are_applied() {
        let stream = report(json!({"id": "stream", "stdout": "x".repeat(5000)}));
        let sanitized = sanitize_report(&stream, &[]).unwrap();
        assert_eq!(
            sanitized.cases[0].steps[0]["stdout"]
                .as_str()
                .unwrap()
                .len(),
            STREAM_LIMIT
        );

        let context = report(json!({"id": "context", "message": "x".repeat(70_000)}));
        let sanitized = sanitize_report(&context, &[]).unwrap();
        assert_eq!(sanitized.cases[0].steps[0]["truncated"], true);
        assert!(
            sanitized.cases[0].steps[0]["original_length"]
                .as_u64()
                .unwrap()
                > 65_536
        );
        assert_eq!(
            sanitized.cases[0].steps[0]["sha256"]
                .as_str()
                .unwrap()
                .len(),
            64
        );
    }

    struct FixedClock(SystemTime);

    impl Clock for FixedClock {
        fn now(&self) -> SystemTime {
            self.0
        }
    }

    #[test]
    fn retention_keeps_before_boundary_and_deletes_at_boundary() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("report.json");
        let report = report(json!({"id": "step"}));
        fs::write(&path, render_json(&report, &[]).unwrap()).unwrap();
        let created = parse_utc("2026-07-14T00:00:00Z").unwrap();
        let seven_days = Duration::from_secs(7 * 24 * 60 * 60);
        assert!(
            prune_reports(
                directory.path(),
                7,
                &FixedClock(created + seven_days - Duration::from_secs(1))
            )
            .unwrap()
            .is_empty()
        );
        assert_eq!(
            prune_reports(directory.path(), 7, &FixedClock(created + seven_days)).unwrap(),
            vec![path]
        );
    }
}
