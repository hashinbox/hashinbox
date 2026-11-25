//! The kept logs: [`DbLayer`], the `tracing` layer that queues each event,
//! with its spans' fields, to one batched writer of the `log` table and
//! publishes it to the live [`Feed`]; [`Line`], how stdout shows one;
//! [`find_page`], the read; and [`listen`], the feed's.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    time::{SystemTime, UNIX_EPOCH},
};

use futures_util::Stream;
use tokio::sync::{broadcast, mpsc};
use tracing::{
    Event, Subscriber,
    field::{Field, Visit},
    span,
};
use tracing_subscriber::{
    fmt::{
        FmtContext, FormatEvent, FormatFields,
        format::Writer,
        time::{self, FormatTime as _},
    },
    layer::Context,
    registry::{LookupSpan, Scope},
};

use super::Error;
use crate::{
    db::{Db, Page},
    events,
    models::{Log, LogFilter, log},
};

/// Rows waiting for the writer; beyond them one is dropped.
const QUEUED: usize = 10_000;

/// The most rows one write stores.
const BATCH: usize = 500;

/// Entries the feed holds for its slowest listener; beyond them that one
/// misses the oldest.
const FEED: usize = 1_000;

/// The live feed: every entry [`DbLayer`] keeps, as it is kept, unstored
/// (its id 0).
pub(crate) type Feed = broadcast::Sender<Log>;

/// A new feed with no listener.
pub(crate) fn feed() -> Feed {
    broadcast::channel(FEED).0
}

/// Every event it is given, as a [`Log`] row with its spans' fields
/// (`network`, `node`, `job_id`, …) and, as `purpose`, the name of its
/// innermost span named with a dot (`index.fetch`, `send.sign`: why it
/// happened), queued to its own writer and published to its feed.
pub(crate) struct DbLayer {
    sender: mpsc::Sender<Log>,
    feed: Feed,
}

impl DbLayer {
    /// Starts the writer over `db` on a task of its own: every row queued
    /// since its last write, up to [`BATCH`], in one write. A failed write
    /// loses its rows; the next goes on. It ends when the layer is dropped.
    /// Each row is also published to `feed`.
    pub(crate) fn new(db: Db, feed: Feed) -> Self {
        let (sender, mut receiver) = mpsc::channel(QUEUED);
        tokio::spawn(async move {
            let mut batch = Vec::with_capacity(BATCH);
            while receiver.recv_many(&mut batch, BATCH).await > 0 {
                let logs = std::mem::take(&mut batch);
                let _lost = db
                    .write(move |connection| Ok(log::db::insert_all(connection, &logs)?))
                    .await;
            }
        });
        Self { sender, feed }
    }
}

/// A span's fields, kept in its extensions (by [`DbLayer`], read by
/// [`Line`] too).
struct SpanFields(BTreeMap<String, String>);

impl<S: Subscriber + for<'a> LookupSpan<'a>> tracing_subscriber::Layer<S> for DbLayer {
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut fields = Fields::default();
        attrs.record(&mut fields);
        let name = attrs.metadata().name();
        if name.contains('.') {
            fields.fields.insert("purpose".to_owned(), name.to_owned());
        }
        span.extensions_mut().insert(SpanFields(fields.fields));
    }

    fn on_record(&self, id: &span::Id, values: &span::Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut extensions = span.extensions_mut();
        let Some(SpanFields(kept)) = extensions.get_mut::<SpanFields>() else {
            return;
        };
        let mut fields = Fields::default();
        values.record(&mut fields);
        kept.extend(fields.fields);
    }

    /// Queues and publishes the event without waiting: the hook is
    /// synchronous and may fire inside a database closure, so it never
    /// writes itself.
    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let Fields { message, fields } = Fields::of(event, ctx.event_scope(event));
        let metadata = event.metadata();
        let target = metadata.target().to_owned();
        let row = Log::new(now(), *metadata.level(), target, message, fields);
        let _unheard = self.feed.send(row.clone());
        let _dropped = self.sender.try_send(row);
    }
}

