//! [`Id`]: a row id, its parts joined with dots (see `init.sql`). Each model
//! whose id has parts builds it from its own typed id (`WalletId`, …), which
//! checks the parts once and keeps the text; the model's `db` functions take
//! any `AsRef<str>`, so a typed id, an `Id` or stored text all fit.
//! `typed_id!` makes a typed id a model field: its text in JSON and in its
//! column.

use std::{fmt, ops::Deref};

/// The plumbing of typed id `$id`, whose text is its field `id` and whose
/// `parse` reads that back: `Networked` (its network at `$network`),
/// `AsRef<str>`, `Into<String>`, and its text in serde and in a diesel
/// `Text` column; a text that does not parse fails to read. Its struct
/// derives `AsExpression` and `FromSqlRow` (`sql_type = Text`); a model
/// field of it declares `#[schema(value_type = String)]`.
macro_rules! typed_id {
    ($id:ident, $($network:ident).+) => {
        impl base::Networked for $id {
            fn network(&self) -> &base::Network {
                &self.$($network).+
            }
        }

        impl AsRef<str> for $id {
            fn as_ref(&self) -> &str {
                &self.id
            }
        }

        impl From<$id> for String {
            fn from(id: $id) -> Self {
                id.id.into()
            }
        }

        impl serde::Serialize for $id {
            fn serialize<S: serde::Serializer>(&self, to: S) -> Result<S::Ok, S::Error> {
                to.serialize_str(&self.id)
            }
        }

        impl<'de> serde::Deserialize<'de> for $id {
            fn deserialize<D: serde::Deserializer<'de>>(from: D) -> Result<Self, D::Error> {
                let text = <String as serde::Deserialize>::deserialize(from)?;
                Self::parse(&text).map_err(serde::de::Error::custom)
            }
        }

        impl diesel::serialize::ToSql<diesel::sql_types::Text, diesel::sqlite::Sqlite> for $id {
            fn to_sql<'b>(
                &'b self,
                out: &mut diesel::serialize::Output<'b, '_, diesel::sqlite::Sqlite>,
            ) -> diesel::serialize::Result {
                diesel::serialize::ToSql::<diesel::sql_types::Text, _>::to_sql(&*self.id, out)
            }
        }

        impl diesel::deserialize::FromSql<diesel::sql_types::Text, diesel::sqlite::Sqlite>
            for $id
        {
            fn from_sql(
                value: diesel::sqlite::SqliteValue<'_, '_, '_>,
            ) -> diesel::deserialize::Result<Self> {
                let text: String =
                    diesel::deserialize::FromSql::<diesel::sql_types::Text, _>::from_sql(value)?;
                Ok(Self::parse(&text)?)
            }
        }
    };
}

pub(crate) use typed_id;

/// A row id: parts joined with dots, no part empty or holding one, so it
/// splits back into the same parts.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Id(String);

impl Id {
    /// `parts` joined.
    ///
    /// # Errors
    ///
    /// When a part is empty or holds a dot.
    pub(crate) fn new(parts: &[&str]) -> Result<Self, IdError> {
        if let Some(part) = parts
            .iter()
            .find(|part| part.is_empty() || part.contains('.'))
        {
            return Err(IdError(format!(
                "an id part is empty or holds a dot: {part:?}"
            )));
        }
        Ok(Self(parts.join(".")))
    }

    /// The id of a row under row `parent`: `parent` and `parts` joined.
    ///
    /// # Errors
    ///
    /// When a part is empty or holds a dot.
    pub(crate) fn child(parent: &str, parts: &[&str]) -> Result<Self, IdError> {
        Ok(Self(format!("{parent}.{}", Self::new(parts)?)))
    }

    /// `text` as exactly `N` parts, the last holding the rest.
    ///
    /// # Errors
    ///
    /// When it has fewer parts, or one is empty; `shape` names what was
    /// expected, e.g. `{chain}.{network}.{address}`.
    pub(crate) fn parts<'a, const N: usize>(
        text: &'a str,
        shape: &str,
    ) -> Result<[&'a str; N], IdError> {
        let mut split = text.splitn(N, '.');
        let parts: [&str; N] = std::array::from_fn(|_| split.next().unwrap_or_default());
        if parts.iter().any(|part| part.is_empty()) {
            return Err(IdError(format!("an id must be {shape}, not {text:?}")));
        }
        Ok(parts)
    }
}

impl Deref for Id {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for Id {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Id {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<Id> for String {
    fn from(id: Id) -> Self {
        id.0
    }
}

/// An id that doesn't split into, or can't be built from, its parts: always
/// the caller's mistake (a 400 in the API).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IdError(pub(super) String);

impl fmt::Display for IdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for IdError {}

#[cfg(test)]
mod tests {
    use super::Id;

    #[test]
    fn an_id_joins_its_parts_and_splits_back() {
        let shape = "{chain}.{network}.{address}";
        let id = Id::new(&["solana", "devnet", "Abc"]).unwrap();
        assert_eq!(&*id, "solana.devnet.Abc");
        assert_eq!(Id::parts::<3>(&id, shape), Ok(["solana", "devnet", "Abc"]));
        assert_eq!(
            &*Id::child(&id, &["usdc"]).unwrap(),
            "solana.devnet.Abc.usdc"
        );
        let short = Id::parts::<3>("solana.devnet", shape).unwrap_err();
        assert_eq!(
            short.to_string(),
            r#"an id must be {chain}.{network}.{address}, not "solana.devnet""#
        );
        assert!(Id::parts::<3>("solana..x", shape).is_err());
        assert_eq!(Id::parts::<2>("a.b.c", "{a}.{b}"), Ok(["a", "b.c"]));
        assert!(Id::new(&["a.b"]).is_err() && Id::new(&[""]).is_err());
    }
}
