//! `trigger`: a pipe's selector, filter, check and action, stored as JSON.

use base::Decimal;
use diesel::{prelude::*, sqlite::Sqlite};

use serde::{Deserialize, Serialize};

use super::{Json, schema::triggers, wallet_index_selector::Selector};

/// Why a trigger cannot be kept: which rule it breaks.
#[derive(Debug)]
pub(crate) struct Invalid(String);

impl Invalid {
    pub(crate) fn new(why: impl Into<String>) -> Self {
        Self(why.into())
    }
}

impl std::fmt::Display for Invalid {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for Invalid {}

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

impl Target {
    /// Whether a chosen set names wallets on purpose ([`Selector::validate`]).
    pub(crate) fn validate(&self) -> Result<(), Invalid> {
        match self {
            Self::This(_) => Ok(()),
            Self::Select(selector) => selector.validate(),
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
    /// Which balance of `of` it reads.
    #[serde(default, skip_serializing_if = "Balance::is_confirmed")]
    pub(crate) balance: Balance,
    pub(crate) operator: Operator,
    pub(crate) value: Amount,
}

impl Compare {
    fn validate(&self) -> Result<(), Invalid> {
        self.of.validate()?;
        self.value.validate()?;
        // A count compares with a plain whole number.
        let whole = matches!(&self.value, Amount::Value(value) if value.scale() == 0);
        if self.aggregate == Aggregate::Count && !whole {
            return Err(Invalid::new("a count compares with a whole number"));
        }
        Ok(())
    }

    /// Whether it reads the wallet in context (`This`) on either side: only
    /// then can it tell one wallet from another.
    fn reads_this(&self) -> bool {
        matches!(self.of, Target::This(_)) || self.value.reads_this()
    }
}

/// Which balance a trigger reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Balance {
    /// Spendable at the confirmed block: what a reorg cannot undo, less
    /// what is on its way out. For how much to send.
    #[default]
    Confirmed,
    /// What it will hold once every pending transaction lands, in and out.
    /// For whether it still needs something: what is already on its way in
    /// counts (a prefund is not sent twice).
    Pending,
}

impl Balance {
    const fn is_confirmed(&self) -> bool {
        matches!(self, Self::Confirmed)
    }
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
    /// The summed `balance` of `of` (default: `This {}`, confirmed).
    Balance {
        #[serde(default)]
        of: Target,
        #[serde(default, skip_serializing_if = "Balance::is_confirmed")]
        balance: Balance,
    },
    /// The native fee of sending everything `of` holds to `to`, one
    /// address: priced as to a recipient that holds the asset when it is a
    /// wallet kept here that does. Without `to`, or when it names several
    /// addresses, to a new address, the dearest case.
    Fee {
        #[serde(default)]
        of: Target,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<Target>,
    },
    /// A fixed value in the asset's display units.
    Value(#[serde(with = "text")] Decimal),
    Add(Box<Amount>, Box<Amount>),
    Sub(Box<Amount>, Box<Amount>),
    Min(Box<Amount>, Box<Amount>),
    Max(Box<Amount>, Box<Amount>),
}

impl Amount {
    /// Whether every wallet set it reads names wallets on purpose.
    pub(crate) fn validate(&self) -> Result<(), Invalid> {
        match self {
            Self::Balance { of, .. } | Self::Fee { of, to: None } => of.validate(),
            Self::Fee { of, to: Some(to) } => {
                of.validate()?;
                to.validate()
            }
            Self::Value(_) => Ok(()),
            Self::Add(a, b) | Self::Sub(a, b) | Self::Min(a, b) | Self::Max(a, b) => {
                a.validate()?;
                b.validate()
            }
        }
    }

