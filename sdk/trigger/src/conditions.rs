//! Whether something holds: the [`Condition`] trait, its building blocks
//! [`Compare`] and [`Since`], and [`Logic`] (`all`/`any`/`not`) over any of
//! them.

use base::{Asset, Decimal, Future};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::DeserializeOwned};

use crate::{
    amounts::Amount,
    error::{Class, Error},
    selection::{BalanceKind, Target},
    trigger::Context,
};

/// Whether something holds about the wallets being evaluated.
pub trait Condition: Send + Sync {
    /// Whether it holds. A term it cannot know (a sibling not fully indexed,
    /// no asset) is `false`; a read error stays an error.
    fn holds<'a>(&'a self, on: &'a mut Context<'_>) -> Future<'a, anyhow::Result<bool>>;

    /// The rules it must keep to be stored.
    ///
    /// # Errors
    ///
    /// The first rule it breaks.
    fn validate(&self) -> Result<(), Error>;

    /// Whether it reads `this`: a filter may only use ones that do.
    fn reads_this(&self) -> bool;
}

/// Many wallet indexes' balances into one number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Aggregate {
    /// Their summed balance (empty: 0).
    Sum,
    /// The smallest balance (empty: unknown).
    Min,
    /// The largest balance (empty: unknown).
    Max,
    /// How many there are, balances not read.
    Count,
}

/// How the aggregate compares with the value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operator {
    /// `>`.
    Above,
    /// `>=`.
    AtLeast,
    /// `<`.
    Below,
    /// `<=`.
    AtMost,
    /// `==`.
    Equal,
}

impl Operator {
    /// Whether `left` (the aggregate) stands so to `right` (the value).
    #[must_use]
    pub fn holds(self, left: &Decimal, right: &Decimal) -> bool {
        match self {
            Self::Above => left > right,
            Self::AtLeast => left >= right,
            Self::Below => left < right,
            Self::AtMost => left <= right,
            Self::Equal => left == right,
        }
    }
}

/// `aggregate` of the `balance` of `of`, compared with `value`:
/// `{"compare": {"of": …, "aggregate": …, "balance": …, "operator": …, "value": …}}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Compare<A> {
    /// Whose balances.
    pub of: Target,
    /// How they become one number.
    pub aggregate: Aggregate,
    /// Which balance of `of` it reads.
    #[serde(default, skip_serializing_if = "BalanceKind::is_confirmed")]
    pub balance: BalanceKind,
    /// How the aggregate compares with the value.
    pub operator: Operator,
    /// What it compares with, in the asset of `of`.
    pub value: A,
}

impl<A: Amount> Compare<A> {
    /// A count: how many `of` names, against a whole number.
    fn count(&self, many: usize) -> anyhow::Result<bool> {
        let Some(value) = self.value.constant() else {
            anyhow::bail!("a count compares with a value");
        };
        anyhow::ensure!(value.scale() == 0, "a count compares with a whole number");
        let count = Decimal::from(u64::try_from(many)?);
        Ok(self.operator.holds(&count, value))
    }

    /// The asset `of` is read in: the one it names, else that of the first
    /// wallet being evaluated (B1).
    fn asset(&self, on: &Context<'_>) -> Option<Asset> {
        let named = self.of.asset().map(|id| Asset {
            network: on.network().clone(),
            id: id.to_owned(),
        });
        named.or_else(|| on.this().values().next().map(|one| one.asset.clone()))
    }
}

impl<A: Amount> Condition for Compare<A> {
    fn holds<'a>(&'a self, on: &'a mut Context<'_>) -> Future<'a, anyhow::Result<bool>> {
        Box::pin(async move {
            let Some(wallets) = on.wallets(&self.of).await? else {
                return Ok(false);
            };
            if self.aggregate == Aggregate::Count {
                return self.count(wallets.len());
            }
            let Some(asset) = self.asset(on) else {
                return Ok(false);
            };
            let balances = wallets.values().map(|one| one.held(self.balance).clone());
            let left = match self.aggregate {
                Aggregate::Min => balances.min(),
                Aggregate::Max => balances.max(),
                Aggregate::Sum | Aggregate::Count => Some(balances.sum()),
            };
            let right = self.value.atomic(on, &asset).await?;
            let (Some(left), Some(right)) = (left, right) else {
                return Ok(false);
            };
            Ok(self.operator.holds(&left, &right))
        })
    }

    fn validate(&self) -> Result<(), Error> {
        self.of.validate()?;
        self.value.validate()?;
        let whole = self
            .value
            .constant()
            .is_some_and(|value| value.scale() == 0);
        if self.aggregate == Aggregate::Count && !whole {
            return Err(Error::new(
                Class::OutOfRange,
                "a count compares with a whole number",
            ));
        }
        Ok(())
    }

    fn reads_this(&self) -> bool {
        self.of.is_this() || self.value.reads_this()
    }
}

