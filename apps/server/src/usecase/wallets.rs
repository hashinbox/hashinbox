//! Wallets: create one (import a key, watch an address, or generate), list
//! and read them; their tags; their indexes, one per asset (create from a
//! start, pause or resume, reconcile, delete) and each one's mined legs; and
//! an ordered send from one, a queued job. A wallet is named by its id; its
//! address is taken in its chain's canonical form.

use base::{Address, Birthday, Container, Decimal, WalletIndexState};
use tasks::Worker as _;

use super::{
    Error, Transport,
    tasks::Queue,
    workers::broadcast::{Broadcast, Payment, SendIntent},
};
use crate::{
    db::{Db, Page},
    models::{
        Id, Wallet, int,
        leg::{self, Movement},
        wallet::{self, WalletId},
        wallet_index::{self, WalletIndex, WalletIndexId},
    },
};

/// A wallet to create in `network`, without indexes: with `secret`, its key
/// imported and kept; else with `address`, the address watched without a
/// key; with neither, a new key generated and kept.
pub(crate) struct NewWallet {
    pub(crate) network: base::Network,
    /// A raw private key in the chain's import format.
    pub(crate) secret: Option<String>,
    /// Address text in any form the chain accepts; with `secret`, the
    /// address the key must derive.
    pub(crate) address: Option<String>,
    /// The caller's own reference for the wallet.
    pub(crate) external_id: Option<String>,
}

/// One transfer of a send, as written.
pub(crate) struct Transfer {
    /// Recipient address text, as the wallet's chain writes it.
    pub(crate) destination: String,
    /// Exact positive decimal in display units.
    pub(crate) amount: String,
}

/// Creates `new`, and whether it was created: an address already kept
/// answers its wallet as is, except that importing the key of a watch-only
/// wallet keeps the key, and an external id is kept on a wallet that has
/// none.
///
/// # Errors
///
/// Not found for an unconfigured network; invalid for a malformed address
/// or key, or a key that derives another address; unavailable when
/// generation or the store is.
pub(crate) async fn create(
    container: &Container<Transport>,
    db: &Db,
    new: NewWallet,
) -> Result<(bool, Wallet), Error> {
    let factory = &container.service(&new.network)?.wallets.factory;
    let given = new
        .address
        .map(|text| factory.parse_address(&text))
        .transpose()?;
    let mut row = if let Some(secret) = new.secret {
        let wallet = factory.from_pk_text(&secret).await?;
        if let Some(given) = given.filter(|given| given != wallet.address()) {
            return Err(Error::invalid(format!(
                "the key derives {}, not {}",
                wallet.address().value,
                given.value
            )));
        }
        Wallet::try_from(&*wallet)
    } else if let Some(address) = given {
        Wallet::from_address(&address)
    } else {
        let wallet = factory.from_pk(factory.generate().await?).await?;
        Wallet::try_from(&*wallet)
    }
    .map_err(|error| Error::invalid(format!("{error:#}")))?;
    row.external_id = new.external_id;
    let (created, wallet) = db
        .write(move |connection| {
            let created = wallet::db::insert(connection, &row)?;
            Ok((created, wallet::db::find(connection, &row.id)?))
        })
        .await?;
    let wallet = wallet.ok_or_else(|| Error::not_found("wallet is unknown"))?;
    Ok((created, wallet))
}

/// A page of wallets in id order, each with its tags and indexes.
///
/// # Errors
///
/// The read.
pub(crate) async fn list_page(db: &Db, page: Page<String>) -> Result<Vec<Wallet>, Error> {
    Ok(db
        .read(move |connection| wallet::db::find_page(connection, page))
        .await?)
}

/// Wallet `id`, with its indexes; never its key.
///
/// # Errors
///
/// Not found for an unconfigured network or an unknown wallet; invalid for
/// an address the chain refuses; the read.
pub(crate) async fn read(
    container: &Container<Transport>,
    db: &Db,
    id: &WalletId,
) -> Result<Wallet, Error> {
    let id = WalletId::of(&container.parse(id)?)?;
    db.read(move |connection| wallet::db::find(connection, &id))
        .await?
        .ok_or_else(|| Error::not_found("wallet is unknown"))
}

/// A page of the tags in use on any wallet, sorted, each once.
///
/// # Errors
///
/// The read.
pub(crate) async fn tags_page(db: &Db, page: Page<String>) -> Result<Vec<String>, Error> {
    Ok(db
        .read(move |connection| Ok(wallet::db::find_tags_page(connection, page)?))
        .await?)
}

/// Tags wallet `id` with `tag` (`add`; twice keeps one tag), or untags it.
///
/// # Errors
///
/// Invalid for a malformed tag or address; not found for an unconfigured
/// network, an unknown wallet, or untagging a tag it does not carry; the
/// store.
pub(crate) async fn set_tag(
    container: &Container<Transport>,
    db: &Db,
    id: &WalletId,
    tag: String,
    add: bool,
) -> Result<(), Error> {
    let id = WalletId::of(&container.parse(id)?)?;
    if !wallet::is_tag(&tag) {
        return Err(Error::invalid(format!(
            "a tag is 1 to 64 of [a-z0-9-]: {tag:?}"
        )));
    }
    let changed = db
        .write(move |connection| wallet::db::set_tag(connection, &id, &tag, add))
        .await?
        .ok_or_else(|| Error::not_found("wallet is unknown"))?;
    if !add && !changed {
        return Err(Error::not_found("no such tag"));
    }
    Ok(())
}

