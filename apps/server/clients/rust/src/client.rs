//! Hand-written: what the generated operations (`operations`) and models
//! (`models`) use. cligen (via `apps/server/build.rs`) writes those two
//! modules beside this; it never touches this file. The default transport,
//! [`http::Client`], is behind the `reqwest` feature.

#[cfg(feature = "reqwest")]
pub mod http;

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use futures_core::Stream;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::models::Kind;
use crate::scope::{escape_id, fill};

/// An HTTP method.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Method {
    /// `GET`.
    Get,
    /// `POST`.
    Post,
    /// `PUT`.
    Put,
    /// `PATCH`.
    Patch,
    /// `DELETE`.
    Delete,
}

impl Method {
    /// `GET`, `POST`, …
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        }
    }
}

/// An operation's static description.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Operation {
    /// Its `operationId`.
    pub id: &'static str,
    /// Its method.
    pub method: Method,
    /// Its path template: `/v1/wallets/{id}`.
    pub path: &'static str,
    /// Its scope templates.
    pub templates: &'static [&'static str],
}

/// An operation's input, as each generated request struct gives it.
pub trait Request {
    /// What a 2xx answer decodes to: a model, `()`, or a stream's event.
    type Response;

    /// The operation.
    fn operation(&self) -> &'static Operation;

    /// The path parameter `name`.
    fn param(&self, name: &str) -> Option<String> {
        let _ = name;
        None
    }

    /// Adds its query parameters to `query`.
    fn query(&self, query: &mut Vec<(&'static str, String)>) {
        let _ = query;
    }

    /// Its JSON body.
    fn body(&self) -> Option<String> {
        None
    }

    /// Its path, each `{name}` its parameter, percent-encoded.
    fn path(&self) -> String {
        fill(self.operation().path, |name| {
            encode(self.param(name).unwrap_or_default())
        })
    }

    /// The scopes it needs, each hole escaped.
    fn scopes(&self) -> Vec<String> {
        let templates = self.operation().templates.iter();
        templates
            .map(|template| {
                fill(template, |name| {
                    escape_id(&self.param(name).unwrap_or_default())
                })
            })
            .collect()
    }

    /// Whether `session` holds every scope it needs.
    fn can(&self, session: &dyn Session) -> bool {
        self.scopes().iter().all(|scope| session.covers(scope))
    }
}

/// One call, as a transport sends it: its path filled and escaped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Http {
    /// The operation's id.
    pub operation: &'static str,
    /// Its method.
    pub method: Method,
    /// Its path, without the base URL.
    pub path: String,
    /// Its query parameters.
    pub query: Vec<(&'static str, String)>,
    /// Its JSON body.
    pub body: Option<String>,
}

impl Http {
    /// The call `request` makes.
    pub fn of(request: &impl Request) -> Self {
        let mut query = Vec::new();
        request.query(&mut query);
        let operation = request.operation();
        Self {
            operation: operation.id,
            method: operation.method,
            path: request.path(),
            query,
            body: request.body(),
        }
    }
}

/// One answer: its status and its body's text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    /// Its HTTP status.
    pub status: u16,
    /// Its body.
    pub body: String,
}

/// One server-sent event, as read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawEvent {
    /// Its `event` name: `message` when none was sent.
    pub event: String,
    /// Its `data`.
    pub data: String,
    /// Its `id`.
    pub id: Option<String>,
}

/// A credential and the scopes it holds.
pub trait Session {
    /// Whether it holds `scope`.
    fn covers(&self, scope: &str) -> bool;
}

/// Sends one call.
pub trait Client: Sync {
    /// Why sending failed.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Sends `http`.
    fn send(&self, http: Http) -> impl Future<Output = Result<Response, Self::Error>> + Send;

    /// The first of `scopes` it knows its session lacks: the call then
    /// fails with [`Error::MissingScope`] unsent. `None`: it holds them,
    /// or it cannot tell (the server checks).
    fn missing(&self, scopes: &[String]) -> Option<String> {
        let _ = scopes;
        None
    }
}

/// A transport's raw events.
pub type RawEvents<E> = Pin<Box<dyn Stream<Item = Result<RawEvent, E>> + Send>>;

/// Opens one call as a stream of events.
pub trait Streamer: Sync {
    /// Why opening or reading failed.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Opens `http`: its events, or a non-2xx answer.
    fn open(
        &self,
        http: Http,
    ) -> impl Future<Output = Result<Result<RawEvents<Self::Error>, Response>, Self::Error>> + Send;

    /// Waits `duration`: a stream's backoff before it reconnects.
    fn sleep(&self, duration: Duration) -> impl Future<Output = ()> + Send;

    /// As [`Client::missing`].
    fn missing(&self, scopes: &[String]) -> Option<String> {
        let _ = scopes;
        None
    }
}

