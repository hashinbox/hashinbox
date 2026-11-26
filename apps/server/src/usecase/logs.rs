//! The kept logs: [`DbLayer`], the `tracing` layer that queues each event,
//! with its spans' fields, to one batched writer of the `log` table and
//! publishes it to the live [`Feed`]; [`Line`], how stdout shows one;
//! [`find_page`], the read; and [`listen`], the feed's.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt,
    sync::{Arc, Mutex, PoisonError},
    time::{SystemTime, UNIX_EPOCH},
};

use futures_util::{Stream, StreamExt as _};
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

use crate::error::Error;
use crate::{
    db::{Db, Page},
    events,
    models::{Log, LogFilter, log},
};

/// Rows waiting for the writer; beyond them one is dropped.
const QUEUED: usize = 10_000;

/// The most rows one write stores.
const BATCH: usize = 500;

/// The stored entries one read of a resuming listener's takes.
const PAGE: i64 = 500;

/// Entries the feed holds for its slowest listener, beyond which that one
/// misses the oldest; and the newest it keeps for a listener resuming.
const FEED: usize = 1_000;

/// The live feed: every entry [`DbLayer`] keeps, as it is kept, given its
/// id then (before it is stored); the newest [`FEED`] of them held for a
/// listener resuming after one ([`listen`]).
#[derive(Clone)]
pub(crate) struct Feed(Arc<Mutex<Held>>);

/// What a [`Feed`] holds, under its one lock.
struct Held {
    sender: broadcast::Sender<Log>,
    /// The newest entries, oldest first.
    recent: VecDeque<Log>,
    /// Every entry from this id on is in `recent`.
    from: i64,
    /// The id given last.
    last: i64,
}

/// A new feed with no listener.
pub(crate) fn feed() -> Feed {
    let now = now_micros();
    Feed(Arc::new(Mutex::new(Held {
        sender: broadcast::channel(FEED).0,
        recent: VecDeque::with_capacity(FEED),
        from: now,
        last: now,
    })))
}

impl Feed {
    /// `log` given its id, held and sent to every listener; as sent. Its id
    /// is now in microseconds, or one more than the last: newest highest,
    /// on past a restart.
    fn publish(&self, mut log: Log) -> Log {
        let mut held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        held.last = now_micros().max(held.last + 1);
        log.id = held.last;
        if held.recent.len() == FEED
            && let Some(oldest) = held.recent.pop_front()
        {
            held.from = oldest.id + 1;
        }
        held.recent.push_back(log.clone());
        let _unheard = held.sender.send(log.clone());
        log
    }

    /// A listener of every entry from now on.
    #[cfg(test)]
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<Log> {
        let held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        held.sender.subscribe()
    }

    /// A listener of every entry from now on, the held ones after `after`
    /// (oldest first), and the id from which every entry is held: at once,
    /// so none is missed or heard twice.
    fn resume(&self, after: i64) -> (broadcast::Receiver<Log>, Vec<Log>, i64) {
        let held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let recent = held.recent.iter().filter(|log| log.id > after).cloned();
        (held.sender.subscribe(), recent.collect(), held.from)
    }
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
        let row = self.feed.publish(row);
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

/// Now, Unix µs: what a new entry's id is at least.
fn now_micros() -> i64 {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(since.as_micros()).unwrap_or(i64::MAX)
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

/// Every entry `filter` reads from now on, and each count missed by
/// lagging; ends when the feed does. With `after` (an entry's id), first
/// every entry since it: those the feed holds, and before them, when it no
/// longer holds all, the stored ones. None is sent twice.
pub(crate) fn listen(
    db: Db,
    feed: &Feed,
    filter: LogFilter,
    after: Option<i64>,
) -> impl Stream<Item = Heard> + use<> {
    let (receiver, recent, from) = feed.resume(after.unwrap_or(i64::MAX));
    let gap = after.filter(|after| after + 1 < from);
    let stored = stored(db, filter.clone(), gap, from);
    let held = recent.into_iter().filter({
        let filter = filter.clone();
        move |log| filter.matches(log)
    });
    let held = futures_util::stream::iter(held.map(|log| Heard::Entry(Box::new(log))));
    let live = futures_util::stream::unfold(receiver, move |mut receiver| {
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
    });
    stored.chain(held).chain(live)
}

/// The stored entries `filter` reads after `after` and before `before`,
/// oldest first, a page at a time; none without `after`. A failed read
/// ends them.
fn stored(db: Db, filter: LogFilter, after: Option<i64>, before: i64) -> impl Stream<Item = Heard> {
    let oldest_first = LogFilter {
        after: filter.after.or(Some(0)),
        ..filter
    };
    let pages = futures_util::stream::unfold(after, move |cursor| {
        let (db, filter) = (db.clone(), oldest_first.clone());
        async move {
            let page = Page {
                after: Some(cursor?),
                limit: PAGE,
            };
            let logs = find_page(&db, filter, page).await.ok()?;
            let logs: Vec<Log> = logs.into_iter().filter(|log| log.id < before).collect();
            let full = i64::try_from(logs.len()) == Ok(PAGE);
            let next = logs.last().map(|log| log.id).filter(|_| full);
            let heard = logs.into_iter().map(|log| Heard::Entry(Box::new(log)));
            Some((futures_util::stream::iter(heard), next))
        }
    });
    pages.flatten()
}
