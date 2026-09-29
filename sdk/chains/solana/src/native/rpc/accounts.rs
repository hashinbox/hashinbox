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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::native::rpc::script::{Script, Scripted};

    fn account(owner: &Address, lamports: u64, data: &str, space: u64) -> Value {
        json!({
            "lamports": lamports,
            "owner": owner.to_string(),
            "executable": false,
            "data": [data, "base64"],
            "space": space
        })
    }

    #[tokio::test]
    async fn balance_context_floor_accepts_equal_maximum_and_optional_absence() {
        let address = Address::from_bytes([7; 32]);
        for (slot, minimum, accepted) in [
            (0, None, true),
            (0, Some(0), true),
            (u64::MAX, Some(u64::MAX), true),
            (u64::MAX - 1, Some(u64::MAX), false),
        ] {
            let mut config = json!({"commitment":"finalized"});
            if let Some(min_slot) = minimum {
                config["minContextSlot"] = json!(min_slot);
            }
            let rpc = Script::exchanges([(
                "getBalance",
                json!([address.to_string(), config]),
                json!({"context":{"slot":slot},"value":u64::MAX}),
            )]);
            let result = rpc.client().finalized_balance(&address, minimum).await;
            if accepted {
                let context = result.unwrap();
                assert_eq!(context.slot, slot);
                assert_eq!(context.value.units(), u64::MAX);
            } else {
                assert_eq!(result.unwrap_err(), Error::below_min_slot());
            }
            rpc.exhausted();
        }
    }

    #[tokio::test]
    async fn decodes_complete_accounts_and_finalized_balances() {
        let address = Address::from_bytes([7; 32]);
        let owner = Address::from_bytes([0; 32]);
        let rpc = Script::exchanges([
            (
                "getAccountInfo",
                json!([
                    address.to_string(),
                    {"encoding": "base64", "commitment": "confirmed", "minContextSlot": 4},
                ]),
                json!({"context":{"slot":5,"apiVersion":"x"},"value":account(&owner,9,"AQI=",2)}),
            ),
            (
                "getMultipleAccounts",
                json!([
                    [address.to_string(), owner.to_string()],
                    {"encoding": "base64", "commitment": "confirmed", "minContextSlot": 5},
                ]),
                json!({"context":{"slot":6},"value":[null,account(&owner,u64::MAX,"",0)]}),
            ),
            (
                "getBalance",
                json!([address.to_string(), {"commitment":"finalized"}]),
                json!({"context":{"slot":6},"value":u64::MAX}),
            ),
        ]);
        let client = rpc.client();
        let one = client
            .account(&address, Commitment::Confirmed, Some(4))
            .await
            .unwrap();
        assert_eq!(one.slot, 5);
        assert_eq!(one.value.unwrap().data(), &[1, 2]);
        let many = client
            .accounts(&[address.clone(), owner], Commitment::Confirmed, Some(5))
            .await
            .unwrap();
        assert_eq!(many.slot, 6);
        assert!(many.value[0].is_none());
        assert_eq!(many.value[1].as_ref().unwrap().lamports().units(), u64::MAX);
        let balance = client.balance(&address, &AssetKind::Native).await.unwrap();
        assert_eq!(balance.decimal().to_string(), "18446744073.709551615");
        rpc.exhausted();
    }

    #[tokio::test]
    async fn rejects_multi_bounds_cardinality_and_every_account_encoding_violation() {
        let address = Address::from_bytes([7; 32]);
        let owner = Address::from_bytes([0; 32]);
        let client = Script::exchanges([]).client();
        for addresses in [Vec::new(), vec![address.clone(); 101]] {
            let error = client
                .accounts(&addresses, Commitment::Confirmed, None)
                .await
                .unwrap_err();
            assert_eq!(error.kind, ErrorKind::InvalidConfiguration);
        }

        for (value, method) in [
            (json!([]), "getMultipleAccounts"),
            (json!([account(&owner, 1, "AQI=", 1)]), "account"),
            (json!([account(&owner, 1, "AQ", 1)]), "account"),
            (
                json!([{
                    "lamports": 1, "owner": "bad", "executable": false,
                    "data": ["", "base64"], "space": 0,
                }]),
                "account",
            ),
            (
                json!([{
                    "lamports": 1, "owner": owner.to_string(), "executable": false,
                    "data": ["", "base58"], "space": 0,
                }]),
                "account",
            ),
        ] {
            let client = Script::exchanges([(
                "getMultipleAccounts",
                json!([[address.to_string()], {"encoding":"base64", "commitment":"confirmed"}]),
                json!({"context":{"slot":1},"value":value}),
            )])
            .client();
            let error = client
                .accounts(std::slice::from_ref(&address), Commitment::Confirmed, None)
                .await
                .unwrap_err();
            assert_eq!(error, Error::malformed(method));
        }
    }

    #[tokio::test]
    async fn account_shape_and_cardinality_errors_follow_context_floor() {
        let address = Address::from_bytes([7; 32]);
        for (slot, value, message) in [
            (
                3,
                json!([]),
                "Solana RPC response is below its requested context floor",
            ),
            (
                4,
                json!([]),
                "Solana RPC getMultipleAccounts returned malformed data",
            ),
            (
                4,
                json!([{
                    "lamports": 1, "owner": "bad", "executable": false,
                    "data": ["", "base64"], "space": 0,
                }]),
                "Solana RPC account returned malformed data",
            ),
        ] {
            let rpc = Script::exchanges([(
                "getMultipleAccounts",
                json!([
                    [address.to_string()],
                    {"encoding": "base64", "commitment": "confirmed", "minContextSlot": 4},
                ]),
                json!({"context":{"slot":slot},"value":value}),
            )]);
            let error = rpc
                .client()
                .accounts(
                    std::slice::from_ref(&address),
                    Commitment::Confirmed,
                    Some(4),
                )
                .await
                .unwrap_err();
            assert_eq!(error.kind, ErrorKind::RpcUnavailable);
            assert_eq!(error.message, message);
            rpc.exhausted();
        }
    }

    /// The base64 SPL Token account layout: mint, owner, amount, and the
    /// initialized state byte, in 165 bytes.
    fn token_data(mint: &Address, owner: &Address, units: u64) -> String {
        let mut data = vec![0; 165];
        data[..32].copy_from_slice(mint.as_bytes());
        data[32..64].copy_from_slice(owner.as_bytes());
        data[64..72].copy_from_slice(&units.to_le_bytes());
        data[108] = 1;
        STANDARD.encode(data)
    }

    /// The base64 SPL Token mint layout: `decimals` at 44, initialized at 45.
    fn mint_data(decimals: u8) -> String {
        let mut data = vec![0; 82];
        data[44] = decimals;
        data[45] = 1;
        STANDARD.encode(data)
    }

    fn token_program() -> Address {
        Address::from_bytes(crate::native::spl::TOKEN_PROGRAM)
    }

    fn held(mint: &Address, owner: &Address, units: u64) -> Value {
        json!({
            "pubkey": Address::from_bytes([42; 32]).to_string(),
            "account": account(&token_program(), 2_039_280, &token_data(mint, owner, units), 165),
        })
    }

    fn token_script(owner: &Address, mint: &Address, value: Value) -> Script {
        Script::exchanges([(
            "getTokenAccountsByOwner",
            json!([
                owner.to_string(),
                {"mint": mint.to_string()},
                {"encoding": "base64", "commitment": "finalized"},
            ]),
            json!({"context": {"slot": 12}, "value": value}),
        )])
    }

    #[tokio::test]
    async fn a_token_balance_is_every_token_account_of_that_mint_and_owner() {
        let (owner, mint) = (Address::from_bytes([7; 32]), Address::from_bytes([9; 32]));
        let rpc = token_script(
            &owner,
            &mint,
            json!([held(&mint, &owner, 400_000), held(&mint, &owner, 200_000)]),
        );
        let reading = rpc
            .client()
            .finalized_token_balance(&owner, &mint)
            .await
            .expect("two token accounts of one mint");
        assert_eq!((reading.slot, reading.value.units()), (12, 600_000));
        rpc.exhausted();

        let rpc = token_script(&owner, &mint, json!([]));
        let empty = rpc
            .client()
            .finalized_token_balance(&owner, &mint)
            .await
            .expect("no token account is zero, not an error");
        assert_eq!(empty.value, Lamport::ZERO);
        rpc.exhausted();
    }

    #[tokio::test]
    async fn a_token_balance_refuses_a_foreign_account_and_an_overflowing_sum() {
        let (owner, mint) = (Address::from_bytes([7; 32]), Address::from_bytes([9; 32]));
        let other = Address::from_bytes([8; 32]);
        for value in [
            json!([held(&other, &owner, 1)]),
            json!([held(&mint, &other, 1)]),
            json!([held(&mint, &owner, u64::MAX), held(&mint, &owner, 1)]),
        ] {
            let rpc = token_script(&owner, &mint, value);
            let error = rpc
                .client()
                .finalized_token_balance(&owner, &mint)
                .await
                .expect_err("a balance must be exactly this owner's units of this mint");
            assert_eq!(error, Error::malformed("getTokenAccountsByOwner"));
            rpc.exhausted();
        }
    }

    #[tokio::test]
    async fn a_configured_mint_is_checked_against_the_chain_before_anything_opens() {
        let mint = Address::from_bytes([9; 32]);
        let script = |value: Value| {
            Script::exchanges([(
                "getAccountInfo",
                json!([
                    mint.to_string(),
                    {"encoding": "base64", "commitment": "finalized"},
                ]),
                json!({"context": {"slot": 12}, "value": value}),
            )])
        };
        let rpc = script(account(&token_program(), 1_461_600, &mint_data(6), 82));
        rpc.client()
            .validate_token(&mint, 6)
            .await
            .expect("the chain agrees with the configured decimals");
        rpc.exhausted();

        let rpc = script(account(&token_program(), 1_461_600, &mint_data(9), 82));
        let error = rpc.client().validate_token(&mint, 6).await.unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidConfiguration);
        assert_eq!(
            error.message,
            "Solana mint decimals 9 do not match configured decimals 6"
        );
        rpc.exhausted();

        let rpc = script(Value::Null);
        let error = rpc.client().validate_token(&mint, 6).await.unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidConfiguration);
        assert_eq!(
            error.message,
            "configured Solana mint account does not exist"
        );
        rpc.exhausted();

        let confidential = Address::from_bytes(crate::native::spl::TOKEN_2022_PROGRAM);
        let rpc = script(account(&confidential, 1_461_600, &mint_data(6), 82));
        let error = rpc.client().validate_token(&mint, 6).await.unwrap_err();
        assert_eq!(error.kind, ErrorKind::Unsupported);
        assert!(!error.retryable);
        rpc.exhausted();
    }

    fn bracket_script(
        query: Vec<String>,
        values: Value,
        opening: u64,
        read: u64,
        closing: u64,
    ) -> Script {
        Script::exchanges(vec![
            ("getHealth", json!([]), json!("ok")),
            (
                "getSlot",
                json!([{"commitment":"confirmed"}]),
                json!(opening),
            ),
            (
                "getMultipleAccounts",
                json!([query, {
                    "encoding": "base64", "commitment": "confirmed", "minContextSlot": opening,
                }]),
                json!({"context": {"slot": read}, "value": values}),
            ),
            (
                "getSlot",
                json!([{"commitment":"confirmed","minContextSlot":read}]),
                json!(closing),
            ),
        ])
    }

    #[tokio::test]
    async fn bracket_reads_each_address_once_at_one_bracketed_slot() {
        let (source, destination, other) = (
            Address::from_bytes([7; 32]),
            Address::from_bytes([8; 32]),
            Address::from_bytes([9; 32]),
        );
        let system = Address::from_bytes([0; 32]);
        let rpc = bracket_script(
            vec![
                source.to_string(),
                destination.to_string(),
                other.to_string(),
            ],
            json!([
                account(&system, 20, "", 0),
                null,
                account(&system, 30, "", 0)
            ]),
            10,
            11,
            12,
        );
        let bracket = rpc
            .client()
            .bracket(&[
                source.clone(),
                destination.clone(),
                other.clone(),
                destination.clone(),
                source.clone(),
            ])
            .await
            .unwrap();
        assert_eq!(bracket.min_slot, 12);
        assert_eq!(bracket.balance(&source, &destination).unwrap().units(), 20);
        assert_eq!(bracket.balance(&other, &destination).unwrap().units(), 30);
        assert_eq!(bracket.balance(&destination, &source).unwrap().units(), 0);
        rpc.exhausted();
    }

    #[tokio::test]
    async fn bracket_preserves_first_appearance_through_the_public_maximum() {
        let mut addresses = Vec::new();
        let mut expected = Vec::new();
        for index in 0..50_u8 {
            let source = Address::from_bytes([index + 1; 32]);
            let destination = Address::from_bytes([index + 101; 32]);
            expected.extend([source.to_string(), destination.to_string()]);
            addresses.extend([source, destination]);
        }
        let rpc = bracket_script(expected, json!(vec![Value::Null; 100]), 10, 11, 12);
        let bracket = rpc.client().bracket(&addresses).await.unwrap();
        assert_eq!((bracket.min_slot, bracket.accounts.len()), (12, 100));
        rpc.exhausted();
    }

    #[tokio::test]
    async fn the_maximum_slot_is_published_only_after_a_matching_closing_read() {
        let (source, destination) = (Address::from_bytes([7; 32]), Address::from_bytes([8; 32]));
        let system = Address::from_bytes([0; 32]);
        for (closing, accepted) in [(u64::MAX, true), (u64::MAX - 1, false)] {
            let rpc = bracket_script(
                vec![source.to_string(), destination.to_string()],
                json!([account(&system, 1, "", 0), null]),
                u64::MAX,
                u64::MAX,
                closing,
            );
            let result = rpc
                .client()
                .bracket(&[source.clone(), destination.clone()])
                .await;
            match result {
                Ok(bracket) => {
                    assert!(accepted);
                    assert_eq!(bracket.min_slot, u64::MAX);
                }
                Err(error) => {
                    assert!(!accepted);
                    assert_eq!(error, Error::below_min_slot());
                }
            }
            rpc.exhausted();
        }
    }
}
