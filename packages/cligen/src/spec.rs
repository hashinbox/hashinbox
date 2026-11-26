//! The API as generators read it: plain data, resolved once. Inline
//! objects, enums and unions are hoisted into models named after where
//! they were (`PetOwner`), `allOf` flattened, aliases inlined, `null`
//! folded into [`Type::Optional`]. Models are sorted by name, operations by
//! tag then id, and every name is unique.

use std::collections::BTreeMap;

use crate::error::{Error, Result, invalid, unsupported};
use crate::name::pascal;
use crate::openapi::{self, Document, Extra, Location, Schema};

/// The resolved API.
#[derive(Clone, Debug, PartialEq)]
pub struct Spec {
    /// `info.title`.
    pub title: String,
    /// `info.version`.
    pub version: String,
    /// Every model, by name.
    pub models: Vec<Model>,
    /// Every operation, by tag then id.
    pub operations: Vec<Operation>,
    /// The model most error bodies are: what an `ApiError` decodes.
    pub error: Option<String>,
}

/// A named type.
#[derive(Clone, Debug, PartialEq)]
pub enum Model {
    /// An object with these fields, by name.
    Struct {
        /// Its name.
        name: String,
        /// Its description; empty: none.
        doc: String,
        /// Its fields.
        fields: Vec<Field>,
    },
    /// One of these strings.
    Enum {
        /// Its name.
        name: String,
        /// Its description.
        doc: String,
        /// Its values.
        values: Vec<String>,
    },
    /// The first of these types the value decodes as.
    Union {
        /// Its name.
        name: String,
        /// Its description.
        doc: String,
        /// Its variants, in order.
        variants: Vec<Type>,
    },
}

impl Model {
    /// Its name.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Struct { name, .. } | Self::Enum { name, .. } | Self::Union { name, .. } => name,
        }
    }
}

/// A field of a struct.
#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    /// Its name on the wire.
    pub name: String,
    /// Its type.
    pub ty: Type,
    /// Whether it is always present.
    pub required: bool,
    /// Its description.
    pub doc: String,
}

/// A type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Type {
    /// A string.
    String,
    /// A 64-bit integer.
    Int,
    /// A 32-bit integer (`format: int32`).
    Int32,
    /// A floating-point number.
    Float,
    /// `true` or `false`.
    Bool,
    /// Any JSON value.
    Any,
    /// The model of this name.
    Named(String),
    /// An array.
    Array(Box<Type>),
    /// An object with string keys.
    Map(Box<Type>),
    /// The type, or `null`.
    Optional(Box<Type>),
}

/// An HTTP method.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// `GET`.
    Get,
    /// `PUT`.
    Put,
    /// `POST`.
    Post,
    /// `DELETE`.
    Delete,
    /// `PATCH`.
    Patch,
}

impl Type {
    /// A name for it as a union's variant: its model's, else its kind's.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Named(name) => name.clone(),
            Self::Optional(inner) => inner.label(),
            Self::String => "String".to_owned(),
            Self::Int | Self::Int32 => "Integer".to_owned(),
            Self::Float => "Number".to_owned(),
            Self::Bool => "Boolean".to_owned(),
            Self::Any => "Value".to_owned(),
            Self::Array(_) => "List".to_owned(),
            Self::Map(_) => "Map".to_owned(),
        }
    }
}

impl Method {
    /// `GET`, `POST`, …
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Put => "PUT",
            Self::Post => "POST",
            Self::Delete => "DELETE",
            Self::Patch => "PATCH",
        }
    }
}

