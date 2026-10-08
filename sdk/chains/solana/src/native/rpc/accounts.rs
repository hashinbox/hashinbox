//! Account reads: balances and the batch bracket.

use std::collections::{BTreeMap, BTreeSet};

use base::BoxFuture;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Client, Commitment, Reading, wire::ContextWire};
use crate::native::{Account, Address, AssetKind, Bracket, Error, ErrorKind, Lamport};

/// Account reads used by wallet balances and transaction preparation.
pub trait Accounts: Send + Sync {
    /// The finalized balance of `address` in `asset`: its lamports for native
    /// SOL, and the units its token accounts hold for a mint.
    fn balance<'a>(
        &'a self,
        address: &'a Address,
        asset: &'a AssetKind,
    ) -> BoxFuture<'a, Result<Lamport, Error>>;

    /// Every address in `addresses` (deduplicated, first appearance first),
    /// read at one confirmed slot between two slot reads: health, opening
    /// slot, accounts at or after it, closing slot at or after the read.
    fn bracket<'a>(&'a self, addresses: &'a [Address]) -> BoxFuture<'a, Result<Bracket, Error>>;
}

impl<C: json_rpc::Client> Accounts for Client<C> {
    fn balance<'a>(
        &'a self,
        address: &'a Address,
        asset: &'a AssetKind,
    ) -> BoxFuture<'a, Result<Lamport, Error>> {
        Box::pin(async move {
            match asset {
                AssetKind::Native => Ok(self.finalized_balance(address, None).await?.value),
                AssetKind::SplToken(mint) => {
                    Ok(self.finalized_token_balance(address, mint).await?.value)
                }
            }
        })
    }

    fn bracket<'a>(&'a self, addresses: &'a [Address]) -> BoxFuture<'a, Result<Bracket, Error>> {
        Box::pin(async move {
            let mut seen = BTreeSet::new();
            let query = addresses
                .iter()
                .filter(|address| seen.insert(*address))
                .cloned()
                .collect::<Vec<_>>();
            self.health().await?;
            let opening = self.slot(Commitment::Confirmed, None).await?;
            let context = self
                .accounts(&query, Commitment::Confirmed, Some(opening))
                .await?;
            let min_slot = self.slot(Commitment::Confirmed, Some(context.slot)).await?;
            Ok(Bracket {
                min_slot,
                accounts: query
                    .into_iter()
                    .zip(context.value)
                    .collect::<BTreeMap<_, _>>(),
            })
        })
    }
}

/// An account as the node spells it: the owner as text and the data as a
/// base64 tuple beside its claimed size. [`Account`] is built from it only
/// once the owner parses and the data is canonical and of that size.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AccountWire {
    lamports: u64,
    owner: String,
    executable: bool,
    data: Value,
    space: u64,
}

/// One entry of `getTokenAccountsByOwner`; its `pubkey` is the token
/// account's own address, which a position never needs.
#[derive(Deserialize)]
struct HeldWire {
    account: AccountWire,
}

impl<C: json_rpc::Client> Client<C> {
    /// One account at `commitment`, no older than `minimum`; `None` when it
    /// does not exist.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::RpcUnavailable`], also below `minimum`.
    pub async fn account(
        &self,
        address: &Address,
        commitment: Commitment,
        minimum: Option<u64>,
    ) -> Result<Reading<Option<Account>>, Error> {
        let context = self
            .request::<ContextWire<Option<AccountWire>>>(
                "getAccountInfo",
                json!([
                    address.to_string(),
                    Self::account_config(commitment, minimum)
                ]),
            )
            .await?
            .at_least(minimum)?;
        Ok(Reading {
            slot: context.slot,
            value: context.value.map(Account::try_from).transpose()?,
        })
    }

