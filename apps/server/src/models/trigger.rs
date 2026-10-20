//! `trigger`: a pipe's selector, filter, check and action, stored as JSON.

use base::Decimal;
use diesel::{prelude::*, sqlite::Sqlite};

use serde::{Deserialize, Serialize};

use super::{Json, schema::triggers, wallet_index::Selector};

/// The wallet indexes in context.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct This {
    /// Read the same addresses in this asset instead; `None`: as they are.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) asset: Option<String>,
}

/// What a balance, fee or compare refers to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Target {
    /// The wallet indexes in context.
    This(This),
    /// An absolute choice of the network's wallet indexes.
    Select(Selector),
}

impl Target {
    /// The one asset id its balances are in, when it names one; plain `This`
    /// names none (it is in the asset of the indexes in context).
    pub(crate) fn asset(&self) -> Option<&str> {
        match self {
            Self::This(This { asset }) => asset.as_deref(),
            Self::Select(selector) => selector.asset(),
        }
    }
}

impl Default for Target {
    fn default() -> Self {
        Self::This(This::default())
    }
}

/// What a trigger's check may test: balances, and the trigger's own runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Gate {
    /// No earlier run of this trigger is in flight.
    Idle,
    /// At least `seconds` since this trigger's last run started.
    Since { seconds: u64 },
    /// A balance comparison.
    Compare(Compare),
}

/// What a trigger's filter may test: balances only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Filter {
    Compare(Compare),
}

/// `aggregate` of the balances of `of`, compared with `value`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Compare {
    pub(crate) of: Target,
    pub(crate) aggregate: Aggregate,
    pub(crate) operator: Operator,
    pub(crate) value: Amount,
}

/// Many wallet indexes' balances into one number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Aggregate {
    /// Their summed balance (empty: 0).
    Sum,
    /// The smallest balance (empty: no value).
    Min,
    /// The largest balance (empty: no value).
    Max,
    /// How many there are, balances not read.
    Count,
}

/// How the aggregate compares with the value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Operator {
    Above,
    AtLeast,
    Below,
    AtMost,
    Equal,
}

impl Operator {
    /// Whether `left` (the aggregate) stands so to `right` (the value).
    pub(crate) fn holds(self, left: &Decimal, right: &Decimal) -> bool {
        match self {
            Self::Above => left > right,
            Self::AtLeast => left >= right,
            Self::Below => left < right,
            Self::AtMost => left <= right,
            Self::Equal => left == right,
        }
    }
}

/// An amount expression, evaluated in atomic units.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Amount {
    /// The summed confirmed balance of `of` (default: `This {}`).
    Balance {
        #[serde(default)]
        of: Target,
    },
    /// The native fee of sending everything `of` holds to one address.
    Fee {
        #[serde(default)]
        of: Target,
    },
    /// A fixed value in the asset's display units.
    Value(#[serde(with = "text")] Decimal),
    Add(Box<Amount>, Box<Amount>),
    Sub(Box<Amount>, Box<Amount>),
    Min(Box<Amount>, Box<Amount>),
    Max(Box<Amount>, Box<Amount>),
}

/// What a trigger does when its check passes. Send is the only action now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Action {
    Send(Send),
}

/// Moves `amount` of `asset` from the `source` wallet indexes to the
/// `destination` addresses. At most one side may be many: many sources pay
/// one destination (`amount` per source, `This` being it), or one source
/// pays many destinations (`amount` per destination, `This` being it).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Send {
    /// Where the funds come from; every source must be keyed here.
    pub(crate) source: Target,
    /// The asset id sent; every source is read in it.
    pub(crate) asset: String,
    /// Where the funds go: the addresses of these wallet indexes.
    pub(crate) destination: Target,
    /// How much, per send, in `asset`.
    pub(crate) amount: Amount,
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

/// AND / OR / NOT over leaves of one kind. In JSON a leaf is written bare;
/// the combinators are `{"all": […]}`, `{"any": […]}`, `{"not": …}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    from = "Form<L>",
    into = "Form<L>",
    bound(
        serialize = "L: Serialize + Clone",
        deserialize = "L: Deserialize<'de>"
    )
)]
pub(crate) enum Logic<L> {
    /// One leaf.
    Leaf(L),
    /// Every one passes (AND; empty passes).
    All(Vec<Logic<L>>),
    /// Any passes (OR; empty fails).
    Any(Vec<Logic<L>>),
    /// The opposite (NOT).
    Not(Box<Logic<L>>),
}

impl<L> Logic<L> {
    /// Evaluates in written order, stopping early; `leaf` decides each leaf.
    /// An error stays an error, under `Not` too.
    ///
    /// # Errors
    ///
    /// The first error `leaf` returns.
    pub(crate) fn eval(
        &self,
        leaf: &mut impl FnMut(&L) -> anyhow::Result<bool>,
    ) -> anyhow::Result<bool> {
        Ok(match self {
            Self::Leaf(one) => leaf(one)?,
            Self::All(all) => {
                for one in all {
                    if !one.eval(leaf)? {
                        return Ok(false);
                    }
                }
                true
            }
            Self::Any(any) => {
                for one in any {
                    if one.eval(leaf)? {
                        return Ok(true);
                    }
                }
                false
            }
            Self::Not(inner) => !inner.eval(leaf)?,
        })
    }
}

