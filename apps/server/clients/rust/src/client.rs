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
    /// fails unsent with the server's 403 `missing_scope` [`Error::Api`]. `None`: it holds them,
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
    /// A non-2xx answer (or the server's refusal, made here unsent), as
    /// [`ApiError::from`] reads it.
    Api(ApiError),
    /// A 2xx answer that did not decode.
    Decode(serde_json::Error),
    /// The transport failed.
    Transport(T),
}

impl<T> Error<T> {
    /// Its reason, when an answer gave one (`totp_required`, …).
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Api(error) => error.reason.as_deref(),
            _ => None,
        }
    }
}

impl<T: fmt::Display> fmt::Display for Error<T> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Api(error) => write!(out, "HTTP {}: {}", error.status, error.message),
            Self::Decode(error) => write!(out, "cannot decode the answer: {error}"),
            Self::Transport(error) => error.fmt(out),
        }
    }
}

impl<T: std::error::Error + 'static> std::error::Error for Error<T> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Api(_) => None,
            Self::Decode(error) => Some(error),
            Self::Transport(error) => Some(error),
        }
    }
}

/// The fixed machine strings of [`ApiError::reason`]: what a client acts on.
pub mod reason {
    /// A login's email, password or two-factor code is wrong, or the
    /// current password or the code of a two-factor change (401).
    pub const WRONG_CREDENTIALS: &str = "wrong_credentials";
    /// The password was right and the user's two-factor code is needed:
    /// ask it, post again (401).
    pub const TOTP_REQUIRED: &str = "totp_required";
    /// A genuine session (or key) no longer accepted: expired, or
    /// invalidated since it was issued (401). Log in again.
    pub const SESSION_EXPIRED: &str = "session_expired";
    /// The caller lacks a scope; metadata `scope` names it (403).
    pub const MISSING_SCOPE: &str = "missing_scope";
}

/// Any failure as the API's error: its kind, message, reason, metadata,
/// and the answer's status (0 when none came). A plain value, made with its
/// fields (`..ApiError::default()`: `unavailable`, status 0, no metadata);
/// `ApiError::from` turns a [`Response`] or an [`Error`] into one.
#[derive(Debug)]
pub struct ApiError {
    /// Its status: 0 when no answer gave one.
    pub status: u16,
    /// Its kind (`unauthenticated`, `permission_denied`, …).
    pub kind: Kind,
    /// Its message, for people.
    pub message: String,
    /// What a client acts on: one of [`reason`].
    pub reason: Option<String>,
    /// The reason's details: `{"scope": "…"}`; `response`: an answer's
    /// body that did not parse, its first 1024 characters.
    pub metadata: BTreeMap<String, String>,
    /// The error it wraps, when not an answer.
    pub cause: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl Default for ApiError {
    fn default() -> Self {
        Self {
            status: 0,
            kind: Kind::Unavailable,
            message: String::new(),
            reason: None,
            metadata: BTreeMap::new(),
            cause: None,
        }
    }
}

/// A non-2xx answer: the API's error when its body is one (a kind this
/// client knows, a message), with its status; else `unavailable`, its
/// status, its body's first 1024 characters as `metadata.response`.
impl From<Response> for ApiError {
    fn from(response: Response) -> Self {
        let body = serde_json::from_str::<crate::models::Error>(&response.body);
        let Some(body) = body
            .ok()
            .filter(|body| !matches!(body.kind, Kind::Unknown(_)))
        else {
            let text = response.body.chars().take(1024).collect();
            return Self {
                status: response.status,
                message: format!("could not parse the response (HTTP {})", response.status),
                metadata: BTreeMap::from([("response".to_owned(), text)]),
                ..Self::default()
            };
        };
        Self {
            status: response.status,
            kind: body.kind,
            message: body.message,
            reason: body.reason,
            metadata: body.metadata.unwrap_or_default(),
            cause: None,
        }
    }
}

/// [`Error::Api`] as it is; any other `unavailable`, its message, itself as
/// `cause`.
impl<T: std::error::Error + Send + Sync + 'static> From<Error<T>> for ApiError {
    fn from(error: Error<T>) -> Self {
        let Error::Api(api) = error else {
            return Self {
                message: error.to_string(),
                cause: Some(Box::new(error)),
                ..Self::default()
            };
        };
        api
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.cause.as_deref().map(|cause| cause as _)
    }
}

/// The server's 403 `missing_scope` [`Error::Api`], unsent, when `missing`
/// names a scope of `request`'s.
fn check<T, R: Request>(
    request: &R,
    missing: impl Fn(&[String]) -> Option<String>,
) -> Result<(), Error<T>> {
    let Some(scope) = missing(&request.scopes()) else {
        return Ok(());
    };
    let operation = request.operation().id;
    Err(Error::Api(ApiError {
        status: 403,
        kind: Kind::PermissionDenied,
        message: format!("{operation} needs scope {scope}"),
        reason: Some(reason::MISSING_SCOPE.to_owned()),
        metadata: BTreeMap::from([
            ("scope".to_owned(), scope),
            ("operation".to_owned(), operation.to_owned()),
        ]),
        cause: None,
    }))
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
        return Err(Error::Api(response.into()));
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
        .map_err(|response| Error::Api(response.into()))?;
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
                        return Poll::Ready(Some(Err(Error::Api(response.into()))));
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
