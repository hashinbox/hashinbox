//! Python 3.11+, as ruff format (black) writes it, in two modules of one
//! package: `models` (frozen dataclasses, each with `from_json` and
//! `to_json`; enums as `StrEnum`; unions as `X | Y` aliases with a
//! `…_from_json` trying each variant) and `operations`. An operation is a
//! descriptor (`WALLET_GET = Operation(…)`), a request dataclass (its path
//! and query parameters, and `body`) implementing the hand-written
//! `Request` protocol, a response alias, and the call (`await
//! wallet_get(client, request)`). What they use (`Operation`, `Request`,
//! `Client`, `encode`, …) is the package's hand-written `client` module. The
//! syntax is [`expr`]'s; this maps the [`Spec`] onto it.

mod expr;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::Files;
use crate::out::quote;
use crate::spec::{Model, Operation, Spec, Type};
use expr::{Body, Class, Def, Enum, Import, Item, Module, PyName, PyType, TypeAlias, Value};

/// Names a model's fields cannot take: its methods.
const MODEL: [&str; 2] = ["from_json", "to_json"];

/// Names a request's fields cannot take: its methods and `body`.
const REQUEST: [&str; 5] = ["body", "content", "operation", "params", "query"];

/// The Python client.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Python;

impl Python {
    /// `models.py` and `operations.py`, modules of a package whose `client`
    /// module is hand-written.
    #[must_use]
    pub fn emit(&self, spec: &Spec) -> Files {
        let models: BTreeMap<&str, &Model> = spec
            .models
            .iter()
            .map(|model| (model.name(), model))
            .collect();
        let mut json = Json {
            models: &models,
            uses: BTreeSet::new(),
            local: true,
        };
        let mut items = Vec::new();
        spec.models
            .iter()
            .for_each(|model| self.model(&mut items, model, &mut json));
        let models_py = module(spec, "Every model of the API.", json.uses, items);

        let mut json = Json {
            models: &models,
            uses: BTreeSet::new(),
            local: false,
        };
        let depends = spec.depends.iter().map(|(source, targets)| {
            let targets = targets.iter().map(|target| Value::text(quote(target)));
            Value::tuple(targets.collect()).prefix(&format!("{}: ", quote(source)))
        });
        let mut items = vec![Item::Assign(
            "What holding a scope needs beyond its subtree (`x-hashinbox-depends`): source → targets."
                .into(),
            Value::dict(depends.collect()).prefix("DEPENDS: dict[str, tuple[str, ...]] = "),
        )];
        spec.operations
            .iter()
            .for_each(|op| self.operation(&mut items, op, &mut json));
        let operations = module(spec, "Every operation of the API.", json.uses, items);
        Files::from([
            (PathBuf::from("models.py"), models_py),
            (PathBuf::from("operations.py"), operations),
        ])
    }