/// At least `seconds` since this trigger's last run started; never run:
/// holds. `{"since": {"seconds": 3600}}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Since {
    /// How long, in seconds.
    pub seconds: u64,
}

impl Condition for Since {
    fn holds<'a>(&'a self, on: &'a mut Context<'_>) -> Future<'a, anyhow::Result<bool>> {
        Box::pin(std::future::ready(self.passed(on)))
    }

    fn validate(&self) -> Result<(), Error> {
        Ok(())
    }

    fn reads_this(&self) -> bool {
        false
    }
}

impl Since {
    fn passed(&self, on: &Context<'_>) -> anyhow::Result<bool> {
        let Some(last) = on.last_run() else {
            return Ok(true);
        };
        let wait = self.seconds.checked_mul(1000);
        let wait = wait.filter(|wait| i64::try_from(*wait).is_ok());
        let Some(wait) = wait else {
            anyhow::bail!("a wait is out of range");
        };
        Ok(on.now().saturating_sub(last) >= wait)
    }
}

/// AND / OR / NOT over conditions of one kind, evaluated in written order
/// and stopping early. In JSON a condition is written bare; the combinators
/// are `{"all": […]}`, `{"any": […]}`, `{"not": …}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Logic<C> {
    /// One condition.
    One(C),
    /// Every one holds (AND; empty holds).
    All(Vec<Self>),
    /// Any holds (OR; empty does not).
    Any(Vec<Self>),
    /// The opposite (NOT).
    Not(Box<Self>),
}

impl<C: Condition> Condition for Logic<C> {
    /// An error stays an error, under `not` too.
    fn holds<'a>(&'a self, on: &'a mut Context<'_>) -> Future<'a, anyhow::Result<bool>> {
        Box::pin(async move {
            match self {
                Self::One(one) => one.holds(on).await,
                Self::All(all) => {
                    for one in all {
                        if !one.holds(on).await? {
                            return Ok(false);
                        }
                    }
                    Ok(true)
                }
                Self::Any(any) => {
                    for one in any {
                        if one.holds(on).await? {
                            return Ok(true);
                        }
                    }
                    Ok(false)
                }
                Self::Not(one) => Ok(!one.holds(on).await?),
            }
        })
    }

    /// Every condition keeps its rules, no "any of" is empty (it could never
    /// hold) and no "not" sits right inside a "not".
    fn validate(&self) -> Result<(), Error> {
        match self {
            Self::One(one) => one.validate(),
            Self::Any(any) if any.is_empty() => Err(Error::new(
                Class::Impossible,
                "an empty \"any of\" never holds",
            )),
            Self::All(many) | Self::Any(many) => many.iter().try_for_each(Self::validate),
            Self::Not(one) if matches!(**one, Self::Not(_)) => Err(Error::new(
                Class::Redundant,
                "a \"not\" inside a \"not\" cancels out",
            )),
            Self::Not(one) => one.validate(),
        }
    }

    /// Whether every condition in it reads `this`: a filter needs each to.
    fn reads_this(&self) -> bool {
        match self {
            Self::One(one) => one.reads_this(),
            Self::All(many) | Self::Any(many) => many.iter().all(Self::reads_this),
            Self::Not(one) => one.reads_this(),
        }
    }
}

impl<C: Serialize> Serialize for Logic<C> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::One(one) => one.serialize(serializer),
            Self::All(all) => serializer.serialize_newtype_variant("Logic", 0, "all", all),
            Self::Any(any) => serializer.serialize_newtype_variant("Logic", 1, "any", any),
            Self::Not(one) => serializer.serialize_newtype_variant("Logic", 2, "not", one),
        }
    }
}

impl<'de, C: DeserializeOwned> Deserialize<'de> for Logic<C> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match Form::<C>::deserialize(deserializer)? {
            Form::Combinator(Combinator::All(all)) => Self::All(all),
            Form::Combinator(Combinator::Any(any)) => Self::Any(any),
            Form::Combinator(Combinator::Not(one)) => Self::Not(one),
            Form::One(one) => Self::One(one),
        })
    }
}

/// The JSON form: a combinator, else a bare condition.
#[derive(Deserialize)]
#[serde(untagged, bound(deserialize = "C: DeserializeOwned"))]
enum Form<C> {
    Combinator(Combinator<C>),
    One(C),
}

/// The tagged combinators.
#[derive(Deserialize)]
#[serde(rename_all = "snake_case", bound(deserialize = "C: DeserializeOwned"))]
enum Combinator<C> {
    All(Vec<Logic<C>>),
    Any(Vec<Logic<C>>),
    Not(Box<Logic<C>>),
}
