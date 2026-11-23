//! Cost panel calculations and formatting (upstream `CostPanel.tsx`).

/// One committed assistant response's cost entry.
#[derive(Clone, Debug, PartialEq)]
pub struct AssistantCost {
    pub entry_id: String,
    pub cost: f64,
    pub cache_hit: Option<f64>,
    /// The model name shown in the table.
    pub model: String,
    /// How the response stopped (`stop Reason`).
    pub stop_reason: String,
}

/// A dollar value with 4 decimals at >= 1 cent, else 6 (upstream `preciseCost`).
pub fn precise_cost(value: f64) -> String {
    let decimals = if value >= 0.01 { 4 } else { 6 };
    format!("${value:.decimals$}")
}

/// A chart axis dollar value: 2 decimals at >= $1, 3 at >= 1 cent, else 5 (`axisCost`).
pub fn axis_cost(value: f64) -> String {
    if value == 0.0 {
        "$0".to_owned()
    } else if value >= 1.0 {
        format!("${value:.2}")
    } else if value >= 0.01 {
        format!("${value:.3}")
    } else {
        format!("${value:.5}")
    }
}

/// A percentage with no decimals, or an em dash when absent (upstream `percent`).
pub fn percent(value: Option<f64>) -> String {
    match value {
        Some(value) => format!("{}%", (value * 100.0).round() as i64),
        None => "—".to_owned(),
    }
}

/// The greatest response cost (at least zero).
pub fn maximum(costs: &[AssistantCost]) -> f64 {
    costs.iter().fold(0.0f64, |max, entry| max.max(entry.cost))
}

/// The sum of all response costs.
pub fn total(costs: &[AssistantCost]) -> f64 {
    costs.iter().fold(0.0, |sum, entry| sum + entry.cost)
}

/// The height (as a percent of the plot) of a cost bar: at least 2%, scaled by the max.
pub fn cost_bar_height(cost: f64, max: f64) -> f64 {
    if max > 0.0 {
        (cost / max * 100.0).max(2.0)
    } else {
        2.0
    }
}

/// The height of a cache-hit bar: at least 2% for a present value (even 0% hit).
pub fn cache_bar_height(cache_hit: f64) -> f64 {
    (cache_hit * 100.0).max(2.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cost(id: &str, value: f64, cache: Option<f64>) -> AssistantCost {
        AssistantCost {
            entry_id: id.into(),
            cost: value,
            cache_hit: cache,
            model: "test-model".into(),
            stop_reason: "stop".into(),
        }
    }

    #[test]
    fn precise_and_axis_formatting() {
        assert_eq!(precise_cost(0.012345), "$0.0123");
        assert_eq!(precise_cost(0.0012345), "$0.001234");
        assert_eq!(axis_cost(0.0), "$0");
        assert_eq!(axis_cost(1.234), "$1.23");
        assert_eq!(axis_cost(0.0123), "$0.012");
        assert_eq!(axis_cost(0.00012), "$0.00012");
    }

    #[test]
    fn percent_formatting() {
        assert_eq!(percent(Some(0.456)), "46%");
        assert_eq!(percent(Some(1.0)), "100%");
        assert_eq!(percent(None), "—");
    }

    #[test]
    fn summary_stats_and_bars() {
        let costs = vec![
            cost("1", 0.2, Some(0.0)),
            cost("2", 0.8, Some(0.5)),
            cost("3", 0.5, None),
        ];
        assert!((maximum(&costs) - 0.8).abs() < f64::EPSILON);
        assert!((total(&costs) - 1.5).abs() < f64::EPSILON);
        assert!((cost_bar_height(0.8, 0.8) - 100.0).abs() < f64::EPSILON);
        assert_eq!(cost_bar_height(0.0, 0.0), 2.0);
        assert_eq!(cache_bar_height(0.0), 2.0);
        assert_eq!(cache_bar_height(0.5), 50.0);
    }

    #[test]
    fn empty_costs() {
        assert_eq!(maximum(&[]), 0.0);
        assert_eq!(total(&[]), 0.0);
    }
}
