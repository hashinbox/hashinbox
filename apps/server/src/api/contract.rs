use serde::{Deserialize, Serialize};
use utoipa::IntoParams;

use super::Custody;

// Deliberately per read rather than per wallet: `overdrawn` is a fact about
// one fold and `incomplete` a claim about one start, and a single
// wallet-level state would have to pick one grain for both.
/// How much of the history behind an answer the index can prove it holds.
///
/// It is a property of the read, not of the wallet, and the balance and the
/// history page share the vocabulary; opening evidence applies only to balances.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum Completeness {
    /// Every change the address ever made is indexed, because observation
    /// started at or before its first use.
    ///
    /// The repository claims it only for an address observed from the chain's
    /// first block and inspected up to the line the answer was read at. Treat
    /// its absence as "not proven", never as "wrong".
    CompleteHistory,
    /// An independent read verified an opening amount at the same block,
    /// and the index has continuously inspected subsequent confirmed changes.
    /// Balance reads may report this when complete history is not proven;
    /// it never replaces `overdrawn` and history reads cannot claim it.
    VerifiedOpeningBalance,
    /// Every change since observation began is indexed, and nothing proves it
    /// began early enough. The ordinary answer.
    Incomplete,
    /// The fold went negative, which a complete index cannot do: more left the
    /// address than ever reached it, so credits are provably missing and
    /// `amount` is zero because no honest number exists.
    Overdrawn,
}

impl From<indexing::Completeness> for Completeness {
    fn from(value: indexing::Completeness) -> Self {
        match value {
            indexing::Completeness::CompleteHistory => Self::CompleteHistory,
            indexing::Completeness::Incomplete => Self::Incomplete,
            indexing::Completeness::Overdrawn => Self::Overdrawn,
        }
    }
}

/// A block reference: where a transaction was included, or a checkpoint.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct Block {
    /// Chain-native position (a block height or slot).
    pub(super) position: u64,
    /// Block height.
    pub(super) height: u64,
    /// Lowercase hex block hash.
    pub(super) hash: String,
    /// The parent block, when known; always present.
    #[schema(required)]
    pub(super) parent: Option<ParentBlock>,
}

/// The parent of a [`Block`].
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct ParentBlock {
    /// Chain-native position of the parent.
    position: u64,
    /// Lowercase hex parent hash.
    hash: String,
}

impl From<base::BlockRef> for Block {
    fn from(value: base::BlockRef) -> Self {
        Self::from(&value)
    }
}

impl From<&base::BlockRef> for Block {
    fn from(value: &base::BlockRef) -> Self {
        Self {
            position: value.position.0,
            height: value.height.0,
            hash: hex::encode(&value.hash.0),
            parent: value.parent.as_ref().map(|parent| ParentBlock {
                position: parent.position.0,
                hash: hex::encode(&parent.hash.0),
            }),
        }
    }
}

/// The chain and network a wallet, address, or transaction belongs to. It
/// always ships as one nested object, never as two sibling fields.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub(super) struct Network {
    /// Chain name.
    pub(super) chain: String,
    /// Configured network name, e.g. `regtest`.
    pub(super) name: String,
}

impl From<base::Network> for Network {
    fn from(value: base::Network) -> Self {
        Self {
            chain: value.chain.to_string(),
            name: value.name,
        }
    }
}

/// Public wallet fields returned by wallet creation, import, watch, lookup,
/// and listing.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub(crate) struct Wallet {
    /// Wallet ID (a UUIDv7 for wallets and addresses the API registered).
    id: String,
    /// The configured asset tag this wallet holds and sends.
    #[schema(pattern = "^[a-z0-9][a-z0-9-]{0,31}$")]
    asset: String,
    /// The chain and network of the wallet's address.
    network: Network,
    /// Canonical address text.
    address: String,
    /// Observed without a key: history only, no balance or sends.
    watch_only: bool,
}

/// The same two facts as before, now each read from its owner: the index says
/// what it observes, and the process's key store says whether it can sign.
impl From<Custody> for Wallet {
    fn from(value: Custody) -> Self {
        Self {
            watch_only: value.wallet.is_none(),
            id: value.watch.id,
            asset: value.tag.to_string(),
            network: value.watch.address.network.into(),
            address: value.watch.address.value,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, IntoParams)]
#[into_params(parameter_in = Path)]
pub(super) struct WalletPath {
    /// The wallet ID a create or watch call returned.
    pub(super) id: String,
}

/// A canonical address within its network.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub(super) struct Address {
    /// The address's chain and network.
    pub(super) network: Network,
    /// Canonical address text.
    pub(super) value: String,
}

impl From<base::Address> for Address {
    fn from(value: base::Address) -> Self {
        Self {
            network: value.network.into(),
            value: value.value,
        }
    }
}
