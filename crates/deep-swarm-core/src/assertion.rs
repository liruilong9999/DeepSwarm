use std::{collections::BTreeMap, sync::Arc};

use regex::Regex;
use serde_json::Value;

use crate::{CoreError, ErrorKind, dsl::Assertion, value::interpolate};

pub trait SimilarityEvaluator: Send + Sync {
    fn name_and_version(&self) -> &str;
    fn evaluate(&self, actual: &str, expected: &str) -> Result<f64, CoreError>;
}

#[derive(Default)]
pub struct SimilarityRegistry(BTreeMap<String, Arc<dyn SimilarityEvaluator>>);

impl SimilarityRegistry {
    pub fn register(&mut self, evaluator: Arc<dyn SimilarityEvaluator>) -> Result<(), CoreError> {
        let name = evaluator.name_and_version().to_owned();
        if name.split_once('@').is_none() || name.starts_with('@') || name.ends_with('@') {
            return Err(CoreError::invalid("相似度求值器名称必须为 名称@版本"));
        }
        if self.0.insert(name.clone(), evaluator).is_some() {
            return Err(CoreError::invalid(format!("重复相似度求值器: {name}")));
        }
        Ok(())
    }

    pub fn contains(&self, name: &str) -> bool {
        self.0.contains_key(name)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MetricSample {
    pub phase: String,
    pub metric: String,
    pub value: f64,
}

#[derive(Clone, Debug, Default)]
pub struct Metrics(BTreeMap<(String, String), Vec<f64>>);

impl Metrics {
    pub fn record(&mut self, sample: MetricSample) {
        self.0
            .entry((sample.phase, sample.metric))
            .or_default()
            .push(sample.value);
    }

    fn values(&self, phase: &str, metric: &str) -> &[f64] {
        self.0
            .get(&(phase.to_owned(), metric.to_owned()))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AssertionFailure {
    pub assertion_id: String,
    pub message: String,
}

pub(crate) fn evaluate_assertions(
    assertions: &[Assertion],
    context: &Value,
    evaluators: &SimilarityRegistry,
    metrics: &Metrics,
) -> Result<(), AssertionFailure> {
    for assertion in assertions {
        evaluate_assertion(assertion, context, evaluators, metrics).map_err(|message| {
            AssertionFailure {
                assertion_id: assertion.id().to_owned(),
                message,
            }
        })?;
    }
    Ok(())
}

fn evaluate_assertion(
    assertion: &Assertion,
    context: &Value,
    evaluators: &SimilarityRegistry,
    metrics: &Metrics,
) -> Result<(), String> {
    match assertion {
        Assertion::Exists { actual, .. } => {
            let actual = resolve(actual, context)?;
            if actual.is_null() {
                Err("值不存在".to_owned())
            } else {
                Ok(())
            }
        }
        Assertion::JsonSchema { actual, schema, .. } => {
            let actual = resolve(actual, context)?;
            let validator = jsonschema::validator_for(schema)
                .map_err(|error| format!("断言 Schema 无效: {error}"))?;
            validator
                .validate(&actual)
                .map_err(|error| format!("JSON Schema 断言失败: {error}"))
        }
        Assertion::Contains {
            actual, expected, ..
        } => {
            let actual = resolve(actual, context)?;
            let expected = resolve(expected, context)?;
            let contains = match (&actual, &expected) {
                (Value::String(actual), Value::String(expected)) => actual.contains(expected),
                (Value::Array(actual), expected) => actual.contains(expected),
                (Value::Object(actual), Value::String(expected)) => actual.contains_key(expected),
                _ => false,
            };
            contains
                .then_some(())
                .ok_or_else(|| "contains 断言失败".to_owned())
        }
        Assertion::Regex {
            actual, expected, ..
        } => {
            let actual = string(resolve(actual, context)?, "regex.actual")?;
            let expected = string(resolve(expected, context)?, "regex.expected")?;
            Regex::new(&expected)
                .map_err(|error| format!("正则无效: {error}"))?
                .is_match(&actual)
                .then_some(())
                .ok_or_else(|| "regex 断言失败".to_owned())
        }
        Assertion::Similarity {
            actual,
            expected,
            evaluator,
            min,
            ..
        } => {
            let actual = string(resolve(actual, context)?, "similarity.actual")?;
            let expected = string(resolve(expected, context)?, "similarity.expected")?;
            let evaluator = evaluators
                .0
                .get(evaluator)
                .ok_or_else(|| "相似度求值器未注册".to_owned())?;
            let score = evaluator
                .evaluate(&actual, &expected)
                .map_err(|error| error.to_string())?;
            if !score.is_finite() || !(0.0..=1.0).contains(&score) {
                return Err(format!("相似度求值器返回非法分数: {score}"));
            }
            (score >= *min)
                .then_some(())
                .ok_or_else(|| format!("相似度 {score} 低于 {min}"))
        }
        Assertion::Metric {
            phase,
            metric,
            aggregate,
            op,
            value,
            min_samples,
            ..
        } => {
            let samples = metrics.values(phase, metric);
            if samples.len() < *min_samples {
                return Err(format!(
                    "metric 样本不足: {} < {min_samples}",
                    samples.len()
                ));
            }
            let actual = match aggregate.as_str() {
                "p95" => nearest_rank_p95(samples).expect("样本非空"),
                "max" => samples.iter().copied().fold(f64::NEG_INFINITY, f64::max),
                "rate" => samples.iter().sum::<f64>() / samples.len() as f64,
                _ => return Err(format!("未知聚合方式: {aggregate}")),
            };
            compare(op, actual, *value)
                .then_some(())
                .ok_or_else(|| format!("metric 断言失败: {actual} {op} {value}"))
        }
    }
}

fn resolve(value: &Value, context: &Value) -> Result<Value, String> {
    interpolate(value, context).map_err(|error| error.to_string())
}

fn string(value: Value, field: &str) -> Result<String, String> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("{field} 必须为字符串"))
}

fn compare(op: &str, left: f64, right: f64) -> bool {
    match op {
        "lt" => left < right,
        "lte" => left <= right,
        "gt" => left > right,
        "gte" => left >= right,
        _ => false,
    }
}

pub fn nearest_rank_p95(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = (0.95 * sorted.len() as f64).ceil() as usize;
    sorted.get(rank.saturating_sub(1)).copied()
}

pub(crate) fn assertion_error(failure: AssertionFailure) -> CoreError {
    CoreError::new(
        ErrorKind::AssertionFailed,
        format!("{}: {}", failure.assertion_id, failure.message),
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    struct Exact;

    impl SimilarityEvaluator for Exact {
        fn name_and_version(&self) -> &str {
            "exact@1"
        }

        fn evaluate(&self, actual: &str, expected: &str) -> Result<f64, CoreError> {
            Ok(if actual == expected { 1.0 } else { 0.0 })
        }
    }

    #[test]
    fn p95_uses_nearest_rank() {
        let values = (1..=20).map(f64::from).collect::<Vec<_>>();
        assert_eq!(nearest_rank_p95(&values), Some(19.0));
    }

    #[test]
    fn metric_rejects_too_few_samples() {
        let assertion = Assertion::Metric {
            id: "latency".into(),
            phase: "steady".into(),
            metric: "latency_ms".into(),
            aggregate: "p95".into(),
            op: "lt".into(),
            value: 200.0,
            min_samples: 2,
        };
        let error = evaluate_assertions(
            &[assertion],
            &Value::Null,
            &SimilarityRegistry::default(),
            &Metrics::default(),
        )
        .unwrap_err();
        assert!(error.message.contains("样本不足"));
    }

    #[test]
    fn every_assertion_type_has_pass_and_fail_paths() {
        let context = json!({"text": "hello world", "object": {"id": 1}, "missing": null});
        let mut registry = SimilarityRegistry::default();
        registry.register(Arc::new(Exact)).unwrap();
        let mut metrics = Metrics::default();
        for value in [10.0, 20.0, 30.0] {
            metrics.record(MetricSample {
                phase: "steady".into(),
                metric: "latency_ms".into(),
                value,
            });
        }
        let passes = vec![
            Assertion::Exists {
                id: "exists".into(),
                actual: json!("${object.id}"),
            },
            Assertion::JsonSchema {
                id: "schema".into(),
                actual: json!("${object}"),
                schema: json!({"type": "object", "required": ["id"]}),
            },
            Assertion::Contains {
                id: "contains".into(),
                actual: json!("${text}"),
                expected: json!("world"),
            },
            Assertion::Regex {
                id: "regex".into(),
                actual: json!("${text}"),
                expected: json!("^hello"),
            },
            Assertion::Similarity {
                id: "similarity".into(),
                actual: json!("same"),
                expected: json!("same"),
                evaluator: "exact@1".into(),
                min: 1.0,
            },
            Assertion::Metric {
                id: "metric".into(),
                phase: "steady".into(),
                metric: "latency_ms".into(),
                aggregate: "p95".into(),
                op: "lte".into(),
                value: 30.0,
                min_samples: 3,
            },
        ];
        evaluate_assertions(&passes, &context, &registry, &metrics).unwrap();

        let failures = vec![
            Assertion::Exists {
                id: "exists".into(),
                actual: json!("${missing}"),
            },
            Assertion::JsonSchema {
                id: "schema".into(),
                actual: json!("${object}"),
                schema: json!({"type": "array"}),
            },
            Assertion::Contains {
                id: "contains".into(),
                actual: json!("${text}"),
                expected: json!("absent"),
            },
            Assertion::Regex {
                id: "regex".into(),
                actual: json!("${text}"),
                expected: json!("^world"),
            },
            Assertion::Similarity {
                id: "similarity".into(),
                actual: json!("left"),
                expected: json!("right"),
                evaluator: "exact@1".into(),
                min: 0.5,
            },
            Assertion::Metric {
                id: "metric".into(),
                phase: "steady".into(),
                metric: "latency_ms".into(),
                aggregate: "p95".into(),
                op: "lt".into(),
                value: 30.0,
                min_samples: 3,
            },
        ];
        for assertion in failures {
            assert!(evaluate_assertions(&[assertion], &context, &registry, &metrics).is_err());
        }
    }
}
