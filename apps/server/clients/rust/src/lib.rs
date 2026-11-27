//! A typed client of the hashinbox API. `client`, `scope`, `session` and
//! `secret` are hand-written; `models` and `operations` are written by
//! cligen (`apps/server/build.rs`).
//!
//! ```no_run
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! use hashinbox_client::{Client, Secret, operations::*};
//!
//! let secret = Secret::parse("hip/…", "his/…")?;
//! let client = Client::builder("https://pay.example.com").secret(secret).build()?;
//! let assets = assets_list(&client, AssetsListRequest).await?;
//! # Ok(()) }
//! ```

pub mod client;
pub mod models;
pub mod operations;
pub mod scope;
pub mod secret;
pub mod session;

#[cfg(feature = "reqwest")]
pub use client::http::Client;
pub use client::{ApiError, Error, Operation, reason};
pub use scope::{Packed, Scope, Scopes, fill};
pub use secret::{Issue, Secret};
pub use session::{Invalid, Session};
