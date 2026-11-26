//! Rust syntax, as rustfmt writes it with its defaults (100 columns, four
//! spaces, a list vertical once its items pass 60 columns), in the shape of
//! the `codegen` crate: small structs with public fields and a builder
//! method or two, each writing itself through a [`Formatter`] that tracks
//! indentation, its [`Formatter::block`] writing `{`, the body one level
//! deeper, `}`.

use std::fmt::{self, Write};

use crate::name::{pascal, snake};
use crate::out::{Style, fit};
use crate::spec::Type;

/// rustfmt's `max_width`.
const WIDTH: usize = 100;

/// rustfmt's `array_width`.
const LIST: usize = 60;

/// One level of indentation.
const UNIT: &str = "    ";

/// A `String` being written, and how deep the current line is indented.
pub struct Formatter<'a> {
    /// The text.
    dst: &'a mut String,
    /// The indentation level.
    level: usize,
}

impl<'a> Formatter<'a> {
    /// A formatter writing into `dst`.
    pub fn new(dst: &'a mut String) -> Self {
        Self { dst, level: 0 }
    }

    /// `text` at the current level, each of its lines indented.
    ///
    /// # Errors
    ///
    /// Never, writing into a `String`.
    pub fn line(&mut self, text: &str) -> fmt::Result {
        for line in text.split('\n') {
            if !line.is_empty() {
                write!(self.dst, "{}", UNIT.repeat(self.level))?;
            }
            writeln!(self.dst, "{line}")?;
        }
        Ok(())
    }

    /// `head {`, what `body` writes a level deeper, `}`.
    ///
    /// # Errors
    ///
    /// What `body` returns.
    pub fn block(
        &mut self,
        head: &str,
        body: impl FnOnce(&mut Self) -> fmt::Result,
    ) -> fmt::Result {
        self.line(&format!("{head} {{"))?;
        self.level += 1;
        body(self)?;
        self.level -= 1;
        self.line("}")
    }

    /// `///` doc lines and `#[…]` attributes.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn head(&mut self, doc: &str, attrs: &[String]) -> fmt::Result {
        for line in doc.lines().map(str::trim_end) {
            self.line(&if line.is_empty() {
                "///".to_owned()
            } else {
                format!("/// {line}")
            })?;
        }
        attrs
            .iter()
            .try_for_each(|attr| self.line(&format!("#[{attr}]")))
    }

    /// The column the current level starts at.
    fn column(&self) -> usize {
        self.level * UNIT.len()
    }
}

/// A field of a [`Struct`], or a variant of an [`Enum`].
pub struct Field {
    /// Its doc comment.
    pub doc: String,
    /// Its attributes, without `#[` `]`.
    pub attrs: Vec<String>,
    /// Its name.
    pub name: String,
    /// Its type: a field's, or a tuple variant's payload; empty: a unit
    /// variant.
    pub ty: String,
}

/// `pub struct Name { pub fields }`, or `pub struct Name;` without fields.
pub struct Struct {
    /// Its doc comment.
    pub doc: String,
    /// Its attributes.
    pub attrs: Vec<String>,
    /// Its name.
    pub name: String,
    /// Its fields.
    pub fields: Vec<Field>,
}

impl Struct {
    /// The struct `name` with `attrs`, of no fields yet.
    #[must_use]
    pub fn new(name: &str, doc: &str, attrs: &[&str]) -> Self {
        Self {
            doc: doc.to_owned(),
            attrs: attrs.iter().map(|attr| (*attr).to_owned()).collect(),
            name: name.to_owned(),
            fields: Vec::new(),
        }
    }

    /// Adds a field.
    pub fn field(&mut self, name: &str, ty: &str, doc: &str, attrs: Vec<String>) -> &mut Self {
        self.fields.push(Field {
            doc: doc.to_owned(),
            attrs,
            name: name.to_owned(),
            ty: ty.to_owned(),
        });
        self
    }

    /// Writes it.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.head(&self.doc, &self.attrs)?;
        if self.fields.is_empty() {
            return f.line(&format!("pub struct {};", self.name));
        }
        f.block(&format!("pub struct {}", self.name), |f| {
            self.fields.iter().try_for_each(|field| {
                f.head(&field.doc, &field.attrs)?;
                f.line(&format!("pub {}: {},", field.name, field.ty))
            })
        })
    }
}

/// `pub enum Name { variants }`.
pub struct Enum {
    /// Its doc comment.
    pub doc: String,
    /// Its attributes.
    pub attrs: Vec<String>,
    /// Its name.
    pub name: String,
    /// Its variants.
    pub variants: Vec<Field>,
}