    /// Between 1 and 100 accounts read at one slot, in `addresses` order.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidConfiguration`] for another count, else as
    /// [`Client::account`].
    pub async fn accounts(
        &self,
        addresses: &[Address],
        commitment: Commitment,
        minimum: Option<u64>,
    ) -> Result<Reading<Vec<Option<Account>>>, Error> {
        if addresses.is_empty() || addresses.len() > 100 {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                "Solana multi-account reads require between 1 and 100 addresses",
            ));
        }
        let texts = addresses
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let context = self
            .request::<ContextWire<Vec<Option<AccountWire>>>>(
                "getMultipleAccounts",
                json!([texts, Self::account_config(commitment, minimum)]),
            )
            .await?
            .at_least(minimum)?;
        if context.value.len() != addresses.len() {
            return Err(Error::malformed("getMultipleAccounts"));
        }
        let value = context
            .value
            .into_iter()
            .map(|account| account.map(Account::try_from).transpose())
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Reading {
            slot: context.slot,
            value,
        })
    }

    /// The finalized balance of `address`, no older than `minimum`.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::RpcUnavailable`], also below `minimum`.
    pub async fn finalized_balance(
        &self,
        address: &Address,
        minimum: Option<u64>,
    ) -> Result<Reading<Lamport>, Error> {
        let context = self
            .request::<ContextWire<u64>>(
                "getBalance",
                json!([address.to_string(), Commitment::Finalized.config(minimum)]),
            )
            .await?
            .at_least(minimum)?;
        Ok(Reading {
            slot: context.slot,
            value: Lamport::from_atomic(context.value),
        })
    }

    /// The finalized units of `mint` that every token account `owner` holds
    /// adds up to; zero when it holds none.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::RpcUnavailable`], also for an account of another mint or
    /// owner, or a sum beyond the 64-bit unit range.
    pub async fn finalized_token_balance(
        &self,
        owner: &Address,
        mint: &Address,
    ) -> Result<Reading<Lamport>, Error> {
        let context = self
            .request::<ContextWire<Vec<HeldWire>>>(
                "getTokenAccountsByOwner",
                json!([
                    owner.to_string(),
                    {"mint": mint.to_string()},
                    Self::account_config(Commitment::Finalized, None)
                ]),
            )
            .await?
            .at_least(None)?;
        let mut units = Lamport::ZERO;
        for held in context.value {
            let account = Account::try_from(held.account)?;
            units = account
                .token_units(mint, owner)
                .and_then(|held| units.checked_add(held))
                .ok_or_else(|| Error::malformed("getTokenAccountsByOwner"))?;
        }
        Ok(Reading {
            slot: context.slot,
            value: units,
        })
    }

    /// Verifies that `mint` is an initialized SPL Token mint of exactly
    /// `decimals` decimals, so a deployment that configures a token this
    /// build cannot index refuses to start.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Unsupported`] for a Token-2022 mint, and
    /// [`ErrorKind::InvalidConfiguration`] for an account that is no mint,
    /// one this node does not hold, or decimals the chain contradicts.
    pub async fn validate_token(&self, mint: &Address, decimals: u8) -> Result<(), Error> {
        let account = self
            .account(mint, Commitment::Finalized, None)
            .await?
            .value
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::InvalidConfiguration,
                    "configured Solana mint account does not exist",
                )
            })?;
        let declared = account.mint_decimals()?;
        if declared != decimals {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                format!(
                    "Solana mint decimals {declared} do not match configured decimals {decimals}"
                ),
            ));
        }
        Ok(())
    }

    fn account_config(commitment: Commitment, minimum: Option<u64>) -> Value {
        let mut config = commitment.config(minimum);
        config["encoding"] = json!("base64");
        config
    }
}

impl TryFrom<AccountWire> for Account {
    type Error = Error;

    /// Requires canonical base64 data whose length equals the reported space.
    fn try_from(wire: AccountWire) -> Result<Self, Self::Error> {
        let malformed = || Error::malformed("account");
        let owner = wire.owner.parse::<Address>().map_err(|_| malformed())?;
        let tuple = wire.data.as_array().ok_or_else(malformed)?;
        if tuple.len() != 2 || tuple[1].as_str() != Some("base64") {
            return Err(malformed());
        }
        let encoded = tuple[0].as_str().ok_or_else(malformed)?;
        let data = STANDARD.decode(encoded).map_err(|_| malformed())?;
        if STANDARD.encode(&data) != encoded || u64::try_from(data.len()) != Ok(wire.space) {
            return Err(malformed());
        }
        Ok(Self::new(
            owner,
            Lamport::from_atomic(wire.lamports),
            wire.executable,
            data,
        ))
    }
}
