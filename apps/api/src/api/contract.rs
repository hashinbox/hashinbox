use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use utoipa::{
    IntoParams, PartialSchema,
    openapi::{RefOr, Schema, schema::KnownFormat, schema::ObjectBuilder, schema::OneOfBuilder},
};

use super::Custody;

/// The one sentinel that is not a block position.
const FRESH: &str = "fresh";

// A birthday the caller did not state was once accepted silently and indexed
// nothing (`docs/design/024` section 1.2), which is why it is required here and
// has no default.
/// Where an address begins to be observed.
///
/// Required on every add path, and with no default. `0` is genesis on every
/// supported chain, so the sentinel cannot be a number — it would collide with
/// the one position a caller is most likely to mean literally.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Start {
    /// This key or address has never been used, so observation begins at the
    /// block after the index's current checkpoint and claims no history. An
    /// assertion of fact: a caller who does not know cannot honestly write it.
    Fresh,
    /// Observe from this native block position — a height on chains whose
    /// positions are heights, a slot on chains whose positions are slots.
    /// Below the checkpoint it is backfilled, never discarded, and a position
    /// further below it than the index's policy allows is refused by this
    /// call rather than by a background pass an hour later.
    At(u64),
}

impl From<Start> for indexing::Start {
    fn from(value: Start) -> Self {
        match value {
            Start::Fresh => Self::Fresh,
            Start::At(position) => Self::At(base::BlockPosition(position)),
        }
    }
}

impl Serialize for Start {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match *self {
            Self::Fresh => serializer.serialize_str(FRESH),
            Self::At(position) => serializer.serialize_u64(position),
        }
    }
}

/// Untagged, so the two forms are told apart by their JSON type and a caller
/// never writes a wrapper object around one integer.
impl<'de> Deserialize<'de> for Start {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Stated {
            Position(u64),
            Sentinel(String),
        }

        match Stated::deserialize(deserializer)? {
            Stated::Position(position) => Ok(Self::At(position)),
            Stated::Sentinel(text) if text == FRESH => Ok(Self::Fresh),
            Stated::Sentinel(text) => Err(de::Error::invalid_value(
                de::Unexpected::Str(&text),
                &"\"fresh\" or a non-negative block position",
            )),
        }
    }
}

/// Hand-written because the type is a union of two scalars: a derived schema
/// would publish an anonymous object per member, which a generator turns into
/// an unnamed client type.
impl PartialSchema for Start {
    fn schema() -> RefOr<Schema> {
        OneOfBuilder::new()
            .item(
                ObjectBuilder::new()
                    .schema_type(utoipa::openapi::Type::String)
                    .enum_values(Some([FRESH]))
                    .description(Some(
                        "This key or address has never been used: observation begins at the \
                         block after the current checkpoint and claims no history.",
                    )),
            )
            .item(
                ObjectBuilder::new()
                    .schema_type(utoipa::openapi::Type::Integer)
                    .format(Some(utoipa::openapi::SchemaFormat::KnownFormat(
                        KnownFormat::Int64,
                    )))
                    .minimum(Some(0))
                    .description(Some(
                        "Observe from this native block position. Below the checkpoint the \
                         history is backfilled; further below it than the index's policy \
                         allows, this call answers 400 naming the earliest acceptable start.",
                    )),
            )
            .description(Some(
                "Where an address begins to be observed: \"fresh\" for a key or address that \
                 has never been used, or a native block position to observe from.",
            ))
            .into()
    }
}

impl utoipa::ToSchema for Start {}

// Deliberately per read rather than per wallet: `overdrawn` is a fact about
// one fold and `incomplete` a claim about one start, and a single
// wallet-level state would have to pick one grain for both
// (`docs/design/030` section 5.5 (c)).
/// How much of the history behind an answer the index can prove it holds.
///
/// It is a property of the read, not of the wallet, and the balance and the
/// history page report it in the same four words.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Completeness {
    /// Every change the address ever made is indexed, because observation
    /// started at or before its first use.
    ///
    /// Nothing produces it today, and the index refuses to pass it on for an
    /// address whose observation began above the chain's first block. Treat its
    /// absence as "not proven", never as "wrong".
    CompleteHistory,
    /// The settled total was checked against an independent read of the same
    /// holding at the same block, and the two agreed.
    ///
    /// Only a chain that can pin such a read to the same block ever reports it,
    /// so most assets never do, and none does while that address still owes a
    /// backfill. It is evidence about the *total*, not about how far back
    /// observation reached.
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
            indexing::Completeness::VerifiedOpening => Self::VerifiedOpeningBalance,
            indexing::Completeness::Incomplete => Self::Incomplete,
            indexing::Completeness::Overdrawn => Self::Overdrawn,
        }
    }
}

/// A block reference: where a transaction was included, or a checkpoint.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Block {
    /// Chain-native position (a block height or slot).
    pub position: u64,
    /// Block height.
    pub height: u64,
    /// Lowercase hex block hash.
    pub hash: String,
    /// The parent block, when known; always present.
    #[schema(required)]
    pub parent: Option<ParentBlock>,
}

/// The parent of a [`Block`].
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ParentBlock {
    /// Chain-native position of the parent.
    pub position: u64,
    /// Lowercase hex parent hash.
    pub hash: String,
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
pub struct Network {
    /// Chain name.
    pub chain: String,
    /// Configured network name, e.g. `regtest`.
    pub name: String,
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
pub struct Wallet {
    /// Wallet ID (a UUIDv7 for wallets and addresses the API registered).
    pub id: String,
    /// The configured asset tag this wallet holds and sends.
    #[schema(pattern = "^[a-z0-9][a-z0-9-]{0,31}$")]
    pub asset: String,
    /// The chain and network of the wallet's address.
    pub network: Network,
    /// Canonical address text.
    pub address: String,
    /// Observed without a key: history only, no balance or sends.
    pub watch_only: bool,
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
pub struct WalletPath {
    /// The wallet ID a create, import, or watch call returned.
    pub id: String,
}

/// A canonical address within its network.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct Address {
    /// The address's chain and network.
    pub network: Network,
    /// Canonical address text.
    pub value: String,
}

impl From<base::Address> for Address {
    fn from(value: base::Address) -> Self {
        Self {
            network: value.network.into(),
            value: value.value,
        }
    }
}