/// Indexes wallet `id` in `asset` from `start` (`None`: the chain's tip),
/// at once; and whether it was created: an asset already indexed answers its
/// index as is. A start the node no longer serves moves forward; on a chain
/// that reads balances the index starts at the chain's final block instead
/// (the indexer's), its balance read there.
///
/// # Errors
///
/// Invalid for a malformed address or a start below the chain's tip less
/// the network's `max_depth`; not found
/// for an unknown wallet or an unconfigured network or asset; unavailable
/// when the node or the store is.
pub(crate) async fn create_index(
    container: &Container<Transport>,
    db: &Db,
    id: &WalletId,
    asset: &str,
    start: Option<u64>,
) -> Result<(bool, WalletIndex), Error> {
    let address = container.parse(id)?;
    let asset = asset_info(container, &address.network, asset)?.asset;
    let index = WalletIndexId::of(&address, &asset)?;
    let wallet_id = index.wallet.clone();
    let Some(mut wallet) = db
        .read(move |connection| wallet::db::find(connection, &wallet_id))
        .await?
    else {
        return Err(Error::not_found("wallet is unknown"));
    };
    if let Some(index) = wallet.indexes.remove(&asset.id) {
        return Ok((false, index));
    }
    let service = container.service(&address)?;
    let birthday = birthday(&service, start).await?;
    // `Indexer::watch` borrows the address as `&dyn Addressed`, which is not
    // `Sync`, so its future is not `Send`: it runs on a blocking thread.
    let (indexer, runtime) = (service.indexer.clone(), tokio::runtime::Handle::current());
    tokio::task::spawn_blocking(move || {
        runtime.block_on(indexer.watch(&address, &asset, birthday))
    })
    .await
    .map_err(anyhow::Error::from)??;
    let created = db
        .read(move |connection| wallet_index::db::find_at_tip(connection, &index))
        .await?
        .ok_or_else(|| Error::not_found("no such index"))?;
    Ok((true, created))
}

/// Where to index from: `start`, refused when below the chain's tip less
/// the network's `max_depth` (accepted when it has no limit or cannot read
/// its tip); `None`, the chain's tip.
async fn birthday(service: &base::Service, start: Option<u64>) -> Result<Birthday, Error> {
    let Some(position) = start else {
        return Ok(Birthday::Fresh);
    };
    // No limit, or no way to read the tip: any start goes.
    let (Some(dated), Some(limit)) = (&service.dated, service.max_depth) else {
        return Ok(Birthday::At(position));
    };
    let earliest = dated.tips().await?.produced.saturating_sub(limit);
    if position < earliest {
        return Err(Error::invalid(format!(
            "start {position} is more than this network's max_depth of {limit} \
             positions below the tip: start at {earliest} or later"
        )));
    }
    Ok(Birthday::At(position))
}

/// Pauses or resumes wallet `id`'s index of `asset`; the index. On a chain
/// that reads balances a resumed index left behind the final block settles
/// there (the indexer's).
///
/// # Errors
///
/// Invalid for a malformed address or asset; not found for an unconfigured
/// network or no such index; the store.
pub(crate) async fn set_index_state(
    container: &Container<Transport>,
    db: &Db,
    id: &WalletId,
    asset: String,
    state: WalletIndexState,
) -> Result<WalletIndex, Error> {
    let (address, index) = index_id(container, id, &asset)?;
    db.write(move |connection| {
        if state == WalletIndexState::Indexing {
            wallet_index::db::resume(connection, &index)?;
        } else {
            wallet_index::db::set_state(connection, &address, Some(&asset), state, None)?;
        }
        wallet_index::db::find_at_tip(connection, &index)
    })
    .await?
    .ok_or_else(|| Error::not_found("no such index"))
}

/// Reads wallet `id`'s index of `asset` again from the chain's balance
/// (design 083 §9.1): in one write, its mined legs above the chain's final
/// block `F` (the indexer's last read) are deleted and its progress unset,
/// so the indexer settles it at a final block `S` and reads on from `S + 1`;
/// its pending legs stay. The index.
///
/// # Errors
///
/// Invalid for a malformed address or asset; not found for an unconfigured
/// network or no such index; conflict on a chain that reads no balance, or
/// for a paused or `no_data` index; unavailable before the indexer read the
/// final block, or when the store is.
pub(crate) async fn reconcile(
    container: &Container<Transport>,
    db: &Db,
    id: &WalletId,
    asset: &str,
) -> Result<WalletIndex, Error> {
    let (address, index) = index_id(container, id, asset)?;
    let service = container.service(&address)?;
    if service.balances.is_none() {
        return Err(Error::conflict(
            "this chain reads no balance to reconcile with",
        ));
    }
    let Some(confirmed) = service.indexer.stats().confirmed else {
        return Err(Error::unavailable(
            "the indexer has not read the chain's final block yet",
        ));
    };
    let confirmed = int(confirmed)?;
    let (changed, found) = db
        .write(move |connection| {
            let changed = wallet_index::db::reconcile(connection, &index, confirmed)?;
            if changed > 0 {
                leg::db::delete_above(connection, &index, confirmed)?;
            }
            Ok((changed, wallet_index::db::find_at_tip(connection, &index)?))
        })
        .await?;
    let found = found.ok_or_else(|| Error::not_found("no such index"))?;
    if changed == 0 {
        return Err(Error::conflict(
            "a paused or no_data index is not reconciled",
        ));
    }
    Ok(found)
}