/// Now, Unix ms.
fn now() -> i64 {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
}

/// The event's line on stdout: time, level, code (`target`); its own
/// `message`, or its code's template ([`events::MESSAGES`]) filled from its
/// fields and its spans'; then each field the message did not show, as
/// `key=val`.
pub(crate) struct Line;

impl<S, N> FormatEvent<S, N> for Line
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let Fields { message, fields } = Fields::of(event, ctx.event_scope());
        let metadata = event.metadata();
        let template = events::MESSAGES.get(metadata.target());
        let (text, shown) = match (message, template) {
            (Some(message), _) => (message, BTreeSet::new()),
            (None, Some(template)) => events::fill(template, &fields),
            (None, None) => (String::new(), BTreeSet::new()),
        };
        time::SystemTime.format_time(&mut writer)?;
        write!(writer, " {:>5} {}:", metadata.level(), metadata.target())?;
        if !text.is_empty() {
            write!(writer, " {text}")?;
        }
        for (key, value) in &fields {
            if !shown.contains(key.as_str()) {
                write!(writer, " {key}={value}")?;
            }
        }
        writeln!(writer)
    }
}

/// The message, if it has one, and the other fields of one event or span,
/// as text.
#[derive(Default)]
struct Fields {
    message: Option<String>,
    fields: BTreeMap<String, String>,
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.keep(field, value.to_owned());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.keep(field, format!("{value:?}"));
    }
}

impl Fields {
    /// `event`'s, then those of each span of `scope` it does not have
    /// itself, innermost first.
    fn of<S: for<'a> LookupSpan<'a>>(event: &Event<'_>, scope: Option<Scope<'_, S>>) -> Self {
        let mut fields = Self::default();
        event.record(&mut fields);
        for span in scope.into_iter().flatten() {
            let extensions = span.extensions();
            let Some(SpanFields(kept)) = extensions.get::<SpanFields>() else {
                continue;
            };
            for (key, value) in kept {
                let absent = fields.fields.entry(key.clone());
                absent.or_insert_with(|| value.clone());
            }
        }
        fields
    }

    fn keep(&mut self, field: &Field, value: String) {
        if field.name() == "message" {
            self.message = Some(value);
        } else {
            self.fields.insert(field.name().to_owned(), value);
        }
    }
}

/// A page of the kept entries `filter` reads: newest first; oldest first
/// from `filter.after`.
///
/// # Errors
///
/// The read.
pub(crate) async fn find_page(
    db: &Db,
    filter: LogFilter,
    page: Page<i64>,
) -> Result<Vec<Log>, Error> {
    let logs = db.read(move |connection| Ok(log::db::find_page(connection, &filter, page)?));
    Ok(logs.await?)
}

/// Every event (`target`) an entry is kept of, each once, in order.
///
/// # Errors
///
/// The read.
pub(crate) async fn targets(db: &Db) -> Result<Vec<String>, Error> {
    Ok(db
        .read(|connection| Ok(log::db::find_targets(connection)?))
        .await?)
}

/// What a feed listener hears.
pub(crate) enum Heard {
    /// An entry its filter reads.
    Entry(Box<Log>),
    /// How many entries it missed by lagging.
    Missed(u64),
}

/// Every entry of `feed` from now on that `filter` reads, and each count
/// missed by lagging; ends when the feed does.
pub(crate) fn listen(feed: &Feed, filter: LogFilter) -> impl Stream<Item = Heard> + use<> {
    futures_util::stream::unfold(feed.subscribe(), move |mut receiver| {
        let filter = filter.clone();
        async move {
            loop {
                let heard = match receiver.recv().await {
                    Ok(log) if filter.matches(&log) => Heard::Entry(Box::new(log)),
                    Ok(_) => continue,
                    Err(broadcast::error::RecvError::Lagged(missed)) => Heard::Missed(missed),
                    Err(broadcast::error::RecvError::Closed) => return None,
                };
                return Some((heard, receiver));
            }
        }
    })
}