    /// Whether a balance or fee in it, at any depth, reads `This`.
    fn reads_this(&self) -> bool {
        match self {
            Self::Balance { of, .. } | Self::Fee { of, .. } => matches!(of, Target::This(_)),
            Self::Value(_) => false,
            Self::Add(a, b) | Self::Sub(a, b) | Self::Min(a, b) | Self::Max(a, b) => {
                a.reads_this() || b.reads_this()
            }
        }
    }
}

/// What a trigger does when its check passes: an action by name, with its
/// own static input (`usecase::workers::trigger::actions`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Action {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) input: serde_json::Value,
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
    /// Whether every leaf passes `leaf`, no "any of" is empty (it could
    /// never hold) and no "not" sits right inside a "not".
    fn validate(&self, leaf: &impl Fn(&L) -> Result<(), Invalid>) -> Result<(), Invalid> {
        match self {
            Self::Leaf(one) => leaf(one),
            Self::Any(any) if any.is_empty() => {
                Err(Invalid::new("an empty \"any of\" never holds"))
            }
            Self::All(many) | Self::Any(many) => many.iter().try_for_each(|one| one.validate(leaf)),
            Self::Not(one) if matches!(**one, Self::Not(_)) => {
                Err(Invalid::new("a \"not\" inside a \"not\" cancels out"))
            }
            Self::Not(one) => one.validate(leaf),
        }
    }

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

/// What one round saw of a trigger: how many wallet indexes its selector
/// picked, how many of them its filter kept, whether its check held over
/// those, and how many send jobs its action queued.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = TriggerStats)]
pub(crate) struct Stats {
    /// Wallet indexes its selector picked.
    pub(crate) selected: usize,
    /// How many of them its filter kept (all of them without a filter).
    pub(crate) kept: usize,
    /// Whether its check held over the kept ones.
    pub(crate) checked: bool,
    /// Send jobs its action queued.
    pub(crate) jobs: usize,
}

/// A trigger's last round, whether it acted or not.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub(crate) struct LastRound {
    /// When it decided, Unix ms.
    pub(crate) at: i64,
    /// The confirmed block position it decided at.
    pub(crate) confirmed: i64,
    /// What it saw.
    #[serde(flatten)]
    pub(crate) stats: Stats,
    /// Why it was refused (it then acted on nothing); always present.
    #[schema(required)]
    pub(crate) refused: Option<String>,
}

/// One `trigger` row; the API shows it as is.
#[derive(Clone, Debug, Queryable, Selectable, Insertable, Serialize, utoipa::ToSchema)]
#[diesel(table_name = triggers, check_for_backend(Sqlite))]
pub(crate) struct Trigger {
    /// Its slug: 1 to 64 of `[a-z0-9-]`.
    pub(crate) id: String,
    /// Chain name, as `GET /v1/assets` lists it.
    pub(crate) chain: String,
    /// Network name, as `GET /v1/assets` lists it.
    pub(crate) network: String,
    /// The batch it was created in: every trigger of one create shares it.
    #[serde(rename = "group")]
    pub(crate) group_id: String,
    /// Which wallet indexes it acts on: `{"ids": […]}`, `{"asset": "…"}`,
    /// `{"tag": "…"}`, `{"untagged": {}}`, `{"all": […]}`, `{"any": […]}`, `{"not": …}`.
    #[schema(value_type = Object)]
    pub(crate) selector: Json<Selector>,
    /// Which of them it acts on: compares under `all`/`any`/`not`; null:
    /// every one.
    #[schema(value_type = Option<Object>)]
    pub(crate) filter: Option<Json<Logic<Filter>>>,
    /// When it acts: `{"since": {"seconds": …}}` and compares under
    /// `all`/`any`/`not`.
    #[schema(value_type = Object)]
    pub(crate) check: Json<Logic<Gate>>,
    /// What it does: `{"name": "send", "input": {"source": target, "asset":
    /// id, "destination": target, "amount": amount}}`.
    #[schema(value_type = Object)]
    pub(crate) action: Json<Action>,
    /// Whether it runs.
    pub(crate) enabled: bool,
    /// Unix ms.
    pub(crate) created_at: i64,
    /// Unix ms.
    pub(crate) updated_at: i64,
    /// What its last round saw; null before its first round. Always present.
    #[schema(value_type = Option<LastRound>, required)]
    pub(crate) last_round: Option<Json<LastRound>>,
}

impl Trigger {
    /// Whether it may be kept: a well-formed id, wallets named on purpose
    /// (see [`Selector::validate`]), a filter whose every compare reads the
    /// wallet it tests, and a filter and check that can hold.
    /// What needs the running configuration (the network, the action's
    /// input) is checked where it is known.
    ///
    /// # Errors
    ///
    /// The first rule it breaks.
    pub(crate) fn validate(&self) -> Result<(), Invalid> {
        if !super::wallet::is_tag(&self.id) {
            return Err(Invalid::new("a trigger id is 1 to 64 of [a-z0-9-]"));
        }
        self.selector.0.validate()?;
        if let Some(filter) = &self.filter {
            // A filter runs per wallet: a compare that never reads that wallet
            // gives every wallet the same answer, a switch that is When's.
            filter.0.validate(&|Filter::Compare(compare)| {
                compare.validate()?;
                if compare.reads_this() {
                    Ok(())
                } else {
                    Err(Invalid::new(
                        "a filter that never looks at this wallet keeps all or none; put it under When",
                    ))
                }
            })?;
        }
        self.check.0.validate(&|gate| match gate {
            Gate::Since { .. } => Ok(()),
            Gate::Compare(compare) => compare.validate(),
        })
    }
}

/// Its id.
impl crate::db::Keyed for Trigger {
    fn key(&self) -> String {
        self.id.clone()
    }
}

/// The `trigger` table's statements.
#[allow(
    dead_code,
    reason = "the table's whole API; not every statement has a caller yet"
)]
pub(crate) mod db {
    use base::Network;
    use diesel::{prelude::*, sql_types::BigInt};

