//! Go, as gofmt writes it, in two files of one package: `models.go` (structs
//! with `json` tags, enums as string types with constants, unions as one
//! pointer per variant) and `operations.go`. An operation is a descriptor
//! (`var walletGet = Operation{…}`), a request struct (its path and query
//! parameters as fields, its body embedded) whose methods are what the
//! hand-written `Request` interface asks, a response type, and the call.
//! What they use (`Operation`, `Request`, `Client`, `Stream`, `text`) is the
//! package's hand-written code. The syntax is [`expr`]'s; this maps the
//! [`Spec`] onto it.

mod expr;

use std::fmt::Write;
use std::path::PathBuf;

use crate::Files;
use crate::out::quote;
use crate::spec::{Model, Operation, Spec, Type};
use expr::{Block, Body, Enum, File, Func, GoName, GoType, Item, Struct, Switch, Var};

/// The Go client.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Go {
    /// The package name.
    pub package: String,
}

impl Go {
    /// `models.go` and `operations.go`.
    #[must_use]
    pub fn emit(&self, spec: &Spec) -> Files {
        let head = |imports: Vec<String>| {
            File {
                marker: spec.marker(),
                package: self.package.clone(),
                imports,
                items: Vec::new(),
            }
            .to_string()
        };
        let mut models = head(Vec::new());
        spec.models
            .iter()
            .for_each(|model| self.model(&mut models, model));
        let mut imports = vec!["context".to_owned()];
        if spec.operations.iter().any(|op| op.body.is_some()) {
            imports.push("encoding/json".to_owned());
        }
        imports.push("net/url".to_owned());
        let mut operations = head(imports);
        spec.operations
            .iter()
            .for_each(|op| self.operation(&mut operations, op));
        Files::from([
            (PathBuf::from("models.go"), models),
            (PathBuf::from("operations.go"), operations),
        ])
    }

    /// One model: a struct, a string type and its constants, a union with
    /// its JSON methods.
    fn model(&self, out: &mut String, model: &Model) {
        let items = match model {
            Model::Struct { name, doc, fields } => {
                let mut item = Struct::new(name, &GoName(name).is(doc));
                for field in fields {
                    let tag = format!(
                        "json:{}",
                        quote(&format!(
                            "{}{}",
                            field.name,
                            if field.required { "" } else { ",omitempty" }
                        ))
                    );
                    item.field(
                        &GoName(&field.name).exported(),
                        &GoType(&field.ty).field(field.required),
                        &tag,
                        &field.doc,
                    );
                }
                vec![Item::Struct(item)]
            }
            Model::Enum { name, doc, values } => {
                let values = values
                    .iter()
                    .map(|value| (format!("{name}{}", GoName(value).exported()), value.clone()))
                    .collect();
                vec![Item::Enum(Enum {
                    doc: GoName(name).is(doc),
                    name: name.clone(),
                    values,
                })]
            }
            Model::Union {
                name,
                doc,
                variants,
            } => {
                let doc = if doc.is_empty() {
                    format!("{name} is one of its fields, the one set.")
                } else {
                    GoName(name).is(doc)
                };
                let mut item = Struct::new(name, &doc);
                let mut marshal = Func::new(
                    "MarshalJSON",
                    "",
                    "([]byte, error)",
                    "MarshalJSON writes the variant that is set.",
                )
                .receiver(&format!("u {name}"));
                let doc =
                    "UnmarshalJSON reads the first variant data decodes as, no field left over.";
                let mut unmarshal = Func::new("UnmarshalJSON", "data []byte", "error", doc)
                    .receiver(&format!("u *{name}"));
                unmarshal.line(format!("*u = {name}{{}}"));
                for of in variants {
                    let field = of.label();
                    item.field(&field, &format!("*{}", GoType(of)), "", "");
                    let mut set = Block::new(&format!("if u.{field} != nil"));
                    set.line(format!("return encodeJSON(u.{field})"));
                    marshal.push(Body::Block(set));
                    let mut fits = Block::new(&format!(
                        "if v := new({}); decodeStrict(data, v)",
                        GoType(of)
                    ));
                    fits.line(format!("u.{field} = v")).line("return nil");
                    unmarshal.push(Body::Block(fits));
                }
                marshal.line("return []byte(\"null\"), nil");
                unmarshal.line(format!("return noVariant({})", quote(name)));
                vec![
                    Item::Struct(item),
                    Item::Func(marshal),
                    Item::Func(unmarshal),
                ]
            }
        };
        write_items(out, items);
    }

