//! `Selector`: a set of wallet indexes a trigger picks, resolved in SQL.

use serde::{Deserialize, Serialize};

/// A set of wallet indexes; resolved in SQL through [`Selector::sql`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Selector {
    /// These wallet indexes, by id (`chain.network.address.asset`).
    Ids(Vec<String>),
    /// Every wallet index of this asset id.
    Asset(String),
    /// Every wallet index whose wallet carries this tag.
    Tag(String),
    /// Every wallet index whose wallet carries no tag at all.
    Untagged {},
    /// In every one of these (AND; empty: all).
    All(Vec<Selector>),
    /// In any of these (OR; empty: none).
    Any(Vec<Selector>),
    /// Not in this one (relative to the network's fully indexed indexes).
    Not(Box<Selector>),
}

impl Selector {
    /// Whether it names wallets on purpose: no empty group (an empty "all
    /// of" would be every wallet, an empty "any of" none), no "not" right
    /// inside a "not", no empty id list, no empty asset, every tag well
    /// formed.
    ///
    /// # Errors
    ///
    /// The first part that does not.
    pub(crate) fn validate(&self) -> Result<(), super::trigger::Invalid> {
        use super::trigger::Invalid;
        match self {
            Self::Ids(ids) if ids.is_empty() => Err(Invalid::new("a wallet list picks no wallet")),
            Self::Ids(_) => Ok(()),
            Self::Asset(asset) if asset.is_empty() => Err(Invalid::new("an asset is named")),
            Self::Asset(_) => Ok(()),
            Self::Tag(tag) if !super::wallet::is_tag(tag) => {
                Err(Invalid::new("a tag is 1 to 64 of [a-z0-9-]"))
            }
            Self::Tag(_) | Self::Untagged {} => Ok(()),
            Self::All(all) if all.is_empty() => Err(Invalid::new(
                "an empty \"all of\" would select every wallet; name them by tag, asset or id",
            )),
            Self::Any(any) if any.is_empty() => {
                Err(Invalid::new("an empty \"any of\" selects no wallet"))
            }
            Self::All(many) | Self::Any(many) => many.iter().try_for_each(Self::validate),
            Self::Not(one) if matches!(**one, Self::Not(_)) => {
                Err(Invalid::new("a \"not\" inside a \"not\" cancels out"))
            }
            Self::Not(one) => one.validate(),
        }
    }
}

impl Selector {
    /// It as one SQL condition over `wi` (`wallet_index`) and `w` (its
    /// `wallet`), its values pushed to `binds` in placeholder order: `All`
    /// is AND (empty: true), `Any` OR (empty: false), `Not` NOT.
    pub(crate) fn sql(&self, binds: &mut Vec<String>) -> String {
        let group = |many: &[Self], binds: &mut Vec<String>, join: &str, empty: &str| {
            if many.is_empty() {
                return empty.to_owned();
            }
            let parts: Vec<String> = many.iter().map(|one| one.sql(binds)).collect();
            format!("({})", parts.join(join))
        };
        match self {
            Self::Ids(ids) => {
                binds.extend(ids.iter().cloned());
                format!("wi.id IN ({})", vec!["?"; ids.len()].join(", "))
            }
            Self::Asset(asset) => {
                binds.push(asset.clone());
                "wi.asset = ?".to_owned()
            }
            Self::Tag(name) => {
                binds.push(name.clone());
                "EXISTS (SELECT 1 FROM json_each(w.tags) j WHERE j.value = ?)".to_owned()
            }
            Self::Untagged {} => "json_array_length(w.tags) = 0".to_owned(),
            Self::All(all) => group(all, binds, " AND ", "1"),
            Self::Any(any) => group(any, binds, " OR ", "0"),
            Self::Not(inner) => format!("NOT ({})", inner.sql(binds)),
        }
    }

    /// The one asset it is restricted to: an `Asset` leaf at its top or
    /// directly under a top `All`.
    pub(crate) fn asset(&self) -> Option<&str> {
        match self {
            Self::Asset(asset) => Some(asset),
            Self::All(all) => all.iter().find_map(|one| match one {
                Self::Asset(asset) => Some(asset.as_str()),
                _ => None,
            }),
            _ => None,
        }
    }
}
