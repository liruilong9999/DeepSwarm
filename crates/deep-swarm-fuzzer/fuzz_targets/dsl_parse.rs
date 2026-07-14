#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use deep_swarm_core::{FixtureRoot, InputFormat, SimilarityRegistry, prepare};
use deep_swarm_fuzzer::{MAX_ARRAY_ITEMS, bounded_bytes, bounded_str, json_depth_within};
use libfuzzer_sys::fuzz_target;
use serde_json::{Value, json};

#[derive(Arbitrary, Debug)]
struct DslInput {
    invalid: bool,
    labels: Vec<String>,
    repeat: u8,
}

fuzz_target!(|data: &[u8]| {
    let data = bounded_bytes(data);
    let temp = tempfile::tempdir().expect("temporary fixture directory");
    let fixtures = FixtureRoot::new(temp.path()).expect("fixture root");
    let evaluators = SimilarityRegistry::default();

    let format = if data
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
        == Some(b'{')
    {
        InputFormat::Json
    } else {
        InputFormat::Yaml
    };
    if !matches!(format, InputFormat::Json) || json_depth_within(data) {
        let _ = prepare(data, format, &fixtures, &evaluators);
    }

    let mut unstructured = Unstructured::new(data);
    let Ok(generated) = DslInput::arbitrary(&mut unstructured) else {
        return;
    };
    let labels = if generated.labels.is_empty() {
        vec![String::new()]
    } else {
        generated.labels.into_iter().take(MAX_ARRAY_ITEMS).collect()
    };
    let cases = labels
        .iter()
        .enumerate()
        .map(|(index, label)| {
            let label: String = bounded_str(label)
                .chars()
                .filter(|character| character.is_ascii_alphanumeric())
                .take(32)
                .collect();
            json!({
                "name": format!("case-{index}-{label}"),
                "variables": {},
                "for_each": null,
                "weight": 1.0,
                "repeat": usize::from(generated.repeat % 3) + 1,
                "steps": [{
                    "id": format!("step_{index}"),
                    "name": null,
                    "variables": {},
                    "repeat": 1,
                    "when": true,
                    "tool": "diagnostics",
                    "with": {},
                    "capture": {},
                    "assertions": []
                }]
            })
        })
        .collect::<Vec<_>>();
    let mut document = json!({
        "version": 1,
        "variables": {},
        "data_sources": [],
        "suites": [{
            "name": "fuzz-suite",
            "variables": {},
            "load": null,
            "assertions": [],
            "cases": cases
        }]
    });
    if generated.invalid {
        document["unexpected"] = Value::Bool(true);
    }
    let encoded = serde_json::to_vec(&document).expect("generated DSL serializes");
    let result = prepare(&encoded, InputFormat::Json, &fixtures, &evaluators);
    assert_eq!(result.is_err(), generated.invalid);
});