/// An operation.
#[derive(Clone, Debug, PartialEq)]
pub struct Operation {
    /// `operationId`, as written: `read_wallet`.
    pub id: String,
    /// Its first tag; `default` when untagged.
    pub tag: String,
    /// Its summary, without a full stop; empty: none.
    pub summary: String,
    /// Its description.
    pub doc: String,
    /// Its method.
    pub method: Method,
    /// Its path template: `/v1/wallets/{id}`.
    pub path: String,
    /// Path parameters, then query parameters.
    pub params: Vec<Param>,
    /// Its JSON body; an optional one is [`Type::Optional`].
    pub body: Option<Type>,
    /// Its 2xx JSON body; none: empty, or a stream.
    pub response: Option<Type>,
    /// A `text/event-stream`'s events (`x-events`), name and data type;
    /// empty: raw events; none: not a stream.
    pub events: Option<Vec<(String, Type)>>,
    /// Its scope templates, each `{hole}` a path parameter.
    pub scopes: Vec<String>,
}

/// A path or query parameter.
#[derive(Clone, Debug, PartialEq)]
pub struct Param {
    /// Its name on the wire.
    pub name: String,
    /// Its type: a scalar or an enum.
    pub ty: Type,
    /// In the path; else in the query.
    pub path: bool,
    /// Whether it must be given.
    pub required: bool,
    /// Its description.
    pub doc: String,
}

/// A piece of a template.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Piece<'a> {
    /// Literal text.
    Text(&'a str),
    /// A `{name}` hole.
    Hole(&'a str),
}

/// `template` split at its `{name}` holes.
#[must_use]
pub fn split(template: &str) -> Vec<Piece<'_>> {
    let mut pieces = Vec::new();
    let mut rest = template;
    while let (Some(open), Some(close)) = (rest.find('{'), rest.find('}')) {
        if open > 0 {
            pieces.push(Piece::Text(&rest[..open]));
        }
        pieces.push(Piece::Hole(&rest[open + 1..close]));
        rest = &rest[close + 1..];
    }
    if !rest.is_empty() {
        pieces.push(Piece::Text(rest));
    }
    pieces
}

impl Spec {
    /// Parses an OpenAPI 3.1 document and resolves it.
    ///
    /// # Errors
    ///
    /// [`Error::Parse`] for text outside the supported subset; a feature
    /// cligen refuses, an unknown `$ref`, a scope hole naming no path
    /// parameter, or two things of one name.
    pub fn from_openapi(json: &str) -> Result<Self> {
        let doc = Document::parse(json).map_err(|error| Error::Parse(error.to_string()))?;
        if !doc.openapi.starts_with("3.1") {
            return unsupported("#/openapi", &format!("OpenAPI {}", doc.openapi));
        }
        let mut resolver = Resolver {
            doc: &doc,
            models: BTreeMap::new(),
            depth: 0,
        };
        for (name, schema) in doc.schemas() {
            let at = format!("#/components/schemas/{name}");
            if compound(schema) {
                if schema.is_nullable() {
                    return unsupported(&at, "a nullable object, enum or union model");
                }
                let model = resolver.model(schema, name, &at)?;
                resolver.insert(model)?;
            }
        }
        let mut operations = Vec::new();
        let mut errors = Vec::new();
        for (path, method, op) in doc.operations() {
            operations.push(resolver.operation(path, method, op, &mut errors)?);
        }
        operations.sort_by(|a, b| (&a.tag, &a.id).cmp(&(&b.tag, &b.id)));
        for pair in operations.windows(2) {
            if pair[0].id == pair[1].id {
                return Err(Error::Collision {
                    within: "operations".to_owned(),
                    name: pair[0].id.clone(),
                });
            }
        }
        for op in &operations {
            let named = pascal(&op.id);
            for name in [
                named.clone(),
                format!("{named}Request"),
                format!("{named}Response"),
                format!("{named}Event"),
            ] {
                if resolver.models.contains_key(&name) {
                    return Err(Error::Collision {
                        within: format!("operation `{}`", op.id),
                        name,
                    });
                }
            }
        }
        let error = errors
            .iter()
            .max_by_key(|name| errors.iter().filter(|other| other == name).count())
            .cloned();
        Ok(Self {
            title: doc.info.title.clone(),
            version: doc.info.version.clone(),
            models: resolver.models.into_values().collect(),
            operations,
            error,
        })
    }

