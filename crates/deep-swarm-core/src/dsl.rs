use std::collections::{BTreeMap, BTreeSet};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{
    CoreError, FixtureRoot, SimilarityRegistry,
    data::preflight_sources,
    value::{lookup_path, resolve_static_scopes},
};

const DSL_SCHEMA: &str = include_str!("../../../schemas/dsl-v1.schema.json");
const MAX_EXPANDED_STEPS: usize = 10_000;

#[derive(Clone, Copy, Debug)]
pub enum InputFormat {
    Json,
    Yaml,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Document {
    pub version: u64,
    #[serde(default)]
    pub variables: Map<String, Value>,
    #[serde(default)]
    pub data_sources: Vec<DataSource>,
    pub suites: Vec<Suite>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DataSource {
    pub id: String,
    pub format: DataFormat,
    pub path: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DataFormat {
    Csv,
    Jsonl,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Suite {
    pub name: String,
    #[serde(default)]
    pub variables: Map<String, Value>,
    pub load: Option<Load>,
    #[serde(default)]
    pub assertions: Vec<Assertion>,
    pub cases: Vec<Case>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Load {
    pub phases: Vec<LoadPhase>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LoadPhase {
    pub name: String,
    pub duration_seconds: u64,
    pub start_rate: f64,
    pub target_rate: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    pub name: String,
    #[serde(default)]
    pub variables: Map<String, Value>,
    pub for_each: Option<String>,
    pub weight: Option<f64>,
    pub repeat: Option<usize>,
    pub steps: Vec<Step>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub id: String,
    pub name: Option<String>,
    #[serde(default)]
    pub variables: Map<String, Value>,
    pub repeat: Option<usize>,
    pub when: Option<Value>,
    pub tool: String,
    #[serde(rename = "with")]
    pub params: Map<String, Value>,
    #[serde(default)]
    pub capture: BTreeMap<String, String>,
    #[serde(default)]
    pub assertions: Vec<Assertion>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Assertion {
    Exists {
        id: String,
        actual: Value,
    },
    JsonSchema {
        id: String,
        actual: Value,
        schema: Value,
    },
    Contains {
        id: String,
        actual: Value,
        expected: Value,
    },
    Regex {
        id: String,
        actual: Value,
        expected: Value,
    },
    Similarity {
        id: String,
        actual: Value,
        expected: Value,
        evaluator: String,
        min: f64,
    },
    Metric {
        id: String,
        phase: String,
        metric: String,
        aggregate: String,
        op: String,
        value: f64,
        min_samples: usize,
    },
}

impl Assertion {
    pub(crate) fn id(&self) -> &str {
        match self {
            Self::Exists { id, .. }
            | Self::JsonSchema { id, .. }
            | Self::Contains { id, .. }
            | Self::Regex { id, .. }
            | Self::Similarity { id, .. }
            | Self::Metric { id, .. } => id,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PreparedRun {
    pub document: Document,
    pub data: BTreeMap<String, crate::DataSet>,
}

pub fn prepare(
    input: &[u8],
    format: InputFormat,
    fixtures: &FixtureRoot,
    evaluators: &SimilarityRegistry,
) -> Result<PreparedRun, CoreError> {
    let value: Value = match format {
        InputFormat::Json => serde_json::from_slice(input)
            .map_err(|error| CoreError::invalid(format!("DSL 解析失败: {error}")))?,
        InputFormat::Yaml => serde_yaml::from_slice(input)
            .map_err(|error| CoreError::invalid(format!("DSL 解析失败: {error}")))?,
    };
    let schema: Value = serde_json::from_str(DSL_SCHEMA)
        .map_err(|error| CoreError::invalid(format!("内置 DSL Schema 无效: {error}")))?;
    let validator = jsonschema::validator_for(&schema)
        .map_err(|error| CoreError::invalid(format!("内置 DSL Schema 无效: {error}")))?;
    if let Err(error) = validator.validate(&value) {
        return Err(CoreError::invalid(format!("DSL Schema 校验失败: {error}")));
    }
    let document: Document = serde_json::from_value(value)
        .map_err(|error| CoreError::invalid(format!("DSL 结构无效: {error}")))?;
    preflight_document(&document, evaluators)?;
    let data = preflight_sources(fixtures, &document.data_sources)?;
    validate_expansion(&document, &data)?;
    Ok(PreparedRun { document, data })
}

fn preflight_document(
    document: &Document,
    evaluators: &SimilarityRegistry,
) -> Result<(), CoreError> {
    check_reserved(&document.variables)?;
    unique(
        document.suites.iter().map(|suite| suite.name.as_str()),
        "套件名称",
    )?;
    let global_variables = document.variables.keys().cloned().collect::<BTreeSet<_>>();
    validate_static_values(&document.variables, &global_variables)?;
    let source_ids = unique(
        document
            .data_sources
            .iter()
            .map(|source| source.id.as_str()),
        "数据源",
    )?;
    for suite in &document.suites {
        check_reserved(&suite.variables)?;
        unique(
            suite.cases.iter().map(|case| case.name.as_str()),
            "用例名称",
        )?;
        let mut suite_variables = global_variables.clone();
        suite_variables.extend(suite.variables.keys().cloned());
        validate_static_values(&suite.variables, &suite_variables)?;
        let phases = suite
            .load
            .as_ref()
            .map(|load| {
                unique(
                    load.phases.iter().map(|phase| phase.name.as_str()),
                    "负载阶段",
                )
            })
            .transpose()?
            .unwrap_or_default();
        validate_assertions(&suite.assertions, true, &phases, evaluators)?;
        for case in &suite.cases {
            check_reserved(&case.variables)?;
            let mut case_variables = suite_variables.clone();
            case_variables.extend(case.variables.keys().cloned());
            validate_static_values(&case.variables, &case_variables)?;
            if let Some(source) = &case.for_each
                && !source_ids.contains(source.as_str())
            {
                return Err(CoreError::invalid(format!("未知数据源: {source}")));
            }
            preflight_case(document, suite, case, evaluators)?;
        }
    }
    Ok(())
}

fn preflight_case(
    document: &Document,
    suite: &Suite,
    case: &Case,
    evaluators: &SimilarityRegistry,
) -> Result<(), CoreError> {
    let case_static =
        resolve_static_scopes(&[&document.variables, &suite.variables, &case.variables])?;
    let mut variables = document.variables.keys().cloned().collect::<BTreeSet<_>>();
    variables.extend(suite.variables.keys().cloned());
    variables.extend(case.variables.keys().cloned());
    let mut prior_steps = BTreeSet::new();
    let mut runtime = BTreeSet::new();
    let mut all_steps = BTreeSet::new();
    for step in &case.steps {
        if !all_steps.insert(step.id.clone()) {
            return Err(CoreError::invalid(format!("重复步骤 ID: {}", step.id)));
        }
    }
    for step in &case.steps {
        check_reserved(&step.variables)?;
        let mut step_variables = variables.clone();
        step_variables.extend(step.variables.keys().cloned());
        let step_static = resolve_static_scopes(&[&case_static, &step.variables])?;
        validate_embedded_static_types(
            std::iter::once(&Value::Object(step.params.clone()))
                .chain(step.when.iter())
                .chain(step.assertions.iter().flat_map(assertion_values)),
            &step_static,
        )?;
        validate_references(
            step,
            &step_variables,
            case.for_each.is_some(),
            &prior_steps,
            &all_steps,
            &runtime,
        )?;
        for capture in step.capture.keys() {
            if step_variables.contains(capture) || runtime.contains(capture) {
                return Err(CoreError::invalid(format!("运行时变量不可覆盖: {capture}")));
            }
            runtime.insert(capture.clone());
        }
        let mut assertion_steps = prior_steps.clone();
        assertion_steps.insert(step.id.clone());
        validate_reference_values(
            step.assertions.iter().flat_map(assertion_values),
            &step_variables,
            case.for_each.is_some(),
            &assertion_steps,
            &all_steps,
            &runtime,
        )?;
        validate_assertions(&step.assertions, false, &BTreeSet::new(), evaluators)?;
        prior_steps.insert(step.id.clone());
    }
    Ok(())
}

fn validate_assertions(
    assertions: &[Assertion],
    suite_level: bool,
    phases: &BTreeSet<&str>,
    evaluators: &SimilarityRegistry,
) -> Result<(), CoreError> {
    unique(assertions.iter().map(Assertion::id), "断言")?;
    for assertion in assertions {
        match assertion {
            Assertion::Metric { phase, .. } if !suite_level => {
                return Err(CoreError::invalid("metric 断言只能位于套件级"));
            }
            Assertion::Metric { phase, .. } if !phases.contains(phase.as_str()) => {
                return Err(CoreError::invalid(format!(
                    "metric 引用了未知阶段: {phase}"
                )));
            }
            Assertion::Similarity { evaluator, .. } if !evaluators.contains(evaluator) => {
                return Err(CoreError::invalid(format!(
                    "未注册相似度求值器: {evaluator}"
                )));
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_references(
    step: &Step,
    variables: &BTreeSet<String>,
    has_data: bool,
    prior_steps: &BTreeSet<String>,
    all_steps: &BTreeSet<String>,
    runtime: &BTreeSet<String>,
) -> Result<(), CoreError> {
    let parameters = Value::Object(step.params.clone());
    let values = std::iter::once(&parameters)
        .chain(step.when.iter())
        .chain(step.variables.values());
    validate_reference_values(values, variables, has_data, prior_steps, all_steps, runtime)
}

fn validate_reference_values<'a>(
    values: impl IntoIterator<Item = &'a Value>,
    variables: &BTreeSet<String>,
    has_data: bool,
    prior_steps: &BTreeSet<String>,
    all_steps: &BTreeSet<String>,
    runtime: &BTreeSet<String>,
) -> Result<(), CoreError> {
    for value in values {
        for path in placeholder_paths(value)? {
            let mut parts = path.split('.');
            let root = parts.next().unwrap_or_default();
            match root {
                "data" if has_data => {}
                "data" => return Err(CoreError::invalid("未使用 for_each 的用例不能引用 data")),
                "runtime" => {
                    let name = parts.next().unwrap_or_default();
                    if !runtime.contains(name) {
                        return Err(CoreError::invalid(format!("未定义运行时变量: {name}")));
                    }
                }
                "steps" => {
                    let id = parts.next().unwrap_or_default();
                    if all_steps.contains(id) && !prior_steps.contains(id) {
                        return Err(CoreError::invalid(format!("前向步骤引用: {id}")));
                    }
                    if !prior_steps.contains(id) {
                        return Err(CoreError::invalid(format!("未知步骤引用: {id}")));
                    }
                }
                name if variables.contains(name) => {}
                name => return Err(CoreError::invalid(format!("未定义变量: {name}"))),
            }
        }
    }
    Ok(())
}

fn assertion_values(assertion: &Assertion) -> Vec<&Value> {
    match assertion {
        Assertion::Exists { actual, .. } | Assertion::JsonSchema { actual, .. } => vec![actual],
        Assertion::Contains {
            actual, expected, ..
        }
        | Assertion::Regex {
            actual, expected, ..
        }
        | Assertion::Similarity {
            actual, expected, ..
        } => vec![actual, expected],
        Assertion::Metric { .. } => Vec::new(),
    }
}

fn placeholder_paths(value: &Value) -> Result<Vec<String>, CoreError> {
    let regex = Regex::new(r"\$\{([^{}]+)\}").expect("固定正则有效");
    let mut paths = Vec::new();
    match value {
        Value::String(value) => {
            for capture in regex.captures_iter(value) {
                paths.push(capture[1].to_owned());
            }
            if value.contains("${") && paths.is_empty() {
                return Err(CoreError::invalid(format!("占位符语法无效: {value}")));
            }
        }
        Value::Array(values) => {
            for value in values {
                paths.extend(placeholder_paths(value)?);
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                paths.extend(placeholder_paths(value)?);
            }
        }
        _ => {}
    }
    Ok(paths)
}

fn validate_expansion(
    document: &Document,
    data: &BTreeMap<String, crate::DataSet>,
) -> Result<(), CoreError> {
    let mut total = 0usize;
    for suite in &document.suites {
        for case in &suite.cases {
            let records = case
                .for_each
                .as_ref()
                .map(|id| data[id].records)
                .unwrap_or(1);
            let case_repeat = case.repeat.unwrap_or(1);
            let steps = case
                .steps
                .iter()
                .try_fold(0usize, |sum, step| {
                    sum.checked_add(step.repeat.unwrap_or(1))
                })
                .ok_or_else(|| CoreError::invalid("步骤展开数量溢出"))?;
            total = total
                .checked_add(records.saturating_mul(case_repeat).saturating_mul(steps))
                .ok_or_else(|| CoreError::invalid("步骤展开数量溢出"))?;
            if total > MAX_EXPANDED_STEPS {
                return Err(CoreError::invalid("展开后的总步骤数超过 10000"));
            }
        }
    }
    Ok(())
}

fn check_reserved(variables: &Map<String, Value>) -> Result<(), CoreError> {
    if let Some(name) = variables
        .keys()
        .find(|name| matches!(name.as_str(), "data" | "steps" | "runtime"))
    {
        return Err(CoreError::invalid(format!("保留命名空间不可声明: {name}")));
    }
    Ok(())
}

fn validate_static_values(
    values: &Map<String, Value>,
    variables: &BTreeSet<String>,
) -> Result<(), CoreError> {
    for value in values.values() {
        for path in placeholder_paths(value)? {
            let root = path.split('.').next().unwrap_or_default();
            if !variables.contains(root) {
                return Err(CoreError::invalid(format!("未定义静态变量: {root}")));
            }
        }
    }
    Ok(())
}

fn validate_embedded_static_types<'a>(
    values: impl IntoIterator<Item = &'a Value>,
    variables: &Map<String, Value>,
) -> Result<(), CoreError> {
    let context = Value::Object(variables.clone());
    let regex = Regex::new(r"\$\{([^{}]+)\}").expect("固定正则有效");
    for value in values {
        for text in strings(value) {
            let exact = regex
                .captures(text)
                .and_then(|capture| capture.get(0))
                .is_some_and(|matched| matched.as_str() == text);
            for capture in regex.captures_iter(text) {
                let path = &capture[1];
                if matches!(path.split('.').next(), Some("data" | "steps" | "runtime")) {
                    continue;
                }
                let resolved = lookup_path(&context, path)
                    .ok_or_else(|| CoreError::invalid(format!("未定义静态变量路径: {path}")))?;
                if !exact && !resolved.is_string() {
                    return Err(CoreError::invalid(format!(
                        "字符串插值只接受字符串: {path}"
                    )));
                }
            }
        }
    }
    Ok(())
}

fn strings(value: &Value) -> Vec<&str> {
    match value {
        Value::String(value) => vec![value],
        Value::Array(values) => values.iter().flat_map(strings).collect(),
        Value::Object(values) => values.values().flat_map(strings).collect(),
        _ => Vec::new(),
    }
}

fn unique<'a>(
    values: impl IntoIterator<Item = &'a str>,
    subject: &str,
) -> Result<BTreeSet<&'a str>, CoreError> {
    let mut unique = BTreeSet::new();
    for value in values {
        if !unique.insert(value) {
            return Err(CoreError::invalid(format!("重复{subject}: {value}")));
        }
    }
    Ok(unique)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn yaml_and_json_share_schema_and_preflight() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("rows.jsonl"), "{\"name\":\"甲\"}\n").unwrap();
        let root = FixtureRoot::new(directory.path()).unwrap();
        let yaml = br#"
version: 1
data_sources:
  - {id: rows, format: jsonl, path: rows.jsonl}
suites:
  - name: suite
    cases:
      - name: case
        for_each: rows
        steps:
          - id: first
            tool: diagnostics
            with: {name: "${data.name}"}
"#;
        let prepared = prepare(
            yaml,
            InputFormat::Yaml,
            &root,
            &SimilarityRegistry::default(),
        )
        .unwrap();
        assert_eq!(prepared.data["rows"].records, 1);

        let invalid = br#"{"version":1,"unknown":true,"suites":[]}"#;
        let error = prepare(
            invalid,
            InputFormat::Json,
            &root,
            &SimilarityRegistry::default(),
        )
        .unwrap_err();
        assert!(error.message.contains("Schema"));
    }

    #[test]
    fn forward_reference_and_invalid_data_fail_in_preflight() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("bad.csv"), "id,id\n1,2\n").unwrap();
        let root = FixtureRoot::new(directory.path()).unwrap();
        let forward = br#"
version: 1
suites:
  - name: suite
    cases:
      - name: case
        steps:
          - id: first
            tool: diagnostics
            with: {value: "${steps.later.value}"}
          - id: later
            tool: diagnostics
            with: {}
"#;
        assert!(
            prepare(
                forward,
                InputFormat::Yaml,
                &root,
                &SimilarityRegistry::default()
            )
            .unwrap_err()
            .message
            .contains("前向")
        );

        let bad_csv = br#"
version: 1
data_sources:
  - {id: rows, format: csv, path: bad.csv}
suites:
  - name: suite
    cases:
      - name: case
        for_each: rows
        steps: []
"#;
        assert!(
            prepare(
                bad_csv,
                InputFormat::Yaml,
                &root,
                &SimilarityRegistry::default()
            )
            .unwrap_err()
            .message
            .contains("表头")
        );

        let static_type = br#"
version: 1
variables: {number: 7}
suites:
  - name: suite
    cases:
      - name: case
        steps:
          - {id: step, tool: diagnostics, with: {value: "number=${number}"}}
"#;
        assert!(
            prepare(
                static_type,
                InputFormat::Yaml,
                &root,
                &SimilarityRegistry::default()
            )
            .unwrap_err()
            .message
            .contains("只接受字符串")
        );
    }
}
