//! [`Transaction`]: an Ethereum transaction and its base form, kept together.
//! Built for sending and signed, or read from a block.

use std::collections::BTreeMap;

use alloy_consensus::{SignableTransaction as _, TxEip1559, TxEnvelope};
use alloy_eips::Encodable2718 as _;
use alloy_primitives::{B256, Bytes, Signature, TxKind, U64, U256, hex, keccak256};
use anyhow::{Context as _, anyhow, bail, ensure};
use base::{
    Addressed as _, Asset, Decimal, Leg, Networked as _, Output, Reason, Transacted, TransactionId,
    TransactionState,
};
use crypto::PrivateKey as _;
use num_bigint::BigUint;
use serde::Deserialize;
use serde_json::Value;

use crate::{Address, KeyPair, Network, Tokens, erc20};

/// An Ethereum transaction: the EIP-1559 fields it signs (none for one read
/// from a block, which is read as JSON, not as its signed bytes) and its
/// base form (id, state, order, legs, bytes). Legs never change after
/// construction; [`Transaction::sign`] sets the id and bytes, since an
/// Ethereum id is the hash of the signed bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transaction {
    native: Option<TxEip1559>,
    base: base::Transaction,
}

impl Transaction {
    /// Pending and unsigned: `native` (the transfer's call, with its nonce, gas
    /// and fees set) sent by `from` to pay `output`. Its legs are the
    /// transfer and the worst-case fee, `gas limit * max fee`, which is all a
    /// sender knows before it is mined.
    #[must_use]
    pub fn pending(native: TxEip1559, from: &Address, output: &Output<Address>) -> Self {
        let network = from.network().clone();
        let fee = Decimal::from(native.gas_limit) * decimal(U256::from(native.max_fee_per_gas));
        let legs = vec![
            Leg::Spent {
                address: from.address().clone(),
                asset: output.asset.clone(),
                amount: output.amount.clone(),
                coin: None,
            },
            Leg::Received {
                address: output.address.address().clone(),
                asset: output.asset.clone(),
                amount: output.amount.clone(),
                coin: None,
            },
            Leg::Fee {
                address: from.address().clone(),
                asset: Asset::native(network.clone()),
                amount: fee,
            },
        ];
        Self::new(
            Some(native),
            TransactionId::new(network, String::new()),
            TransactionState::Pending,
            None,
            legs,
        )
    }

    /// A mined transaction: its object in `eth_getBlockByNumber(n, true)`,
    /// its receipt, and the token `Transfer` logs `eth_getLogs` returned,
    /// by transaction hash. The receipt gives only its status and fee (until
    /// those come from elsewhere, every block's receipts are still fetched).
    /// Ether a contract moves inside the transaction (internal transfers) is
    /// not seen: standard RPC records it nowhere. Legs are of ether and of
    /// the allowlisted `tokens` only; a reverted transaction keeps only its
    /// fee.
    ///
    /// # Errors
    ///
    /// When a field the legs need does not decode.
    pub(crate) fn from_rpc(
        transaction: &Value,
        receipt: &Value,
        logs: &BTreeMap<B256, Vec<Log>>,
        network: &Network,
        tokens: &Tokens,
    ) -> anyhow::Result<Self> {
        let listed = Listed::deserialize(transaction).context("Ethereum transaction")?;
        let receipt = Receipt::deserialize(receipt).context("Ethereum receipt")?;
        let succeeded = receipt.status == U64::from(1);
        let logs = logs.get(&listed.hash).map_or(&[][..], Vec::as_slice);
        let legs = legs(&listed, &receipt, logs, succeeded, network, tokens);
        let state = if succeeded {
            TransactionState::Succeeded
        } else {
            TransactionState::Reverted(Some(Reason::Reverted))
        };
        Ok(Self::new(
            None,
            TransactionId::new(network.into(), hex::encode_prefixed(listed.hash)),
            state,
            Some(listed.transaction_index.to()),
            legs,
        ))
    }