/// The JSON form: a combinator, else a bare leaf.
#[derive(Clone, Serialize, Deserialize)]
#[serde(untagged)]
#[serde(bound(
    serialize = "L: Serialize + Clone",
    deserialize = "L: Deserialize<'de>"
))]
enum Form<L> {
    Combinator(Combinator<L>),
    Leaf(L),
}

/// The tagged combinators.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[serde(bound(
    serialize = "L: Serialize + Clone",
    deserialize = "L: Deserialize<'de>"
))]
enum Combinator<L> {
    All(Vec<Logic<L>>),
    Any(Vec<Logic<L>>),
    Not(Box<Logic<L>>),
}

impl<L> From<Form<L>> for Logic<L> {
    fn from(form: Form<L>) -> Self {
        match form {
            Form::Leaf(one) => Self::Leaf(one),
            Form::Combinator(Combinator::All(all)) => Self::All(all),
            Form::Combinator(Combinator::Any(any)) => Self::Any(any),
            Form::Combinator(Combinator::Not(inner)) => Self::Not(inner),
        }
    }
}

impl<L> From<Logic<L>> for Form<L> {
    fn from(logic: Logic<L>) -> Self {
        match logic {
            Logic::Leaf(one) => Self::Leaf(one),
            Logic::All(all) => Self::Combinator(Combinator::All(all)),
            Logic::Any(any) => Self::Combinator(Combinator::Any(any)),
            Logic::Not(inner) => Self::Combinator(Combinator::Not(inner)),
        }
    }
}

/// One `trigger` row.
#[derive(Clone, Debug, Queryable, Selectable, Insertable)]
#[diesel(table_name = triggers, check_for_backend(Sqlite))]
pub(crate) struct Trigger {
    pub(crate) id: String,
    pub(crate) chain: String,
    pub(crate) network: String,
    pub(crate) selector: Json<Selector>,
    pub(crate) filter: Option<Json<Logic<Filter>>>,
    pub(crate) check: Json<Logic<Gate>>,
    pub(crate) action: Json<Action>,
    pub(crate) enabled: bool,
    /// Unix ms.
    pub(crate) created_at: i64,
    /// Unix ms.
    pub(crate) updated_at: i64,
}

collection! {
    /// Triggers, ordered by id.
    Triggers(Trigger)
}

/// The `trigger` table's statements.
#[allow(
    dead_code,
    reason = "the table's whole API; not every statement has a caller yet"
)]
pub(crate) mod db {
    use base::Network;
    use diesel::prelude::*;

    use super::{Trigger, Triggers};
    use crate::models::schema::triggers;

    /// Stores `trigger`; an existing id is refused.
    pub(crate) fn insert(
        connection: &mut SqliteConnection,
        trigger: &Trigger,
    ) -> QueryResult<usize> {
        diesel::insert_into(triggers::table)
            .values(trigger)
            .execute(connection)
    }

    /// The trigger `id`.
    pub(crate) fn get_by_id(connection: &mut SqliteConnection, id: &str) -> QueryResult<Trigger> {
        triggers::table
            .find(id)
            .select(Trigger::as_select())
            .first(connection)
    }

    /// Every trigger, by id.
    pub(crate) fn find_all(connection: &mut SqliteConnection) -> QueryResult<Triggers> {
        let triggers = triggers::table
            .order(triggers::id)
            .select(Trigger::as_select())
            .load(connection)?;
        Ok(triggers.into())
    }

    /// Replaces trigger `trigger.id` with `trigger`, keeping its
    /// `created_at`; how many changed (0 or 1).
    pub(crate) fn update(
        connection: &mut SqliteConnection,
        trigger: &Trigger,
    ) -> QueryResult<usize> {
        diesel::update(triggers::table.find(&trigger.id))
            .set((
                triggers::chain.eq(&trigger.chain),
                triggers::network.eq(&trigger.network),
                triggers::selector.eq(&trigger.selector),
                triggers::filter.eq(&trigger.filter),
                triggers::check.eq(&trigger.check),
                triggers::action.eq(&trigger.action),
                triggers::enabled.eq(trigger.enabled),
                triggers::updated_at.eq(trigger.updated_at),
            ))
            .execute(connection)
    }

    /// The enabled triggers of `network`, by id.
    pub(crate) fn find_enabled(
        connection: &mut SqliteConnection,
        network: &Network,
    ) -> QueryResult<Triggers> {
        let triggers = triggers::table
            .filter(triggers::chain.eq(network.chain.as_str()))
            .filter(triggers::network.eq(&network.name))
            .filter(triggers::enabled.eq(true))
            .order(triggers::id)
            .select(Trigger::as_select())
            .load(connection)?;
        Ok(triggers.into())
    }

    /// Removes trigger `id` and its runs; how many triggers went (0 or 1).
    pub(crate) fn delete(connection: &mut SqliteConnection, id: &str) -> QueryResult<usize> {
        diesel::delete(triggers::table.find(id)).execute(connection)
    }
}