    /// One model: a dataclass with `from_json` and `to_json`, a `StrEnum`, a
    /// union alias and its `…_from_json`.
    fn model(&self, out: &mut Vec<Item>, model: &Model, json: &mut Json<'_>) {
        match model {
            Model::Struct { name, doc, fields } => {
                json.uses
                    .extend([use_("dataclasses", "dataclass"), use_("typing", "Any")]);
                let class = PyName(name).class();
                let mut item = Class::dataclass(&class, doc);
                let (mut args, mut entries, mut optional) = (Vec::new(), Vec::new(), Vec::new());
                for field in fields {
                    let py = PyName(&field.name).snake(&MODEL);
                    let wire = quote(&field.name);
                    let (ty, got, default) = if field.required {
                        (field.ty.clone(), format!("data[{wire}]"), "")
                    } else {
                        (optional_of(&field.ty), format!("data.get({wire})"), "None")
                    };
                    item.field(&py, &PyType(&ty).to_string(), default, &field.doc);
                    args.push(json.decode(&ty, &got).prefix(&format!("{py}=")));
                    let value = json.encode(&field.ty, &format!("self.{py}"));
                    if field.required {
                        entries.push(Value::text(format!("{wire}: {value}")));
                    } else {
                        optional.push(Body::Block(
                            format!("if self.{py} is not None"),
                            vec![Body::Line(format!("data[{wire}] = {value}"))],
                        ));
                    }
                }
                let mut from = Def::new("from_json", &["cls", "data: Any"], &class, "")
                    .decorator("classmethod");
                from.push(Body::Value(Value::call("return cls", args)));
                let mut to = Def::new("to_json", &["self"], "dict[str, Any]", "");
                if optional.is_empty() {
                    to.push(Body::Value(Value::dict(entries).prefix("return ")));
                } else {
                    to.push(Body::Value(
                        Value::dict(entries).prefix("data: dict[str, Any] = "),
                    ));
                    optional.into_iter().for_each(|body| {
                        to.push(body);
                    });
                    to.line("return data");
                }
                item.defs = vec![from, to];
                out.push(Item::Class(item));
            }
            Model::Enum { name, doc, values } => {
                json.uses.insert(use_("enum", "StrEnum"));
                let members = values
                    .iter()
                    .map(|value| (PyName(value).constant(), quote(value)))
                    .collect();
                out.push(Item::Enum(Enum {
                    doc: doc.clone(),
                    name: PyName(name).class(),
                    members,
                }));
            }
            Model::Union {
                name,
                doc,
                variants,
            } => {
                json.uses
                    .extend([use_("typing", "Any"), use_("typing", "TypeAlias")]);
                let class = PyName(name).class();
                let ty: Vec<String> = variants.iter().map(|of| PyType(of).to_string()).collect();
                out.push(Item::TypeAlias(TypeAlias {
                    doc: doc.clone(),
                    name: class.clone(),
                    ty: quote(&ty.join(" | ")),
                }));
                let mut args = vec![Value::text("data")];
                for of in variants {
                    let decoder = json.decoder(of);
                    args.push(Value::text(
                        decoder.unwrap_or_else(|| "lambda value: value".to_owned()),
                    ));
                }
                json.uses.insert(client("first"));
                let doc = format!("The first variant of `{class}` `data` decodes as.");
                let mut def = Def::new(&union_decoder(name), &["data: Any"], &class, &doc);
                def.push(Body::Value(Value::call(
                    &format!("value: {class} = first"),
                    args,
                )))
                .line("return value");
                out.push(Item::Def(def));
            }
        }
    }

