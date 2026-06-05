//! Pure cost-calculation utilities.
//!
//! [`cost_for`] accepts a model-cost table (keyed by model name, with optional
//! trailing-`*` wildcards) and returns the USD cost for a pair of token counts.

use std::collections::HashMap;

use crate::ModelCost;

/// Calculate the cost in USD for a given model and token counts.
///
/// ## Lookup order
///
/// 1. Exact key match (e.g. `"gpt-4o"`).
/// 2. Wildcard keys with a trailing `*` (e.g. `"gpt-4*"`).
///    Among all matching wildcard keys the **longest prefix** wins (most-specific first).
/// 3. Returns `None` if no key matches.
///
/// Zero token counts are valid and produce `Some(0.0)` when a key is found.
///
/// # Examples
///
/// ```rust
/// use std::collections::HashMap;
/// use drgtw_events::{cost_for, ModelCost};
///
/// let mut costs = HashMap::new();
/// costs.insert("gpt-4o".to_string(), ModelCost { input_per_1m: 2.5, output_per_1m: 10.0 });
/// costs.insert("gpt-4*".to_string(), ModelCost { input_per_1m: 30.0, output_per_1m: 60.0 });
///
/// // Exact match wins over wildcard
/// let c = cost_for(&costs, "gpt-4o", 1_000_000, 500_000).unwrap();
/// assert!((c - 7.5).abs() < 1e-9); // 2.5 + 5.0
///
/// // Wildcard fallback
/// let c = cost_for(&costs, "gpt-4-turbo", 1_000_000, 0).unwrap();
/// assert!((c - 30.0).abs() < 1e-9);
///
/// // No match
/// assert!(cost_for(&costs, "claude-3", 100, 100).is_none());
/// ```
pub fn cost_for(
    model_costs: &HashMap<String, ModelCost>,
    model: &str,
    input_tokens: u64,
    output_tokens: u64,
) -> Option<f64> {
    // 1. Exact match
    if let Some(mc) = model_costs.get(model) {
        return Some(compute(mc, input_tokens, output_tokens));
    }

    // 2. Wildcard keys — find the longest matching prefix
    let best: Option<&ModelCost> = model_costs
        .iter()
        .filter_map(|(k, v)| {
            let prefix = k.strip_suffix('*')?;
            if model.starts_with(prefix) {
                Some((prefix.len(), v))
            } else {
                None
            }
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, v)| v);

    best.map(|mc| compute(mc, input_tokens, output_tokens))
}

#[inline]
fn compute(mc: &ModelCost, input_tokens: u64, output_tokens: u64) -> f64 {
    (input_tokens as f64 / 1_000_000.0) * mc.input_per_1m
        + (output_tokens as f64 / 1_000_000.0) * mc.output_per_1m
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> HashMap<String, ModelCost> {
        let mut m = HashMap::new();
        m.insert(
            "gpt-4o".to_string(),
            ModelCost {
                input_per_1m: 2.5,
                output_per_1m: 10.0,
            },
        );
        m.insert(
            "gpt-4*".to_string(),
            ModelCost {
                input_per_1m: 30.0,
                output_per_1m: 60.0,
            },
        );
        m.insert(
            "gpt-4-turbo*".to_string(),
            ModelCost {
                input_per_1m: 10.0,
                output_per_1m: 30.0,
            },
        );
        m.insert(
            "claude-3-5*".to_string(),
            ModelCost {
                input_per_1m: 3.0,
                output_per_1m: 15.0,
            },
        );
        m
    }

    #[test]
    fn exact_match() {
        let costs = table();
        // 1M input @ $2.5 + 500k output @ $10 = $2.5 + $5.0 = $7.5
        let c = cost_for(&costs, "gpt-4o", 1_000_000, 500_000).unwrap();
        assert!((c - 7.5).abs() < 1e-9, "expected 7.5, got {c}");
    }

    #[test]
    fn exact_beats_wildcard() {
        let costs = table();
        // "gpt-4o" is exact; "gpt-4*" is also a prefix match — exact wins
        let c = cost_for(&costs, "gpt-4o", 1_000_000, 0).unwrap();
        assert!((c - 2.5).abs() < 1e-9, "exact should win; expected 2.5, got {c}");
    }

    #[test]
    fn wildcard_fallback() {
        let costs = table();
        // "gpt-4-vision" matches "gpt-4*" but NOT "gpt-4-turbo*"
        let c = cost_for(&costs, "gpt-4-vision", 1_000_000, 0).unwrap();
        assert!((c - 30.0).abs() < 1e-9, "expected 30.0, got {c}");
    }

    #[test]
    fn longest_prefix_wins() {
        let costs = table();
        // "gpt-4-turbo-preview" matches both "gpt-4*" (5 chars prefix "gpt-4") and
        // "gpt-4-turbo*" (11 chars prefix "gpt-4-turbo") — longer prefix wins
        let c = cost_for(&costs, "gpt-4-turbo-preview", 1_000_000, 0).unwrap();
        assert!((c - 10.0).abs() < 1e-9, "longest prefix should win; expected 10.0, got {c}");
    }

    #[test]
    fn no_match_returns_none() {
        let costs = table();
        assert!(cost_for(&costs, "mistral-7b", 1000, 1000).is_none());
    }

    #[test]
    fn zero_tokens_returns_zero_cost() {
        let costs = table();
        let c = cost_for(&costs, "gpt-4o", 0, 0).unwrap();
        assert_eq!(c, 0.0);
    }

    #[test]
    fn zero_tokens_with_wildcard() {
        let costs = table();
        let c = cost_for(&costs, "claude-3-5-sonnet", 0, 0).unwrap();
        assert_eq!(c, 0.0);
    }

    #[test]
    fn wildcard_prefix_match_anthropic() {
        let costs = table();
        // 1M input @ $3 + 200k output @ $15 = $3 + $3 = $6
        let c = cost_for(&costs, "claude-3-5-sonnet", 1_000_000, 200_000).unwrap();
        assert!((c - 6.0).abs() < 1e-9, "expected 6.0, got {c}");
    }

    #[test]
    fn empty_table_returns_none() {
        let costs: HashMap<String, ModelCost> = HashMap::new();
        assert!(cost_for(&costs, "gpt-4o", 100, 100).is_none());
    }
}
