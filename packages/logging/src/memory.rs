use std::{
    collections::{BTreeMap, VecDeque},
    fmt::{self, Write},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{Level, Log, Metadata, Record, kv};

const RECORD_BYTES: usize = 8192;

/// One retained event; sequence numbers increase across eviction and clearing.
#[derive(Clone, Debug)]
pub struct Entry {
    /// Process-local cursor.
    pub sequence: u64,
    /// Reception time, milliseconds since the Unix epoch.
    pub timestamp: u64,
    /// Severity supplied by the emitter.
    pub level: Level,
    /// Stable event code (`Record::target`).
    pub event: String,
    /// Human-readable safe description.
    pub message: String,
    /// Structured context, including any chain, network, node and component.
    pub fields: BTreeMap<String, String>,
    /// Some content exceeded the per-record bound.
    pub truncated: bool,
}

/// Exact-match context filters. Empty means all events.
#[derive(Clone, Debug, Default)]
pub struct Filter(pub BTreeMap<String, String>);

impl Filter {
    fn matches(&self, entry: &Entry) -> bool {
        self.0
            .iter()
            .all(|(key, value)| entry.fields.get(key) == Some(value))
    }
}

/// A bounded page, including enough information to detect missed records.
#[derive(Clone, Debug)]
pub struct Page {
    /// Identifies this memory buffer; consumers reset their cursor when it changes.
    pub session: String,
    /// Entries after the requested cursor in reception order.
    pub entries: Vec<Entry>,
    /// Resume cursor, advanced even when scanned records did not match.
    pub next: u64,
    /// Requested data may have been evicted, cleared, or belong to an older process.
    pub gap: bool,
}

/// A bounded, thread-safe `Log` sink. Defaults: 10,000 events and 8 MiB text.
/// Retention is process-local, best effort, and never a durable delivery channel.
pub struct Memory {
    session: String,
    state: Mutex<Buffer>,
    capacity: usize,
    bytes: usize,
}

#[derive(Default)]
struct Buffer {
    entries: VecDeque<(Entry, usize)>,
    bytes: usize,
    sequence: u64,
    lost_through: u64,
}

impl Memory {
    /// Bounds both entry count and retained text; zero bounds disable retention.
    #[must_use]
    pub fn new(capacity: usize, bytes: usize) -> Self {
        static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let serial = SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Self {
            session: format!("{time}-{serial}"),
            state: Mutex::new(Buffer::default()),
            capacity,
            bytes,
        }
    }

    /// Reads at most 1,000 entries, without invoking a caller while locked.
    #[must_use]
    pub fn page(&self, after: u64, limit: usize, filter: &Filter) -> Page {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let restarted = after > state.sequence;
        let after = if restarted { 0 } else { after };
        let entries: Vec<_> = state
            .entries
            .iter()
            .filter(|(entry, _)| entry.sequence > after && filter.matches(entry))
            .take(limit.clamp(1, 1000))
            .map(|(entry, _)| entry.clone())
            .collect();
        let next = entries
            .last()
            .map_or(state.sequence, |entry| entry.sequence);
        Page {
            session: self.session.clone(),
            entries,
            next,
            gap: restarted || after < state.lost_through,
        }
    }

    /// Removes matching records only; future records keep their sequence order.
    pub fn clear(&self, filter: &Filter) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let mut removed = 0;
        let mut lost = state.lost_through;
        state.entries.retain(|(entry, bytes)| {
            if !filter.matches(entry) {
                return true;
            }
            removed += bytes;
            lost = lost.max(entry.sequence);
            false
        });
        state.bytes -= removed;
        state.lost_through = lost;
    }
}

impl Default for Memory {
    fn default() -> Self {
        Self::new(10_000, 8 * 1024 * 1024)
    }
}

impl Log for Memory {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        self.capacity > 0 && self.bytes > 0
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let mut text = Text {
            value: String::new(),
            remaining: RECORD_BYTES.min(self.bytes),
            truncated: false,
        };
        let event = text.capture(format_args!("{}", record.target()));
        let message = text.capture(*record.args());
        let mut fields = Fields {
            text,
            values: BTreeMap::new(),
        };
        if record.key_values().visit(&mut fields).is_err() {
            fields.text.truncated = true;
        }
        let bytes = RECORD_BYTES.min(self.bytes) - fields.text.remaining;
        let mut entry = Entry {
            sequence: 0,
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |time| {
                    u64::try_from(time.as_millis()).unwrap_or(u64::MAX)
                }),
            level: record.level(),
            event,
            message,
            fields: fields.values,
            truncated: fields.text.truncated,
        };
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.sequence = state.sequence.saturating_add(1);
        entry.sequence = state.sequence;
        state.bytes += bytes;
        state.entries.push_back((entry, bytes));
        while state.entries.len() > self.capacity || state.bytes > self.bytes {
            if let Some((entry, bytes)) = state.entries.pop_front() {
                state.bytes -= bytes;
                state.lost_through = state.lost_through.max(entry.sequence);
            }
        }
    }

    fn flush(&self) {}
}

struct Text {
    value: String,
    remaining: usize,
    truncated: bool,
}

impl Text {
    fn capture(&mut self, args: fmt::Arguments<'_>) -> String {
        let _ = self.write_fmt(args);
        std::mem::take(&mut self.value)
    }
}

impl Write for Text {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let mut end = text.len().min(self.remaining);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        self.value.push_str(&text[..end]);
        self.remaining -= end;
        if end != text.len() {
            self.truncated = true;
            return Err(fmt::Error);
        }
        Ok(())
    }
}

struct Fields {
    text: Text,
    values: BTreeMap<String, String>,
}

impl<'kvs> kv::VisitSource<'kvs> for Fields {
    fn visit_pair(&mut self, key: kv::Key<'kvs>, value: kv::Value<'kvs>) -> Result<(), kv::Error> {
        if self.text.remaining == 0 || self.values.len() == 32 {
            self.text.truncated = true;
            return Ok(());
        }
        let key = self.text.capture(format_args!("{key}"));
        let value = self.text.capture(format_args!("{value}"));
        self.values.insert(key, value);
        Ok(())
    }
}
