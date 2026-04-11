use serde_json::Value;

use super::transformer::base_response;

/// Helper: single-response entry for a token.
fn entry(token: &str, responses: Vec<Value>) -> (String, Vec<Value>) {
    (token.to_string(), responses)
}

/// 3 Plus accounts: 0%, 30%, 60% used (5h), low 7d usage.
pub fn healthy_pool() -> Vec<(String, Vec<Value>)> {
    vec![
        entry("tok_healthy_a", vec![base_response("plus", 0.0, 18000, 10.0, 604800)]),
        entry("tok_healthy_b", vec![base_response("plus", 30.0, 14400, 15.0, 604800)]),
        entry("tok_healthy_c", vec![base_response("plus", 60.0, 10800, 20.0, 604800)]),
    ]
}

/// 1 Team(50%) + 2 Plus(10%, 20%). Team has priority.
pub fn team_priority() -> Vec<(String, Vec<Value>)> {
    vec![
        entry("tok_team", vec![base_response("team", 50.0, 10800, 30.0, 604800)]),
        entry("tok_plus_a", vec![base_response("plus", 10.0, 16200, 5.0, 604800)]),
        entry("tok_plus_b", vec![base_response("plus", 20.0, 14400, 10.0, 604800)]),
    ]
}

/// Team(100%, resets 2h) + 2 Plus(20%, 45%).
pub fn team_exhausted() -> Vec<(String, Vec<Value>)> {
    vec![
        entry("tok_team_exhausted", vec![base_response("team", 100.0, 7200, 80.0, 604800)]),
        entry("tok_plus_c", vec![base_response("plus", 20.0, 14400, 15.0, 604800)]),
        entry("tok_plus_d", vec![base_response("plus", 45.0, 10800, 25.0, 604800)]),
    ]
}

/// A(40%, resets 20min) + B(30%, resets 4h). A should be drained first.
pub fn drain_window() -> Vec<(String, Vec<Value>)> {
    vec![
        entry("tok_drain_a", vec![base_response("plus", 40.0, 1200, 20.0, 604800)]),
        entry("tok_drain_b", vec![base_response("plus", 30.0, 14400, 15.0, 604800)]),
    ]
}

/// A(5h:0%, 7d:95% resets 6d) + B(5h:50%, 7d:30%).
/// 7d sustainability should penalize A despite low 5h usage.
pub fn seven_day_crisis() -> Vec<(String, Vec<Value>)> {
    vec![
        entry("tok_7d_crisis_a", vec![base_response("plus", 0.0, 18000, 95.0, 518400)]),
        entry("tok_7d_crisis_b", vec![base_response("plus", 50.0, 10800, 30.0, 604800)]),
    ]
}

/// 3 accounts 100% used, reset in 30min/2h/4h.
pub fn all_exhausted() -> Vec<(String, Vec<Value>)> {
    vec![
        entry("tok_exhausted_a", vec![base_response("plus", 100.0, 1800, 80.0, 604800)]),
        entry("tok_exhausted_b", vec![base_response("plus", 100.0, 7200, 70.0, 604800)]),
        entry("tok_exhausted_c", vec![base_response("plus", 100.0, 14400, 60.0, 604800)]),
    ]
}

/// Timeline: A goes 30%->60%->90%->100% over 4 ticks.
/// The mock server advances through Vec<Value> per token on each request.
pub fn gradual_exhaustion() -> Vec<(String, Vec<Value>)> {
    vec![
        entry("tok_gradual_a", vec![
            base_response("plus", 30.0, 14400, 10.0, 604800),
            base_response("plus", 60.0, 10800, 15.0, 604800),
            base_response("plus", 90.0, 7200, 20.0, 604800),
            base_response("plus", 100.0, 3600, 25.0, 604800),
        ]),
        entry("tok_gradual_b", vec![
            base_response("plus", 20.0, 16200, 10.0, 604800),
            base_response("plus", 20.0, 16200, 10.0, 604800),
            base_response("plus", 20.0, 16200, 10.0, 604800),
            base_response("plus", 20.0, 16200, 10.0, 604800),
        ]),
    ]
}
