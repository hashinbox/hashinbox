//! PHP 8.2+, strictly typed, as PER-CS 2.0 writes it, in two files of one
//! namespace: `Models.php` (`final readonly` classes with promoted
//! properties, `fromJson(array)` and `toJson()`; backed enums; a union as a
//! native union type and a `…FromJson` function taking the first variant
//! that decodes) and `Operations.php`. An operation is a descriptor (`const
//! WALLET_GET = new Operation(…)`), a request class (its path and query
//! parameters, and `body`) implementing the hand-written `Request`
//! interface, and the call (`walletGet($client, $request): Wallet`), whose
//! return type is the response (PHP has no type aliases). Both files hold
//! several declarations and functions, so composer loads them through
//! `"files"`; the hand-written classes they use (`Operation`, `Request`,
//! `Client`, `Json`, `Event`, `RawEvent`) load by PSR-4. PHPDoc types
//! (`list<Pet>`, `array<string, int>`) carry what native types cannot, for
//! PHPStan at level max. The syntax is [`expr`]'s; this maps the [`Spec`]
//! onto it.

mod expr;

use std::path::PathBuf;

use crate::Files;
use crate::spec::{Model, Operation, Spec, Type};
use expr::{
    Body, Class, Const, DocBlock, Enum, File, Item, Method, Models, PhpName, PhpType, Use, Value,
    quote,
};

/// The PHP client.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Php {
    /// The namespace: `Hashinbox`.
    pub namespace: String,
}

impl Php {
    /// `Models.php` and `Operations.php`, for a package whose `src/` holds
    /// them beside its hand-written classes.
    #[must_use]
    pub fn emit(&self, spec: &Spec) -> Files {
        let models: Models = spec
            .models
            .iter()
            .map(|model| (model.name(), model))
            .collect();
        let json = Json { models: &models };
        let file = |uses: Vec<Use>, items: Vec<Item>| {
            File {
                marker: spec.marker(),
                namespace: self.namespace.clone(),
                uses,
                items,
            }
            .to_string()
        };
        let mut items = Vec::new();
        spec.models
            .iter()
            .for_each(|model| self.model(&mut items, model, &json));
        let models_php = file(Vec::new(), items);
        let mut items = Vec::new();
        spec.operations
            .iter()
            .for_each(|op| self.operation(&mut items, op, &json));
        let uses = if spec.operations.iter().any(|op| op.events.is_some()) {
            vec![Use("Generator".to_owned())]
        } else {
            Vec::new()
        };
        Files::from([
            (PathBuf::from("Models.php"), models_php),
            (PathBuf::from("Operations.php"), file(uses, items)),
        ])
    }

    /// One model: a readonly class with `fromJson` and `toJson`, a backed
    /// enum, or a union's `…FromJson`.
    fn model(&self, out: &mut Vec<Item>, model: &Model, json: &Json<'_>) {
        match model {
            Model::Struct { name, doc, fields } => {
                let mut item = Class::readonly(&PhpName(name).class(), DocBlock::new(doc));
                let mut args = Vec::new();
                let mut entries = Vec::new();
                let mut optional = Vec::new();
                for field in fields {
                    let php = PhpName(&field.name).variable();
                    let wire = quote(&field.name);
                    let inner = inner(&field.ty);
                    let present = field.required && !matches!(field.ty, Type::Optional(_));
                    let ty = if present {
                        field.ty.clone()
                    } else {
                        optional_of(&field.ty)
                    };
                    let written = json.ty(&ty);
                    let default = if field.required { "" } else { "null" };
                    item.property(
                        &php,
                        &written.to_string(),
                        &written.doc_only(),
                        default,
                        &field.doc,
                    );
                    let arg = if present {
                        json.decode(&field.ty, &format!("Json::field($data, {wire})"))
                    } else if *inner == Type::Any {
                        Value::text(format!("$data[{wire}] ?? null"))
                    } else {
                        json.decode(inner, &format!("$data[{wire}]"))
                            .prefix(&format!("isset($data[{wire}]) ? "))
                            .suffix(" : null")
                    };
                    args.push(arg.prefix(&format!("{php}: ")));
                    let this = format!("$this->{php}");
                    if field.required {
                        entries.push(json.encode(&field.ty, &this).prefix(&format!("{wire} => ")));
                    } else {
                        let set = json
                            .encode(inner, &this)
                            .prefix(&format!("$data[{wire}] = "))
                            .suffix(";");
                        optional.push(Body::Block(
                            format!("if ({this} !== null)"),
                            vec![Body::Value(set)],
                        ));
                    }
                }
                let doc = DocBlock::default().tag("@param array<string, mixed> $data");
                let mut from = Method::public("fromJson", &["array $data"], "self", doc).static_();
                from.push(Body::Value(
                    Value::call("return new self", args).suffix(";"),
                ));
                let doc = DocBlock::default().tag("@return array<string, mixed>");
                let mut to = Method::public("toJson", &[], "array", doc);
                if optional.is_empty() {
                    to.push(Body::Value(
                        Value::array(entries).prefix("return ").suffix(";"),
                    ));
                } else {
                    to.push(Body::Value(
                        Value::array(entries).prefix("$data = ").suffix(";"),
                    ));
                    optional.into_iter().for_each(|body| {
                        to.push(body);
                    });
                    to.line("return $data;");
                }
                item.methods = vec![from, to];
                out.push(Item::Class(item));
            }
            Model::Enum { name, doc, values } => {
                let cases = values
                    .iter()
                    .map(|value| (PhpName(value).case(), value.clone()))
                    .collect();
                out.push(Item::Enum(Enum {
                    doc: DocBlock::new(doc),
                    name: PhpName(name).class(),
                    cases,
                }));
            }
            Model::Union { name, doc, .. } => {
                let ty = Type::Named(name.clone());
                let written = json.ty(&ty);
                let Some(variants) = json.variants(name) else {
                    return;
                };
                let lead = if doc.is_empty() {
                    format!("`{name}`")
                } else {
                    doc.trim_end_matches('.').to_owned()
                };
                let mut doc = DocBlock::new(&format!(
                    "{lead}: the first of its variants `$data` decodes as."
                ));
                if !written.doc_only().is_empty() {
                    doc = doc.tag(format!("@return {}", written.doc()));
                }
                let mut args = vec![Value::text("$data")];
                args.extend(variants.iter().map(|of| json.decoder(of)));
                let mut func = Method::function(
                    &union_decoder(name),
                    &["mixed $data"],
                    &written.to_string(),
                    doc,
                );
                func.push(Body::Value(
                    Value::call("return Json::first", args).suffix(";"),
                ));
                out.push(Item::Function(func));
            }
        }
    }

