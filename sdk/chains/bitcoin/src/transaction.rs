//! [`Transaction`]: a Bitcoin transaction, the outputs its inputs spend, and
//! its base form, kept together. Built from inputs and signed, or read from a
//! block.

use anyhow::{Context as _, bail};
use base::{Asset, Inputs, Leg, Output, Transacted, TransactionId, TransactionState};
use bitcoin::{
    Amount, Denomination, EcdsaSighashType, OutPoint, Script, ScriptBuf, Sequence, TapSighashType,
    TxIn, TxOut, Witness, absolute, consensus,
    hashes::Hash as _,
    secp256k1::{ecdsa, schnorr},
    sighash::{Prevouts, SighashCache},
};
use crypto::{PrivateKey as _, Secp256k1Schnorr};

use crate::{Address, KeyPair, Network};

/// A Bitcoin transaction: the consensus transaction, one spent output per
/// input (empty for the coinbase), and its base form (id, state, order,
/// legs, bytes). The base form is kept in step: legs and id never change
/// after construction, and [`Transaction::sign`] refreshes the bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transaction {
    native: bitcoin::Transaction,
    spent: Vec<TxOut>,
    base: base::Transaction,
}

impl Transaction {
    /// Pending and unsigned: spends `inputs` (each paying its own address)
    /// and pays `outputs`, in order; version 2, no lock time, replaceable.
    ///
    /// # Errors
    ///
    /// For a stored txid or amount that is not Bitcoin's.
    pub fn from_inputs<'a>(
        inputs: &Inputs<Address>,
        outputs: impl IntoIterator<Item = &'a Output<Address>>,
        network: Network,
    ) -> anyhow::Result<Self> {
        let mut input = Vec::with_capacity(inputs.len());
        let mut spent = Vec::with_capacity(inputs.len());
        for coin in inputs.iter() {
            let txid = coin.coin.transaction.id.parse().context("stored txid")?;
            input.push(TxIn {
                previous_output: OutPoint::new(txid, coin.coin.index),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                witness: Witness::new(),
            });
            spent.push(TxOut {
                value: Amount::from_sat(coin.amount.to_atomic_u64(0)?),
                script_pubkey: coin.address.script_pubkey(),
            });
        }
        let output = outputs
            .into_iter()
            .map(|output| {
                Ok(TxOut {
                    value: Amount::from_sat(output.amount.to_atomic_u64(0)?),
                    script_pubkey: output.address.script_pubkey(),
                })
            })
            .collect::<anyhow::Result<_>>()?;
        let native = bitcoin::Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input,
            output,
        };
        Ok(Self::new(
            native,
            spent,
            network,
            TransactionState::Pending,
            None,
        ))
    }

    /// A mined transaction of Bitcoin Core's `getblock <hash> 3` (its `hex`
    /// and each input's `prevout`), at `order` in its block.
    ///
    /// # Errors
    ///
    /// When the hex, a prevout value or a prevout script does not decode.
    pub fn from_core(
        json: &serde_json::Value,
        network: Network,
        order: u64,
    ) -> anyhow::Result<Self> {
        let hex = json["hex"]
            .as_str()
            .context("Bitcoin transaction without hex")?;
        let native: bitcoin::Transaction =
            consensus::encode::deserialize_hex(hex).context("Bitcoin transaction bytes")?;
        let inputs = json["vin"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        let spent = inputs
            .iter()
            .filter_map(|input| input.get("prevout"))
            .map(|prevout| {
                Ok(TxOut {
                    value: Amount::from_str_in(
                        &prevout["value"].to_string(),
                        Denomination::Bitcoin,
                    )
                    .context("Bitcoin prevout value")?,
                    script_pubkey: ScriptBuf::from_hex(
                        prevout["scriptPubKey"]["hex"].as_str().unwrap_or_default(),
                    )
                    .context("Bitcoin prevout script")?,
                })
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(Self::new(
            native,
            spent,
            network,
            TransactionState::Succeeded,
            Some(order),
        ))
    }

    /// `native` spending `spent`, in `state` at `order`; computes its base
    /// form once.
    fn new(
        native: bitcoin::Transaction,
        spent: Vec<TxOut>,
        network: Network,
        state: TransactionState,
        order: Option<u64>,
    ) -> Self {
        let base = base::Transaction {
            id: TransactionId::new(network.into(), native.compute_txid().to_string()),
            state,
            order,
            legs: legs(&native, &spent, network),
            bytes: consensus::serialize(&native),
            expires: None,
        };
        Self {
            native,
            spent,
            base,
        }
    }

    /// Signs every input with the one of `keys` that owns it (`SIGHASH_ALL`;
    /// P2WPKH or P2TR by its script) and refreshes the bytes. The id and
    /// legs stay: a SegWit txid excludes witnesses.
    ///
    /// # Errors
    ///
    /// For an input no key owns, or a failed signature.
    pub fn sign(&mut self, keys: &[KeyPair]) -> anyhow::Result<()> {
        let mut cache = SighashCache::new(&self.native);
        let mut witnesses = Vec::with_capacity(self.spent.len());
        for (index, output) in self.spent.iter().enumerate() {
            let Some(key) = keys
                .iter()
                .find(|key| key.native_address().script_pubkey() == output.script_pubkey)
            else {
                bail!("Bitcoin input {index} belongs to no signing key");
            };
            let witness = if output.script_pubkey.is_p2wpkh() {
                let sighash = cache.p2wpkh_signature_hash(
                    index,
                    &output.script_pubkey,
                    output.value,
                    EcdsaSighashType::All,
                )?;
                let signature = key.key.sign(&sighash.to_byte_array())?;
                let signature =
                    ecdsa::Signature::from_compact(signature.get(..64).unwrap_or_default())?;
                Witness::p2wpkh(
                    &bitcoin::ecdsa::Signature::sighash_all(signature),
                    &key.public.0,
                )
            } else {
                let sighash = cache.taproot_key_spend_signature_hash(
                    index,
                    &Prevouts::All(&self.spent),
                    TapSighashType::Default,
                )?;
                let signature = Secp256k1Schnorr::from_bytes(key.secret().as_bytes())?
                    .tweaked(None)?
                    .sign(&sighash.to_byte_array())?;
                Witness::p2tr_key_spend(&bitcoin::taproot::Signature {
                    signature: schnorr::Signature::from_slice(&signature)?,
                    sighash_type: TapSighashType::Default,
                })
            };
            witnesses.push(witness);
        }
        for (input, witness) in self.native.input.iter_mut().zip(witnesses) {
            input.witness = witness;
        }
        self.base.bytes = consensus::serialize(&self.native);
        Ok(())
    }

    /// The consensus transaction.
    #[must_use]
    pub const fn native(&self) -> &bitcoin::Transaction {
        &self.native
    }

    /// One spent output per input, in input order; empty for the coinbase.
    #[must_use]
    pub fn spent(&self) -> &[TxOut] {
        &self.spent
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

/// The legs of `native` spending `spent`, in order:
///
/// 1. `Received` per output, in output order, with the coin it creates.
/// 2. Per input: `Spent` with the coin it consumes and the amount it paid to
///    outputs, then `Fee` with its leftover, when it has one. Inputs fill
///    outputs in order, so a transaction's fee is normally all the last
///    input's leftover.
///
/// Outputs and inputs whose script has no address (`OP_RETURN`, bare
/// multisig, other non-standard scripts) get no leg. The coinbase has only
/// `Received` legs: its outputs are coins like any other.
fn legs(native: &bitcoin::Transaction, spent: &[TxOut], network: Network) -> Vec<Leg> {
    let base: base::Network = network.into();
    let id = |txid: bitcoin::Txid| TransactionId::new(base.clone(), txid.to_string());
    let address = |script: &Script| Address::from_script(script, network).map(base::Address::from);
    let asset = Asset::native(base.clone());
    let txid = native.compute_txid();
    let mut legs = Vec::new();
    for (vout, output) in (0_u32..).zip(&native.output) {
        if let Some(address) = address(&output.script_pubkey) {
            legs.push(Leg::Received {
                address,
                asset: asset.clone(),
                amount: output.value.to_sat().into(),
                coin: Some(base::Coin {
                    transaction: id(txid),
                    index: vout,
                }),
            });
        }
    }
    let inputs = native.input.iter().zip(spent);
    for ((input, spent), (paid, leftover)) in inputs.zip(fills(native, spent)) {
        let Some(address) = address(&spent.script_pubkey) else {
            continue;
        };
        legs.push(Leg::Spent {
            address: address.clone(),
            asset: asset.clone(),
            amount: paid.into(),
            coin: Some(base::Coin {
                transaction: id(input.previous_output.txid),
                index: input.previous_output.vout,
            }),
        });
        if leftover > 0 {
            legs.push(Leg::Fee {
                address,
                asset: asset.clone(),
                amount: leftover.into(),
            });
        }
    }
    legs
}

/// The in-order pairing: inputs fill the outputs in order, each paying what
/// is still owed. Per input, `(paid to outputs, leftover)`; the leftovers
/// are the fee, normally all on the last input. Empty for the coinbase.
fn fills(native: &bitcoin::Transaction, spent: &[TxOut]) -> Vec<(u64, u64)> {
    if native.is_coinbase() {
        return Vec::new();
    }
    let mut owed: u64 = native
        .output
        .iter()
        .map(|output| output.value.to_sat())
        .sum();
    spent
        .iter()
        .map(|spent| {
            let value = spent.value.to_sat();
            let paid = value.min(owed);
            owed -= paid;
            (paid, value - paid)
        })
        .collect()
}
