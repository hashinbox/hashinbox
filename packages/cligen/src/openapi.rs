//! The OpenAPI 3.1 subset cligen reads, as serde types: parsing only;
//! [`Spec::from_openapi`](crate::Spec::from_openapi) resolves it. Every type denies
//! unknown fields, so a feature outside the subset (`anyOf`, `not`, header
//! parameters, callbacks) fails to parse instead of being dropped.
//! Validation keywords (`minimum`, `pattern`, …) are read and ignored.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

/// An OpenAPI document.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Document {
    /// `3.1.x`.
    pub openapi: String,
    /// Its title and version.
    pub info: Info,
    /// Its servers, unread.
    pub servers: Option<Value>,
    /// Its routes by path.
    #[serde(default)]
    pub paths: BTreeMap<String, PathItem>,
    /// Its named schemas.
    pub components: Option<Components>,
    /// Its default security.
    pub security: Option<Vec<Requirement>>,
    /// Its tags, unread.
    pub tags: Option<Value>,
}

/// `info`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Info {
    /// The API's name.
    pub title: String,
    /// Its version.
    pub version: String,
    /// What it is.
    pub description: Option<String>,
    /// Unread.
    pub license: Option<Value>,
    /// Unread.
    pub contact: Option<Value>,
    /// Unread.
    pub terms_of_service: Option<Value>,
}

/// `components`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Components {
    /// The schemas by name.
    #[serde(default)]
    pub schemas: BTreeMap<String, Schema>,
    /// Unread: the requirements name them.
    pub security_schemes: Option<Value>,
}

/// One path's operations.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathItem {
    /// `GET`.
    pub get: Option<Operation>,
    /// `PUT`.
    pub put: Option<Operation>,
    /// `POST`.
    pub post: Option<Operation>,
    /// `DELETE`.
    pub delete: Option<Operation>,
    /// `PATCH`.
    pub patch: Option<Operation>,
    /// Unread.
    pub summary: Option<String>,
    /// Unread.
    pub description: Option<String>,
}

impl PathItem {
    /// Its operations with their methods, `GET` first.
    pub fn operations(&self) -> impl Iterator<Item = (&'static str, &Operation)> {
        let all = [
            ("GET", &self.get),
            ("PUT", &self.put),
            ("POST", &self.post),
            ("DELETE", &self.delete),
            ("PATCH", &self.patch),
        ];
        all.into_iter()
            .filter_map(|(method, op)| Some((method, op.as_ref()?)))
    }
}

/// A security requirement: scheme name → scopes.
pub type Requirement = BTreeMap<String, Vec<String>>;

/// One operation.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Operation {
    /// Its `operationId`.
    pub operation_id: String,
    /// Its tags; the first one groups it.
    #[serde(default)]
    pub tags: Vec<String>,
    /// One line on what it does.
    pub summary: Option<String>,
    /// More on what it does.
    pub description: Option<String>,
    /// Its path and query parameters.
    #[serde(default)]
    pub parameters: Vec<Parameter>,
    /// Its body.
    pub request_body: Option<RequestBody>,
    /// Its answers by status.
    pub responses: BTreeMap<String, Response>,
    /// What a caller needs; none: the document's.
    pub security: Option<Vec<Requirement>>,
    /// Unread.
    pub deprecated: Option<bool>,
}

impl Operation {
    /// Its path parameters, in order.
    pub fn path_params(&self) -> impl Iterator<Item = &Parameter> {
        self.parameters
            .iter()
            .filter(|param| param.at == Location::Path)
    }

    /// Its query parameters, in order.
    pub fn query_params(&self) -> impl Iterator<Item = &Parameter> {
        self.parameters
            .iter()
            .filter(|param| param.at == Location::Query)
    }

    /// The tag it is grouped by: its first, else `default`.
    pub fn tag(&self) -> &str {
        self.tags.first().map_or("default", String::as_str)
    }

    /// Its requirements: its own `security`, else `fallback` (the
    /// document's).
    pub fn requirements<'a>(&'a self, fallback: Option<&'a Vec<Requirement>>) -> &'a [Requirement] {
        self.security
            .as_ref()
            .or(fallback)
            .map_or(&[], Vec::as_slice)
    }
}

/// Where a parameter goes. Headers and cookies are not supported.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Location {
    /// In the path.
    Path,
    /// In the query.
    Query,
}

/// A parameter.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Parameter {
    /// Its name.
    pub name: String,
    /// Where it goes.
    #[serde(rename = "in")]
    pub at: Location,
    /// What it is.
    pub description: Option<String>,
    /// Whether it must be given.
    #[serde(default)]
    pub required: bool,
    /// Its type.
    pub schema: Schema,
    /// Unread.
    pub deprecated: Option<bool>,
    /// Unread.
    pub example: Option<Value>,
}

/// A request body.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestBody {
    /// What it is.
    pub description: Option<String>,
    /// Its media types.
    pub content: BTreeMap<String, MediaType>,
    /// Whether it must be sent.
    #[serde(default)]
    pub required: bool,
}

/// An answer.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    /// What it means.
    pub description: String,
    /// Its media types; none: no body.
    #[serde(default)]
    pub content: BTreeMap<String, MediaType>,
}

