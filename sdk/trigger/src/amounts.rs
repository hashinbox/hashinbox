//! How much: the [`Amount`] trait, its building blocks [`Value`],
//! [`Balance`] and [`Fee`], and [`Arithmetic`] over any of them.

use base::{Asset, Decimal, Future};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::DeserializeOwned};

use crate::{
    error::{Class, Error},
    selection::{BalanceKind, Target},
    trigger::Context,
};

/// An amount in atomic units of an asset; `None`: a term is unknown.
pub trait Amount: Send + Sync {
    /// It in atomic units of `asset`, over the wallets `on` evaluates.
    fn atomic<'a>(
        &'a self,
        on: &'a mut Context<'_>,
        asset: &'a Asset,
    ) -> Future<'a, anyhow::Result<Option<Decimal>>>;

    /// The rules it must keep to be stored.
    ///
    /// # Errors
    ///
    /// The first rule it breaks.
    fn validate(&self) -> Result<(), Error>;

    /// Whether it reads `this`, at any depth.
    fn reads_this(&self) -> bool;

    /// Its fixed display value when it is one: a count compares only with a
    /// whole one.
    fn constant(&self) -> Option<&Decimal> {
        None
    }
}

/// A fixed value in the asset's display units: `{"value": "1.5"}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Value(#[serde(with = "text")] pub Decimal);

impl Amount for Value {
    fn atomic<'a>(
        &'a self,
        on: &'a mut Context<'_>,
        asset: &'a Asset,
    ) -> Future<'a, anyhow::Result<Option<Decimal>>> {
        Box::pin(std::future::ready(on.atomic(asset, &self.0).map(Some)))
    }

    fn validate(&self) -> Result<(), Error> {
        Ok(())
    }

    fn reads_this(&self) -> bool {
        false
    }

    fn constant(&self) -> Option<&Decimal> {
        Some(&self.0)
    }
}

/// The summed `balance` of `of`: `{"balance": {"of": …, "balance": "pending"}}`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Balance {
    /// Whose balance; `This {}` by default.
    #[serde(default)]
    pub of: Target,
    /// Which balance; confirmed by default.
    #[serde(default, skip_serializing_if = "BalanceKind::is_confirmed")]
    pub balance: BalanceKind,
}

impl Amount for Balance {
    fn atomic<'a>(
        &'a self,
        on: &'a mut Context<'_>,
        _asset: &'a Asset,
    ) -> Future<'a, anyhow::Result<Option<Decimal>>> {
        Box::pin(async move {
            let Some(wallets) = on.wallets(&self.of).await? else {
                return Ok(None);
            };
            Ok(Some(
                wallets.values().map(|one| one.held(self.balance)).sum(),
            ))
        })
    }

    fn validate(&self) -> Result<(), Error> {
        self.of.validate()
    }

    fn reads_this(&self) -> bool {
        self.of.is_this()
    }
}

/// The native fee of sending everything `of` holds to `to`:
/// `{"fee": {"of": …, "to": …}}`. One recipient address is priced as it is;
/// none named, or several, as a new address. Wallets holding nothing cost 0,
/// and nothing is asked.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fee {
    /// Who sends; `This {}` by default.
    #[serde(default)]
    pub of: Target,
    /// To whom; `None`: a new address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<Target>,
}

impl Amount for Fee {
    fn atomic<'a>(
        &'a self,
        on: &'a mut Context<'_>,
        _asset: &'a Asset,
    ) -> Future<'a, anyhow::Result<Option<Decimal>>> {
        Box::pin(async move {
            let Some(senders) = on.wallets(&self.of).await? else {
                return Ok(None);
            };
            // One recipient address is priced as it is; several as a new one.
            let to = match &self.to {
                Some(to) => on.wallets(to).await?.map(|to| to.address().cloned()),
                None => Some(None),
            };
            let Some(to) = to else {
                return Ok(None);
            };
            let spends = senders.spends(to.as_ref());
            if spends.is_empty() {
                return Ok(Some(Decimal::zero()));
            }
            Ok(Some(on.fees().estimate(&spends).await?))
        })
    }

    fn validate(&self) -> Result<(), Error> {
        self.of.validate()?;
        let Some(to) = &self.to else {
            return Ok(());
        };
        to.validate()
    }

    fn reads_this(&self) -> bool {
        self.of.is_this()
    }
}

/// Arithmetic over amounts of one kind. In JSON a term is written bare; the
/// operations are `{"add": [a, b]}`, `{"sub": …}`, `{"min": …}`, `{"max": …}`
/// and `{"times": [a, "1.25"]}`. Unknown in, unknown out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Arithmetic<A> {
    /// One term.
    Term(A),
    /// `a + b`.
    Add(Box<Self>, Box<Self>),
    /// `a - b`.
    Sub(Box<Self>, Box<Self>),
    /// The smaller.
    Min(Box<Self>, Box<Self>),
    /// The larger.
    Max(Box<Self>, Box<Self>),
    /// An amount times a positive factor, rounded up to a whole atomic
    /// unit: headroom over a fee, say.
    Times(Box<Self>, Decimal),
}