    /// The generated-code marker: `Code generated by cligen from
    /// hashinbox-api 0.1.0. DO NOT EDIT.`
    #[must_use]
    pub fn marker(&self) -> String {
        format!(
            "Code generated by cligen from {} {}. DO NOT EDIT.",
            self.title, self.version
        )
    }

    /// The operations of each tag, by tag.
    #[must_use]
    pub fn tags(&self) -> BTreeMap<&str, Vec<&Operation>> {
        let mut tags: BTreeMap<&str, Vec<&Operation>> = BTreeMap::new();
        for op in &self.operations {
            tags.entry(&op.tag).or_default().push(op);
        }
        tags
    }
}

/// Whether `schema` is an object, enum or union: what becomes a model.
fn compound(schema: &Schema) -> bool {
    let real = schema.one_of.iter().filter(|one| !one.is_null()).count();
    schema.reference.is_none()
        && (real > 1
            || schema.all_of.len() > 1
            || schema.values.is_some()
            || !schema.properties.is_empty())
}

/// The resolving state.
struct Resolver<'a> {
    /// The document.
    doc: &'a Document,
    /// The models so far.
    models: BTreeMap<String, Model>,
    /// How deep references are followed, to refuse a cycle.
    depth: usize,
}

impl Resolver<'_> {
    /// Adds a model; the same one twice is fine, two different ones a
    /// collision.
    fn insert(&mut self, model: Model) -> Result<()> {
        match self.models.get(model.name()) {
            Some(have) if *have == model => Ok(()),
            Some(_) => Err(Error::Collision {
                within: "models".to_owned(),
                name: model.name().to_owned(),
            }),
            None => {
                self.models.insert(model.name().to_owned(), model);
                Ok(())
            }
        }
    }

    /// `schema` as a type; an object, enum or union in it becomes the model
    /// `name`.
    fn ty(&mut self, schema: &Schema, name: &str, at: &str) -> Result<Type> {
        let ty = if let Some(reference) = &schema.reference {
            self.reference(reference, at)?
        } else if compound(schema) {
            let model = self.model(schema, name, at)?;
            self.insert(model)?;
            Type::Named(name.to_owned())
        } else {
            self.plain(schema, name, at)?
        };
        let null = schema.is_nullable() || schema.one_of.iter().any(Schema::is_null);
        Ok(match ty {
            Type::Optional(_) => ty,
            ty if null => Type::Optional(Box::new(ty)),
            ty => ty,
        })
    }

    /// What a `$ref` resolves to: the model, or the type it aliases.
    fn reference(&mut self, reference: &str, at: &str) -> Result<Type> {
        let found = reference
            .strip_prefix("#/components/schemas/")
            .and_then(|name| Some((name, self.doc.schema(name)?)));
        let Some((name, schema)) = found else {
            return invalid(at, format!("unknown $ref {reference}"));
        };
        if compound(schema) {
            return Ok(Type::Named(name.to_owned()));
        }
        self.depth += 1;
        if self.depth > 32 {
            return invalid(at, "a $ref cycle");
        }
        let ty = self.ty(schema, name, &format!("#/components/schemas/{name}"));
        self.depth -= 1;
        ty
    }

    /// A schema that is no model: a scalar, an array, a map, any value, or
    /// a `oneOf`/`allOf` of one.
    fn plain(&mut self, schema: &Schema, name: &str, at: &str) -> Result<Type> {
        if let Some(only) = schema.one_of.iter().find(|one| !one.is_null()) {
            return self.ty(only, name, &format!("{at}/oneOf"));
        }
        if let [only] = &schema.all_of[..] {
            return self.ty(only, name, &format!("{at}/allOf/0"));
        }
        Ok(match schema.types()[..] {
            [] | ["object"] => match &schema.additional_properties {
                Some(Extra::Values(values)) => {
                    Type::Map(Box::new(self.ty(values, &format!("{name}Value"), at)?))
                }
                Some(Extra::Allowed(true)) => Type::Map(Box::new(Type::Any)),
                _ => Type::Any,
            },
            ["string"] => Type::String,
            ["integer"] if schema.format.as_deref() == Some("int32") => Type::Int32,
            ["integer"] => Type::Int,
            ["number"] => Type::Float,
            ["boolean"] => Type::Bool,
            ["array"] => match &schema.items {
                Some(items) => Type::Array(Box::new(self.ty(
                    items,
                    &format!("{name}Item"),
                    &format!("{at}/items"),
                )?)),
                None => return unsupported(at, "an array without items"),
            },
            _ if schema.is_null() => return unsupported(at, "null outside oneOf"),
            _ => return unsupported(&format!("{at}/type"), "this type"),
        })
    }

    /// The model `schema` is.
    fn model(&mut self, schema: &Schema, name: &str, at: &str) -> Result<Model> {
        let (name, doc) = (
            name.to_owned(),
            schema
                .description
                .clone()
                .unwrap_or_default()
                .trim()
                .to_owned(),
        );
        if let Some(values) = &schema.values {
            let values: Option<Vec<String>> = values
                .iter()
                .filter(|value| !value.is_null())
                .map(|value| value.as_str().map(str::to_owned))
                .collect();
            return match values {
                Some(values) if matches!(schema.types()[..], [] | ["string"]) => {
                    Ok(Model::Enum { name, doc, values })
                }
                _ => unsupported(&format!("{at}/enum"), "a non-string enum"),
            };
        }
        if schema.one_of.len() > 1 {
            let mut variants = Vec::new();
            for (index, one) in schema
                .one_of
                .iter()
                .enumerate()
                .filter(|(_, one)| !one.is_null())
            {
                variants.push(self.ty(
                    one,
                    &format!("{name}{}", index + 1),
                    &format!("{at}/oneOf/{index}"),
                )?);
            }
            return Ok(Model::Union {
                name,
                doc,
                variants,
            });
        }
        let mut fields: Vec<Field> = Vec::new();
        for (index, part) in schema.all_of.iter().enumerate() {
            let at = format!("{at}/allOf/{index}");
            let part = match part
                .reference
                .as_deref()
                .and_then(|reference| reference.strip_prefix("#/components/schemas/"))
            {
                Some(target) => {
                    let Some(referred) = self.doc.schema(target) else {
                        return invalid(&at, format!("unknown $ref {target}"));
                    };
                    self.depth += 1;
                    if self.depth > 32 {
                        return invalid(&at, "an allOf cycle");
                    }
                    let model = self.model(referred, target, &at);
                    self.depth -= 1;
                    model?
                }
                None => self.model(part, &name, &at)?,
            };
            let Model::Struct { fields: part, .. } = part else {
                return unsupported(&at, "an allOf of a non-object");
            };
            for field in part {
                match fields.iter_mut().find(|have| have.name == field.name) {
                    Some(have) if have.ty != field.ty => {
                        return invalid(&at, format!("field `{}` has two types", field.name));
                    }
                    Some(have) => have.required |= field.required,
                    None => fields.push(field),
                }
            }
        }
        if matches!(
            schema.additional_properties,
            Some(Extra::Values(_)) | Some(Extra::Allowed(true))
        ) && !schema.properties.is_empty()
        {
            return unsupported(
                &format!("{at}/additionalProperties"),
                "additionalProperties beside properties",
            );
        }
        for (field, property) in &schema.properties {
            let ty = self.ty(
                property,
                &format!("{name}{}", pascal(field)),
                &format!("{at}/properties/{field}"),
            )?;
            let doc = property
                .description
                .clone()
                .unwrap_or_default()
                .trim()
                .to_owned();
            fields.push(Field {
                name: field.clone(),
                ty,
                required: schema.required.contains(field),
                doc,
            });
        }
        fields.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Model::Struct { name, doc, fields })
    }

    /// One operation; the models its error bodies are go to `errors`.
    fn operation(
        &mut self,
        path: &str,
        method: &'static str,
        op: &openapi::Operation,
        errors: &mut Vec<String>,
    ) -> Result<Operation> {
        let at = format!(
            "#/paths/{}/{}",
            path.replace('/', "~1"),
            method.to_lowercase()
        );
        let named = pascal(&op.operation_id);
        let mut params = Vec::new();
        for param in op.path_params().chain(op.query_params()) {
            let at = format!("{at}/parameters/{}", param.name);
            let ty = self.ty(
                &param.schema,
                &format!("{named}{}", pascal(&param.name)),
                &at,
            )?;
            let scalar = |ty: &Type| {
                matches!(
                    ty,
                    Type::String | Type::Int | Type::Int32 | Type::Float | Type::Bool
                )
            };
            let enumeration = |ty: &Type| matches!(ty, Type::Named(name) if matches!(self.models.get(name), Some(Model::Enum { .. })));
            if !scalar(&ty) && !enumeration(&ty) {
                return unsupported(&at, "a parameter that is not a scalar or enum");
            }
            let path = param.at == Location::Path;
            let doc = param
                .description
                .clone()
                .unwrap_or_default()
                .trim()
                .to_owned();
            params.push(Param {
                name: param.name.clone(),
                ty,
                path,
                required: param.required || path,
                doc,
            });
        }
        let bound = |template: &str| {
            split(template).into_iter().find_map(|piece| match piece {
                Piece::Hole(hole)
                    if !params.iter().any(|param| param.path && param.name == hole) =>
                {
                    Some(hole.to_owned())
                }
                _ => None,
            })
        };
        if let Some(hole) = bound(path) {
            return invalid(&at, format!("path hole `{{{hole}}}` has no path parameter"));
        }
        let body = match &op.request_body {
            None => None,
            Some(body) => {
                let ty = self.ty(json(&body.content, &at)?, &format!("{named}Body"), &at)?;
                Some(if body.required {
                    ty
                } else {
                    Type::Optional(Box::new(ty))
                })
            }
        };
        let mut success = None;
        for (status, answer) in &op.responses {
            let at = format!("{at}/responses/{status}");
            let Ok(code) = status.parse::<u16>() else {
                return unsupported(&at, &format!("status `{status}`"));
            };
            if !(200..300).contains(&code) {
                if !answer.content.is_empty()
                    && let Type::Named(name) =
                        self.ty(json(&answer.content, &at)?, &format!("{named}Error"), &at)?
                {
                    errors.push(name);
                }
                continue;
            }
            let this = self.response(answer, &named, &at)?;
            if success.as_ref().is_some_and(|have| *have != this) {
                return invalid(&at, "2xx responses of different types");
            }
            success = Some(this);
        }
        let Some((response, events)) = success else {
            return invalid(&at, "no 2xx response");
        };
        let scopes: Vec<String> = match op.requirements(self.doc.security.as_ref()) {
            [] => Vec::new(),
            [one] if one.len() <= 1 => one.values().flatten().cloned().collect(),
            _ => {
                return unsupported(
                    &format!("{at}/security"),
                    "several security requirements or schemes",
                );
            }
        };
        if let Some(hole) = scopes.iter().find_map(|scope| bound(scope)) {
            return Err(Error::Hole {
                operation: op.operation_id.clone(),
                hole,
            });
        }
        Ok(Operation {
            id: op.operation_id.clone(),
            tag: op.tag().to_owned(),
            summary: op
                .summary
                .as_deref()
                .unwrap_or_default()
                .trim()
                .trim_end_matches('.')
                .to_owned(),
            doc: op
                .description
                .as_deref()
                .unwrap_or_default()
                .trim()
                .to_owned(),
            method: match method {
                "GET" => Method::Get,
                "PUT" => Method::Put,
                "POST" => Method::Post,
                "DELETE" => Method::Delete,
                _ => Method::Patch,
            },
            path: path.to_owned(),
            params,
            body,
            response,
            events,
            scopes,
        })
    }

    /// What one 2xx answer carries: a JSON body, or a stream's events.
    #[allow(clippy::type_complexity)]
    fn response(
        &mut self,
        answer: &openapi::Response,
        named: &str,
        at: &str,
    ) -> Result<(Option<Type>, Option<Vec<(String, Type)>>)> {
        let mut content = answer.content.iter();
        let Some((media, body)) = content.next() else {
            return Ok((None, None));
        };
        if content.next().is_some() {
            return unsupported(at, "several media types");
        }
        match media.as_str() {
            "application/json" => {
                let ty = match &body.schema {
                    Some(schema) => self.ty(schema, &format!("{named}Output"), at)?,
                    None => Type::Any,
                };
                Ok((Some(ty), None))
            }
            "text/event-stream" => {
                let mut events = Vec::new();
                for (event, schema) in body.events.iter().flatten() {
                    events.push((
                        event.clone(),
                        self.ty(
                            schema,
                            &format!("{named}{}", pascal(event)),
                            &format!("{at}/x-events/{event}"),
                        )?,
                    ));
                }
                Ok((None, Some(events)))
            }
            _ => unsupported(at, media),
        }
    }
}

