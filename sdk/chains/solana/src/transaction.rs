//! [`Transaction`]: a Solana transaction and its base form, kept together.
//! Built for sending and signed, or read from a block.

use std::{collections::BTreeMap, ops::Range};

use anyhow::{Context as _, anyhow, ensure};
use base::{
    Addressed as _, Asset, Decimal, Enveloped, Leg, Networked as _, Output, Outputs, Reason,
    Transacted, TransactionId, TransactionState,
};
use crypto::PrivateKey as _;
use serde::Deserialize;
use serde_json::Value;
use solana_hash::Hash;
use solana_instruction::Instruction;
use solana_message::Message;
use solana_signature::Signature;

use crate::{Address, KeyPair, Network, Token, Tokens, spl};

/// The largest signed transaction a node accepts, in bytes.
const PACKET_DATA_SIZE: usize = 1_232;
/// What one signature adds to the message: its count and its bytes.
const SIGNATURE_BYTES: usize = 1 + 64;

/// A Solana transaction: the legacy message it signs (none for one read from
/// a block, which is read as JSON) and its base form (id, state, order, legs,
/// bytes). Legs never change after construction; [`Transaction::sign`] sets
/// the id and bytes, since a Solana id is the first signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transaction {
    native: Option<Message>,
    base: base::Transaction,
}

impl Transaction {
    /// Pending and unsigned: `message`, paid by `from` with `fee` lamports,
    /// paying `outputs`. Its legs are the fee, then a `Spent` and a
    /// `Received` per output. The rent of a token account it creates is not
    /// a leg.
    #[must_use]
    pub fn pending(
        message: Message,
        from: &Address,
        outputs: &[Output<Address>],
        fee: u64,
    ) -> Self {
        let network = from.network().clone();
        let mut legs = vec![Leg::Fee {
            address: from.address().clone(),
            asset: Asset::native(network.clone()),
            amount: Decimal::from(fee),
        }];
        for output in outputs {
            legs.push(Leg::Spent {
                address: from.address().clone(),
                asset: output.asset.clone(),
                amount: output.amount.clone(),
                coin: None,
            });
            legs.push(Leg::Received {
                address: output.address.address().clone(),
                asset: output.asset.clone(),
                amount: output.amount.clone(),
                coin: None,
            });
        }
        Self::new(
            Some(message),
            TransactionId::new(network, String::new()),
            TransactionState::Pending,
            None,
            legs,
        )
    }

    /// A mined transaction: entry `order` of a `getBlock` (`json` encoding,
    /// full details). Legs are of SOL and of the allowlisted `tokens` only; a
    /// failed transaction keeps only its fee.
    ///
    /// # Errors
    ///
    /// When a field the legs need does not decode, or its balances do not
    /// match its accounts.
    pub fn from_rpc(
        transaction: &Value,
        order: u64,
        network: &Network,
        tokens: &Tokens,
    ) -> anyhow::Result<Self> {
        let wire = Wire::deserialize(transaction).context("Solana transaction")?;
        let id = wire
            .transaction
            .signatures
            .first()
            .context("Solana transaction has no signature")?
            .clone();
        let meta = wire.meta.context("Solana transaction has no metadata")?;
        let succeeded = meta.err.is_null();
        let legs = legs(wire.transaction.message.account_keys, meta, network, tokens)?;
        let state = if succeeded {
            TransactionState::Succeeded
        } else {
            TransactionState::Reverted(Some(Reason::Reverted))
        };
        Ok(Self::new(
            None,
            TransactionId::new(network.into(), id),
            state,
            Some(order),
            legs,
        ))
    }

    fn new(
        native: Option<Message>,
        id: TransactionId,
        state: TransactionState,
        order: Option<u64>,
        legs: Vec<Leg>,
    ) -> Self {
        let base = base::Transaction {
            id,
            state,
            order,
            legs,
            bytes: Vec::new(),
        };
        Self { native, base }
    }

