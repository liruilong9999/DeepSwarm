use std::collections::BTreeMap;

use async_trait::async_trait;
use serde_json::{Map, Value, json};

use crate::{
    CoreError, ErrorKind, FixtureRoot, Metrics, PreparedRun, SimilarityRegistry,
    assertion::{assertion_error, evaluate_assertions},
    dsl::{Case, Step},
    value::{interpolate, resolve_static_scopes},
};

#[derive(Clone, Debug, PartialEq)]
pub struct ActionOutput {
    pub value: Value,
    pub metadata: Value,
}

#[async_trait]
pub trait ActionExecutor: Send + Sync {
    fn parameters_schema(&self, _tool: &str) -> Option<Value> {
        None
    }

    async fn execute(&self, tool: &str, parameters: Value) -> Result<ActionOutput, CoreError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaseStatus {
    Passed,
    Failed,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CaseOutcome {
    pub id: String,
    pub status: CaseStatus,
    pub steps: Value,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExecutionResult {
    pub cases: Vec<CaseOutcome>,
    pub suite_failures: BTreeMap<String, String>,
    pub tool_calls: usize,
}

pub async fn run(
    prepared: &PreparedRun,
    fixtures: &FixtureRoot,
    executor: &dyn ActionExecutor,
    evaluators: &SimilarityRegistry,
    metrics: &Metrics,
) -> Result<ExecutionResult, CoreError> {
    let mut result = ExecutionResult::default();
    for suite in &prepared.document.suites {
        for case in &suite.cases {
            let records = match &case.for_each {
                Some(source) => prepared.data[source].read(fixtures)?,
                None => vec![Value::Null],
            };
            for (record_index, data) in records.into_iter().enumerate() {
                for repetition in 0..case.repeat.unwrap_or(1) {
                    let id = case_id(
                        case,
                        record_index + 1,
                        repetition + 1,
                        case.repeat.unwrap_or(1),
                    );
                    let mut context = resolve_static_scopes(&[
                        &prepared.document.variables,
                        &suite.variables,
                        &case.variables,
                    ])?;
                    context.insert("data".to_owned(), data.clone());
                    context.insert("steps".to_owned(), Value::Object(Map::new()));
                    context.insert("runtime".to_owned(), Value::Object(Map::new()));
                    let outcome = run_case(
                        id,
                        case,
                        context,
                        executor,
                        evaluators,
                        metrics,
                        &mut result.tool_calls,
                    )
                    .await;
                    result.cases.push(outcome);
                }
            }
        }
        let context = Value::Object(Map::new());
        if let Err(failure) = evaluate_assertions(&suite.assertions, &context, evaluators, metrics)
        {
            result
                .suite_failures
                .insert(suite.name.clone(), assertion_error(failure).to_string());
        }
    }
    Ok(result)
}

async fn run_case(
    id: String,
    case: &Case,
    mut context: Map<String, Value>,
    executor: &dyn ActionExecutor,
    evaluators: &SimilarityRegistry,
    metrics: &Metrics,
    tool_calls: &mut usize,
) -> CaseOutcome {
    for step in &case.steps {
        for _ in 0..step.repeat.unwrap_or(1) {
            if let Err(error) = run_step(
                step,
                &mut context,
                executor,
                evaluators,
                metrics,
                tool_calls,
            )
            .await
            {
                return CaseOutcome {
                    id,
                    status: CaseStatus::Failed,
                    steps: context["steps"].clone(),
                    error: Some(error.to_string()),
                };
            }
        }
    }
    CaseOutcome {
        id,
        status: CaseStatus::Passed,
        steps: context["steps"].clone(),
        error: None,
    }
}

async fn run_step(
    step: &Step,
    context: &mut Map<String, Value>,
    executor: &dyn ActionExecutor,
    evaluators: &SimilarityRegistry,
    metrics: &Metrics,
    tool_calls: &mut usize,
) -> Result<(), CoreError> {
    let mut scoped = context.clone();
    let step_variables = resolve_step_variables(context, &step.variables)?;
    for (key, value) in step_variables {
        scoped.insert(key, value);
    }
    let scoped_value = Value::Object(scoped);
    let parameters = interpolate(&Value::Object(step.params.clone()), &scoped_value)?;
    let when = step
        .when
        .as_ref()
        .map(|when| interpolate(when, &scoped_value).and_then(evaluate_when))
        .transpose()?
        .unwrap_or(true);
    if !when {
        insert_step(context, &step.id, json!({"status": "skipped"}));
        return Ok(());
    }
    if let Some(schema) = executor.parameters_schema(&step.tool) {
        let validator = jsonschema::validator_for(&schema)
            .map_err(|error| CoreError::invalid(format!("工具 Schema 无效: {error}")))?;
        if let Err(error) = validator.validate(&parameters) {
            return Err(CoreError::invalid(format!("工具参数无效: {error}")));
        }
    }
    *tool_calls += 1;
    let output = match executor.execute(&step.tool, parameters).await {
        Ok(output) => output,
        Err(error) => {
            insert_step(
                context,
                &step.id,
                json!({"status": "failed", "error": error.to_string()}),
            );
            return Err(error);
        }
    };
    insert_step(
        context,
        &step.id,
        json!({
            "status": "succeeded",
            "value": output.value,
            "metadata": output.metadata,
        }),
    );
    let step_value = context["steps"][&step.id].clone();
    for (name, pointer) in &step.capture {
        let value = step_value.pointer(pointer).cloned().ok_or_else(|| {
            CoreError::new(
                ErrorKind::UnresolvedOutput,
                format!("capture 指针不存在: {pointer}"),
            )
        })?;
        context["runtime"]
            .as_object_mut()
            .expect("runtime 始终为对象")
            .insert(name.clone(), value);
    }
    if let Err(failure) = evaluate_assertions(
        &step.assertions,
        &Value::Object(context.clone()),
        evaluators,
        metrics,
    ) {
        let step_state = context["steps"][&step.id]
            .as_object_mut()
            .expect("当前步骤状态始终为对象");
        step_state.insert("status".to_owned(), Value::String("failed".to_owned()));
        step_state.insert(
            "assertion_id".to_owned(),
            Value::String(failure.assertion_id.clone()),
        );
        return Err(assertion_error(failure));
    }
    Ok(())
}

fn resolve_step_variables(
    context: &Map<String, Value>,
    variables: &Map<String, Value>,
) -> Result<Map<String, Value>, CoreError> {
    let mut scoped = context.clone();
    scoped.extend(
        variables
            .iter()
            .map(|(key, value)| (key.clone(), value.clone())),
    );
    for _ in 0..=variables.len() {
        let snapshot = Value::Object(scoped.clone());
        let mut changed = false;
        for key in variables.keys() {
            let resolved = interpolate(&scoped[key], &snapshot)?;
            changed |= resolved != scoped[key];
            scoped.insert(key.clone(), resolved);
        }
        if !changed {
            return Ok(variables
                .keys()
                .map(|key| (key.clone(), scoped[key].clone()))
                .collect());
        }
    }
    Err(CoreError::invalid("步骤变量存在循环引用"))
}

fn insert_step(context: &mut Map<String, Value>, id: &str, value: Value) {
    context["steps"]
        .as_object_mut()
        .expect("steps 始终为对象")
        .insert(id.to_owned(), value);
}

fn evaluate_when(value: Value) -> Result<bool, CoreError> {
    match value {
        Value::Bool(value) => Ok(value),
        Value::Object(mut value) => {
            let op = value
                .remove("op")
                .and_then(|value| value.as_str().map(str::to_owned))
                .ok_or_else(|| CoreError::invalid("when.op 无效"))?;
            let left = value
                .remove("left")
                .ok_or_else(|| CoreError::invalid("when.left 缺失"))?;
            let right = value
                .remove("right")
                .ok_or_else(|| CoreError::invalid("when.right 缺失"))?;
            compare_when(&op, &left, &right)
        }
        _ => Err(CoreError::invalid("when 必须为布尔值或比较对象")),
    }
}

fn compare_when(op: &str, left: &Value, right: &Value) -> Result<bool, CoreError> {
    Ok(match op {
        "eq" => left == right,
        "ne" => left != right,
        "lt" | "lte" | "gt" | "gte" => {
            let left = left
                .as_f64()
                .ok_or_else(|| CoreError::invalid("when 数值比较类型不匹配"))?;
            let right = right
                .as_f64()
                .ok_or_else(|| CoreError::invalid("when 数值比较类型不匹配"))?;
            match op {
                "lt" => left < right,
                "lte" => left <= right,
                "gt" => left > right,
                "gte" => left >= right,
                _ => unreachable!(),
            }
        }
        "contains" => match (left, right) {
            (Value::String(left), Value::String(right)) => left.contains(right),
            (Value::Array(left), right) => left.contains(right),
            (Value::Object(left), Value::String(right)) => left.contains_key(right),
            _ => return Err(CoreError::invalid("when contains 类型不匹配")),
        },
        _ => return Err(CoreError::invalid(format!("未知 when.op: {op}"))),
    })
}

fn case_id(case: &Case, record: usize, repeat: usize, repeat_total: usize) -> String {
    let base = match &case.for_each {
        Some(source) => format!("{}:{source}:{record}", case.name),
        None => case.name.clone(),
    };
    if repeat_total > 1 {
        format!("{base}:repeat:{repeat}")
    } else {
        base
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        sync::{Arc, Mutex},
        task::{Context, Poll, Waker},
    };

    use serde_json::json;

    use super::*;
    use crate::{InputFormat, prepare};

    struct MockExecutor(Arc<Mutex<Vec<Value>>>);

    #[async_trait]
    impl ActionExecutor for MockExecutor {
        async fn execute(&self, tool: &str, parameters: Value) -> Result<ActionOutput, CoreError> {
            self.0.lock().unwrap().push(parameters.clone());
            Ok(ActionOutput {
                value: if tool == "create" {
                    json!({"id": format!("item-{}", parameters["number"])})
                } else {
                    json!({"summary": parameters["subject"]})
                },
                metadata: json!({"mock": true}),
            })
        }
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        let mut future = std::pin::pin!(future);
        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(value) => return value,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    #[test]
    fn binds_data_capture_and_prior_step_output_in_order() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("rows.jsonl"), "{\"number\":7}\n").unwrap();
        let root = FixtureRoot::new(directory.path()).unwrap();
        let source = br#"
version: 1
variables: {prefix: "result"}
data_sources:
  - {id: rows, format: jsonl, path: rows.jsonl}
suites:
  - name: suite
    cases:
      - name: case
        for_each: rows
        steps:
          - id: create
            tool: create
            with: {number: "${data.number}"}
            capture: {item_id: /value/id}
            assertions:
              - {id: has_id, type: exists, actual: "${runtime.item_id}"}
          - id: query
            variables: {label: "${prefix}"}
            tool: query
            with: {subject: "${runtime.item_id}"}
            assertions:
              - {id: contains_id, type: contains, actual: "${steps.query.value.summary}", expected: "item-7"}
"#;
        let registry = SimilarityRegistry::default();
        let prepared = prepare(source, InputFormat::Yaml, &root, &registry).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let result = block_on(run(
            &prepared,
            &root,
            &MockExecutor(calls.clone()),
            &registry,
            &Metrics::default(),
        ))
        .unwrap();
        assert_eq!(result.tool_calls, 2);
        assert_eq!(result.cases[0].id, "case:rows:1");
        assert_eq!(result.cases[0].status, CaseStatus::Passed);
        assert_eq!(calls.lock().unwrap()[0]["number"], 7);
    }

    #[test]
    fn skipped_output_fails_before_next_tool_call() {
        let directory = tempfile::tempdir().unwrap();
        let root = FixtureRoot::new(directory.path()).unwrap();
        let source = br#"
version: 1
suites:
  - name: suite
    cases:
      - name: case
        steps:
          - {id: skipped, when: false, tool: create, with: {}}
          - {id: query, tool: query, with: {subject: "${steps.skipped.value}"}}
"#;
        let registry = SimilarityRegistry::default();
        let prepared = prepare(source, InputFormat::Yaml, &root, &registry).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let result = block_on(run(
            &prepared,
            &root,
            &MockExecutor(calls.clone()),
            &registry,
            &Metrics::default(),
        ))
        .unwrap();
        assert_eq!(result.tool_calls, 0);
        assert_eq!(result.cases[0].status, CaseStatus::Failed);
        assert!(
            result.cases[0]
                .error
                .as_ref()
                .unwrap()
                .contains("UnresolvedOutput")
        );
    }
}
