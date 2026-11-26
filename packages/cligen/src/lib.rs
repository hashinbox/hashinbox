//! cligen: typed API clients from an OpenAPI 3.1 document.
//!
//! [`Spec::from_openapi`] parses the supported subset ([`openapi`]; anything
//! else fails to parse) and resolves it into plain data ([`spec`]). Each
//! language ([`typescript`], [`go`], [`rust`], [`python`], [`php`]) is a struct of its options
//! whose `emit` writes it as two files: every model, and every operation (a
//! descriptor, a request, a response and the call). What those call (the
//! client, its scopes, the transport) is hand-written next to them.
//!
//! ```no_run
//! let spec = cligen::Spec::from_openapi(&std::fs::read_to_string("docs/openapi.json")?)?;
//! let files = cligen::go::Go { package: "hashinbox".into() }.emit(&spec);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod error;
pub mod go;
pub mod name;
pub mod openapi;
pub mod out;
pub mod php;
pub mod python;
pub mod rust;
pub mod spec;
pub mod typescript;

use std::collections::BTreeMap;
use std::path::PathBuf;

pub use error::{Error, Result};
pub use spec::Spec;

/// Generated files: path → contents.
pub type Files = BTreeMap<PathBuf, String>;