/// A body of one media type.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaType {
    /// Its schema.
    pub schema: Option<Schema>,
    /// A `text/event-stream`'s events: name → data schema.
    #[serde(rename = "x-events")]
    pub events: Option<BTreeMap<String, Schema>>,
    /// Unread.
    pub example: Option<Value>,
}

/// A schema. All of it optional: `{}` is any value.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Schema {
    /// `#/components/schemas/Name`.
    #[serde(rename = "$ref")]
    pub reference: Option<String>,
    /// `"string"`, or `["string", "null"]`.
    #[serde(rename = "type")]
    pub types: Option<Types>,
    /// `int32`, `int64`, `uuid`, …
    pub format: Option<String>,
    /// What it is.
    pub description: Option<String>,
    /// Unread.
    pub title: Option<String>,
    /// Its values.
    #[serde(rename = "enum")]
    pub values: Option<Vec<Value>>,
    /// An object's fields.
    #[serde(default)]
    pub properties: BTreeMap<String, Schema>,
    /// The fields always present.
    #[serde(default)]
    pub required: Vec<String>,
    /// A map's values, or whether other fields are allowed.
    pub additional_properties: Option<Extra>,
    /// Unread: keys are strings.
    pub property_names: Option<Box<Schema>>,
    /// An array's items.
    pub items: Option<Box<Schema>>,
    /// One of these.
    #[serde(default)]
    pub one_of: Vec<Schema>,
    /// All of these (objects, flattened).
    #[serde(default)]
    pub all_of: Vec<Schema>,
    /// Validation and examples, unread.
    pub default: Option<Value>,
    /// Unread.
    pub example: Option<Value>,
    /// Unread.
    pub examples: Option<Value>,
    /// Unread.
    pub minimum: Option<Value>,
    /// Unread.
    pub maximum: Option<Value>,
    /// Unread.
    pub min_length: Option<Value>,
    /// Unread.
    pub max_length: Option<Value>,
    /// Unread.
    pub min_items: Option<Value>,
    /// Unread.
    pub max_items: Option<Value>,
    /// Unread.
    pub pattern: Option<Value>,
    /// Unread.
    pub deprecated: Option<Value>,
    /// Unread.
    pub read_only: Option<Value>,
    /// Unread.
    pub write_only: Option<Value>,
}

/// `type`: one, or several.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum Types {
    /// `"string"`.
    One(String),
    /// `["string", "null"]`.
    Many(Vec<String>),
}

/// `additionalProperties`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum Extra {
    /// `true`: any; `false`: none.
    Allowed(bool),
    /// The values' schema.
    Values(Box<Schema>),
}

impl Schema {
    /// Its types without `null`.
    pub fn types(&self) -> Vec<&str> {
        let all = match &self.types {
            None => Vec::new(),
            Some(Types::One(one)) => vec![one.as_str()],
            Some(Types::Many(many)) => many.iter().map(String::as_str).collect(),
        };
        all.into_iter().filter(|ty| *ty != "null").collect()
    }

    /// Whether `null` is one of its types or values.
    pub fn is_nullable(&self) -> bool {
        let typed = match &self.types {
            Some(Types::One(one)) => one == "null",
            Some(Types::Many(many)) => many.iter().any(|ty| ty == "null"),
            None => false,
        };
        typed || self.values.iter().flatten().any(Value::is_null)
    }

    /// Whether it is `{"type": "null"}` alone.
    pub fn is_null(&self) -> bool {
        self.is_nullable() && self.types().is_empty() && self.values.is_none()
    }

    /// The schema name its `$ref` names, if it is a local one.
    pub fn ref_name(&self) -> Option<&str> {
        self.reference
            .as_deref()?
            .strip_prefix("#/components/schemas/")
    }
}

impl Document {
    /// Parses `json`.
    ///
    /// # Errors
    ///
    /// serde's message, with line and column, for text outside the subset.
    pub fn parse(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// The schema `name` of the components.
    pub fn schema(&self, name: &str) -> Option<&Schema> {
        self.components.as_ref()?.schemas.get(name)
    }

    /// The named schemas.
    pub fn schemas(&self) -> impl Iterator<Item = (&String, &Schema)> {
        self.components
            .iter()
            .flat_map(|components| &components.schemas)
    }

    /// Every operation with its path and method, by path.
    pub fn operations(&self) -> impl Iterator<Item = (&str, &'static str, &Operation)> {
        self.paths.iter().flat_map(|(path, item)| {
            item.operations()
                .map(move |(method, op)| (path.as_str(), method, op))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_fields_fail() {
        let error = serde_json::from_str::<Schema>(r#"{"anyOf": []}"#).unwrap_err();
        assert!(error.to_string().contains("unknown field `anyOf`"));
        let error =
            serde_json::from_str::<Parameter>(r#"{"name": "x", "in": "header", "schema": {}}"#)
                .unwrap_err();
        assert!(error.to_string().contains("unknown variant `header`"));
    }

    #[test]
    fn schemas_read_as_written() {
        let schema: Schema =
            serde_json::from_str(r#"{"type": ["integer", "null"], "format": "int64"}"#).unwrap();
        assert_eq!(schema.types(), ["integer"]);
        assert!(schema.is_nullable());
        let any: Schema = serde_json::from_str(r#"{"description": "anything"}"#).unwrap();
        assert!(any.types().is_empty() && !any.is_nullable());
        let null: Schema = serde_json::from_str(r#"{"type": "null"}"#).unwrap();
        assert!(null.is_null());
    }
}