/// The one `application/json` schema of `content`.
fn json<'a>(content: &'a BTreeMap<String, openapi::MediaType>, at: &str) -> Result<&'a Schema> {
    match content.iter().collect::<Vec<_>>()[..] {
        [
            (
                media,
                openapi::MediaType {
                    schema: Some(schema),
                    ..
                },
            ),
        ] if media == "application/json" => Ok(schema),
        _ => unsupported(at, "a body that is not one application/json schema"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The spec of a document of `paths` and `schemas`.
    fn spec(paths: &str, schemas: &str) -> Result<Spec> {
        Spec::from_openapi(&format!(
            r#"{{"openapi":"3.1.0","info":{{"title":"t","version":"1"}},"paths":{paths},"components":{{"schemas":{schemas}}}}}"#
        ))
    }

    #[test]
    fn hoists_inline_and_folds_null() {
        let schemas = r##"{"Pet":{"type":"object","required":["owner"],"properties":{
            "owner":{"oneOf":[{"type":"null"},{"type":"object","properties":{"name":{"type":"string"}}}]},
            "kind":{"type":["string","null"],"enum":["cat","dog",null]}, "label": {"$ref": "#/components/schemas/Label"}}},
            "Label": {"type": "string"}}"##;
        let spec = spec("{}", schemas).unwrap();
        let names: Vec<&str> = spec.models.iter().map(Model::name).collect();
        assert_eq!(names, ["Pet", "PetKind", "PetOwner"]);
        let Model::Struct { fields, .. } = &spec.models[0] else {
            panic!()
        };
        assert_eq!(
            fields[0].ty,
            Type::Optional(Box::new(Type::Named("PetKind".into())))
        );
        assert_eq!(fields[1].ty, Type::String);
        assert_eq!(
            fields[2].ty,
            Type::Optional(Box::new(Type::Named("PetOwner".into())))
        );
    }

    #[test]
    fn refuses_holes_clashes_and_unsupported() {
        let paths = r#"{"/x":{"get":{"operationId":"read_x","responses":{"204":{"description":""}},
            "security":[{"bearer":["a[{id}].$read"]}]}}}"#;
        assert_eq!(
            spec(paths, "{}").unwrap_err(),
            Error::Hole {
                operation: "read_x".into(),
                hole: "id".into()
            }
        );
        let paths =
            r#"{"/x":{"get":{"operationId":"read_x","responses":{"204":{"description":""}}}}}"#;
        let clash = spec(
            paths,
            r#"{"ReadXRequest":{"type":"object","properties":{"a":{}}}}"#,
        )
        .unwrap_err();
        assert_eq!(
            clash,
            Error::Collision {
                within: "operation `read_x`".into(),
                name: "ReadXRequest".into()
            }
        );
        assert!(
            spec("{}", r#"{"A":{"anyOf":[]}}"#)
                .unwrap_err()
                .to_string()
                .contains("unknown field `anyOf`")
        );
    }

    #[test]
    fn splits_templates() {
        assert_eq!(
            split("a[{id}].$read"),
            [Piece::Text("a["), Piece::Hole("id"), Piece::Text("].$read")]
        );
    }
}
