//! Rust, as rustfmt writes it, in two modules: `models` (serde types; an
//! enum keeps a value it does not know) and `operations`. An operation is a
//! descriptor (`pub const WALLET_GET: Operation`), a request struct (its
//! path and query parameters, and `body`) implementing the hand-written
//! `Request` trait, a response type, and the call (`wallet_get(&client,
//! request)`). What they use (`Operation`, `Request`, `Client`, `call`, …)
//! is the crate's hand-written `client` module. The syntax is [`expr`]'s;
//! this maps the [`Spec`] onto it.

mod expr;

use std::collections::BTreeSet;
use std::fmt::Write;
use std::path::PathBuf;

use crate::Files;
use crate::name::snake;
use crate::spec::{Model, Operation, Spec, Type};
use expr::{Alias, Body, Const, Enum, Fn, Impl, ImplItem, RustName, RustType, Struct, Use, Value};

/// A model's derives.
const DERIVE: &str = "derive(Clone, Debug, PartialEq, Serialize, Deserialize)";

/// The Rust client.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rust;

/// rustfmt's widest tuple or array on one line (`fn_call_width`,
/// `array_width`).
const WIDTH: usize = 60;

/// `DEPENDS`: the document's `x-hashinbox-depends`, each entry as
/// rustfmt lays it out (on one line, else its tuple, then its array, one
/// item per line).
fn depends(out: &mut String, spec: &Spec) {
    let _ = write!(
        out,
        "\n/// What holding a scope needs beyond its subtree (`x-hashinbox-depends`):\n\
         /// source → targets.\npub const DEPENDS: &[(&str, &[&str])] = &["
    );
    if spec.depends.is_empty() {
        out.push_str("];\n");
        return;
    }
    out.push('\n');
    for (source, targets) in &spec.depends {
        let source = RustName(source).literal();
        let targets: Vec<String> = targets
            .iter()
            .map(|target| RustName(target).literal())
            .collect();
        let array = format!("&[{}]", targets.join(", "));
        let tuple = format!("({source}, {array})");
        if tuple.len() <= WIDTH {
            let _ = writeln!(out, "    {tuple},");
        } else if array.len() <= WIDTH {
            let _ = writeln!(out, "    (\n        {source},\n        {array},\n    ),");
        } else {
            let items: String = targets
                .iter()
                .map(|target| format!("            {target},\n"))
                .collect();
            let _ = writeln!(
                out,
                "    (\n        {source},\n        &[\n{items}        ],\n    ),"
            );
        }
    }
    out.push_str("];\n");
}

impl Rust {
    /// `models.rs` and `operations.rs`, modules of a crate whose `client`
    /// module is hand-written.
    #[must_use]
    pub fn emit(&self, spec: &Spec) -> Files {
        let marker = format!("// {}\n", spec.marker());
        let mut uses = BTreeSet::from([
            "serde::Deserialize".to_owned(),
            "serde::Serialize".to_owned(),
        ]);
        let mut used = BTreeSet::new();
        for model in &spec.models {
            match model {
                Model::Enum { .. } => {
                    uses.insert("std::fmt".to_owned());
                }
                Model::Struct { fields, .. } => {
                    fields.iter().for_each(|field| paths(&field.ty, &mut uses))
                }
                Model::Union { variants, .. } => {
                    variants.iter().for_each(|of| paths(of, &mut uses))
                }
            }
        }
        uses.retain(|path| !path.starts_with("crate::models::"));
        let mut models = format!(
            "{marker}//! Every model of the API.\n\n{}",
            Use(uses.into_iter().collect())
        );
        spec.models
            .iter()
            .for_each(|model| self.model(&mut models, model));
        for op in &spec.operations {
            let client = ["Client", "Error", "Method", "Operation", "Request"]
                .into_iter()
                .chain(match &op.events {
                    Some(events) if events.is_empty() => {
                        vec!["Events", "RawEvent", "Streamer", "open"]
                    }
                    Some(_) => vec!["Event", "Events", "RawEvent", "Streamer", "data", "open"],
                    None => vec!["call"],
                });
            used.extend(client.map(|name| format!("crate::client::{name}")));
            if op.body.is_some() {
                used.insert("crate::client::json".to_owned());
            }
            let types = op
                .params
                .iter()
                .map(|param| &param.ty)
                .chain(&op.body)
                .chain(&op.response);
            types
                .chain(op.events.iter().flatten().map(|(_, ty)| ty))
                .for_each(|ty| paths(ty, &mut used));
        }
        let mut operations = format!(
            "{marker}//! Every operation of the API.\n\n{}",
            Use(used.into_iter().collect())
        );
        depends(&mut operations, spec);
        spec.operations
            .iter()
            .for_each(|op| self.operation(&mut operations, op));
        Files::from([
            (PathBuf::from("models.rs"), models),
            (PathBuf::from("operations.rs"), operations),
        ])
    }