impl Enum {
    /// The enum `name` with `attrs`, of no variants yet.
    #[must_use]
    pub fn new(name: &str, doc: &str, attrs: &[&str]) -> Self {
        Self {
            doc: doc.to_owned(),
            attrs: attrs.iter().map(|attr| (*attr).to_owned()).collect(),
            name: name.to_owned(),
            variants: Vec::new(),
        }
    }

    /// Adds a variant carrying `ty` (empty: none).
    pub fn variant(&mut self, name: &str, ty: &str, doc: &str, attrs: Vec<String>) -> &mut Self {
        self.variants.push(Field {
            doc: doc.to_owned(),
            attrs,
            name: name.to_owned(),
            ty: ty.to_owned(),
        });
        self
    }

    /// Writes it.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.head(&self.doc, &self.attrs)?;
        f.block(&format!("pub enum {}", self.name), |f| {
            self.variants.iter().try_for_each(|variant| {
                f.head(&variant.doc, &variant.attrs)?;
                if variant.ty.is_empty() {
                    f.line(&format!("{},", variant.name))
                } else {
                    f.line(&format!("{}({}),", variant.name, variant.ty))
                }
            })
        })
    }
}

/// A statement of a function body.
pub enum Body {
    /// One line.
    Line(String),
    /// `head { body }`: an `if let`.
    Block(String, Vec<Body>),
    /// `match on { pattern => value, }`.
    Match(String, Vec<(String, String)>),
}

impl Body {
    /// Writes it.
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Line(line) => f.line(line),
            Self::Block(head, body) => {
                f.block(head, |f| body.iter().try_for_each(|body| body.fmt(f)))
            }
            Self::Match(on, arms) => f.block(&format!("match {on}"), |f| {
                arms.iter()
                    .try_for_each(|(pattern, value)| f.line(&format!("{pattern} => {value},")))
            }),
        }
    }
}

/// `qualifiers fn name(params) -> ret { body }`, the parameters one per
/// line when the signature does not fit.
pub struct Fn {
    /// Its doc comment.
    pub doc: String,
    /// What comes before `fn`: `pub`, `pub async`; empty: none.
    pub qualifiers: String,
    /// Its name, with its generics: `wallet_get<C: Client>`.
    pub name: String,
    /// Its parameters, `&self` first if any, as written.
    pub params: Vec<String>,
    /// What it returns; empty: `()`.
    pub ret: String,
    /// Its statements, the last one its value.
    pub body: Vec<Body>,
}

impl Fn {
    /// The function `name(params) -> ret` of no statements yet.
    #[must_use]
    pub fn new(name: &str, params: &[&str], ret: &str) -> Self {
        let params = params.iter().map(|param| (*param).to_owned()).collect();
        Self {
            doc: String::new(),
            qualifiers: String::new(),
            name: name.to_owned(),
            params,
            ret: ret.to_owned(),
            body: Vec::new(),
        }
    }

    /// Adds a statement.
    pub fn push(&mut self, body: Body) -> &mut Self {
        self.body.push(body);
        self
    }

    /// Adds a line.
    pub fn line(&mut self, line: impl Into<String>) -> &mut Self {
        self.push(Body::Line(line.into()))
    }

    /// Writes it.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.head(&self.doc, &[])?;
        let qualifiers = if self.qualifiers.is_empty() {
            String::new()
        } else {
            format!("{} ", self.qualifiers)
        };
        let ret = if self.ret.is_empty() {
            String::new()
        } else {
            format!(" -> {}", self.ret)
        };
        let style = Style {
            width: WIDTH,
            unit: UNIT,
        };
        let head = fit(
            style,
            f.column(),
            &format!("{qualifiers}fn {}", self.name),
            ("(", ")"),
            &self.params,
            &format!("{ret} {{"),
            false,
        );
        f.line(&head)?;
        f.level += 1;
        self.body.iter().try_for_each(|body| body.fmt(f))?;
        f.level -= 1;
        f.line("}")
    }
}

/// `pub const NAME: Type = Type { field: value, … };`, a field per line, an
/// array value vertical once its items pass 60 columns.
pub struct Const {
    /// Its doc comment.
    pub doc: String,
    /// Its name.
    pub name: String,
    /// Its type, which the literal builds.
    pub ty: String,
    /// Each field's name and value; an array value is its items.
    pub fields: Vec<(String, Value)>,
}

/// A value of a [`Const`]'s field.
pub enum Value {
    /// As written.
    Text(String),
    /// `&[items]`.
    List(Vec<String>),
}