    /// Signs with `key` and sets the id (the signature) and the wire bytes.
    ///
    /// # Errors
    ///
    /// For a mined transaction, one `key` does not pay for, or a failed
    /// signature.
    pub fn sign(&mut self, key: &KeyPair) -> anyhow::Result<()> {
        let message = self
            .native
            .as_ref()
            .context("a mined Solana transaction cannot be signed again")?;
        ensure!(
            message.account_keys.first() == Some(key.native_address().as_native()),
            "Solana transaction does not belong to the signing key"
        );
        let signature: [u8; 64] = key
            .key
            .sign(&message.serialize())?
            .try_into()
            .map_err(|_| anyhow!("Solana signature is not 64 bytes"))?;
        let signature = Signature::from(signature);
        let signed = solana_transaction::Transaction {
            signatures: vec![signature],
            message: message.clone(),
        };
        self.base.bytes = bincode::serialize(&signed).context("Solana transaction encoding")?;
        self.base.id.id = signature.to_string();
        Ok(())
    }

    /// The message it signs; `None` when read from a block.
    #[must_use]
    pub const fn native(&self) -> Option<&Message> {
        self.native.as_ref()
    }

    /// Its base form.
    #[must_use]
    pub const fn base(&self) -> &base::Transaction {
        &self.base
    }
}

impl Transacted for Transaction {
    fn txid(&self) -> &TransactionId {
        &self.base.id
    }
}

impl Enveloped for Transaction {
    fn envelope(&self) -> &[u8] {
        &self.base.bytes
    }
}

impl From<Transaction> for base::Transaction {
    fn from(transaction: Transaction) -> Self {
        transaction.base
    }
}

/// `outputs` from `from`, in order, as many per message as fit a packet, on
/// `blockhash`: lamports, or `token` with each recipient's token account
/// created when missing. Each message ends with a random Memo-v3, so two
/// equal transfers are distinct transactions. Each message comes with the
/// range of `outputs` it pays.
///
/// # Errors
///
/// For an amount that is not a whole `u64` of atomic units, no randomness, or
/// one transfer that does not fit a transaction.
pub(crate) fn messages(
    from: &Address,
    token: Option<&Token>,
    outputs: &Outputs<Address>,
    blockhash: &Hash,
) -> anyhow::Result<Vec<(Message, Range<usize>)>> {
    let from = from.as_native();
    let mut groups = Vec::with_capacity(outputs.len());
    for output in outputs.iter() {
        let units = output
            .amount
            .to_atomic_u64(0)
            .map_err(|error| anyhow!("Solana amount: {}", error.message))?;
        let to = output.address.as_native();
        groups.push(match token {
            None => vec![solana_system_interface::instruction::transfer(
                from, to, units,
            )],
            Some(token) => vec![
                spl::create_account(from, to, token)?,
                spl::transfer(from, to, token, units)?,
            ],
        });
    }
    let mut messages = Vec::new();
    let mut start = 0;
    while start < groups.len() {
        let memo = Instruction {
            program_id: spl_memo_interface::v3::ID,
            accounts: Vec::new(),
            data: memo()?,
        };
        let mut fitting = None;
        for end in start + 1..=groups.len() {
            let mut instructions: Vec<Instruction> =
                groups[start..end].iter().flatten().cloned().collect();
            instructions.push(memo.clone());
            let message = Message::new_with_blockhash(&instructions, Some(from), blockhash);
            if message.serialize().len() + SIGNATURE_BYTES > PACKET_DATA_SIZE {
                break;
            }
            fitting = Some((message, start..end));
        }
        let (message, range) = fitting.context("a Solana transfer does not fit a transaction")?;
        start = range.end;
        messages.push((message, range));
    }
    Ok(messages)
}

/// A random 32-byte token in Base58, the memo's exact data.
fn memo() -> anyhow::Result<Vec<u8>> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| anyhow!("no randomness for a Solana memo"))?;
    Ok(bs58::encode(bytes).into_string().into_bytes())
}

/// One entry of a block's `transactions`.
#[derive(Deserialize)]
struct Wire {
    transaction: Signed,
    meta: Option<Meta>,
}

