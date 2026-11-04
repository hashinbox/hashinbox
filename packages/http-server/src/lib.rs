#![doc = include_str!("../README.md")]

mod auth;
mod config;
mod runtime;
mod r#static;

pub use auth::BearerToken;
pub use config::{Config, ConfigError, ConfigErrorKind, RequestLimits, TransportSecurity};
pub use runtime::{
    Error, ErrorKind, LIVENESS_PATH, READINESS_PATH, UNAUTHORIZED_BODY, protected_router, serve,
};
pub use r#static::{Cache, Disk, Embedded, Mount, Root, Source};
