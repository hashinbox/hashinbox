//! Which wallets a trigger reads, and their balances at the confirmed block.
//!
//! A [`Selector`] picks wallet indexes from a [`Store`](crate::Store), a [`Target`]
//! names them from inside a trigger, and a [`Selection`] holds what was
//! picked, by id.

use std::collections::BTreeMap;

use anyhow::ensure;
use base::{Address, Asset, Decimal, Network, Spends};
use serde::{Deserialize, Serialize};

use crate::error::{Class, Error};

/// One asset of one address, fully indexed, with its balances at the
/// network's confirmed block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Index {
    /// `{chain}.{network}.{address}.{asset}`: what [`Selector::Ids`] names.
    pub id: String,
    /// The address it indexes.
    pub address: Address,
    /// The asset it indexes.
    pub asset: Asset,
    /// What it can spend at the confirmed block: what a reorg cannot undo,
    /// less what is on its way out. Atomic units.
    pub confirmed: Decimal,
    /// What it will hold once every pending transaction lands, in and out.
    /// Atomic units.
    pub pending: Decimal,
    /// Whether its wallet's key is kept here.
    pub keyed: bool,
}

impl Index {
    /// The index of `asset` of `address`, holding nothing, its key not kept.
    #[must_use]
    pub fn new(address: Address, asset: Asset) -> Self {
        let network = &address.network;
        let id = format!(
            "{}.{}.{}.{}",
            network.chain, network.name, address.value, asset.id
        );
        Self {
            id,
            address,
            asset,
            confirmed: Decimal::zero(),
            pending: Decimal::zero(),
            keyed: false,
        }
    }

    /// The balance `kind` names.
    #[must_use]
    pub const fn held(&self, kind: BalanceKind) -> &Decimal {
        match kind {
            BalanceKind::Confirmed => &self.confirmed,
            BalanceKind::Pending => &self.pending,
        }
    }
}

/// Which balance a trigger reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BalanceKind {
    /// Spendable at the confirmed block. For how much to send.
    #[default]
    Confirmed,
    /// What it will hold once every pending transaction lands. For whether
    /// it still needs something: what is on its way in counts.
    Pending,
}

impl BalanceKind {
    /// Whether it is the default, [`BalanceKind::Confirmed`]: the stored
    /// JSON leaves it out.
    #[must_use]
    pub const fn is_confirmed(&self) -> bool {
        matches!(self, Self::Confirmed)
    }
}

/// Wallet indexes of one network, by id: what a trigger selects and
/// decides on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    network: Network,
    indexes: BTreeMap<String, Index>,
}

impl Selection {
    /// None yet, of `network`.
    #[must_use]
    pub const fn new(network: Network) -> Self {
        Self {
            network,
            indexes: BTreeMap::new(),
        }
    }

    /// `indexes`, of `network`.
    ///
    /// # Errors
    ///
    /// When one is of another network.
    pub fn of(network: Network, indexes: impl IntoIterator<Item = Index>) -> anyhow::Result<Self> {
        let mut selection = Self::new(network);
        for one in indexes {
            selection.insert(one)?;
        }
        Ok(selection)
    }

    /// The network they are of.
    #[must_use]
    pub const fn network(&self) -> &Network {
        &self.network
    }

    /// Adds `one`.
    ///
    /// # Errors
    ///
    /// When it is of another network.
    pub fn insert(&mut self, one: Index) -> anyhow::Result<()> {
        ensure!(
            one.address.network == self.network,
            "a wallet index is of another network"
        );
        self.indexes.insert(one.id.clone(), one);
        Ok(())
    }

    /// `one` alone, of the same network.
    ///
    /// # Errors
    ///
    /// When it is of another network.
    pub fn only(&self, one: &Index) -> anyhow::Result<Self> {
        Self::of(self.network.clone(), [one.clone()])
    }

    /// Their ids, in order.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        self.indexes.keys().cloned().collect()
    }

    /// What sending everything each one holds to `to` (`None`: a new
    /// address) is: one spend per index with a spendable balance. One that
    /// holds nothing sends nothing, so it is left out.
    #[must_use]
    pub fn spends(&self, to: Option<&Address>) -> Spends {
        self.indexes
            .values()
            .filter(|one| one.confirmed > Decimal::zero())
            .map(|one| base::Spend {
                from: one.address.clone(),
                asset: one.asset.clone(),
                to: to.cloned(),
            })
            .collect()
    }

    /// The one address all of them are of, if they share one.
    #[must_use]
    pub fn address(&self) -> Option<&Address> {
        let mut addresses = self.indexes.values().map(|one| &one.address);
        let first = addresses.next()?;
        addresses.all(|other| other == first).then_some(first)
    }
}