impl<A: Amount> Amount for Arithmetic<A> {
    fn atomic<'a>(
        &'a self,
        on: &'a mut Context<'_>,
        asset: &'a Asset,
    ) -> Future<'a, anyhow::Result<Option<Decimal>>> {
        Box::pin(async move {
            let (a, b, combine): (_, _, fn(Decimal, Decimal) -> Decimal) = match self {
                Self::Term(one) => return one.atomic(on, asset).await,
                Self::Times(one, factor) => {
                    let one = one.atomic(on, asset).await?;
                    return Ok(one.map(|one| up(&(one * factor))));
                }
                Self::Add(a, b) => (a, b, |a, b| a + b),
                Self::Sub(a, b) => (a, b, |a, b| a - b),
                Self::Min(a, b) => (a, b, Decimal::min),
                Self::Max(a, b) => (a, b, Decimal::max),
            };
            let a = a.atomic(on, asset).await?;
            let b = b.atomic(on, asset).await?;
            Ok(a.zip(b).map(|(a, b)| combine(a, b)))
        })
    }

    fn validate(&self) -> Result<(), Error> {
        match self {
            Self::Term(one) => one.validate(),
            Self::Times(_, factor) if *factor <= Decimal::zero() => Err(Error::new(
                Class::OutOfRange,
                "an amount is multiplied by a positive factor",
            )),
            Self::Times(one, _) => one.validate(),
            Self::Add(a, b) | Self::Sub(a, b) | Self::Min(a, b) | Self::Max(a, b) => {
                a.validate()?;
                b.validate()
            }
        }
    }

    fn reads_this(&self) -> bool {
        match self {
            Self::Term(one) => one.reads_this(),
            Self::Times(one, _) => one.reads_this(),
            Self::Add(a, b) | Self::Sub(a, b) | Self::Min(a, b) | Self::Max(a, b) => {
                a.reads_this() || b.reads_this()
            }
        }
    }

    fn constant(&self) -> Option<&Decimal> {
        match self {
            Self::Term(one) => one.constant(),
            _ => None,
        }
    }
}

/// `amount` rounded up to a whole atomic unit.
fn up(amount: &Decimal) -> Decimal {
    let whole = amount.truncate(0);
    if whole < *amount {
        return whole + Decimal::from(1_u64);
    }
    whole
}

impl<A: Serialize> Serialize for Arithmetic<A> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let (index, name, a, b) = match self {
            Self::Term(one) => return one.serialize(serializer),
            Self::Times(one, factor) => {
                let pair = (one, factor.to_string());
                return serializer.serialize_newtype_variant("Arithmetic", 4, "times", &pair);
            }
            Self::Add(a, b) => (0, "add", a, b),
            Self::Sub(a, b) => (1, "sub", a, b),
            Self::Min(a, b) => (2, "min", a, b),
            Self::Max(a, b) => (3, "max", a, b),
        };
        serializer.serialize_newtype_variant("Arithmetic", index, name, &(a, b))
    }
}

impl<'de, A: DeserializeOwned> Deserialize<'de> for Arithmetic<A> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match Form::<A>::deserialize(deserializer)? {
            Form::Operation(Operation::Add(a, b)) => Self::Add(a, b),
            Form::Operation(Operation::Sub(a, b)) => Self::Sub(a, b),
            Form::Operation(Operation::Min(a, b)) => Self::Min(a, b),
            Form::Operation(Operation::Max(a, b)) => Self::Max(a, b),
            Form::Operation(Operation::Times(one, factor)) => Self::Times(one, factor),
            Form::Term(one) => Self::Term(one),
        })
    }
}

/// The JSON form: an operation, else a bare term.
#[derive(Deserialize)]
#[serde(untagged, bound(deserialize = "A: DeserializeOwned"))]
enum Form<A> {
    Operation(Operation<A>),
    Term(A),
}

/// The tagged operations.
#[derive(Deserialize)]
#[serde(rename_all = "snake_case", bound(deserialize = "A: DeserializeOwned"))]
enum Operation<A> {
    Add(Box<Arithmetic<A>>, Box<Arithmetic<A>>),
    Sub(Box<Arithmetic<A>>, Box<Arithmetic<A>>),
    Min(Box<Arithmetic<A>>, Box<Arithmetic<A>>),
    Max(Box<Arithmetic<A>>, Box<Arithmetic<A>>),
    Times(Box<Arithmetic<A>>, #[serde(with = "text")] Decimal),
}

/// A [`Decimal`] as its exact text.
mod text {
    use base::Decimal;
    use serde::{Deserialize, Deserializer, Serializer, de::Error as _};

    pub(super) fn serialize<S: Serializer>(
        value: &Decimal,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_str(value)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Decimal, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(D::Error::custom)
    }
}
