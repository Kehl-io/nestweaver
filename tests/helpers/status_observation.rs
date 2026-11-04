//! Readiness for assertions over completed daemon graph observations.
pub fn is_current(value: &serde_json::Value) -> bool {
    let observation = value
        .get("status_observation")
        .and_then(serde_json::Value::as_object)
        .unwrap_or_else(|| panic!("expected daemon graph observation: {value}"));
    let stale = observation
        .get("stale")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or_else(|| panic!("expected explicit stale flag: {value}"));
    let busy = observation
        .get("busy")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or_else(|| panic!("expected explicit busy flag: {value}"));
    !stale && !busy
}