    use super::{LastRound, Trigger};
    use crate::{
        db::Page,
        models::{
            TriggerRun,
            schema::{trigger_run, triggers},
        },
    };

    /// Stores `trigger`; an existing id is refused.
    pub(crate) fn insert(
        connection: &mut SqliteConnection,
        trigger: &Trigger,
    ) -> QueryResult<usize> {
        diesel::insert_into(triggers::table)
            .values(trigger)
            .execute(connection)
    }

    /// How many triggers are kept, and how many of them are enabled.
    pub(crate) fn count(connection: &mut SqliteConnection) -> QueryResult<(i64, i64)> {
        triggers::table
            .select(diesel::dsl::sql::<(BigInt, BigInt)>(
                "COUNT(*), COALESCE(SUM(enabled), 0)",
            ))
            .get_result(connection)
    }

    /// The trigger `id`.
    pub(crate) fn get_by_id(connection: &mut SqliteConnection, id: &str) -> QueryResult<Trigger> {
        triggers::table
            .find(id)
            .select(Trigger::as_select())
            .first(connection)
    }

    /// At most `page.limit` triggers with an id after `page.after` when
    /// given, by id.
    pub(crate) fn find_page(
        connection: &mut SqliteConnection,
        page: Page<String>,
    ) -> QueryResult<Vec<Trigger>> {
        triggers::table
            .filter(triggers::id.gt(page.after.unwrap_or_default()))
            .order(triggers::id)
            .limit(page.limit)
            .select(Trigger::as_select())
            .load(connection)
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

    /// The enabled triggers of `network`, by id, each with its latest run.
    pub(crate) fn find_enabled(
        connection: &mut SqliteConnection,
        network: &Network,
    ) -> QueryResult<Vec<(Trigger, Option<TriggerRun>)>> {
        triggers::table
            .left_join(trigger_run::table.on(trigger_run::id.nullable().eq(triggers::last_run)))
            .filter(triggers::chain.eq(network.chain.as_str()))
            .filter(triggers::network.eq(&network.name))
            .filter(triggers::enabled.eq(true))
            .order(triggers::id)
            .select((Trigger::as_select(), Option::<TriggerRun>::as_select()))
            .load(connection)
    }

    /// Keeps `round` as trigger `id`'s last.
    pub(crate) fn set_last_round(
        connection: &mut SqliteConnection,
        id: &str,
        round: &LastRound,
    ) -> QueryResult<usize> {
        diesel::update(triggers::table.find(id))
            .set(triggers::last_round.eq(Some(super::Json(round.clone()))))
            .execute(connection)
    }

    /// Removes trigger `id` and its runs; how many triggers went (0 or 1).
    pub(crate) fn delete(connection: &mut SqliteConnection, id: &str) -> QueryResult<usize> {
        diesel::delete(triggers::table.find(id)).execute(connection)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Json, Logic, Trigger};

    /// A trigger with these parts as JSON, the rest valid.
    fn trigger(selector: serde_json::Value, check: serde_json::Value) -> Trigger {
        Trigger {
            id: "sweep".to_owned(),
            chain: "ethereum".to_owned(),
            network: "sepolia".to_owned(),
            group_id: "group".to_owned(),
            selector: Json(serde_json::from_value(selector).expect("a selector")),
            filter: None,
            check: Json(serde_json::from_value::<Logic<_>>(check).expect("a check")),
            action: Json(serde_json::from_value(json!({ "name": "send" })).expect("an action")),
            enabled: true,
            created_at: 0,
            updated_at: 0,
            last_round: None,
        }
    }

    fn refusal(selector: serde_json::Value, check: serde_json::Value) -> String {
        trigger(selector, check)
            .validate()
            .expect_err("refused")
            .to_string()
    }

    #[test]
    fn wallets_are_named_on_purpose() {
        let always = json!({ "all": [] });
        trigger(json!({ "tag": "deposit" }), always.clone())
            .validate()
            .expect("a tag names wallets");
        trigger(json!({ "untagged": {} }), always.clone())
            .validate()
            .expect("no tag names wallets too");
        assert!(refusal(json!({ "all": [] }), always.clone()).contains("every wallet"));
        assert!(refusal(json!({ "any": [] }), always.clone()).contains("no wallet"));
        assert!(refusal(json!({ "ids": [] }), always.clone()).contains("no wallet"));
        assert!(refusal(json!({ "tag": "Not A Tag" }), always.clone()).contains("tag"));
        assert!(
            refusal(json!({ "not": { "not": { "tag": "deposit" } } }), always)
                .contains("cancels out")
        );
    }

    #[test]
    fn a_filter_reads_the_wallet_it_tests() {
        let mut kept = trigger(json!({ "tag": "deposit" }), json!({ "all": [] }));
        let filter = |compare: serde_json::Value| {
            Some(Json(
                serde_json::from_value::<Logic<_>>(compare).expect("a filter"),
            ))
        };
        kept.filter = filter(json!({ "compare": {
            "of": { "this": {} }, "aggregate": "sum", "operator": "above",
            "value": { "fee": {} } } }));
        kept.validate()
            .expect("this wallet's balance against its fee");
        kept.filter = filter(json!({ "compare": {
            "of": { "select": { "untagged": {} } }, "aggregate": "sum", "operator": "above",
            "value": { "add": [{ "fee": {} }, { "value": "1" }] } } }));
        kept.validate()
            .expect("a fee of this wallet, nested, is enough");
        kept.filter = filter(json!({ "compare": {
            "of": { "select": { "untagged": {} } }, "aggregate": "sum", "operator": "above",
            "value": { "fee": { "of": { "select": { "untagged": {} } } } } } }));
        assert!(
            kept.validate()
                .expect_err("refused")
                .to_string()
                .contains("put it under When")
        );
    }

    #[test]
    fn a_check_can_hold() {
        let wallets = json!({ "tag": "deposit" });
        trigger(wallets.clone(), json!({ "all": [] }))
            .validate()
            .expect("an empty all always holds");
        assert!(refusal(wallets.clone(), json!({ "any": [] })).contains("never holds"));
        assert!(
            refusal(
                wallets,
                json!({ "not": { "not": { "since": { "seconds": 60 } } } })
            )
            .contains("cancels out")
        );
    }
}
