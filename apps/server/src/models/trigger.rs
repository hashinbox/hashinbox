//! `trigger`: a pipe's selector, filter, check and action, stored as JSON,
//! and the trigger language they are written in: which of the `trigger`
//! package's building blocks this server offers (design 077).

use ::trigger::{
    Class, Context, Error, LastRound,
    actions::{Call, Registry},
    amounts::{self, Arithmetic},
    conditions::{self, Condition, Logic},
    selection::Selector,
};
use base::{Asset, Assets, Decimal, Future, Network};
use diesel::{prelude::*, sqlite::Sqlite};
use serde::{Deserialize, Serialize};
use tasks::Job;

use super::{Json, schema::triggers};

/// The amount terms this server offers: `{"value": "1.5"}`,
/// `{"balance": {…}}`, `{"fee": {…}}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AmountTerm {
    Value(amounts::Value),
    Balance(amounts::Balance),
    Fee(amounts::Fee),
}

/// An amount: a term, or `add`/`sub`/`min`/`max`/`times` over amounts.
pub(crate) type Amount = Arithmetic<AmountTerm>;

/// A balance comparison.
pub(crate) type Compare = conditions::Compare<Amount>;

/// What a trigger's filter may test, per index: balances only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Filter {
    Compare(Compare),
}

/// What a trigger's check may test, over the kept set: balances, and the
/// trigger's own runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Check {
    Since(conditions::Since),
    Compare(Compare),
}

/// A trigger in this language.
pub(crate) type Definition = ::trigger::Trigger<Filter, Check>;

impl amounts::Amount for AmountTerm {
    fn atomic<'a>(
        &'a self,
        on: &'a mut Context<'_>,
        asset: &'a Asset,
    ) -> Future<'a, anyhow::Result<Option<Decimal>>> {
        match self {
            Self::Value(one) => one.atomic(on, asset),
            Self::Balance(one) => one.atomic(on, asset),
            Self::Fee(one) => one.atomic(on, asset),
        }
    }

    fn validate(&self) -> Result<(), Error> {
        match self {
            Self::Value(one) => one.validate(),
            Self::Balance(one) => one.validate(),
            Self::Fee(one) => one.validate(),
        }
    }

    fn reads_this(&self) -> bool {
        match self {
            Self::Value(one) => one.reads_this(),
            Self::Balance(one) => one.reads_this(),
            Self::Fee(one) => one.reads_this(),
        }
    }

    fn constant(&self) -> Option<&Decimal> {
        match self {
            Self::Value(one) => one.constant(),
            Self::Balance(one) => one.constant(),
            Self::Fee(one) => one.constant(),
        }
    }
}

impl Condition for Filter {
    fn holds<'a>(&'a self, on: &'a mut Context<'_>) -> Future<'a, anyhow::Result<bool>> {
        match self {
            Self::Compare(one) => one.holds(on),
        }
    }

    fn validate(&self) -> Result<(), Error> {
        match self {
            Self::Compare(one) => one.validate(),
        }
    }

    fn reads_this(&self) -> bool {
        match self {
            Self::Compare(one) => one.reads_this(),
        }
    }
}

impl Condition for Check {
    fn holds<'a>(&'a self, on: &'a mut Context<'_>) -> Future<'a, anyhow::Result<bool>> {
        match self {
            Self::Since(one) => one.holds(on),
            Self::Compare(one) => one.holds(on),
        }
    }

    fn validate(&self) -> Result<(), Error> {
        match self {
            Self::Since(one) => one.validate(),
            Self::Compare(one) => one.validate(),
        }
    }

    fn reads_this(&self) -> bool {
        match self {
            Self::Since(one) => one.reads_this(),
            Self::Compare(one) => one.reads_this(),
        }
    }
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
    pub(crate) check: Json<Logic<Check>>,
    /// What it does: `{"name": "send", "input": {"source": target, "asset":
    /// id, "destination": target, "amount": amount}}`.
    #[schema(value_type = Object)]
    pub(crate) action: Json<Call>,
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
    /// Its selector, filter, check and action.
    pub(crate) fn definition(&self) -> Definition {
        Definition {
            selector: self.selector.0.clone(),
            filter: self.filter.as_ref().map(|filter| filter.0.clone()),
            check: self.check.0.clone(),
            action: self.action.0.clone(),
        }
    }

    /// Whether it may be kept on `network` with `assets`: a well-formed
    /// id, then the definition's rules ([`Definition::validate`]): an
    /// action of `actions` that accepts its input, wallets named on
    /// purpose, a filter whose every compare reads the wallet it tests, and
    /// a filter and check that can hold.
    ///
    /// # Errors
    ///
    /// The first rule it breaks.
    pub(crate) fn validate(
        &self,
        actions: &Registry<Job>,
        network: &Network,
        assets: &Assets,
    ) -> Result<(), Error> {
        if !super::wallet::is_tag(&self.id) {
            let why = "a trigger id is 1 to 64 of [a-z0-9-]";
            return Err(Error::new(Class::Malformed, why));
        }
        self.definition().validate(actions, network, assets)
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
    use ::trigger::{Enabled, LastRound};
    use base::Network;
    use diesel::{prelude::*, sql_types::BigInt};

    use super::{Check, Filter, Trigger};
    use crate::{
        db::Page,
        models::{
            TriggerRun,
            schema::{trigger_run, triggers},
            uint,
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

    /// The enabled triggers of `network`, by id, each with when its latest
    /// run started: what a round takes.
    ///
    /// # Errors
    ///
    /// When the read fails or a stored start is negative.
    pub(crate) fn find_enabled(
        connection: &mut SqliteConnection,
        network: &Network,
    ) -> anyhow::Result<Vec<Enabled<Filter, Check>>> {
        let rows: Vec<(Trigger, Option<TriggerRun>)> = triggers::table
            .left_join(trigger_run::table.on(trigger_run::id.nullable().eq(triggers::last_run)))
            .filter(triggers::chain.eq(network.chain.as_str()))
            .filter(triggers::network.eq(&network.name))
            .filter(triggers::enabled.eq(true))
            .order(triggers::id)
            .select((Trigger::as_select(), Option::<TriggerRun>::as_select()))
            .load(connection)?;
        rows.into_iter()
            .map(|(row, run)| {
                Ok(Enabled {
                    trigger: row.definition(),
                    id: row.id,
                    last_run: run.map(|run| uint(run.started_at)).transpose()?,
                })
            })
            .collect()
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