    /// One operation: its descriptor, request class and call.
    fn operation(&self, out: &mut Vec<Item>, op: &Operation, json: &Json<'_>) {
        let call = PhpName(&op.id).function();
        let constant = PhpName(&op.id).constant();
        let request = format!("{}Request", PhpName(&op.id).class());
        let route = format!("`{} {}`", op.method.as_str(), op.path);
        let templates = op.scopes.iter().map(|scope| Value::text(quote(scope)));
        let fields = vec![
            Value::text(format!("id: {}", quote(&op.id))),
            Value::text(format!("method: {}", quote(op.method.as_str()))),
            Value::text(format!("path: {}", quote(&op.path))),
            Value::array(templates.collect()).prefix("templates: "),
        ];
        out.push(Item::Const(Const {
            doc: DocBlock::new(&format!("{route}.")),
            name: constant.clone(),
            value: Value::call("new Operation", fields),
        }));

        let mut item = Class::readonly(&request, DocBlock::new(&format!("The input of `{call}`.")));
        item.implements.push("Request".to_owned());
        let (mut path, mut query) = (Vec::new(), Vec::new());
        for param in &op.params {
            let php = PhpName(&param.name).variable();
            let php = if php == "body" {
                "body_".to_owned()
            } else {
                php
            };
            let (ty, default) = if param.required {
                (param.ty.clone(), "")
            } else {
                (optional_of(&param.ty), "null")
            };
            let written = json.ty(&ty);
            item.property(
                &php,
                &written.to_string(),
                &written.doc_only(),
                default,
                &param.doc,
            );
            let this = format!("$this->{php}");
            let value = json.text(&param.ty, &this);
            let wire = quote(&param.name);
            if param.path {
                path.push(Value::text(format!("{wire} => {value}")));
            } else {
                let append = Body::Line(format!("$pairs[] = [{wire}, {value}];"));
                query.push(if param.required {
                    append
                } else {
                    Body::Block(format!("if ({this} !== null)"), vec![append])
                });
            }
        }
        let mut returns = "?string";
        let content = match &op.body {
            None => Value::text("return null;"),
            Some(body) => {
                let written = json.ty(body);
                let default = if matches!(body, Type::Optional(_)) {
                    "null"
                } else {
                    ""
                };
                item.property(
                    "body",
                    &written.to_string(),
                    &written.doc_only(),
                    default,
                    "",
                );
                let dump = Value::call("Json::dump", vec![json.encode(inner(body), "$this->body")]);
                if default.is_empty() {
                    returns = "string";
                    dump.prefix("return ").suffix(";")
                } else {
                    dump.prefix("return $this->body === null ? null : ")
                        .suffix(";")
                }
            }
        };
        let method = |name: &str, ret: &str| Method::public(name, &[], ret, DocBlock::default());
        let mut operation = method("operation", "Operation");
        operation.line(format!("return {constant};"));
        let mut params = method("params", "array");
        params.push(Body::Value(
            Value::array(path).prefix("return ").suffix(";"),
        ));
        let mut pairs = method("query", "array");
        if query.is_empty() {
            pairs.line("return [];");
        } else {
            pairs.line("$pairs = [];");
            query.into_iter().for_each(|body| {
                pairs.push(body);
            });
            pairs.line("return $pairs;");
        }
        let mut body = method("content", returns);
        body.push(Body::Value(content));
        item.methods = vec![operation, params, pairs, body];
        out.push(Item::Class(item));

        let optional = op.params.iter().all(|param| !param.required)
            && op
                .body
                .as_ref()
                .is_none_or(|body| matches!(body, Type::Optional(_)));
        let input = if optional {
            format!("{request} $request = new {request}()")
        } else {
            format!("{request} $request")
        };
        let first = if op.summary.is_empty() {
            format!("{route}.")
        } else {
            format!("{}: {route}.", op.summary)
        };
        let text = if op.doc.is_empty() {
            first
        } else {
            format!("{first}\n\n{}", op.doc)
        };
        let mut doc = DocBlock::new(&text);
        let params = ["Client $client", input.as_str()];
        let func = match &op.events {
            None => {
                let (ret, value) = match &op.response {
                    None => ("void".to_owned(), None),
                    Some(ty) => {
                        let written = json.ty(ty);
                        if !written.doc_only().is_empty() {
                            doc = doc.tag(format!("@return {}", written.doc()));
                        }
                        let value = json.decode(ty, "$client->send($request)");
                        (written.to_string(), Some(value))
                    }
                };
                let mut func = Method::function(&call, &params, &ret, doc);
                match value {
                    None => func.line("$client->send($request);"),
                    Some(value) => func.push(Body::Value(value.prefix("return ").suffix(";"))),
                };
                func
            }
            Some(events) if events.is_empty() => {
                doc = doc.tag("@return Generator<int, RawEvent, mixed, void>");
                let mut func = Method::function(&call, &params, "Generator", doc);
                func.line("return $client->open($request);");
                func
            }
            Some(events) => {
                let mut types = Vec::new();
                let mut arms = Vec::new();
                for (wire, ty) in events {
                    types.push(format!("Event<{}>", json.ty(ty).doc()));
                    let data = json.decode(ty, "Json::decode($raw->data)");
                    let event = Value::call(
                        "yield new Event",
                        vec![Value::text("$raw->event"), data, Value::text("$raw->id")],
                    );
                    arms.push((
                        format!("$raw->event === {}", quote(wire)),
                        vec![Body::Value(event.suffix(";"))],
                    ));
                }
                doc = doc.tag(format!(
                    "@return Generator<int, {}, mixed, void>",
                    types.join("|")
                ));
                let mut func = Method::function(&call, &params, "Generator", doc);
                func.push(Body::Block(
                    "foreach ($client->open($request) as $raw)".to_owned(),
                    vec![Body::If(arms)],
                ));
                func
            }
        };
        out.push(Item::Function(func));
    }
}