    /// One operation: its descriptor, request dataclass, response alias or
    /// event alias, call.
    fn operation(&self, out: &mut Vec<Item>, op: &Operation, json: &mut Json<'_>) {
        let call = PyName(&op.id).snake(&[]);
        let constant = PyName(&op.id).constant();
        let named = PyName(&op.id).class();
        let route = format!("`{} {}`", op.method.as_str(), op.path);
        json.uses.extend([
            client("Operation"),
            client("Request"),
            use_("dataclasses", "dataclass"),
        ]);
        let templates = op.scopes.iter().map(|scope| Value::text(quote(scope)));
        let fields = vec![
            Value::text(format!("id={}", quote(&op.id))),
            Value::text(format!("method={}", quote(op.method.as_str()))),
            Value::text(format!("path={}", quote(&op.path))),
            Value::tuple(templates.collect()).prefix("templates="),
        ];
        out.push(Item::Assign(
            format!("{route}."),
            Value::call(&format!("{constant} = Operation"), fields),
        ));

        let request = format!("{named}Request");
        let mut item = Class::dataclass(&request, &format!("The input of `{call}`."));
        item.bases = vec!["Request".to_owned()];
        let mut operation = Def::new("operation", &["self"], "Operation", "");
        operation.line(format!("return {constant}"));
        let mut methods = vec![operation];
        let (mut path, mut query) = (Vec::new(), Vec::new());
        for param in &op.params {
            let py = PyName(&param.name).snake(&REQUEST);
            let (ty, default) = if param.required {
                (param.ty.clone(), "")
            } else {
                (optional_of(&param.ty), "None")
            };
            json.names(&ty);
            item.field(&py, &PyType(&ty).to_string(), default, &param.doc);
            let value = if param.ty == Type::String {
                format!("self.{py}")
            } else {
                json.uses.insert(client("text"));
                format!("text(self.{py})")
            };
            let wire = quote(&param.name);
            if param.path {
                path.push(Value::text(format!("{wire}: {value}")));
            } else {
                let append = format!("pairs.append(({wire}, {value}))");
                query.push(if param.required {
                    Body::Line(append)
                } else {
                    Body::Block(
                        format!("if self.{py} is not None"),
                        vec![Body::Line(append)],
                    )
                });
            }
        }
        if !path.is_empty() {
            let mut params = Def::new("params", &["self"], "dict[str, str]", "");
            params.push(Body::Value(Value::dict(path).prefix("return ")));
            methods.push(params);
        }
        if !query.is_empty() {
            let mut pairs = Def::new("query", &["self"], "list[tuple[str, str]]", "");
            pairs.line("pairs: list[tuple[str, str]] = []");
            query.into_iter().for_each(|body| {
                pairs.push(body);
            });
            pairs.line("return pairs");
            methods.push(pairs);
        }
        if let Some(body) = &op.body {
            json.names(body);
            json.uses.insert(client("dump"));
            let (default, encode) = if matches!(body, Type::Optional(_)) {
                (
                    "None",
                    "return None if self.body is None else dump(self.body)",
                )
            } else {
                ("", "return dump(self.body)")
            };
            item.field("body", &PyType(body).to_string(), default, "");
            let mut content = Def::new("content", &["self"], "str | None", "");
            content.line(encode);
            methods.push(content);
        }
        item.defs = methods;
        out.push(Item::Class(item));

        let optional = op.params.iter().all(|param| !param.required)
            && op
                .body
                .as_ref()
                .is_none_or(|body| matches!(body, Type::Optional(_)));
        let input = if optional {
            format!("request: {request} = {request}()")
        } else {
            format!("request: {request}")
        };
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
        let func = match &op.events {
            None => {
                json.uses
                    .extend([client("Client"), use_("typing", "TypeAlias")]);
                let response = format!("{named}Response");
                let ty = op.response.clone();
                ty.iter().for_each(|ty| json.names(ty));
                out.push(Item::TypeAlias(TypeAlias {
                    doc: format!("The output of `{call}`."),
                    name: response.clone(),
                    ty: ty
                        .as_ref()
                        .map_or_else(|| "None".to_owned(), |ty| PyType(ty).to_string()),
                }));
                let mut func =
                    Def::new(&call, &["client: Client", &input], &response, &doc).asynchronous();
                let send = "await client.send(request)";
                match ty.map(|ty| json.decode(&ty, send)) {
                    None => {
                        func.line(send);
                    }
                    Some(Value::Text(text)) if text == send => {
                        func.line(format!("response: {response} = {send}"))
                            .line("return response");
                    }
                    Some(value) => {
                        func.push(Body::Value(value.prefix("return ")));
                    }
                }
                func
            }
            Some(events) if events.is_empty() => {
                json.uses
                    .extend([client("RawEvent"), client("Stream"), client("Streamer")]);
                let mut func = Def::new(
                    &call,
                    &["streamer: Streamer", &input],
                    "Stream[RawEvent]",
                    &doc,
                );
                func.line("return streamer.open(request)");
                func
            }
            Some(events) => {
                json.uses.extend([
                    client("Event"),
                    client("RawEvent"),
                    client("Stream"),
                    client("Streamer"),
                    client("data"),
                    use_("typing", "TypeAlias"),
                ]);
                let event = format!("{named}Event");
                let mut cases = Vec::new();
                let mut types = Vec::new();
                for (wire, ty) in events {
                    json.names(ty);
                    types.push(format!("Event[{}]", PyType(ty)));
                    let data = json.decode(ty, "data(raw)");
                    let returned = Value::call(
                        "return Event",
                        vec![Value::text("raw.event"), data, Value::text("raw.id")],
                    );
                    cases.push(Body::Block(
                        format!("if raw.event == {}", quote(wire)),
                        vec![Body::Value(returned)],
                    ));
                }
                cases.push(Body::Line("return None".to_owned()));
                out.push(Item::TypeAlias(TypeAlias {
                    doc: format!("An event of `{call}`."),
                    name: event.clone(),
                    ty: quote(&types.join(" | ")),
                }));
                let mut func = Def::new(
                    &call,
                    &["streamer: Streamer", &input],
                    &format!("Stream[{event}]"),
                    &doc,
                );
                func.line("")
                    .push(Body::Block(
                        format!("def decode(raw: RawEvent) -> {event} | None"),
                        cases,
                    ))
                    .line("")
                    .line("return streamer.open(request).map(decode)");
                func
            }
        };
        out.push(Item::Def(func));
    }
}