impl std::ops::Deref for Selection {
    type Target = BTreeMap<String, Index>;

    fn deref(&self) -> &Self::Target {
        &self.indexes
    }
}

/// A set of a network's fully indexed wallet indexes. Each store translates
/// it: SQL in the server, a `match` in `memory`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Selector {
    /// These wallet indexes, by id (`chain.network.address.asset`).
    Ids(Vec<String>),
    /// Every wallet index of this asset id.
    Asset(String),
    /// Every wallet index whose wallet carries this tag.
    Tag(String),
    /// Every wallet index whose wallet carries no tag at all.
    Untagged {},
    /// In every one of these (AND; empty: all).
    All(Vec<Self>),
    /// In any of these (OR; empty: none).
    Any(Vec<Self>),
    /// Not in this one (relative to the network's fully indexed indexes).
    Not(Box<Self>),
}

impl Selector {
    /// Whether it names wallets on purpose: no empty group, no "not" right
    /// inside a "not", no empty id list, no empty asset, every tag well
    /// formed.
    ///
    /// # Errors
    ///
    /// The first part that does not.
    pub fn validate(&self) -> Result<(), Error> {
        match self {
            Self::Ids(ids) if ids.is_empty() => Err(Error::new(
                Class::EmptySelection,
                "a wallet list picks no wallet",
            )),
            Self::Asset(asset) if asset.is_empty() => {
                Err(Error::new(Class::EmptySelection, "an asset is named"))
            }
            Self::Tag(tag) if !is_tag(tag) => Err(Error::new(
                Class::Malformed,
                "a tag is 1 to 64 of [a-z0-9-]",
            )),
            Self::All(all) if all.is_empty() => Err(Error::new(
                Class::EmptySelection,
                "an empty \"all of\" would select every wallet; name them by tag, asset or id",
            )),
            Self::Any(any) if any.is_empty() => Err(Error::new(
                Class::EmptySelection,
                "an empty \"any of\" selects no wallet",
            )),
            Self::All(many) | Self::Any(many) => many.iter().try_for_each(Self::validate),
            Self::Not(one) if matches!(**one, Self::Not(_)) => Err(Error::new(
                Class::Redundant,
                "a \"not\" inside a \"not\" cancels out",
            )),
            Self::Not(one) => one.validate(),
            Self::Ids(_) | Self::Asset(_) | Self::Tag(_) | Self::Untagged {} => Ok(()),
        }
    }

    /// The one asset it is restricted to: an `Asset` at its top or directly
    /// under a top `All`.
    #[must_use]
    pub fn asset(&self) -> Option<&str> {
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

/// Whether `tag` is 1 to 64 of `[a-z0-9-]`.
fn is_tag(tag: &str) -> bool {
    (1..=64).contains(&tag.len())
        && tag
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The wallets being evaluated, read as they are or in another asset.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct This {
    /// Read the same addresses in this asset instead; `None`: as they are.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset: Option<String>,
}

/// What a balance, fee, compare or action refers to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    /// The wallets being evaluated.
    This(This),
    /// An absolute choice of the network's wallet indexes.
    Select(Selector),
}

impl Target {
    /// The one asset id its balances are in, when it names one; plain
    /// `This` names none (it is in the asset of the wallets evaluated).
    #[must_use]
    pub fn asset(&self) -> Option<&str> {
        match self {
            Self::This(This { asset }) => asset.as_deref(),
            Self::Select(selector) => selector.asset(),
        }
    }

    /// Whether a chosen set names wallets on purpose ([`Selector::validate`]).
    ///
    /// # Errors
    ///
    /// The selector's.
    pub fn validate(&self) -> Result<(), Error> {
        match self {
            Self::This(_) => Ok(()),
            Self::Select(selector) => selector.validate(),
        }
    }

    /// Whether it is `This`.
    #[must_use]
    pub const fn is_this(&self) -> bool {
        matches!(self, Self::This(_))
    }
}

impl Default for Target {
    fn default() -> Self {
        Self::This(This::default())
    }
}