    /// One model: a struct, an enum with its `as_str` and `Display`, an
    /// untagged enum.
    fn model(&self, out: &mut String, model: &Model) {
        let items = match model {
            Model::Struct { name, doc, fields } => {
                let mut item = Struct::new(name, doc, &[DERIVE]);
                for field in fields {
                    let mut of = RustType(&field.ty).to_string();
                    if field.ty == Type::Named(name.clone())
                        || field.ty == Type::Optional(Box::new(Type::Named(name.clone())))
                    {
                        of = of.replace(name.as_str(), &format!("Box<{name}>"));
                    }
                    let mut attrs = Vec::new();
                    if snake(&field.name) != field.name {
                        attrs.push(format!(
                            "serde(rename = {})",
                            RustName(&field.name).literal()
                        ));
                    }
                    if !field.required {
                        attrs.push(
                            "serde(default, skip_serializing_if = \"Option::is_none\")".to_owned(),
                        );
                        if !of.starts_with("Option<") {
                            of = format!("Option<{of}>");
                        }
                    }
                    item.field(&RustName(&field.name).field(), &of, &field.doc, attrs);
                }
                vec![item.to_string()]
            }
            Model::Enum { name, doc, values } => {
                let variants: Vec<String> = values
                    .iter()
                    .map(|value| RustName(value).pascal())
                    .collect();
                let other = if variants.iter().any(|variant| variant == "Unknown") {
                    "Other"
                } else {
                    "Unknown"
                };
                let mut item = Enum::new(
                    name,
                    doc,
                    &["derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)"],
                );
                for (variant, value) in variants.iter().zip(values) {
                    item.variant(
                        variant,
                        "",
                        "",
                        vec![format!("serde(rename = {})", RustName(value).literal())],
                    );
                }
                item.variant(
                    other,
                    "String",
                    "A value this client does not know.",
                    vec!["serde(untagged)".into()],
                );
                let mut arms: Vec<(String, String)> = variants
                    .iter()
                    .zip(values)
                    .map(|(variant, value)| (format!("Self::{variant}"), RustName(value).literal()))
                    .collect();
                arms.push((format!("Self::{other}(value)"), "value".into()));
                let mut as_str = Fn::new("as_str", &["&self"], "&str");
                as_str.doc = "Its value on the wire.".into();
                as_str.qualifiers = "pub".into();
                as_str.push(Body::Match("self".into(), arms));
                let mut display = Fn::new(
                    "fmt",
                    &["&self", "out: &mut fmt::Formatter<'_>"],
                    "fmt::Result",
                );
                display.line("out.write_str(self.as_str())");
                vec![
                    item.to_string(),
                    Impl {
                        of: None,
                        ty: name.clone(),
                        items: vec![ImplItem::Fn(as_str)],
                    }
                    .to_string(),
                    Impl {
                        of: Some("fmt::Display".into()),
                        ty: name.clone(),
                        items: vec![ImplItem::Fn(display)],
                    }
                    .to_string(),
                ]
            }
            Model::Union {
                name,
                doc,
                variants,
            } => {
                let mut item = Enum::new(name, doc, &[DERIVE, "serde(untagged)"]);
                for of in variants {
                    item.variant(&of.label(), &RustType(of).to_string(), "", Vec::new());
                }
                vec![item.to_string()]
            }
        };
        items.iter().for_each(|item| {
            let _ = write!(out, "\n{item}");
        });
    }

