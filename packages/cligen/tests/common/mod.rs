//! What the tests share: each language, and the spec of a document.

use cligen::{Files, Spec};

/// A language's files for a spec.
pub type Emit = Box<dyn Fn(&Spec) -> Files>;

/// Each language's directory name and emitter.
pub fn languages() -> Vec<(&'static str, Emit)> {
    let go = cligen::go::Go {
        package: "mini".into(),
    };
    let php = cligen::php::Php {
        namespace: "Mini".into(),
    };
    vec![
        (
            "typescript",
            Box::new(|spec| cligen::typescript::Typescript.emit(spec)),
        ),
        ("go", Box::new(move |spec| go.emit(spec))),
        ("rust", Box::new(|spec| cligen::rust::Rust.emit(spec))),
        ("python", Box::new(|spec| cligen::python::Python.emit(spec))),
        ("php", Box::new(move |spec| php.emit(spec))),
    ]
}

/// The spec of the document at `path`, relative to this package.
pub fn spec(path: &str) -> Spec {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    Spec::from_openapi(&text).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}
