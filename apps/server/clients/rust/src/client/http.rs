//! The default transport, over `reqwest`: [`Client::builder`].
//!
//! Its credential is a [`Session`] (a login's, or a JWT handed to it), or a
//! key's [`Secret`]: then it signs its own sessions (the secret never goes
//! over the wire), keeps one, and signs a fresh one within 60 s of its
//! expiry or after a `401 session_expired` (one retry). See
//! [`crate::secret`] for the clock this needs.

use std::fmt;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use super::{Error as CallError, Event, Events, Http, RawEvent, RawEvents, Request, Response};
use crate::models::{CreateSessionRequest, IssuedSession, SessionRead};
use crate::operations::{SessionCreateRequest, SessionDeleteRequest, SessionGetRequest};
use crate::scope::Scopes;
use crate::secret::{Issue, Secret};
use crate::session::{Invalid, Session, now};

/// What a call returns: its answer, or why it failed.
pub type Result<T> = std::result::Result<T, CallError<Error>>;

/// Why the transport failed.
#[derive(Debug)]
pub enum Error {
    /// The HTTP exchange failed.
    Http(reqwest::Error),
    /// A session could not be signed or read.
    Session(Invalid),
}

impl fmt::Display for Error {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http(error) => error.fmt(out),
            Self::Session(error) => write!(out, "session: {error}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Http(error) => Some(error),
            Self::Session(error) => Some(error),
        }
    }
}

impl From<reqwest::Error> for Error {
    fn from(error: reqwest::Error) -> Self {
        Self::Http(error)
    }
}

impl From<Invalid> for Error {
    fn from(error: Invalid) -> Self {
        Self::Session(error)
    }
}

/// A client of one hashinbox server. Cheap to clone: clones share the
/// session.
#[derive(Clone)]
pub struct Client(Arc<Inner>);

struct Inner {
    base: String,
    http: reqwest::Client,
    timeout: Option<Duration>,
    check_scopes: bool,
    session: RwLock<Option<Session>>,
    signer: Option<Signer>,
}

/// A key signing the client's sessions.
struct Signer {
    secret: Secret,
    /// The builder's scopes; `None`: the key's, read once.
    scopes: Option<Scopes>,
    key_scopes: RwLock<Option<Scopes>>,
    ttl: Duration,
}

/// Builds a [`Client`].
pub struct Builder {
    base: String,
    session: Option<Session>,
    secret: Option<Secret>,
    scopes: Option<Scopes>,
    ttl: Duration,
    http: Option<reqwest::Client>,
    timeout: Option<Duration>,
    check_scopes: bool,
}

impl Builder {
    /// Sends `session`'s JWT.
    #[must_use]
    pub fn session(mut self, session: Session) -> Self {
        self.session = Some(session);
        self
    }

    /// Signs its own sessions with `secret`.
    #[must_use]
    pub fn secret(mut self, secret: Secret) -> Self {
        self.secret = Some(secret);
        self
    }

    /// The scopes its signed sessions hold; unset: the key's, read with
    /// `GET /v1/session` (a session of no scopes) on first use.
    #[must_use]
    pub fn scopes(mut self, scopes: Scopes) -> Self {
        self.scopes = Some(scopes);
        self
    }

    /// How long its signed sessions last: 10 minutes unless set.
    #[must_use]
    pub fn ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Sends with `http` (its proxy, TLS, pool).
    #[must_use]
    pub fn http(mut self, http: reqwest::Client) -> Self {
        self.http = Some(http);
        self
    }

    /// Fails a call (not a stream) not answered within `timeout`.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Whether a call its session lacks a scope for fails unsent
    /// (the server's 403 `missing_scope` [`CallError::Api`]); on unless set.
    #[must_use]
    pub fn check_scopes(mut self, check: bool) -> Self {
        self.check_scopes = check;
        self
    }

    /// The client.
    ///
    /// # Errors
    ///
    /// When the TTL is not 1 s to 24 h.
    pub fn build(self) -> std::result::Result<Client, Invalid> {
        let signer = self.secret.map(|secret| Signer {
            secret,
            scopes: self.scopes,
            key_scopes: RwLock::new(None),
            ttl: self.ttl,
        });
        if let Some(signer) = &signer {
            signer.secret.issue(Issue::new().ttl(signer.ttl))?;
        }
        Ok(Client(Arc::new(Inner {
            base: self.base.trim_end_matches('/').to_owned(),
            http: self.http.unwrap_or_default(),
            timeout: self.timeout,
            check_scopes: self.check_scopes,
            session: RwLock::new(self.session),
            signer,
        })))
    }
}

/// A bearer token to send, or the answer that refused making one.
enum Bearer {
    Token(Option<String>),
    Refused(Response),
}

impl Client {
    /// A client of the server at `base_url` (`https://pay.example.com`).
    pub fn builder(base_url: impl Into<String>) -> Builder {
        Builder {
            base: base_url.into(),
            session: None,
            secret: None,
            scopes: None,
            ttl: Duration::from_secs(10 * 60),
            http: None,
            timeout: None,
            check_scopes: true,
        }
    }