/// A file: its marker, docstring, `from __future__ import annotations`,
/// `uses` (the standard library's, then the package's), `items`.
fn module(
    spec: &Spec,
    doc: &str,
    uses: BTreeSet<(&'static str, String)>,
    items: Vec<Item>,
) -> String {
    let mut imports = vec![Import {
        module: "__future__".to_owned(),
        names: vec!["annotations".to_owned()],
    }];
    let mut uses: Vec<_> = uses.into_iter().collect();
    // isort's order: the standard library first; constants, classes, then
    // functions, each ignoring case.
    uses.sort_by_key(|(module, name)| {
        let kind = if name.len() > 1 && *name == name.to_uppercase() {
            0
        } else {
            u8::from(!name.starts_with(char::is_uppercase)) + 1
        };
        (module.starts_with('.'), *module, kind, name.to_lowercase())
    });
    for (module, name) in uses {
        match imports.last_mut() {
            Some(last) if last.module == module => last.names.push(name),
            _ => imports.push(Import {
                module: module.to_owned(),
                names: vec![name],
            }),
        }
    }
    Module {
        marker: spec.marker(),
        doc: doc.to_owned(),
        imports,
        items,
    }
    .to_string()
}

/// `name` from the hand-written `.client`.
fn client(name: &str) -> (&'static str, String) {
    use_(".client", name)
}

/// `name` from `module`.
fn use_(module: &'static str, name: &str) -> (&'static str, String) {
    (module, name.to_owned())
}

/// `of`, or `None`.
fn optional_of(of: &Type) -> Type {
    match of {
        Type::Optional(_) => of.clone(),
        _ => Type::Optional(Box::new(of.clone())),
    }
}

/// The function decoding the union `name`: `listed_from_json`.
fn union_decoder(name: &str) -> String {
    format!("{}_from_json", PyName(name).snake(&[]))
}

/// How a type goes from and to JSON, and what that needs imported.
struct Json<'a> {
    /// Every model, by name.
    models: &'a BTreeMap<&'a str, &'a Model>,
    /// The names to import, by module.
    uses: BTreeSet<(&'static str, String)>,
    /// Whether this is `models.py`: models need no import.
    local: bool,
}

impl Json<'_> {
    /// Imports the models `of` names.
    fn names(&mut self, of: &Type) {
        match of {
            Type::Named(name) if !self.local => {
                self.uses.insert((".models", PyName(name).class()));
            }
            Type::Any => {
                self.uses.insert(use_("typing", "Any"));
            }
            Type::Array(inner) | Type::Map(inner) | Type::Optional(inner) => self.names(inner),
            _ => {}
        }
    }

    /// The function turning JSON into `of`; none when it is the JSON as is.
    fn decoder(&mut self, of: &Type) -> Option<String> {
        match of {
            Type::Named(name) => {
                let class = PyName(name).class();
                let (decoder, import) = match self.models.get(name.as_str()) {
                    Some(Model::Enum { .. }) => (class.clone(), class),
                    Some(Model::Union { .. }) => (union_decoder(name), union_decoder(name)),
                    _ => (format!("{class}.from_json"), class),
                };
                if !self.local {
                    self.uses.insert((".models", import));
                }
                Some(decoder)
            }
            Type::Array(_) | Type::Map(_) | Type::Optional(_) => {
                let value = self.decode(of, "value");
                (value != Value::text("value")).then(|| format!("lambda value: {}", value.flat()))
            }
            _ => None,
        }
    }

    /// `value`, JSON, as `of`.
    fn decode(&mut self, of: &Type, value: &str) -> Value {
        let (helper, inner) = match of {
            Type::Named(_) => {
                let decoder = self.decoder(of).unwrap_or_default();
                return Value::call(&decoder, vec![Value::text(value)]);
            }
            Type::Optional(inner) => ("optional", inner),
            Type::Array(inner) => ("array", inner),
            Type::Map(inner) => ("mapping", inner),
            _ => return Value::text(value),
        };
        let Some(decoder) = self.decoder(inner) else {
            return Value::text(value);
        };
        self.uses.insert(client(helper));
        Value::call(helper, vec![Value::text(decoder), Value::text(value)])
    }

    /// `value`, of type `of`, as JSON: through `encode` when it holds a
    /// model that is not a string.
    fn encode(&mut self, of: &Type, value: &str) -> String {
        if self.holds_object(of) {
            self.uses.insert(client("encode"));
            format!("encode({value})")
        } else {
            value.to_owned()
        }
    }

    /// Whether `of` holds a struct or a union.
    fn holds_object(&self, of: &Type) -> bool {
        match of {
            Type::Named(name) => {
                !matches!(self.models.get(name.as_str()), Some(Model::Enum { .. }))
            }
            Type::Array(inner) | Type::Map(inner) | Type::Optional(inner) => {
                self.holds_object(inner)
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_types() {
        assert_eq!(PyName("from").snake(&[]), "from_");
        assert_eq!(PyName("query").snake(&REQUEST), "query_");
        assert_eq!(PyName("next_cursor").snake(&REQUEST), "next_cursor");
        assert_eq!(PyName("none").class(), "None_");
        assert_eq!(PyName("no_data").constant(), "NO_DATA");
        assert_eq!(PyName("1h").constant(), "V1H");
        let ty = Type::Optional(Box::new(Type::Map(Box::new(Type::Array(Box::new(
            Type::Named("Pet".into()),
        ))))));
        assert_eq!(PyType(&ty).to_string(), "dict[str, list[Pet]] | None");
        assert_eq!(
            PyType(&Type::Optional(Box::new(Type::Any))).to_string(),
            "Any"
        );
    }

    #[test]
    fn decodes_through_helpers() {
        let models = BTreeMap::new();
        let mut json = Json {
            models: &models,
            uses: BTreeSet::new(),
            local: false,
        };
        let pets = Type::Optional(Box::new(Type::Array(Box::new(Type::Named("Pet".into())))));
        assert_eq!(
            json.decode(&pets, "data[\"pets\"]").flat(),
            "optional(lambda value: array(Pet.from_json, value), data[\"pets\"])"
        );
        assert_eq!(
            json.decode(&Type::Array(Box::new(Type::String)), "x")
                .flat(),
            "x"
        );
        assert_eq!(json.encode(&pets, "self.pets"), "encode(self.pets)");
        assert!(json.uses.contains(&(".models", "Pet".to_owned())));
        assert!(json.uses.contains(&client("optional")));
    }
}