impl Const {
    /// Writes it.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.head(&self.doc, &[])?;
        f.line(&format!(
            "pub const {}: {} = {} {{",
            self.name, self.ty, self.ty
        ))?;
        f.level += 1;
        for (name, value) in &self.fields {
            let line = match value {
                Value::Text(text) => format!("{name}: {text},"),
                Value::List(items) if items.join(", ").len() > LIST => {
                    format!(
                        "{name}: &[\n{}],",
                        items
                            .iter()
                            .map(|item| format!("{UNIT}{item},\n"))
                            .collect::<String>()
                    )
                }
                Value::List(items) => format!("{name}: &[{}],", items.join(", ")),
            };
            f.line(&line)?;
        }
        f.level -= 1;
        f.line("};")
    }
}

/// `pub type Name = Type;`.
pub struct Alias {
    /// Its doc comment.
    pub doc: String,
    /// Its name.
    pub name: String,
    /// The type it names.
    pub ty: String,
}

impl Alias {
    /// Writes it.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.head(&self.doc, &[])?;
        f.line(&format!("pub type {} = {};", self.name, self.ty))
    }
}

/// An item of an [`Impl`].
pub enum ImplItem {
    /// `type Name = Type;`.
    Type(String, String),
    /// A function, a blank line before it unless first.
    Fn(Fn),
}

/// `impl Trait for Type { items }`.
pub struct Impl {
    /// The trait; none: an inherent impl.
    pub of: Option<String>,
    /// The type.
    pub ty: String,
    /// Its items.
    pub items: Vec<ImplItem>,
}

impl Impl {
    /// Writes it.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let head = match &self.of {
            Some(of) => format!("impl {of} for {}", self.ty),
            None => format!("impl {}", self.ty),
        };
        f.block(&head, |f| {
            for (index, item) in self.items.iter().enumerate() {
                match item {
                    ImplItem::Type(name, ty) => f.line(&format!("type {name} = {ty};"))?,
                    ImplItem::Fn(func) => {
                        if index > 0 {
                            f.line("")?;
                        }
                        func.fmt(f)?;
                    }
                }
            }
            Ok(())
        })
    }
}

/// `use` lines in rustfmt's order: `self`, `super`, `crate`, then the rest,
/// segment by segment.
pub struct Use(pub Vec<String>);

impl Use {
    /// Writes it.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let mut paths: Vec<&String> = self.0.iter().collect();
        let key = |path: &str| {
            let rank = ["self", "super", "crate"]
                .iter()
                .position(|first| path.split("::").next() == Some(first))
                .unwrap_or(3);
            (
                rank,
                path.split("::").map(str::to_owned).collect::<Vec<_>>(),
            )
        };
        paths.sort_by_key(|path| key(path));
        paths
            .iter()
            .try_for_each(|path| f.line(&format!("use {path};")))
    }
}

/// A spec type as Rust writes it: `Vec<Wallet>`, `Option<String>`,
/// `BTreeMap<String, i64>`.
pub struct RustType<'a>(pub &'a Type);

impl fmt::Display for RustType<'_> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Type::String => out.write_str("String"),
            Type::Int => out.write_str("i64"),
            Type::Int32 => out.write_str("i32"),
            Type::Float => out.write_str("f64"),
            Type::Bool => out.write_str("bool"),
            Type::Any => out.write_str("serde_json::Value"),
            Type::Named(name) => out.write_str(name),
            Type::Array(item) => write!(out, "Vec<{}>", RustType(item)),
            Type::Map(value) => write!(out, "BTreeMap<String, {}>", RustType(value)),
            Type::Optional(inner) if **inner == Type::Any => write!(out, "{}", RustType(inner)),
            Type::Optional(inner) => write!(out, "Option<{}>", RustType(inner)),
        }
    }
}

/// A spec name as Rust writes it.
pub struct RustName<'a>(pub &'a str);

impl RustName<'_> {
    /// A field's name: snake case, a keyword raw (`r#type`), `_` after one
    /// that cannot be raw (`self_`).
    #[must_use]
    pub fn field(&self) -> String {
        match snake(self.0).as_str() {
            name @ ("self" | "Self" | "super" | "crate") => format!("{name}_"),
            name if KEYWORDS.contains(&name) => format!("r#{name}"),
            name => name.to_owned(),
        }
    }

    /// A local's name, never raw (a `format!` argument): `type_`.
    #[must_use]
    pub fn local(&self) -> String {
        let name = snake(self.0);
        if KEYWORDS.contains(&name.as_str()) {
            format!("{name}_")
        } else {
            name
        }
    }

    /// A type's or variant's name: pascal case, `_` after a keyword (`Self_`).
    #[must_use]
    pub fn pascal(&self) -> String {
        let name = pascal(self.0);
        if KEYWORDS.contains(&name.as_str()) {
            format!("{name}_")
        } else {
            name
        }
    }

    /// As a string literal.
    #[must_use]
    pub fn literal(&self) -> String {
        format!("{:?}", self.0)
    }
}