/// `of`, or `null`.
fn optional_of(of: &Type) -> Type {
    match of {
        Type::Optional(_) => of.clone(),
        _ => Type::Optional(Box::new(of.clone())),
    }
}

/// `of` without its `null`.
fn inner(of: &Type) -> &Type {
    match of {
        Type::Optional(inner) => inner,
        _ => of,
    }
}

/// The function decoding the union `name`: `listedFromJson`.
fn union_decoder(name: &str) -> String {
    format!("{}FromJson", PhpName(name).variable())
}

/// How a type goes from and to JSON (arrays, as `json_decode` gives them):
/// through the hand-written `Json` helpers, which check each value's type.
struct Json<'a> {
    /// Every model, by name.
    models: &'a Models<'a>,
}

impl<'a> Json<'a> {
    /// `of`, written.
    fn ty(&self, of: &'a Type) -> PhpType<'a> {
        PhpType::new(of, self.models)
    }

    /// The model `name`.
    fn model(&self, name: &str) -> Option<&'a Model> {
        self.models.get(name).copied()
    }

    /// The variants of the union `name`.
    fn variants(&self, name: &str) -> Option<&'a [Type]> {
        match self.model(name) {
            Some(Model::Union { variants, .. }) => Some(variants),
            _ => None,
        }
    }

    /// `value`, JSON, as `of`.
    fn decode(&self, of: &Type, value: &str) -> Value {
        let check =
            |helper: &str| Value::call(&format!("Json::{helper}"), vec![Value::text(value)]);
        match of {
            Type::String => check("string"),
            Type::Int | Type::Int32 => check("int"),
            Type::Float => check("float"),
            Type::Bool => check("bool"),
            Type::Any => Value::text(value),
            Type::Named(name) => match self.model(name) {
                Some(Model::Enum { .. }) => Value::call(
                    &format!("{}::from", PhpName(name).class()),
                    vec![check("string")],
                ),
                Some(Model::Union { .. }) => {
                    Value::call(&union_decoder(name), vec![Value::text(value)])
                }
                _ => Value::call(
                    &format!("{}::fromJson", PhpName(name).class()),
                    vec![check("object")],
                ),
            },
            Type::Array(item) => {
                Value::call("Json::list", vec![Value::text(value), self.decoder(item)])
            }
            Type::Map(item) => {
                Value::call("Json::map", vec![Value::text(value), self.decoder(item)])
            }
            Type::Optional(inner) => self
                .decode(inner, value)
                .prefix(&format!("{value} === null ? null : ")),
        }
    }

    /// The callable turning JSON into `of`: a first-class callable when one
    /// takes `mixed`, else a closure.
    fn decoder(&self, of: &Type) -> Value {
        let helper = match of {
            Type::String => Some("Json::string".to_owned()),
            Type::Int | Type::Int32 => Some("Json::int".to_owned()),
            Type::Float => Some("Json::float".to_owned()),
            Type::Bool => Some("Json::bool".to_owned()),
            Type::Named(name) if self.variants(name).is_some() => Some(union_decoder(name)),
            _ => None,
        };
        if let Some(helper) = helper {
            return Value::text(format!("{helper}(...)"));
        }
        self.decode(of, "$v")
            .prefix(&format!("static fn(mixed $v): {} => ", self.ty(of)))
    }

    /// `value`, of type `of`, as JSON for `json_encode`: an enum its value,
    /// an object (a model, a map) a `stdClass` so an empty one stays `{}`.
    fn encode(&self, of: &Type, value: &str) -> Value {
        let mapped = |item: &Type| {
            let each = self.encode(item, "$v");
            (each != Value::text("$v")).then(|| {
                Value::call(
                    "array_map",
                    vec![
                        each.prefix(&format!("static fn({} $v): mixed => ", self.ty(item))),
                        Value::text(value),
                    ],
                )
            })
        };
        match of {
            Type::Named(name) => match self.model(name) {
                Some(Model::Enum { .. }) => Value::text(format!("{value}->value")),
                Some(Model::Union { .. }) => Value::call("Json::encode", vec![Value::text(value)]),
                _ => Value::text(format!("(object) {value}->toJson()")),
            },
            Type::Array(item) => mapped(item).unwrap_or_else(|| Value::text(value)),
            Type::Map(item) => mapped(item)
                .unwrap_or_else(|| Value::text(value))
                .prefix("(object) "),
            Type::Optional(inner) => {
                let each = self.encode(inner, value);
                if each == Value::text(value) {
                    each
                } else {
                    each.prefix(&format!("{value} === null ? null : "))
                }
            }
            _ => Value::text(value),
        }
    }

    /// `value`, a scalar or enum parameter of type `of`, as path or query
    /// text.
    fn text(&self, of: &Type, value: &str) -> String {
        match of {
            Type::String => value.to_owned(),
            Type::Bool => format!("{value} ? 'true' : 'false'"),
            Type::Named(_) => format!("{value}->value"),
            _ => format!("(string) {value}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_and_encodes_by_type() {
        let pet = Model::Struct {
            name: "Pet".into(),
            doc: String::new(),
            fields: Vec::new(),
        };
        let kind = Model::Enum {
            name: "Kind".into(),
            doc: String::new(),
            values: vec!["cat".into()],
        };
        let models = Models::from([("Pet", &pet), ("Kind", &kind)]);
        let json = Json { models: &models };
        let pets = Type::Array(Box::new(Type::Named("Pet".into())));
        assert_eq!(
            json.decode(&pets, "$x").flat(),
            "Json::list($x, static fn(mixed $v): Pet => Pet::fromJson(Json::object($v)))"
        );
        let names = Type::Map(Box::new(Type::String));
        assert_eq!(
            json.decode(&names, "$x").flat(),
            "Json::map($x, Json::string(...))"
        );
        assert_eq!(
            json.encode(&pets, "$this->pets").flat(),
            "array_map(static fn(Pet $v): mixed => (object) $v->toJson(), $this->pets)"
        );
        assert_eq!(json.encode(&names, "$this->x").flat(), "(object) $this->x");
        let kind = Type::Optional(Box::new(Type::Named("Kind".into())));
        assert_eq!(
            json.encode(&kind, "$this->kind").flat(),
            "$this->kind === null ? null : $this->kind->value"
        );
        assert_eq!(
            json.text(&Type::Bool, "$this->all"),
            "$this->all ? 'true' : 'false'"
        );
        assert_eq!(
            json.text(&Type::Int, "$this->limit"),
            "(string) $this->limit"
        );
    }
}