    /// One operation: its descriptor, its request struct and methods, its
    /// response type, its event type, its call.
    fn operation(&self, out: &mut String, op: &Operation) {
        let name = GoName(&op.id).exported();
        let descriptor = GoName(&op.id).unexported();
        let route = format!("{} {}", op.method.as_str(), op.path);
        let mut fields = vec![
            ("ID".to_owned(), quote(&op.id)),
            ("Method".to_owned(), quote(op.method.as_str())),
            ("Path".to_owned(), quote(&op.path)),
        ];
        if !op.scopes.is_empty() {
            fields.push((
                "Templates".to_owned(),
                format!(
                    "[]string{{{}}}",
                    op.scopes
                        .iter()
                        .map(|scope| quote(scope))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ));
        }
        let mut items = vec![Item::Var(Var {
            doc: format!("{descriptor} is {route}."),
            name: descriptor.clone(),
            ty: "Operation".into(),
            fields,
        })];

        let request = format!("{name}Request");
        let mut item = Struct::new(
            &request,
            &format!("{request} is {name}'s input: its path and query parameters, and its body."),
        );
        for param in &op.params {
            item.field(
                &GoName(&param.name).exported(),
                &GoType(&param.ty).field(param.required),
                "json:\"-\"",
                &param.doc,
            );
        }
        let body = match &op.body {
            Some(Type::Named(model)) => {
                item.field("", model, "", "");
                Some(format!("r.{model}"))
            }
            Some(other) => {
                item.field("Body", &GoType(other).to_string(), "json:\"-\"", "");
                Some("r.Body".to_owned())
            }
            None => None,
        };
        items.push(Item::Struct(item));
        let method = |name: &str, results: &str, doc: String| {
            Func::new(name, "", results, &doc).receiver(&format!("r {request}"))
        };
        let mut operation = method(
            "Operation",
            "Operation",
            format!("Operation is {descriptor}."),
        );
        operation.line(format!("return {descriptor}"));
        let mut params = method(
            "Params",
            "map[string]string",
            "Params are the path parameters by name.".into(),
        );
        let path: Vec<String> = op
            .params
            .iter()
            .filter(|param| param.path)
            .map(|param| {
                format!(
                    "{}: {}",
                    quote(&param.name),
                    text(&param.ty, &format!("r.{}", GoName(&param.name).exported()))
                )
            })
            .collect();
        params.line(if path.is_empty() {
            "return nil".to_owned()
        } else {
            format!("return map[string]string{{{}}}", path.join(", "))
        });
        let mut query = method("Query", "url.Values", "Query is the query string.".into());
        let given: Vec<_> = op.params.iter().filter(|param| !param.path).collect();
        if given.is_empty() {
            query.line("return nil");
        } else {
            query.line("query := url.Values{}");
            for param in given {
                let field = format!("r.{}", GoName(&param.name).exported());
                if param.required {
                    query.line(format!(
                        "query.Set({}, {})",
                        quote(&param.name),
                        text(&param.ty, &field)
                    ));
                } else {
                    let mut set = Block::new(&format!("if {field} != nil"));
                    set.line(format!(
                        "query.Set({}, {})",
                        quote(&param.name),
                        text(&param.ty, &format!("*{field}"))
                    ));
                    query.push(Body::Block(set));
                }
            }
            query.line("return query");
        }
        let mut encode = method(
            "Body",
            "([]byte, error)",
            "Body is the JSON body; nil when none.".into(),
        );
        encode.line(body.map_or_else(
            || "return nil, nil".to_owned(),
            |body| format!("return json.Marshal({body})"),
        ));
        items.extend([operation, params, query, encode].map(Item::Func));

        let response = format!("{name}Response");
        let doc = format!("{response} is {name}'s output.");
        let (caller, results, call) = match &op.events {
            None => {
                items.push(Item::Struct(match &op.response {
                    Some(Type::Named(model)) => {
                        let mut item = Struct::new(&response, &doc);
                        item.field("", model, "", "");
                        item
                    }
                    Some(other) => {
                        let mut item = Struct::new(&response, &doc);
                        item.field("Value", &GoType(other).to_string(), "", "");
                        item
                    }
                    None => Struct::new(&response, &doc),
                }));
                let call = vec![
                    format!("var response {response}"),
                    "err := client.Do(ctx, request, &response)".to_owned(),
                    "return response, err".to_owned(),
                ];
                ("client Client", format!("({response}, error)"), call)
            }
            Some(events) => {
                let event = if events.is_empty() {
                    "RawEvent".to_owned()
                } else {
                    format!("{name}Event")
                };
                if !events.is_empty() {
                    items.extend(events_type(&name, events));
                }
                (
                    "streamer Streamer",
                    format!("(*Stream[{event}], error)"),
                    vec![format!("return Open[{event}](ctx, streamer, request)")],
                )
            }
        };
        let first = if op.summary.is_empty() {
            format!("{name} calls {route}.")
        } else {
            GoName(&name).does(&op.summary)
        };
        let doc = if op.doc.is_empty() {
            first
        } else {
            format!("{first}\n\n{}", op.doc)
        };
        let mut func = Func::new(
            &name,
            &format!("ctx context.Context, {caller}, request {request}"),
            &results,
            &doc,
        );
        for line in call {
            func.line(line);
        }
        items.push(Item::Func(func));
        write_items(out, items);
    }
}

/// `value`, of type `of`, as a string.
fn text(of: &Type, value: &str) -> String {
    if *of == Type::String {
        value.to_owned()
    } else {
        format!("text({value})")
    }
}

/// Each of `items`, a blank line before it.
fn write_items(out: &mut String, items: Vec<Item>) {
    for item in items {
        let text = match item {
            Item::Struct(item) => item.to_string(),
            Item::Enum(item) => item.to_string(),
            Item::Var(item) => item.to_string(),
            Item::Func(item) => item.to_string(),
        };
        let _ = write!(out, "\n{text}");
    }
}

/// A stream's event type (its name, its id, one pointer per event) and the
/// method decoding one.
fn events_type(name: &str, events: &[(String, Type)]) -> Vec<Item> {
    let event = format!("{name}Event");
    let mut item = Struct::new(
        &event,
        &format!("{event} is one event of {name}: Event names it, its field is set."),
    );
    item.field("Event", "string", "", "")
        .field("ID", "string", "", "");
    let mut cases = Vec::new();
    for (wire, data) in events {
        let field = GoName(wire).exported();
        item.field(&field, &format!("*{}", GoType(data)), "", "");
        cases.push((
            quote(wire),
            vec![
                format!("e.{field} = new({})", GoType(data)),
                format!("return true, decodeData(raw, e.{field})"),
            ],
        ));
    }
    let mut decode =
        Func::new("decode", "raw RawEvent", "(bool, error)", "").receiver(&format!("e *{event}"));
    decode
        .line("e.Event, e.ID = raw.Event, raw.ID")
        .push(Body::Switch(Switch {
            on: "raw.Event".into(),
            cases,
        }))
        .line("return false, nil");
    vec![Item::Struct(item), Item::Func(decode)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_docs() {
        assert_eq!(GoName("external_id").exported(), "ExternalID");
        assert_eq!(
            GoName("network_endpoints_create").unexported(),
            "networkEndpointsCreate"
        );
        assert_eq!(GoName("wallet_get").unexported(), "walletGet");
        assert_eq!(
            GoName("WalletGet").does("Read one wallet"),
            "WalletGet reads one wallet."
        );
        assert_eq!(GoName("Pet").is("The pet"), "Pet is the pet");
        assert_eq!(GoType(&Type::Int).field(false), "*int64");
    }
}
