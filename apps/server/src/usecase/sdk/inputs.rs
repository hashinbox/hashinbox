//! `base::InputStore` on the `leg` model: unspent coins of addresses.

use std::collections::BTreeMap;

use base::{Address, Future, InputStore, Inputs, Network};

use super::Stores;
use crate::models::leg;

impl InputStore for Stores {
    fn find_unspent<'a>(&'a self, addresses: &'a [Address]) -> Future<'a, anyhow::Result<Inputs>> {
        let mut by_network: BTreeMap<Network, Vec<String>> = BTreeMap::new();
        for address in addresses {
            by_network
                .entry(address.network.clone())
                .or_default()
                .push(address.value.clone());
        }
        Box::pin(self.db.read(move |connection| {
            let mut inputs = Vec::new();
            for (network, values) in &by_network {
                let values: Vec<&str> = values.iter().map(String::as_str).collect();
                inputs.extend(leg::db::find_unspent(connection, network, &values)?);
            }
            Ok(Inputs::from(inputs))
        }))
    }
}