/// Why a call failed.
#[derive(Debug)]
pub enum Error<T> {
    /// A non-2xx answer. A body that is not the API's error is `message`,
    /// its kind `Unknown("")`.
    Api {
        /// Its status.
        status: u16,
        /// Its kind (`unauthenticated`, `permission_denied`, …).
        kind: Kind,
        /// Its message, for people.
        message: String,
        /// What a client acts on: `wrong_credentials`, `totp_required`,
        /// `session_expired`, `missing_scope`.
        reason: Option<String>,
        /// The reason's details: `{"scope": "…"}`.
        metadata: BTreeMap<String, String>,
    },
    /// Refused unsent: the session lacks `scope`.
    MissingScope {
        /// The operation's id.
        operation: &'static str,
        /// The scope it needs.
        scope: String,
    },
    /// A 2xx answer that did not decode.
    Decode(serde_json::Error),
    /// The transport failed.
    Transport(T),
}

impl<T> Error<T> {
    /// The non-2xx answer `response` is.
    pub fn api(response: Response) -> Self {
        let Ok(body) = serde_json::from_str::<crate::models::Error>(&response.body) else {
            return Self::Api {
                status: response.status,
                kind: Kind::Unknown(String::new()),
                message: response.body,
                reason: None,
                metadata: BTreeMap::new(),
            };
        };
        Self::Api {
            status: response.status,
            kind: body.kind,
            message: body.message,
            reason: body.reason,
            metadata: body.metadata.unwrap_or_default(),
        }
    }

    /// Its reason, when an answer gave one (`totp_required`, …).
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Api { reason, .. } => reason.as_deref(),
            Self::MissingScope { .. } => Some("missing_scope"),
            _ => None,
        }
    }
}

impl<T: fmt::Display> fmt::Display for Error<T> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Api {
                status, message, ..
            } => write!(out, "HTTP {status}: {message}"),
            Self::MissingScope { operation, scope } => {
                write!(out, "{operation} needs scope {scope}")
            }
            Self::Decode(error) => write!(out, "cannot decode the answer: {error}"),
            Self::Transport(error) => error.fmt(out),
        }
    }
}

impl<T: std::error::Error + 'static> std::error::Error for Error<T> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Api { .. } | Self::MissingScope { .. } => None,
            Self::Decode(error) => Some(error),
            Self::Transport(error) => Some(error),
        }
    }
}

/// [`Error::MissingScope`] when `missing` names a scope of `request`'s.
fn check<T, R: Request>(
    request: &R,
    missing: impl Fn(&[String]) -> Option<String>,
) -> Result<(), Error<T>> {
    let Some(scope) = missing(&request.scopes()) else {
        return Ok(());
    };
    let operation = request.operation().id;
    Err(Error::MissingScope { operation, scope })
}

/// Sends `request` with `client`: the 2xx body decoded.
pub async fn call<C: Client, R: Request>(
    client: &C,
    request: &R,
) -> Result<R::Response, Error<C::Error>>
where
    R::Response: DeserializeOwned,
{
    check(request, |scopes| client.missing(scopes))?;
    let response = client
        .send(Http::of(request))
        .await
        .map_err(Error::Transport)?;
    if !(200..300).contains(&response.status) {
        return Err(Error::api(response));
    }
    let text = if response.body.trim().is_empty() {
        "null"
    } else {
        &response.body
    };
    serde_json::from_str(text).map_err(Error::Decode)
}

/// Opens `request` with `streamer`: a handle of the events this client
/// knows, decoded, across the server's stream windows ([`Events`]).
///
/// # Errors
///
/// A missing scope, or the first answer's error or the transport's.
pub async fn open<'a, S: Streamer, R: Request>(
    streamer: &'a S,
    request: &R,
) -> Result<Events<'a, R::Response, S>, Error<S::Error>>
where
    R::Response: Event,
{
    check(request, |scopes| streamer.missing(scopes))?;
    let http = Http::of(request);
    let raw = streamer
        .open(http.clone())
        .await
        .map_err(Error::Transport)?
        .map_err(Error::api)?;
    Ok(Events {
        streamer,
        http,
        cursor: None,
        state: State::Reading(raw),
        backoff: BACKOFF,
        delay: BACKOFF.0,
        event: PhantomData,
    })
}

/// A stream's event, decoded from a [`RawEvent`].
pub trait Event: Sized {
    /// `raw` decoded; `None` for an event this client does not know.
    fn decode(raw: &RawEvent) -> Result<Option<Self>, serde_json::Error>;
}

impl Event for RawEvent {
    fn decode(raw: &RawEvent) -> Result<Option<Self>, serde_json::Error> {
        Ok(Some(raw.clone()))
    }
}

/// The first wait before a reconnect after an error, and the longest.
const BACKOFF: (Duration, Duration) = (Duration::from_millis(500), Duration::from_secs(8));