    fn new(
        native: Option<TxEip1559>,
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
            expires: None,
        };
        Self { native, base }
    }

    /// Signs with `key` and sets the id and the EIP-2718 bytes.
    ///
    /// # Errors
    ///
    /// For a mined transaction, one `key` does not pay for, or a failed
    /// signature.
    pub fn sign(&mut self, key: &KeyPair) -> anyhow::Result<()> {
        let Some(native) = &self.native else {
            bail!("a mined Ethereum transaction cannot be signed again");
        };
        let payer = self.base.legs.iter().find_map(|leg| match leg {
            Leg::Fee { address, .. } => Some(address),
            _ => None,
        });
        ensure!(
            payer == Some(key.address()),
            "Ethereum transaction does not belong to the signing key"
        );
        let signature = key.key.sign(&native.signature_hash().0)?;
        let signature = Signature::try_from(signature.as_slice())
            .map_err(|_| anyhow!("Ethereum signature is not recoverable"))?;
        let envelope = TxEnvelope::from(native.clone().into_signed(signature));
        self.base.bytes = envelope.encoded_2718();
        self.base.id.id = hex::encode_prefixed(keccak256(&self.base.bytes));
        Ok(())
    }

    /// The EIP-1559 fields it signs; `None` when read from a block.
    #[must_use]
    pub const fn native(&self) -> Option<&TxEip1559> {
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

impl From<Transaction> for base::Transaction {
    fn from(transaction: Transaction) -> Self {
        transaction.base
    }
}

/// What paying `output` calls, with every other field unset: ether to the
/// recipient, or, for a `contract` token, `transfer(recipient, amount)` to
/// the contract with no ether.
///
/// # Errors
///
/// For an amount that is not a whole 256-bit count of atomic units.
pub(crate) fn request(
    output: &Output<Address>,
    contract: Option<alloy_primitives::Address>,
) -> anyhow::Result<TxEip1559> {
    let amount = output
        .amount
        .to_atomic_be_bytes::<32>(0)
        .map_err(|error| anyhow!("Ethereum amount: {}", error.message))?;
    let amount = U256::from_be_bytes(amount);
    let recipient = *output.address.as_native();
    let (to, value, input) = match contract {
        None => (recipient, amount, Vec::new()),
        Some(token) => (token, U256::ZERO, erc20::transfer(recipient, amount)),
    };
    Ok(TxEip1559 {
        to: TxKind::Call(to),
        value,
        input: Bytes::from(input),
        ..TxEip1559::default()
    })
}

/// `amount` atomic units as a decimal count of them.
pub(crate) fn decimal(amount: U256) -> Decimal {
    Decimal::from_atomic(BigUint::from_bytes_be(&amount.to_be_bytes::<32>()), 0)
}

/// The fields of a block's transaction object the legs need.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Listed {
    hash: B256,
    from: alloy_primitives::Address,
    to: Option<alloy_primitives::Address>,
    value: U256,
    transaction_index: U64,
}

/// The fields of a receipt the legs need.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Receipt {
    status: U64,
    gas_used: U256,
    effective_gas_price: U256,
    contract_address: Option<alloy_primitives::Address>,
}

/// One log `eth_getLogs` returned: the fields the legs need, and where it is.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Log {
    address: alloy_primitives::Address,
    topics: Vec<B256>,
    data: Bytes,
    /// The transaction that emitted it.
    pub(crate) transaction_hash: B256,
    /// The block it is in, to check it is the block fetched.
    pub(crate) block_hash: B256,
    /// That block's number.
    pub(crate) block_number: U64,
}

/// The legs of a mined transaction, in order:
///
/// 1. Its ether `value`: `Spent` by the sender, `Received` by the target
///    (or the created contract).
/// 2. Each allowlisted token `Transfer` log of `logs`: `Spent` and `Received`; a mint
///    (from zero) is `Minted`, a burn (to zero) `Burned`.
/// 3. The fee, `gasUsed * effectiveGasPrice`, of the sender.
///
/// A reverted transaction moved nothing but its fee.
fn legs(
    listed: &Listed,
    receipt: &Receipt,
    logs: &[Log],
    succeeded: bool,
    network: &Network,
    tokens: &Tokens,
) -> Vec<Leg> {
    let address = |native| base::Address::from(Address::new(native, network));
    let ether = Asset::native(network.into());
    let fee = decimal(receipt.gas_used) * decimal(receipt.effective_gas_price);
    let mut legs = Vec::new();
    if succeeded {
        let target = listed.to.or(receipt.contract_address);
        let moved = target.filter(|_| !listed.value.is_zero());
        let value = decimal(listed.value);
        if let Some(to) = moved {
            legs.push(Leg::Spent {
                address: address(listed.from),
                asset: ether.clone(),
                amount: value.clone(),
                coin: None,
            });
            legs.push(Leg::Received {
                address: address(to),
                asset: ether.clone(),
                amount: value.clone(),
                coin: None,
            });
        }
        for log in logs.iter().filter(|log| tokens.contains(&log.address)) {
            let Some((from, to, amount)) = erc20::transfer_log(&log.topics, &log.data) else {
                continue;
            };
            if amount.is_zero() || (from.is_zero() && to.is_zero()) {
                continue;
            }
            let asset = Asset {
                network: network.into(),
                id: hex::encode_prefixed(log.address),
            };
            let amount = decimal(amount);
            if from.is_zero() {
                legs.push(Leg::Minted {
                    address: address(to),
                    asset,
                    amount,
                });
            } else if to.is_zero() {
                legs.push(Leg::Burned {
                    address: address(from),
                    asset,
                    amount,
                });
            } else {
                legs.push(Leg::Spent {
                    address: address(from),
                    asset: asset.clone(),
                    amount: amount.clone(),
                    coin: None,
                });
                legs.push(Leg::Received {
                    address: address(to),
                    asset,
                    amount,
                    coin: None,
                });
            }
        }
    }
    if !fee.is_zero() {
        legs.push(Leg::Fee {
            address: address(listed.from),
            asset: ether,
            amount: fee,
        });
    }
    legs
}
