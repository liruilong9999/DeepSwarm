use std::collections::BTreeMap;

use crate::{
    CoreError,
    dsl::{Case, Load},
};

#[derive(Clone, Debug, PartialEq)]
pub struct ScheduledRate<'a> {
    pub phase: &'a str,
    pub rate: f64,
}

pub fn rate_at(load: &Load, elapsed_seconds: f64) -> Option<ScheduledRate<'_>> {
    if elapsed_seconds < 0.0 {
        return None;
    }
    let mut elapsed = elapsed_seconds;
    for phase in &load.phases {
        let duration = phase.duration_seconds as f64;
        if elapsed < duration {
            let progress = elapsed / duration;
            return Some(ScheduledRate {
                phase: &phase.name,
                rate: phase.start_rate + (phase.target_rate - phase.start_rate) * progress,
            });
        }
        elapsed -= duration;
    }
    None
}

pub fn weighted_allocations(
    cases: &[Case],
    total: usize,
) -> Result<BTreeMap<String, usize>, CoreError> {
    if cases
        .iter()
        .map(|case| case.name.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .len()
        != cases.len()
    {
        return Err(CoreError::invalid("用例名称必须唯一"));
    }
    let weights = cases
        .iter()
        .map(|case| case.weight.unwrap_or(1.0))
        .collect::<Vec<_>>();
    let sum = weights.iter().sum::<f64>();
    if !sum.is_finite() || sum <= 0.0 {
        return Err(CoreError::invalid("用例权重总和必须为正数"));
    }
    let mut allocations = BTreeMap::new();
    let mut remainders = Vec::new();
    let mut assigned = 0usize;
    for (index, (case, weight)) in cases.iter().zip(weights).enumerate() {
        let exact = total as f64 * weight / sum;
        let base = exact.floor() as usize;
        assigned += base;
        allocations.insert(case.name.clone(), base);
        remainders.push((exact - base as f64, index, case.name.as_str()));
    }
    remainders.sort_by(|left, right| {
        right
            .0
            .total_cmp(&left.0)
            .then_with(|| left.1.cmp(&right.1))
    });
    for (_, _, name) in remainders.into_iter().take(total - assigned) {
        *allocations.get_mut(name).expect("用例已分配") += 1;
    }
    Ok(allocations)
}

#[cfg(test)]
mod tests {
    use serde_json::Map;

    use super::*;
    use crate::dsl::{LoadPhase, Step};

    fn case(name: &str, weight: f64) -> Case {
        Case {
            name: name.into(),
            variables: Map::new(),
            for_each: None,
            weight: Some(weight),
            repeat: None,
            steps: vec![Step {
                id: "step".into(),
                name: None,
                variables: Map::new(),
                repeat: None,
                when: None,
                tool: "tool".into(),
                params: Map::new(),
                capture: BTreeMap::new(),
                assertions: Vec::new(),
            }],
        }
    }

    #[test]
    fn phase_rate_and_weight_allocation_are_deterministic() {
        let load = Load {
            phases: vec![
                LoadPhase {
                    name: "ramp".into(),
                    duration_seconds: 10,
                    start_rate: 0.0,
                    target_rate: 100.0,
                },
                LoadPhase {
                    name: "steady".into(),
                    duration_seconds: 10,
                    start_rate: 100.0,
                    target_rate: 100.0,
                },
            ],
        };
        assert_eq!(
            rate_at(&load, 5.0),
            Some(ScheduledRate {
                phase: "ramp",
                rate: 50.0
            })
        );
        assert_eq!(rate_at(&load, 10.0).unwrap().phase, "steady");
        assert_eq!(
            weighted_allocations(&[case("a", 1.0), case("b", 2.0)], 10).unwrap(),
            BTreeMap::from([("a".into(), 3), ("b".into(), 7)])
        );
    }
}