#[derive(Deserialize)]
struct Signed {
    signatures: Vec<String>,
    message: Keys,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Keys {
    account_keys: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Meta {
    #[serde(default)]
    err: Value,
    fee: u64,
    pre_balances: Vec<u64>,
    post_balances: Vec<u64>,
    loaded_addresses: Option<Loaded>,
    pre_token_balances: Option<Vec<TokenBalance>>,
    post_token_balances: Option<Vec<TokenBalance>>,
}

#[derive(Deserialize)]
struct Loaded {
    writable: Vec<String>,
    readonly: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TokenBalance {
    mint: String,
    owner: Option<String>,
    ui_token_amount: TokenAmount,
}

#[derive(Deserialize)]
struct TokenAmount {
    amount: Option<String>,
}

/// The legs of a mined transaction, in order:
///
/// 1. The fee, of the fee payer (the first account).
/// 2. Each account's lamport change, the payer's without its fee: a decrease
///    is `Spent`, an increase `Received`.
/// 3. Each owner's net change in each allowlisted mint, the same way. A token
///    balance without an owner or a plain amount (a Token-2022 confidential
///    balance) names no wallet and is left out.
///
/// A failed transaction moved nothing but its fee. Mints and burns show as
/// balance changes, not `Minted`/`Burned` legs.
fn legs(
    mut keys: Vec<String>,
    meta: Meta,
    network: &Network,
    tokens: &Tokens,
) -> anyhow::Result<Vec<Leg>> {
    let network = base::Network::from(network);
    let address = |text: &str| base::Address::new(network.clone(), text);
    let sol = Asset::native(network.clone());
    if let Some(loaded) = meta.loaded_addresses {
        keys.extend(loaded.writable);
        keys.extend(loaded.readonly);
    }
    let payer = keys
        .first()
        .context("Solana transaction has no fee payer")?;
    let fee = Decimal::from(meta.fee);
    let mut legs = Vec::new();
    if !fee.is_zero() {
        legs.push(Leg::Fee {
            address: address(payer),
            asset: sol.clone(),
            amount: fee.clone(),
        });
    }
    if !meta.err.is_null() {
        return Ok(legs);
    }
    ensure!(
        meta.pre_balances.len() == keys.len() && meta.post_balances.len() == keys.len(),
        "Solana balances do not match the resolved accounts"
    );
    let balances = meta.pre_balances.iter().zip(&meta.post_balances);
    for (index, (key, (before, after))) in keys.iter().zip(balances).enumerate() {
        let mut change = Decimal::from(*after) - Decimal::from(*before);
        if index == 0 {
            change += &fee;
        }
        legs.extend(leg(address(key), &sol, change));
    }
    let mut changes = BTreeMap::new();
    let sides = [
        (meta.pre_token_balances, false),
        (meta.post_token_balances, true),
    ];
    for (balances, after) in sides {
        for balance in balances.unwrap_or_default() {
            let listed = balance
                .mint
                .parse()
                .is_ok_and(|mint| tokens.contains(&mint));
            let (Some(owner), Some(amount), true) =
                (balance.owner, balance.ui_token_amount.amount, listed)
            else {
                continue;
            };
            let amount: Decimal = amount.parse().context("Solana token amount")?;
            let change = changes
                .entry((owner, balance.mint))
                .or_insert_with(Decimal::zero);
            if after {
                *change += amount;
            } else {
                *change -= amount;
            }
        }
    }
    for ((owner, mint), change) in changes {
        let asset = Asset {
            network: network.clone(),
            id: mint,
        };
        legs.extend(leg(address(&owner), &asset, change));
    }
    Ok(legs)
}

/// `Received` for a positive `change`, `Spent` for a negative one.
fn leg(address: base::Address, asset: &Asset, change: Decimal) -> Option<Leg> {
    let zero = Decimal::zero();
    let asset = asset.clone();
    if change > zero {
        Some(Leg::Received {
            address,
            asset,
            amount: change,
            coin: None,
        })
    } else if change < zero {
        Some(Leg::Spent {
            address,
            asset,
            amount: zero - change,
            coin: None,
        })
    } else {
        None
    }
}