/// Rust's strict and reserved keywords (edition 2024).
const KEYWORDS: [&str; 52] = [
    "abstract", "as", "async", "await", "become", "box", "break", "const", "continue", "crate",
    "do", "dyn", "else", "enum", "extern", "false", "final", "fn", "for", "gen", "if", "impl",
    "in", "let", "loop", "macro", "match", "mod", "move", "mut", "override", "priv", "pub", "ref",
    "return", "self", "Self", "static", "struct", "super", "trait", "true", "try", "type",
    "typeof", "unsafe", "unsized", "use", "virtual", "where", "while", "yield",
];

/// `Display` through `fmt` with a fresh formatter.
macro_rules! display {
    ($($ty:ty),*) => {$(
        impl fmt::Display for $ty {
            fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
                let mut text = String::new();
                Self::fmt(self, &mut Formatter::new(&mut text))?;
                out.write_str(&text)
            }
        }
    )*};
}

display!(Struct, Enum, Fn, Const, Alias, Impl, Use);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structs_and_enums() {
        let mut pet = Struct::new("Pet", "A pet.", &["derive(Clone)"]);
        pet.field(
            "r#type",
            "Option<String>",
            "",
            vec!["serde(default)".into()],
        );
        assert_eq!(
            pet.to_string(),
            "/// A pet.\n#[derive(Clone)]\npub struct Pet {\n    #[serde(default)]\n    pub r#type: Option<String>,\n}\n"
        );
        assert_eq!(
            Struct::new("Unit", "", &[]).to_string(),
            "pub struct Unit;\n"
        );
        let mut kind = Enum::new("Kind", "", &[]);
        kind.variant("Cat", "", "", Vec::new()).variant(
            "Unknown",
            "String",
            "Any other.",
            Vec::new(),
        );
        assert_eq!(
            kind.to_string(),
            "pub enum Kind {\n    Cat,\n    /// Any other.\n    Unknown(String),\n}\n"
        );
    }

    #[test]
    fn impls_and_consts_break_like_rustfmt() {
        let mut param = Fn::new("param", &["&self", "name: &str"], "Option<String>");
        param.push(Body::Match(
            "name".into(),
            vec![
                ("\"id\"".into(), "Some(self.id.to_string())".into()),
                ("_".into(), "None".into()),
            ],
        ));
        let items = vec![
            ImplItem::Type("Response".into(), "Pet".into()),
            ImplItem::Fn(param),
        ];
        let text = Impl {
            of: Some("Request".into()),
            ty: "PetGetRequest".into(),
            items,
        }
        .to_string();
        let expected = "impl Request for PetGetRequest {\n    type Response = Pet;\n\n    fn param(&self, name: &str) -> Option<String> {\n        match name {\n            \"id\" => Some(self.id.to_string()),\n            _ => None,\n        }\n    }\n}\n";
        assert_eq!(text, expected);
        let templates = vec![
            "\"a.pets.$read\"".into(),
            "\"a.wallets.$read.and.a.much.longer.tail.xyzw\"".into(),
        ];
        let fields = vec![
            ("id".into(), Value::Text("\"pet_get\"".into())),
            ("templates".into(), Value::List(templates)),
        ];
        let text = Const {
            doc: String::new(),
            name: "PET_GET".into(),
            ty: "Operation".into(),
            fields,
        }
        .to_string();
        let expected = "pub const PET_GET: Operation = Operation {\n    id: \"pet_get\",\n    templates: &[\n        \"a.pets.$read\",\n        \"a.wallets.$read.and.a.much.longer.tail.xyzw\",\n    ],\n};\n";
        assert_eq!(text, expected);
        let mut call = Fn::new(
            "pet_get<C: Client>",
            &["client: &C", "request: PetGetRequest"],
            "Result<PetGetResponse, Error<C::Error>>",
        );
        call.qualifiers = "pub async".into();
        call.line("call(client, &request).await");
        let expected = "pub async fn pet_get<C: Client>(\n    client: &C,\n    request: PetGetRequest,\n) -> Result<PetGetResponse, Error<C::Error>> {\n    call(client, &request).await\n}\n";
        assert_eq!(call.to_string(), expected);
        assert_eq!(
            Use(vec![
                "serde::Serialize".into(),
                "crate::client::Request".into()
            ])
            .to_string(),
            "use crate::client::Request;\nuse serde::Serialize;\n"
        );
    }
}