    /// One operation: its descriptor, request and its `Request` impl,
    /// response, event enum, call.
    fn operation(&self, out: &mut String, op: &Operation) {
        let named = RustName(&op.id).pascal();
        let call = RustName(&op.id).field();
        let constant = snake(&op.id).to_uppercase();
        let route = format!("`{} {}`", op.method.as_str(), op.path);
        let fields = vec![
            ("id".to_owned(), Value::Text(RustName(&op.id).literal())),
            (
                "method".to_owned(),
                Value::Text(format!("Method::{}", RustName(op.method.as_str()).pascal())),
            ),
            ("path".to_owned(), Value::Text(RustName(&op.path).literal())),
            (
                "templates".to_owned(),
                Value::List(
                    op.scopes
                        .iter()
                        .map(|scope| RustName(scope).literal())
                        .collect(),
                ),
            ),
        ];
        let mut items = vec![
            Const {
                doc: format!("{route}."),
                name: constant.clone(),
                ty: "Operation".into(),
                fields,
            }
            .to_string(),
        ];

        let request = format!("{named}Request");
        let response = format!("{named}Response");
        let optional = op.params.iter().all(|param| !param.required)
            && op
                .body
                .as_ref()
                .is_none_or(|body| matches!(body, Type::Optional(_)));
        let derive = if optional {
            "derive(Clone, Debug, Default)"
        } else {
            "derive(Clone, Debug)"
        };
        let mut input = Struct::new(&request, &format!("The input of [`{call}`]."), &[derive]);
        for param in &op.params {
            let of = if param.required {
                RustType(&param.ty).to_string()
            } else {
                format!("Option<{}>", RustType(&param.ty))
            };
            input.field(&RustName(&param.name).field(), &of, &param.doc, Vec::new());
        }
        if let Some(body) = &op.body {
            input.field("body", &RustType(body).to_string(), "", Vec::new());
        }
        items.push(input.to_string());

        let output = match &op.events {
            None => {
                let of = op
                    .response
                    .as_ref()
                    .map_or_else(|| "()".to_owned(), |of| RustType(of).to_string());
                items.push(
                    Alias {
                        doc: format!("The output of [`{call}`]."),
                        name: response.clone(),
                        ty: of,
                    }
                    .to_string(),
                );
                response.clone()
            }
            Some(events) if events.is_empty() => "RawEvent".to_owned(),
            Some(events) => {
                let event = format!("{named}Event");
                let mut item = Enum::new(
                    &event,
                    &format!("An event of [`{call}`]."),
                    &["derive(Clone, Debug, PartialEq)"],
                );
                let mut arms = Vec::new();
                for (wire, data) in events {
                    let variant = RustName(wire).pascal();
                    item.variant(
                        &variant,
                        &RustType(data).to_string(),
                        &format!("`{wire}`."),
                        Vec::new(),
                    );
                    arms.push((
                        RustName(wire).literal(),
                        format!("Ok(Some(Self::{variant}(data(raw)?)))"),
                    ));
                }
                arms.push(("_".into(), "Ok(None)".into()));
                let mut decode = Fn::new(
                    "decode",
                    &["raw: &RawEvent"],
                    "Result<Option<Self>, serde_json::Error>",
                );
                decode.push(Body::Match("raw.event.as_str()".into(), arms));
                items.push(item.to_string());
                items.push(
                    Impl {
                        of: Some("Event".into()),
                        ty: event.clone(),
                        items: vec![ImplItem::Fn(decode)],
                    }
                    .to_string(),
                );
                event
            }
        };

        let mut members = vec![ImplItem::Type("Response".into(), output.clone())];
        let mut operation = Fn::new("operation", &["&self"], "&'static Operation");
        operation.line(format!("&{constant}"));
        members.push(ImplItem::Fn(operation));
        let path: Vec<_> = op.params.iter().filter(|param| param.path).collect();
        if !path.is_empty() {
            let mut arms: Vec<(String, String)> = path
                .iter()
                .map(|param| {
                    (
                        RustName(&param.name).literal(),
                        format!("Some(self.{}.to_string())", RustName(&param.name).field()),
                    )
                })
                .collect();
            arms.push(("_".into(), "None".into()));
            let mut param = Fn::new("param", &["&self", "name: &str"], "Option<String>");
            param.push(Body::Match("name".into(), arms));
            members.push(ImplItem::Fn(param));
        }
        let given: Vec<_> = op.params.iter().filter(|param| !param.path).collect();
        if !given.is_empty() {
            let mut query = Fn::new(
                "query",
                &["&self", "query: &mut Vec<(&'static str, String)>"],
                "",
            );
            for param in given {
                let field = format!("self.{}", RustName(&param.name).field());
                let push = |value: &str| {
                    format!(
                        "query.push(({}, {value}.to_string()));",
                        RustName(&param.name).literal()
                    )
                };
                if param.required {
                    query.line(push(&field));
                } else {
                    let local = RustName(&param.name).local();
                    query.push(Body::Block(
                        format!("if let Some({local}) = &{field}"),
                        vec![Body::Line(push(&local))],
                    ));
                }
            }
            members.push(ImplItem::Fn(query));
        }
        if let Some(body) = &op.body {
            let mut encode = Fn::new("body", &["&self"], "Option<String>");
            encode.line(if matches!(body, Type::Optional(_)) {
                "self.body.as_ref().map(json)"
            } else {
                "Some(json(&self.body))"
            });
            members.push(ImplItem::Fn(encode));
        }
        items.push(
            Impl {
                of: Some("Request".into()),
                ty: request.clone(),
                items: members,
            }
            .to_string(),
        );

        let first = if op.summary.is_empty() {
            format!("{route}.")
        } else {
            format!("{}: {route}.", op.summary)
        };
        let doc = if op.doc.is_empty() {
            first
        } else {
            format!("{first}\n\n{}", op.doc)
        };
        let mut func = match &op.events {
            None => {
                let mut func = Fn::new(
                    &format!("{call}<C: Client>"),
                    &["client: &C", &format!("request: {request}")],
                    &format!("Result<{response}, Error<C::Error>>"),
                );
                func.line("call(client, &request).await");
                func
            }
            Some(_) => {
                let ret = format!("Result<Events<'_, {output}, S>, Error<S::Error>>");
                let mut func = Fn::new(
                    &format!("{call}<S: Streamer>"),
                    &["streamer: &S", &format!("request: {request}")],
                    &ret,
                );
                func.line("open(streamer, &request).await");
                func
            }
        };
        func.doc = doc;
        func.qualifiers = "pub async".into();
        items.push(func.to_string());
        items.iter().for_each(|item| {
            let _ = write!(out, "\n{item}");
        });
    }
}

/// The paths `of` needs used: its models, `BTreeMap`.
fn paths(of: &Type, uses: &mut BTreeSet<String>) {
    match of {
        Type::Named(name) => {
            uses.insert(format!("crate::models::{name}"));
        }
        Type::Map(inner) => {
            uses.insert("std::collections::BTreeMap".to_owned());
            paths(inner, uses);
        }
        Type::Array(inner) | Type::Optional(inner) => paths(inner, uses),
        _ => {}
    }
}