type Opening<'a, E> =
    Pin<Box<dyn Future<Output = Result<Result<RawEvents<E>, Response>, E>> + Send + 'a>>;

/// Where an [`Events`] is.
enum State<'a, E> {
    /// Reading an answer's events.
    Reading(RawEvents<E>),
    /// Waiting out a backoff.
    Waiting(Pin<Box<dyn Future<Output = ()> + Send + 'a>>),
    /// Reconnecting.
    Opening(Opening<'a, E>),
    /// Ended, or stopped.
    Done,
}

/// A stream's handle: its typed events across the server's windows. When
/// an answer ends with `event: close` (the window is over) it reconnects at
/// once with the last event id as its `cursor`; after a transport error,
/// a cut answer or a 5xx/429, after a backoff (500 ms doubling to 8 s,
/// [`Self::backoff`]). Another refused reconnect (a 401, …) is its last
/// item; an answer that ends without `close` (a finished search) its end.
/// [`Self::stop`], or dropping it, ends it for good.
pub struct Events<'a, T, S: Streamer> {
    streamer: &'a S,
    http: Http,
    cursor: Option<String>,
    state: State<'a, S::Error>,
    backoff: (Duration, Duration),
    delay: Duration,
    event: PhantomData<fn() -> T>,
}

impl<T, S: Streamer> Events<'_, T, S> {
    /// The last event id read: the `cursor` a reconnect resumes from.
    pub fn cursor(&self) -> Option<&str> {
        self.cursor.as_deref()
    }

    /// Ends it for good: the answer read now is closed, none is opened.
    pub fn stop(&mut self) {
        self.state = State::Done;
    }

    /// Waits `first`, doubling up to `most`, before reconnecting after an
    /// error.
    #[must_use]
    pub fn backoff(mut self, first: Duration, most: Duration) -> Self {
        (self.backoff, self.delay) = ((first, most), first);
        self
    }
}

impl<'a, T: Event, S: Streamer> Stream for Events<'a, T, S> {
    type Item = Result<T, Error<S::Error>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let streamer: &'a S = this.streamer;
        loop {
            match &mut this.state {
                State::Done => return Poll::Ready(None),
                State::Waiting(wait) => {
                    ready!(wait.as_mut().poll(cx));
                    let mut http = this.http.clone();
                    if let Some(cursor) = &this.cursor {
                        http.query.retain(|(name, _)| *name != "cursor");
                        http.query.push(("cursor", cursor.clone()));
                    }
                    this.state = State::Opening(Box::pin(streamer.open(http)));
                }
                State::Opening(opening) => match ready!(opening.as_mut().poll(cx)) {
                    Ok(Ok(raw)) => {
                        this.delay = this.backoff.0;
                        this.state = State::Reading(raw);
                    }
                    Ok(Err(response)) if response.status == 429 || response.status >= 500 => {
                        this.state = this.wait();
                    }
                    Ok(Err(response)) => {
                        this.state = State::Done;
                        return Poll::Ready(Some(Err(Error::api(response))));
                    }
                    Err(_) => this.state = this.wait(),
                },
                State::Reading(raw) => match ready!(raw.as_mut().poll_next(cx)) {
                    None => this.state = State::Done,
                    Some(Err(_)) => this.state = this.wait(),
                    Some(Ok(raw)) if raw.event == "close" => {
                        this.delay = this.backoff.0;
                        let wait = streamer.sleep(Duration::ZERO);
                        this.state = State::Waiting(Box::pin(wait));
                    }
                    Some(Ok(raw)) => {
                        if let Some(id) = raw.id.as_ref().filter(|id| !id.is_empty()) {
                            this.cursor = Some(id.clone());
                        }
                        match T::decode(&raw) {
                            Ok(None) => {}
                            Ok(Some(event)) => return Poll::Ready(Some(Ok(event))),
                            Err(error) => return Poll::Ready(Some(Err(Error::Decode(error)))),
                        }
                    }
                },
            }
        }
    }
}

impl<'a, T, S: Streamer> Events<'a, T, S> {
    /// The backoff's next wait, doubling the one after.
    fn wait(&mut self) -> State<'a, S::Error> {
        let delay = self.delay;
        self.delay = (delay * 2).min(self.backoff.1);
        State::Waiting(Box::pin(self.streamer.sleep(delay)))
    }
}

/// The data of `raw`, decoded.
pub fn data<T: DeserializeOwned>(raw: &RawEvent) -> Result<T, serde_json::Error> {
    serde_json::from_str(&raw.data)
}

/// `value` as JSON text.
pub fn json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("a model is JSON")
}

/// `value` percent-encoded as a path segment: all but `A-Z a-z 0-9 - . _ ~`.
pub fn encode(value: impl fmt::Display) -> String {
    let mut text = String::new();
    for byte in value.to_string().bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            text.push(char::from(byte));
        } else {
            text.push_str(&format!("%{byte:02X}"));
        }
    }
    text
}
