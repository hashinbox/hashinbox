//! [`Json`]: a column holding a value as JSON text.

use std::fmt::Debug;

use diesel::{
    deserialize::{self, FromSql, FromSqlRow},
    expression::AsExpression,
    serialize::{self, IsNull, Output, ToSql},
    sql_types::Text,
    sqlite::{Sqlite, SqliteValue},
};
use serde::{Serialize, de::DeserializeOwned};

/// `T` stored as its JSON text.
#[derive(Clone, Debug, PartialEq, Eq, AsExpression, FromSqlRow)]
#[diesel(sql_type = Text)]
pub(crate) struct Json<T>(pub(crate) T);

impl<T: Serialize + Debug> ToSql<Text, Sqlite> for Json<T> {
    fn to_sql<'b>(&'b self, out: &mut Output<'b, '_, Sqlite>) -> serialize::Result {
        out.set_value(serde_json::to_string(&self.0)?);
        Ok(IsNull::No)
    }
}

/// Serialized as the value itself.
impl<T: Serialize> Serialize for Json<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<T: DeserializeOwned> FromSql<Text, Sqlite> for Json<T> {
    fn from_sql(value: SqliteValue<'_, '_, '_>) -> deserialize::Result<Self> {
        let text = <String as FromSql<Text, Sqlite>>::from_sql(value)?;
        Ok(Self(serde_json::from_str(&text)?))
    }
}

/// [`scope::Scopes`] stored as the JSON [`scope::Scopes::pack`] makes
/// (`{"com.hashinbox": ["wallets.$read"]}`): a `scopes` column, read into
/// the model's `Scopes` (`deserialize_as`).
#[derive(Clone, Debug, PartialEq, Eq, AsExpression, FromSqlRow)]
#[diesel(sql_type = Text)]
pub(crate) struct PackedScopes(pub(crate) scope::Scopes);

impl ToSql<Text, Sqlite> for PackedScopes {
    fn to_sql<'b>(&'b self, out: &mut Output<'b, '_, Sqlite>) -> serialize::Result {
        out.set_value(serde_json::to_string(&self.0.pack())?);
        Ok(IsNull::No)
    }
}

impl FromSql<Text, Sqlite> for PackedScopes {
    fn from_sql(value: SqliteValue<'_, '_, '_>) -> deserialize::Result<Self> {
        let text = <String as FromSql<Text, Sqlite>>::from_sql(value)?;
        Ok(Self(scope::Scopes::unpack(&serde_json::from_str(&text)?)?))
    }
}

impl From<PackedScopes> for scope::Scopes {
    fn from(packed: PackedScopes) -> Self {
        packed.0
    }
}

/// `scopes` in JSON as [`scope::Scopes::pack`] makes them: a model's
/// `serialize_with`.
pub(crate) fn serialize_packed<S: serde::Serializer>(
    scopes: &scope::Scopes,
    to: S,
) -> Result<S::Ok, S::Error> {
    scopes.pack().serialize(to)
}

/// Whether `value` is there, in JSON: a model's `serialize_with` for a
/// column only its presence of which is shown.
pub(crate) fn serialize_is_some<T, S: serde::Serializer>(
    value: &Option<T>,
    to: S,
) -> Result<S::Ok, S::Error> {
    to.serialize_bool(value.is_some())
}