/// Forgets wallet `id`'s index of `asset` and its progress.
///
/// # Errors
///
/// As [`set_index_state`].
pub(crate) async fn delete_index(
    container: &Container<Transport>,
    db: &Db,
    id: &WalletId,
    asset: &str,
) -> Result<(), Error> {
    let (_, index) = index_id(container, id, asset)?;
    let removed = db
        .write(move |connection| Ok(wallet_index::db::delete(connection, &index)?))
        .await?;
    if removed == 0 {
        return Err(Error::not_found("no such index"));
    }
    Ok(())
}

/// A page of the live mined legs of wallet `id`'s index of `asset`, latest
/// first, each with the balance after it.
///
/// # Errors
///
/// Invalid for a malformed address or asset; not found for an unconfigured
/// network; the read.
pub(crate) async fn legs_page(
    container: &Container<Transport>,
    db: &Db,
    id: &WalletId,
    asset: &str,
    page: Page<leg::Key>,
) -> Result<Vec<Movement>, Error> {
    let (_, index) = index_id(container, id, asset)?;
    Ok(db
        .read(move |connection| Ok(leg::db::find_page(connection, &index, page)?))
        .await?)
}

/// Enqueues one send job of `asset` from keyed wallet `id`: `transfers` in
/// order, each amount in the asset's atomic units and each destination
/// parsed by the wallet's chain; the job's id.
///
/// # Errors
///
/// Invalid for a malformed address, amount or destination (naming its
/// transfer), or a send the queue refuses; not found for an unconfigured
/// network or asset; conflict for a wallet that holds no key; unavailable
/// when the store or the queue is.
pub(crate) async fn send(
    container: &Container<Transport>,
    db: &Db,
    queue: &Queue,
    id: &WalletId,
    asset: &str,
    transfers: Vec<Transfer>,
) -> Result<tasks::Id, Error> {
    let from = container.parse(id)?;
    let info = asset_info(container, &from.network, asset)?;
    let id = WalletId::of(&from)?;
    let keyed = db
        .read(move |connection| Ok(wallet::db::is_keyed(connection, &id)?))
        .await?;
    if !keyed {
        return Err(Error::conflict("the wallet holds no key here"));
    }
    let mut payments = Vec::with_capacity(transfers.len());
    for (index, transfer) in transfers.into_iter().enumerate() {
        let units = transfer
            .amount
            .parse::<Decimal>()
            .ok()
            .filter(|amount| amount > &Decimal::zero())
            .and_then(|amount| info.atomic(&amount).ok())
            .ok_or_else(|| {
                Error::invalid(
                    "amount must be a positive exact decimal within the asset's decimals",
                )
                .at(index)
            })?;
        let to = container
            .parse(&Address::new(from.network.clone(), &transfer.destination))
            .map_err(|error| Error::from(error).at(index))?;
        payments.push(Payment {
            to: to.value,
            amount: units.to_string(),
        });
    }
    let intent = SendIntent {
        chain: from.network.chain.to_string(),
        network: from.network.name.clone(),
        address: from.value.clone(),
        asset: info.asset.id.clone(),
        transfers: payments,
    };
    let queued = Broadcast::enqueue(queue, &intent)
        .await
        .map_err(|error| match error {
            tasks::StoreError::Invalid(reason) => Error::invalid(reason),
            error => Error::unavailable(error.to_string()),
        })?;
    // A send is not unique: it is always queued.
    let (tasks::Admission::Queued(id) | tasks::Admission::Duplicate(id)) = queued;
    Ok(id)
}

/// Wallet `id`'s address in its chain's canonical form, and the model id of
/// its index of `asset`.
fn index_id(
    container: &Container<Transport>,
    id: &WalletId,
    asset: &str,
) -> Result<(Address, Id), Error> {
    let address = container.parse(id)?;
    let index = Id::child(WalletId::of(&address)?.id(), &[asset])?;
    Ok((address, index))
}

/// The asset `id` of `network` and its description.
///
/// # Errors
///
/// Not found when the network does not configure it.
fn asset_info(
    container: &Container<Transport>,
    network: &base::Network,
    id: &str,
) -> Result<base::AssetInfo, Error> {
    let asset = base::Asset {
        network: network.clone(),
        id: id.to_owned(),
    };
    container
        .service(network)?
        .assets
        .get(&asset)
        .cloned()
        .ok_or_else(|| Error::not_found("asset is not configured"))
}
