//! Triggers (a selector, an optional filter, a check and an action on one
//! network) and their runs: list them, create a batch all or none, read,
//! replace and delete one, and list one's runs. A trigger is checked against
//! its network and the actions before it is ever kept.

use std::time::{SystemTime, UNIX_EPOCH};

use ::trigger::{
    Class,
    actions::{Call, Registry},
    conditions::Logic,
    selection::Selector,
};
use base::Container;
use diesel::OptionalExtension as _;

use super::{Error, Transport};
use crate::{
    db::{Db, Page},
    models::{
        self, Trigger,
        trigger::{self as triggers, Check, Filter},
        trigger_run::{self, Run},
    },
};

/// A trigger as written: its rules on one network, and the id a create
/// names it by.
pub(crate) struct Definition {
    /// Its slug; required in a create, refused in a replace.
    pub(crate) id: Option<String>,
    pub(crate) chain: String,
    pub(crate) network: String,
    pub(crate) selector: Selector,
    pub(crate) filter: Option<Logic<Filter>>,
    pub(crate) check: Logic<Check>,
    pub(crate) action: Call,
    /// Absent means `true` in a create and the stored value in a replace.
    pub(crate) enabled: Option<bool>,
}

impl Definition {
    /// The trigger it describes as trigger `id` of group `group_id`.
    fn trigger(self, id: String, group_id: String, created_at: i64, updated_at: i64) -> Trigger {
        Trigger {
            id,
            chain: self.chain,
            network: self.network,
            group_id,
            selector: models::Json(self.selector),
            filter: self.filter.map(models::Json),
            check: models::Json(self.check),
            action: models::Json(self.action),
            enabled: self.enabled.unwrap_or(true),
            created_at,
            updated_at,
            last_round: None,
        }
    }
}

/// A page of triggers in id order.
///
/// # Errors
///
/// The read.
pub(crate) async fn list_page(db: &Db, page: Page<String>) -> Result<Vec<Trigger>, Error> {
    Ok(db
        .read(move |connection| Ok(triggers::db::find_page(connection, page)?))
        .await?)
}

/// Stores `definitions` as one group, all or none: 1 to 16 triggers, each
/// with its own id, all on one configured network.
///
/// # Errors
///
/// Invalid for a batch out of size, a trigger without an id, several
/// networks, or a broken rule; not found for an unconfigured network or what
/// an action's input names; conflict for an id taken or repeated; the store.
pub(crate) async fn create(
    container: &Container<Transport>,
    db: &Db,
    actions: &Registry<tasks::Job>,
    definitions: Vec<Definition>,
) -> Result<Vec<Trigger>, Error> {
    let now = now()?;
    if definitions.is_empty() || definitions.len() > 16 {
        return Err(Error::invalid("a batch holds 1 to 16 triggers"));
    }
    // One group for the whole batch, so it can be found (and removed) together.
    let group = uuid::Uuid::now_v7().to_string();
    let mut rows: Vec<Trigger> = Vec::with_capacity(definitions.len());
    for definition in definitions {
        let Some(id) = definition.id.clone() else {
            return Err(Error::invalid("every trigger of a batch needs an id"));
        };
        if rows.iter().any(|row| row.id == id) {
            return Err(Error::conflict("a batch repeats a trigger id"));
        }
        if rows.first().is_some_and(|first| {
            (first.chain.as_str(), first.network.as_str())
                != (definition.chain.as_str(), definition.network.as_str())
        }) {
            return Err(Error::invalid("every trigger of a batch is on one network"));
        }
        let row = definition.trigger(id, group.clone(), now, now);
        check(container, actions, &row)?;
        rows.push(row);
    }
    let kept = rows.clone();
    let taken = db
        .write(move |connection| {
            // Every id is checked before any is written: all or none.
            for row in &kept {
                if triggers::db::get_by_id(connection, &row.id)
                    .optional()?
                    .is_some()
                {
                    return Ok(Some(row.id.clone()));
                }
            }
            for row in &kept {
                triggers::db::insert(connection, row)?;
            }
            Ok(None)
        })
        .await?;
    if let Some(taken) = taken {
        return Err(Error::conflict(format!("a trigger with id {taken} exists")));
    }
    Ok(rows)
}

/// Trigger `id`.
///
/// # Errors
///
/// Not found for no such trigger; the read.
pub(crate) async fn read(db: &Db, id: String) -> Result<Trigger, Error> {
    db.read(move |connection| Ok(triggers::db::get_by_id(connection, &id).optional()?))
        .await?
        .ok_or_else(|| Error::not_found("no such trigger"))
}

/// Replaces trigger `id` with `definition`, keeping its id, creation time
/// and group.
///
/// # Errors
///
/// Invalid for a definition naming an id, or a broken rule; not found for no
/// such trigger, an unconfigured network, or what an action's input names;
/// the store.
pub(crate) async fn replace(
    container: &Container<Transport>,
    db: &Db,
    actions: &Registry<tasks::Job>,
    id: String,
    definition: Definition,
) -> Result<Trigger, Error> {
    if definition.id.is_some() {
        return Err(Error::invalid("a trigger's id never changes"));
    }
    let kept = read(db, id.clone()).await?;
    let definition = Definition {
        enabled: definition.enabled.or(Some(kept.enabled)),
        ..definition
    };
    let row = definition.trigger(id, kept.group_id, kept.created_at, now()?);
    check(container, actions, &row)?;
    let replaced = row.clone();
    let changed = db
        .write(move |connection| Ok(triggers::db::update(connection, &replaced)?))
        .await?;
    if changed == 0 {
        return Err(Error::not_found("no such trigger"));
    }
    Ok(row)
}

/// Deletes trigger `id` and its runs.
///
/// # Errors
///
/// Not found for no such trigger; the store.
pub(crate) async fn delete(db: &Db, id: String) -> Result<(), Error> {
    let deleted = db
        .write(move |connection| Ok(triggers::db::delete(connection, &id)?))
        .await?;
    if deleted == 0 {
        return Err(Error::not_found("no such trigger"));
    }
    Ok(())
}

/// A page of trigger `id`'s runs, latest first.
///
/// # Errors
///
/// The read.
pub(crate) async fn runs_page(db: &Db, id: String, page: Page<i64>) -> Result<Vec<Run>, Error> {
    Ok(db
        .read(move |connection| Ok(trigger_run::db::find_page(connection, &id, page)?))
        .await?)
}

/// `trigger` before it is ever kept: its network configured, then the
/// model's own rules against `actions` and the network's assets
/// ([`Trigger::validate`]).
fn check(
    container: &Container<Transport>,
    actions: &Registry<tasks::Job>,
    trigger: &Trigger,
) -> Result<(), Error> {
    let network = base::Network::new(base::Chain::new(&trigger.chain), trigger.network.clone());
    let assets = &container.service(&network)?.assets;
    trigger.validate(actions, &network, assets).map_err(refused)
}

/// A broken rule: not found when an action's input names something not
/// configured here, invalid for every other rule (an unknown action is a
/// malformed body).
fn refused(error: ::trigger::Error) -> Error {
    match error.class {
        Class::NotConfigured => Error::not_found(error.message()),
        _ => Error::invalid(error.message()),
    }
}

/// Unix ms.
fn now() -> Result<i64, Error> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| Error::internal(error.to_string()))?;
    i64::try_from(elapsed.as_millis()).map_err(|error| Error::internal(error.to_string()))
}
