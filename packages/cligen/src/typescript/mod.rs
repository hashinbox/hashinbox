//! TypeScript, as prettier writes it, in two files: `models.ts`
//! (interfaces and unions of the JSON as sent, wire field names kept) and
//! `operations.ts`. An operation is a descriptor (`walletGetOperation`), a
//! request interface (its path and query parameters, and `body`), a response
//! type and the call, which hands all three to the hand-written `Client`
//! (`./client`). The syntax is [`expr`]'s; this maps the [`Spec`] onto it.

mod expr;

use std::collections::BTreeSet;
use std::fmt::Write;
use std::path::PathBuf;

use crate::Files;
use crate::out::quote;
use crate::spec::{Model, Operation, Spec, Type};
use expr::{Const, Function, Import, Interface, TsName, TsType, TypeAlias};

/// The TypeScript client.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Typescript;

impl Typescript {
    /// `models.ts` and `operations.ts`.
    #[must_use]
    pub fn emit(&self, spec: &Spec) -> Files {
        let marker = format!("// {}\n", spec.marker());
        let mut models = marker.clone();
        spec.models
            .iter()
            .for_each(|model| self.model(&mut models, model));
        let mut operations = marker + &self.imports(spec);
        spec.operations
            .iter()
            .for_each(|op| self.operation(&mut operations, op));
        Files::from([
            (PathBuf::from("models.ts"), models),
            (PathBuf::from("operations.ts"), operations),
        ])
    }

    /// The imports `operations.ts` needs.
    fn imports(&self, spec: &Spec) -> String {
        let mut client: BTreeSet<&str> = BTreeSet::from(["Operation"]);
        let mut models = BTreeSet::new();
        for op in &spec.operations {
            match &op.events {
                Some(events) if events.is_empty() => client.extend(["RawEvent", "Streamer"]),
                Some(_) => {
                    client.insert("Streamer");
                }
                None => {
                    client.insert("Client");
                }
            }
            let types = op
                .params
                .iter()
                .map(|param| &param.ty)
                .chain(&op.body)
                .chain(&op.response);
            types
                .chain(op.events.iter().flatten().map(|(_, ty)| ty))
                .for_each(|ty| named(ty, &mut models));
        }
        let mut lines = format!(
            "\n{}",
            Import {
                names: client.into_iter().map(str::to_owned).collect(),
                from: "./client".into(),
                types: true
            }
        );
        if !models.is_empty() {
            lines.push_str(
                &Import {
                    names: models.into_iter().collect(),
                    from: "./models".into(),
                    types: true,
                }
                .to_string(),
            );
        }
        lines
    }

    /// One model: an interface, or a union of strings or of types.
    fn model(&self, out: &mut String, model: &Model) {
        let item = match model {
            Model::Struct { name, doc, fields } => {
                let mut item = Interface::new(name, doc);
                for field in fields {
                    item.prop(
                        &field.name,
                        &TsType(&field.ty).to_string(),
                        !field.required,
                        &field.doc,
                    );
                }
                item.to_string()
            }
            Model::Enum { name, doc, values } => TypeAlias {
                doc: doc.clone(),
                name: name.clone(),
                members: values.iter().map(|value| quote(value)).collect(),
            }
            .to_string(),
            Model::Union {
                name,
                doc,
                variants,
            } => TypeAlias {
                doc: doc.clone(),
                name: name.clone(),
                members: variants.iter().map(|of| TsType(of).to_string()).collect(),
            }
            .to_string(),
        };
        let _ = write!(out, "\n{item}");
    }

