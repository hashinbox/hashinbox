//! The kept logs: [`DbLayer`], the `tracing` layer that queues each event,
//! with its spans' fields, to one batched writer of the `log` table and
//! publishes it to the live [`Feed`]; [`find_page`], the read; and
//! [`listen`], the feed's.

use std::{
    collections::BTreeMap,
    fmt,
    time::{SystemTime, UNIX_EPOCH},
};

use base::Container;
use futures_util::Stream;
use tokio::sync::{broadcast, mpsc};
use tracing::{
    Event, Subscriber,
    field::{Field, Visit},
    span,
};
use tracing_subscriber::{layer::Context, registry::LookupSpan};

use super::{Error, Transport, jobs};
use crate::{
    db::{Db, Page},
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
/// (`network`, `node`, `job_id`, …), queued to its own writer and
/// published to its feed.
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

/// A span's fields, kept in its extensions.
struct SpanFields(BTreeMap<String, String>);

impl<S: Subscriber + for<'a> LookupSpan<'a>> tracing_subscriber::Layer<S> for DbLayer {
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut fields = Fields::default();
        attrs.record(&mut fields);
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
        let mut fields = Fields::default();
        event.record(&mut fields);
        let Fields {
            message,
            fields: mut all,
        } = fields;
        for span in ctx.event_scope(event).into_iter().flatten() {
            let extensions = span.extensions();
            let Some(SpanFields(kept)) = extensions.get::<SpanFields>() else {
                continue;
            };
            for (key, value) in kept {
                all.entry(key.clone()).or_insert_with(|| value.clone());
            }
        }
        let metadata = event.metadata();
        let target = metadata.target().to_owned();
        let row = Log::new(now(), *metadata.level(), target, message, all);
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

/// The message and the other fields of one event or span, as text.
#[derive(Default)]
struct Fields {
    message: String,
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
    fn keep(&mut self, field: &Field, value: String) {
        if field.name() == "message" {
            self.message = value;
        } else {
            self.fields.insert(field.name().to_owned(), value);
        }
    }
}

/// A page of the kept entries `filter` reads, newest first.
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
    Entry(Log),
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
                    Ok(log) if filter.matches(&log) => Heard::Entry(log),
                    Ok(_) => continue,
                    Err(broadcast::error::RecvError::Lagged(missed)) => Heard::Missed(missed),
                    Err(broadcast::error::RecvError::Closed) => return None,
                };
                return Some((heard, receiver));
            }
        }
    })
}

/// A page of `network`'s kept entries `filter` reads, newest first.
///
/// # Errors
///
/// Not found for an unconfigured network; the read.
pub(crate) async fn network_page(
    container: &Container<Transport>,
    db: &Db,
    network: base::Network,
    filter: LogFilter,
    page: Page<i64>,
) -> Result<Vec<Log>, Error> {
    container.service(&network)?;
    let filter = LogFilter {
        network: Some(network),
        ..filter
    };
    find_page(db, filter, page).await
}

/// A page of job `id`'s kept entries, newest first.
///
/// # Errors
///
/// Not found for an unknown job; the read.
pub(crate) async fn job_page(db: &Db, id: &str, page: Page<i64>) -> Result<Vec<Log>, Error> {
    let job = jobs::read(db, id).await?.job;
    let filter = LogFilter {
        job_id: i64::try_from(job.id.0).ok(),
        ..LogFilter::default()
    };
    find_page(db, filter, page).await
}
