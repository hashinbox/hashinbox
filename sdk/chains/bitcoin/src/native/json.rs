//! Typed field access for the JSON objects Bitcoin Core returns.
//!
//! Node replies and retained block evidence share these rules; only the error
//! differs. A reply failure is retryable (the node may be catching up), while
//! invalid block evidence is terminal, so each [`Object`] carries its `fail`.

use serde_json::{Map, Value};

use super::Error;

/// One JSON object and how its violations are reported.
#[derive(Clone, Copy)]
pub(crate) struct Object<'a> {
    map: &'a Map<String, Value>,
    fail: fn(String) -> Error,
}

impl<'a> Object<'a> {
    /// `value` as an object, or `fail("{context} must be an object")`.
    pub(crate) fn new(
        value: &'a Value,
        context: &str,
        fail: fn(String) -> Error,
    ) -> Result<Self, Error> {
        value
            .as_object()
            .map(|map| Self { map, fail })
            .ok_or_else(|| fail(format!("{context} must be an object")))
    }

    /// A violation reported the way this object reports its own.
    pub(crate) fn fail(&self, message: impl Into<String>) -> Error {
        (self.fail)(message.into())
    }

    /// The raw field, if present.
    pub(crate) fn get(&self, field: &str) -> Option<&'a Value> {
        self.map.get(field)
    }

    /// A non-empty string.
    pub(crate) fn string(&self, field: &str, context: &str) -> Result<&'a str, Error> {
        self.get(field)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| self.missing(context))
    }

    /// An unsigned integer.
    pub(crate) fn u64(&self, field: &str, context: &str) -> Result<u64, Error> {
        self.get(field)
            .and_then(Value::as_u64)
            .ok_or_else(|| self.missing(context))
    }

    /// An unsigned integer that fits `u32`.
    pub(crate) fn u32(&self, field: &str, context: &str) -> Result<u32, Error> {
        let value = self.u64(field, context)?;
        u32::try_from(value).map_err(|_| self.fail(format!("{context} exceeds u32")))
    }

    /// A boolean.
    pub(crate) fn bool(&self, field: &str, context: &str) -> Result<bool, Error> {
        self.get(field)
            .and_then(Value::as_bool)
            .ok_or_else(|| self.missing(context))
    }

    /// An array.
    pub(crate) fn array(&self, field: &str, context: &str) -> Result<&'a Vec<Value>, Error> {
        self.get(field)
            .and_then(Value::as_array)
            .ok_or_else(|| self.fail(format!("{context} must be an array")))
    }

    fn missing(&self, context: &str) -> Error {
        self.fail(format!("{context} is missing or invalid"))
    }
}