    /// The session it sends now, if any.
    pub fn session(&self) -> Option<Session> {
        self.0.session.read().ok()?.clone()
    }

    /// Logs in (`POST /v1/session`, no `Authorization`) and keeps the
    /// session. A wrong email, password or code is `401` with reason
    /// `wrong_credentials`; 2FA on and no `totp`, `totp_required`
    /// ([`CallError::reason`]).
    ///
    /// # Errors
    ///
    /// The answer's, or the transport's.
    pub async fn login(&self, email: &str, password: &str, totp: Option<&str>) -> Result<Session> {
        let request = SessionCreateRequest {
            body: CreateSessionRequest {
                email: Some(email.to_owned()),
                password: Some(password.to_owned()),
                totp: totp.map(str::to_owned),
                passphrase: None,
                scopes: None,
                sub: None,
                ttl: None,
            },
        };
        let http = Http::of(&request);
        let response = self.read(&http, None).await.map_err(CallError::Transport)?;
        if !(200..300).contains(&response.status) {
            return Err(CallError::Api(response.into()));
        }
        let issued: IssuedSession =
            serde_json::from_str(&response.body).map_err(CallError::Decode)?;
        let session = Session::parse(issued.token).map_err(|e| CallError::Transport(e.into()))?;
        self.keep(Some(session.clone()));
        Ok(session)
    }

    /// Logs out (`DELETE /v1/session`) and forgets the session; with
    /// `all`, every session its issuer made until now is refused.
    ///
    /// # Errors
    ///
    /// The answer's, or the transport's.
    pub async fn logout(&self, all: bool) -> Result<()> {
        let request = SessionDeleteRequest { all: Some(all) };
        super::call(self, &request).await?;
        self.keep(None);
        Ok(())
    }

    /// Opens `request`'s server-sent events, typed: a handle reconnecting
    /// across the server's stream windows until it ends, is stopped or
    /// dropped ([`Events`]).
    ///
    /// # Errors
    ///
    /// The answer's, or the transport's.
    pub async fn stream<R: Request>(&self, request: &R) -> Result<Events<'_, R::Response, Self>>
    where
        R::Response: Event,
    {
        super::open(self, request).await
    }

    fn keep(&self, session: Option<Session>) {
        if let Ok(mut kept) = self.0.session.write() {
            *kept = session;
        }
    }

    /// The token to send: the kept session's, or (with a secret) a session
    /// signed now when `fresh`, none is kept, or it ends within 60 s.
    async fn bearer(&self, fresh: bool) -> std::result::Result<Bearer, Error> {
        let kept = self.session();
        let Some(signer) = &self.0.signer else {
            return Ok(Bearer::Token(kept.map(|session| session.token)));
        };
        let margin = (signer.ttl.as_secs() / 2).min(60);
        let margin = i64::try_from(margin).unwrap_or(60);
        if let Some(session) = kept.filter(|kept| !fresh && kept.expires_at - now() > margin) {
            return Ok(Bearer::Token(Some(session.token)));
        }
        let scopes = match &signer.scopes {
            Some(scopes) => scopes.clone(),
            None => match self.key_scopes(signer, fresh).await? {
                Ok(scopes) => scopes,
                Err(response) => return Ok(Bearer::Refused(response)),
            },
        };
        let session = signer
            .secret
            .issue(Issue::new().scopes(scopes).ttl(signer.ttl))?;
        let token = session.token.clone();
        self.keep(Some(session));
        Ok(Bearer::Token(Some(token)))
    }

    /// The key's scopes: kept, or (when `fresh` or unknown) read with a
    /// session of none, `GET /v1/session`.
    async fn key_scopes(
        &self,
        signer: &Signer,
        fresh: bool,
    ) -> std::result::Result<std::result::Result<Scopes, Response>, Error> {
        let kept = signer.key_scopes.read().ok().and_then(|kept| kept.clone());
        if let Some(scopes) = kept.filter(|_| !fresh) {
            return Ok(Ok(scopes));
        }
        let probe = signer
            .secret
            .issue(Issue::new().ttl(Duration::from_secs(60)))?;
        let http = Http::of(&SessionGetRequest);
        let response = self.read(&http, Some(&probe.token)).await?;
        if !(200..300).contains(&response.status) {
            return Ok(Err(response));
        }
        let read: SessionRead = serde_json::from_str(&response.body)
            .map_err(|error| Invalid(format!("GET /v1/session: {error}")))?;
        let key = read
            .key
            .ok_or_else(|| Invalid("no key in GET /v1/session".into()))?;
        let packed = key.scopes.into_iter().map(|(prefix, group)| (prefix, group.into()));
        let scopes = Scopes::unpack(&packed.collect()).map_err(Invalid::from)?;
        if let Ok(mut kept) = signer.key_scopes.write() {
            *kept = Some(scopes.clone());
        }
        Ok(Ok(scopes))
    }

