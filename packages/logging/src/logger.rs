use std::{collections::BTreeMap, sync::Arc};

use crate::{Level, Log, Metadata, Record};

/// An injected sink and the context inherited by its child scopes.
/// No process-global logger or runtime is required. Sinks must return promptly;
/// queue external I/O themselves and never panic. Logging cannot veto an operation.
#[derive(Clone, Default)]
pub struct Logger {
    sink: Option<Arc<dyn Log>>,
    fields: BTreeMap<String, String>,
}

impl Logger {
    /// Uses a caller-owned standard `log::Log` implementation.
    #[must_use]
    pub fn new(sink: Arc<dyn Log>) -> Self {
        Self {
            sink: Some(sink),
            fields: BTreeMap::new(),
        }
    }

    /// Adds or replaces context without changing the parent scope.
    /// Only public identifiers belong here, never URLs, credentials or payloads.
    #[must_use]
    pub fn with(&self, key: impl Into<String>, value: impl Into<String>) -> Self {
        let mut child = self.clone();
        child.fields.insert(key.into(), value.into());
        child
    }

    /// Whether this sink wants an event, before collecting its optional context.
    #[must_use]
    pub fn enabled(&self, level: Level, event: &str) -> bool {
        self.sink.as_ref().is_some_and(|sink| {
            sink.enabled(&Metadata::builder().level(level).target(event).build())
        })
    }

    /// Emits one meaningful event. `event` is a stable code; `message` is safe prose.
    /// Fields are structured strings; exact amounts must never pass through floats.
    /// Call only after releasing domain locks and completing durable changes.
    pub fn event(&self, level: Level, event: &str, message: &str, fields: &[(&str, &str)]) {
        let Some(sink) = &self.sink else { return };
        let metadata = Metadata::builder().level(level).target(event).build();
        if !sink.enabled(&metadata) {
            return;
        }
        let mut values = self.fields.clone();
        values.extend(
            fields
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string())),
        );
        let values: Vec<_> = values.into_iter().collect();
        sink.log(
            &Record::builder()
                .metadata(metadata)
                .args(format_args!("{message}"))
                .key_values(&values)
                .build(),
        );
    }
}
