//! Small helpers for loosely typed JSON (Deezer rows, track meta) and time.

use serde_json::{Map, Value};
use std::time::{SystemTime, UNIX_EPOCH};

pub type Json = Map<String, Value>;

/// Unix time in seconds, like Python's `time.time()`.
pub fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// Python's `str(value)` for JSON scalars; `None` is "".
pub fn as_str(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => if *b { "True" } else { "False" }.into(),
        Some(Value::Number(n)) => n.to_string(),
        Some(other) => other.to_string(),
    }
}

/// A string field that is present and not empty.
pub fn opt_str(v: Option<&Value>) -> Option<String> {
    let s = as_str(v);
    (!s.is_empty()).then_some(s)
}

pub fn as_f64(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// `max(0, int(float(value)))`, 0 when not a number.
pub fn as_uint(v: Option<&Value>) -> i64 {
    as_f64(v).filter(|f| f.is_finite()).map(|f| f.trunc() as i64).unwrap_or(0).max(0)
}

/// Python truthiness of a JSON value.
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

pub fn obj(v: Option<&Value>) -> Json {
    v.and_then(Value::as_object).cloned().unwrap_or_default()
}

pub fn arr(v: Option<&Value>) -> Vec<Value> {
    v.and_then(Value::as_array).cloned().unwrap_or_default()
}

/// `value.get("a").get("b")` through nested objects.
pub fn path<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().try_fold(v, |cur, k| cur.get(*k))
}

/// An error with its causes ("error sending request: dns error: ...").
pub fn chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        out.push_str(&format!(": {cause}"));
        source = cause.source();
    }
    out
}

/// The last `max_chars` characters of `s` (on a char boundary).
pub fn tail(s: &str, max_chars: usize) -> String {
    let n = s.chars().count();
    s.chars().skip(n.saturating_sub(max_chars)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn conversions() {
        assert_eq!(as_str(Some(&json!(5))), "5");
        assert_eq!(as_str(None), "");
        assert_eq!(as_uint(Some(&json!("12.7"))), 12);
        assert_eq!(as_uint(Some(&json!(-3))), 0);
        assert_eq!(as_uint(Some(&json!("x"))), 0);
        assert!(truthy(Some(&json!(1))));
        assert!(!truthy(Some(&json!(""))));
        assert_eq!(tail("abcdef", 3), "def");
        assert_eq!(path(&json!({"a": {"b": 1}}), &["a", "b"]), Some(&json!(1)));
    }
}
