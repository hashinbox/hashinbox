//! Serves an axum `Router` with optional bearer authentication and a body
//! limit, after [`Config::validate`] refuses a plaintext non-loopback bind;
//! plus static files and single-page apps ([`Mount`]), and event streams
//! answered a window at a time ([`sse::windowed`]).

mod auth;
mod config;
mod runtime;
pub mod sse;
mod r#static;

pub use auth::BearerToken;
pub use config::{Config, ConfigError, ConfigErrorKind, RequestLimits, TransportSecurity};
pub use runtime::{
    Error, ErrorKind, LIVENESS_PATH, READINESS_PATH, UNAUTHORIZED_BODY, protected_router, serve,
};
pub use r#static::{Cache, Disk, Embedded, Mount, Root, Source};