    /// One operation: its descriptor, request, response, event union and
    /// call.
    fn operation(&self, out: &mut String, op: &Operation) {
        let name = TsName(&op.id).function();
        let named = TsName(&op.id).pascal();
        let route = format!("`{} {}`", op.method.as_str(), op.path);
        let list = |items: Vec<String>| format!("[{}]", items.join(", "));
        let mut entries = vec![
            ("id".to_owned(), quote(&op.id)),
            ("method".to_owned(), quote(op.method.as_str())),
            ("path".to_owned(), quote(&op.path)),
        ];
        let query: Vec<String> = op
            .params
            .iter()
            .filter(|param| !param.path)
            .map(|param| quote(&param.name))
            .collect();
        if !query.is_empty() {
            entries.push(("query".to_owned(), list(query)));
        }
        entries.push((
            "templates".to_owned(),
            list(op.scopes.iter().map(|scope| quote(scope)).collect()),
        ));
        if let Some(events) = op.events.as_ref().filter(|events| !events.is_empty()) {
            entries.push((
                "events".to_owned(),
                list(events.iter().map(|(wire, _)| quote(wire)).collect()),
            ));
        }
        let descriptor = Const {
            doc: format!("{route}."),
            name: format!("{name}Operation"),
            ty: "Operation".into(),
            entries,
        };
        let _ = write!(out, "\n{descriptor}");

        let request = format!("{named}Request");
        let mut input = Interface::new(&request, &format!("The input of `{name}`."));
        for param in &op.params {
            input.prop(
                &param.name,
                &TsType(&param.ty).to_string(),
                !param.required,
                &param.doc,
            );
        }
        match &op.body {
            Some(Type::Optional(body)) => {
                input.prop("body", &TsType(body).to_string(), true, "");
            }
            Some(body) => {
                input.prop("body", &TsType(body).to_string(), false, "");
            }
            None => {}
        }
        let defaulted = input.props.iter().all(|prop| prop.optional);
        let _ = if input.props.is_empty() {
            write!(
                out,
                "\n{}",
                TypeAlias {
                    doc: input.doc.clone(),
                    name: request.clone(),
                    members: vec!["Record<string, never>".into()]
                }
            )
        } else {
            write!(out, "\n{input}")
        };

        let response = format!("{named}Response");
        let (caller, kind, ret, call) = match &op.events {
            None => {
                let of = op
                    .response
                    .as_ref()
                    .map_or_else(|| "void".to_owned(), |of| TsType(of).to_string());
                let _ = write!(
                    out,
                    "\n{}",
                    TypeAlias {
                        doc: format!("The output of `{name}`."),
                        name: response.clone(),
                        members: vec![of]
                    }
                );
                (
                    "client",
                    "Client",
                    format!("Promise<{response}>"),
                    format!("client.send<{response}>"),
                )
            }
            Some(events) => {
                let event = if events.is_empty() {
                    "RawEvent".to_owned()
                } else {
                    let event = format!("{named}Event");
                    let members = events
                        .iter()
                        .map(|(wire, data)| {
                            format!(
                                "{{ event: {}; data: {}; id?: string }}",
                                quote(wire),
                                TsType(data)
                            )
                        })
                        .collect();
                    let _ = write!(
                        out,
                        "\n{}",
                        TypeAlias {
                            doc: format!("An event of `{name}`."),
                            name: event.clone(),
                            members
                        }
                    );
                    event
                };
                (
                    "streamer",
                    "Streamer",
                    format!("AsyncIterable<{event}>"),
                    format!("streamer.open<{event}>"),
                )
            }
        };
        let default = if defaulted { " = {}" } else { "" };
        let params = vec![
            format!("{caller}: {kind}"),
            format!("request: {request}{default}"),
            "signal?: AbortSignal".to_owned(),
        ];
        let args = vec![
            format!("{name}Operation"),
            "request".to_owned(),
            "signal".to_owned(),
        ];
        let first = if op.summary.is_empty() {
            route
        } else {
            format!("{}. {route}", op.summary)
        };
        let doc = if op.doc.is_empty() {
            first
        } else {
            format!("{first}\n\n{}", op.doc)
        };
        let _ = write!(
            out,
            "\n{}",
            Function {
                doc,
                name,
                params,
                ret,
                callee: call,
                args
            }
        );
    }
}

/// Each model `of` names, into `names`.
fn named(of: &Type, names: &mut BTreeSet<String>) {
    match of {
        Type::Named(name) => {
            names.insert(name.clone());
        }
        Type::Array(inner) | Type::Map(inner) | Type::Optional(inner) => named(inner, names),
        _ => {}
    }
}
