//! `wallet_index_balance`: wallet indexes with their balances at a confirmed
//! tip; no table.

use std::collections::BTreeMap;

use anyhow::ensure;
use base::{Address, Asset, Decimal, Network, Networked};

use super::{balance::Balance, wallet_index::WalletIndex};

/// A wallet index with its balance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WalletIndexBalance {
    pub(crate) index: WalletIndex,
    pub(crate) balance: Balance,
}

/// Wallet indexes of one network with balances, by wallet index id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WalletIndexBalances {
    network: Network,
    /// The confirmed tip position the balances are at.
    confirmed: i64,
    indexes: BTreeMap<String, WalletIndexBalance>,
}

impl WalletIndexBalances {
    /// None yet, of `network` at confirmed tip `confirmed`.
    pub(crate) fn new(network: Network, confirmed: i64) -> Self {
        Self {
            network,
            confirmed,
            indexes: BTreeMap::new(),
        }
    }

    /// The confirmed tip position the balances are at.
    pub(crate) fn confirmed_at(&self) -> i64 {
        self.confirmed
    }

    /// Adds `one`.
    ///
    /// # Errors
    ///
    /// When it is of another network.
    pub(crate) fn insert(&mut self, one: WalletIndexBalance) -> anyhow::Result<()> {
        ensure!(
            *one.index.network() == self.network,
            "a wallet index is of another network"
        );
        self.indexes.insert(one.index.id.clone(), one);
        Ok(())
    }

    /// Their ids, in order.
    pub(crate) fn ids(&self) -> Vec<String> {
        self.indexes.keys().cloned().collect()
    }

    /// Their confirmed balances, in id order.
    pub(crate) fn confirmed(&self) -> impl Iterator<Item = Decimal> + '_ {
        self.indexes
            .values()
            .map(|one| one.balance.confirmed.clone())
    }

    /// Each one's (address, asset), what a fee estimate takes.
    pub(crate) fn sources(&self) -> Vec<(Address, Asset)> {
        self.indexes
            .values()
            .map(|one| (one.index.address.clone(), one.index.asset.clone()))
            .collect()
    }
}

impl std::ops::Deref for WalletIndexBalances {
    type Target = BTreeMap<String, WalletIndexBalance>;

    fn deref(&self) -> &Self::Target {
        &self.indexes
    }
}

impl Networked for WalletIndexBalances {
    fn network(&self) -> &Network {
        &self.network
    }
}

/// Wallet index selects with their balances.
pub(crate) mod db {
    use anyhow::Context as _;
    use base::{Network, Networked as _};
    use diesel::SqliteConnection;

    use super::{WalletIndexBalance, WalletIndexBalances};
    use crate::models::{
        balance,
        trigger::{Target, This},
        wallet_index::{self, Selector, WalletIndexes},
    };

    /// `indexes` of `network` with their balances at `confirmed`.
    ///
    /// # Errors
    ///
    /// As [`balance::db::find_by_ids`], or when an index is of another network.
    pub(crate) fn with_balances(
        connection: &mut SqliteConnection,
        network: &Network,
        indexes: WalletIndexes,
        confirmed: i64,
    ) -> anyhow::Result<WalletIndexBalances> {
        let ids: Vec<&str> = indexes.iter().map(|index| index.id.as_str()).collect();
        let balances = balance::db::find_by_ids(connection, &ids, confirmed)?;
        let mut found = WalletIndexBalances::new(network.clone(), confirmed);
        for index in indexes {
            let balance = balances
                .get(&index.id)
                .cloned()
                .context("a balance was not read")?;
            found.insert(WalletIndexBalance { index, balance })?;
        }
        Ok(found)
    }

    /// What `selector` picks on `network` at `confirmed` (as
    /// [`wallet_index::db::select`]), with balances.
    ///
    /// # Errors
    ///
    /// When a read fails or a stored value does not decode.
    pub(crate) fn select(
        connection: &mut SqliteConnection,
        network: &Network,
        selector: &Selector,
        confirmed: i64,
    ) -> anyhow::Result<WalletIndexBalances> {
        let indexes = wallet_index::db::select(connection, network, selector, confirmed)?;
        with_balances(connection, network, indexes, confirmed)
    }

    /// The fully indexed ones of `ids` on `network` at `confirmed`, with balances.
    ///
    /// # Errors
    ///
    /// As [`select`].
    pub(crate) fn find_by_ids(
        connection: &mut SqliteConnection,
        network: &Network,
        ids: &[String],
        confirmed: i64,
    ) -> anyhow::Result<WalletIndexBalances> {
        select(connection, network, &Selector::Ids(ids.to_vec()), confirmed)
    }

    /// What `target` names on `this`'s network at its tip, with `this` as
    /// `This`; `None` when a sibling is not fully indexed.
    ///
    /// # Errors
    ///
    /// As [`select`].
    pub(crate) fn find_by_target(
        connection: &mut SqliteConnection,
        this: &WalletIndexBalances,
        target: &Target,
    ) -> anyhow::Result<Option<WalletIndexBalances>> {
        let network = this.network();
        let confirmed = this.confirmed_at();
        Ok(match target {
            Target::This(This { asset: None }) => Some(this.clone()),
            Target::This(This { asset: Some(asset) }) => {
                let ids = this
                    .values()
                    .map(|one| one.index.sibling_id(asset))
                    .collect::<anyhow::Result<Vec<_>>>()?;
                let found = find_by_ids(connection, network, &ids, confirmed)?;
                (found.len() == ids.len()).then_some(found)
            }
            Target::Select(selector) => Some(select(connection, network, selector, confirmed)?),
        })
    }
}
