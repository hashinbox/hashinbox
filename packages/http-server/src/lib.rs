#![doc = include_str!("../README.md")]

mod auth;
mod config;
mod runtime;

pub use auth::BearerToken;
pub use config::{Config, ConfigError, ConfigErrorKind, RequestLimits, TransportSecurity};
pub use runtime::{
    Error, ErrorKind, LIVENESS_PATH, READINESS_PATH, UNAUTHORIZED_BODY, protected_router, serve,
};