    /// `http` sent with the bearer, retried once with a fresh session after
    /// `401 session_expired` when it signs its own: a 2xx answer, or the
    /// other one read.
    async fn exchange(
        &self,
        http: &Http,
        events: bool,
    ) -> std::result::Result<std::result::Result<reqwest::Response, Response>, Error> {
        let mut fresh = false;
        loop {
            let token = match self.bearer(fresh).await? {
                Bearer::Token(token) => token,
                Bearer::Refused(response) => return Ok(Err(response)),
            };
            let answer = self.raw(http, token.as_deref(), events).await?;
            if answer.status().is_success() {
                return Ok(Ok(answer));
            }
            let response = text(answer).await?;
            if fresh || self.0.signer.is_none() || !session_expired(&response) {
                return Ok(Err(response));
            }
            fresh = true;
        }
    }

    /// `http` sent once with `token`, its answer read.
    async fn read(&self, http: &Http, token: Option<&str>) -> std::result::Result<Response, Error> {
        text(self.raw(http, token, false).await?).await
    }

    /// `http` sent once with `token`.
    async fn raw(
        &self,
        http: &Http,
        token: Option<&str>,
        events: bool,
    ) -> std::result::Result<reqwest::Response, reqwest::Error> {
        let method = match http.method {
            super::Method::Get => reqwest::Method::GET,
            super::Method::Post => reqwest::Method::POST,
            super::Method::Put => reqwest::Method::PUT,
            super::Method::Patch => reqwest::Method::PATCH,
            super::Method::Delete => reqwest::Method::DELETE,
        };
        let url = format!("{}{}", self.0.base, http.path);
        let mut request = self.0.http.request(method, url).query(&http.query);
        if let Some(body) = &http.body {
            request = request
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.clone());
        }
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        if events {
            request = request.header(reqwest::header::ACCEPT, "text/event-stream");
        } else if let Some(timeout) = self.0.timeout {
            request = request.timeout(timeout);
        }
        request.send().await
    }

    fn missing(&self, scopes: &[String]) -> Option<String> {
        let session = self.session().filter(|_| self.0.check_scopes)?;
        let held = |scope: &String| super::Session::covers(&session, scope);
        scopes.iter().find(|scope| !held(scope)).cloned()
    }
}

impl super::Client for Client {
    type Error = Error;

    async fn send(&self, http: Http) -> std::result::Result<Response, Error> {
        match self.exchange(&http, false).await? {
            Ok(answer) => Ok(text(answer).await?),
            Err(response) => Ok(response),
        }
    }

    fn missing(&self, scopes: &[String]) -> Option<String> {
        Client::missing(self, scopes)
    }
}

impl super::Streamer for Client {
    type Error = Error;

    async fn open(
        &self,
        http: Http,
    ) -> std::result::Result<std::result::Result<RawEvents<Error>, Response>, Error> {
        Ok(self.exchange(&http, true).await?.map(events))
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    fn missing(&self, scopes: &[String]) -> Option<String> {
        Client::missing(self, scopes)
    }
}

/// Whether `response` is `401` with reason `session_expired`.
fn session_expired(response: &Response) -> bool {
    let body = serde_json::from_str::<crate::models::Error>(&response.body);
    response.status == 401
        && body.is_ok_and(|body| body.reason.as_deref() == Some("session_expired"))
}

/// `answer` read whole.
async fn text(answer: reqwest::Response) -> std::result::Result<Response, Error> {
    let status = answer.status().as_u16();
    Ok(Response {
        status,
        body: answer.text().await?,
    })
}

/// The server-sent events of `answer`: each block up to a blank line, its
/// `event`, `data` (lines joined by `\n`) and `id`; comments and blocks of
/// no data skipped.
fn events(answer: reqwest::Response) -> RawEvents<Error> {
    let start = Some((answer, Vec::new()));
    Box::pin(futures_util::stream::unfold(start, |state| async move {
        let (mut answer, mut buffer) = state?;
        loop {
            if let Some(end) = buffer.windows(2).position(|pair| pair == b"\n\n") {
                let block: Vec<u8> = buffer.drain(..end + 2).collect();
                if let Some(event) = parse(&String::from_utf8_lossy(&block)) {
                    return Some((Ok(event), Some((answer, buffer))));
                }
                continue;
            }
            match answer.chunk().await {
                Ok(Some(bytes)) => buffer.extend(bytes.iter().filter(|byte| **byte != b'\r')),
                Ok(None) => return None,
                Err(error) => return Some((Err(Error::Http(error)), None)),
            }
        }
    }))
}

/// One event block.
fn parse(block: &str) -> Option<RawEvent> {
    let (mut event, mut data, mut id) = (None, Vec::new(), None);
    for line in block.lines() {
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => event = Some(value.to_owned()),
            "data" => data.push(value),
            "id" => id = Some(value.to_owned()),
            _ => {}
        }
    }
    (!data.is_empty()).then(|| RawEvent {
        event: event.unwrap_or_else(|| "message".to_owned()),
        data: data.join("\n"),
        id,
    })
}
