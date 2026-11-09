//! [`Recording`]: one reply a real node gave, and the shapes check a mock's
//! answer to the same request must pass.

use rpc::{CallResult, Failure, RawJson};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// One request to a real node and its reply, as a chain's recordings keep
/// it: `{provider, network, recorded_at, method, params, result|error}`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Recording {
    /// Who answered (a host name).
    pub provider: String,
    /// The network it serves.
    pub network: String,
    /// When, as UTC `YYYY-MM-DD`.
    pub recorded_at: String,
    /// The method asked.
    pub method: String,
    /// Its parameters.
    pub params: Value,
    /// The result, `null` included; absent for an error.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub result: Option<Value>,
    /// The error object: `code`, `message`, maybe `data`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
}

/// A present field, `null` included.
fn present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

impl Recording {
    /// The recording in `text`.
    ///
    /// # Panics
    ///
    /// When `text` is not one.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        serde_json::from_str(text).expect("a recording is {provider, network, ..., result|error}")
    }

    /// The recorded result.
    ///
    /// # Panics
    ///
    /// When it recorded an error.
    #[must_use]
    pub fn result(&self) -> &Value {
        self.result.as_ref().expect("a recorded result")
    }

    /// The recorded error object.
    ///
    /// # Panics
    ///
    /// When it recorded a result.
    #[must_use]
    pub fn failure(&self) -> Failure {
        let error = self.error.as_ref().expect("a recorded error");
        Failure {
            code: error["code"].as_i64().expect("an error code"),
            message: error["message"].as_str().unwrap_or_default().to_owned(),
            data: error
                .get("data")
                .and_then(|data| RawJson::from_serializable(data).ok()),
        }
    }

    /// Whether `answer` is what the node recorded, in shape: for a result,
    /// the same fields at every depth with the same value kinds (`null`
    /// included; an empty array matches any); for an error, the same code
    /// and message exactly.
    ///
    /// # Errors
    ///
    /// Every difference, one per line.
    pub fn check(&self, answer: &CallResult) -> Result<(), String> {
        let mut differences = Vec::new();
        match (&self.error, answer) {
            (Some(_), Err(failure)) => {
                let recorded = self.failure();
                if (recorded.code, &recorded.message) != (failure.code, &failure.message) {
                    differences.push(format!(
                        "error {} {:?}, recorded {} {:?}",
                        failure.code, failure.message, recorded.code, recorded.message
                    ));
                }
            }
            (None, Ok(raw)) => {
                let answered: Value = raw.deserialize().map_err(|error| error.to_string())?;
                compare("$", self.result(), &answered, &mut differences);
            }
            (Some(_), Ok(_)) => differences.push("a result, recorded an error".to_owned()),
            (None, Err(failure)) => differences.push(format!(
                "error {} {:?}, recorded a result",
                failure.code, failure.message
            )),
        }
        if differences.is_empty() {
            return Ok(());
        }
        Err(format!("{}: {}", self.method, differences.join("\n")))
    }
}

/// The kind of a JSON value: `0x…` text is `hex`.
fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(text) if text.starts_with("0x") => "hex",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn compare(path: &str, recorded: &Value, answered: &Value, differences: &mut Vec<String>) {
    if kind(recorded) != kind(answered) {
        differences.push(format!(
            "{path}: {}, recorded {}",
            kind(answered),
            kind(recorded)
        ));
        return;
    }
    match (recorded, answered) {
        (Value::Object(recorded), Value::Object(answered)) => {
            for key in recorded.keys().filter(|key| !answered.contains_key(*key)) {
                differences.push(format!("{path}.{key}: missing"));
            }
            for key in answered.keys().filter(|key| !recorded.contains_key(*key)) {
                differences.push(format!("{path}.{key}: not recorded"));
            }
            for (key, value) in recorded {
                if let Some(other) = answered.get(key) {
                    compare(&format!("{path}.{key}"), value, other, differences);
                }
            }
        }
        (Value::Array(recorded), Value::Array(answered)) => {
            for (index, (value, other)) in recorded.iter().zip(answered).enumerate() {
                compare(&format!("{path}[{index}]"), value, other, differences);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recording(result: Value) -> Recording {
        Recording {
            provider: "node.invalid".to_owned(),
            network: "test".to_owned(),
            recorded_at: "2026-11-04".to_owned(),
            method: "m".to_owned(),
            params: Value::Null,
            result: Some(result),
            error: None,
        }
    }

    fn raw(value: Value) -> CallResult {
        Ok(RawJson::from_serializable(&value).unwrap())
    }

    #[test]
    fn shapes_compare_fields_kinds_and_nulls() {
        let recorded = recording(serde_json::json!({"a": "0x1", "b": null, "c": [{"d": 1}]}));
        let same = serde_json::json!({"a": "0x22", "b": null, "c": [{"d": 7}, {"e": 1}]});
        assert_eq!(recorded.check(&raw(same)), Ok(()));
        let other = serde_json::json!({"a": "1", "c": [], "f": 1});
        let differences = recorded.check(&raw(other)).unwrap_err();
        assert!(
            differences.contains("$.a: string, recorded hex"),
            "{differences}"
        );
        assert!(differences.contains("$.b: missing"), "{differences}");
        assert!(differences.contains("$.f: not recorded"), "{differences}");
        let null = recording(Value::Null);
        assert!(null.check(&raw(serde_json::json!([]))).is_err());
    }

    #[test]
    fn a_recorded_null_result_parses_as_present() {
        let text = r#"{"provider":"p","network":"n","recorded_at":"d","method":"m",
            "params":[],"result":null}"#;
        assert_eq!(Recording::parse(text).result, Some(Value::Null));
    }
}
