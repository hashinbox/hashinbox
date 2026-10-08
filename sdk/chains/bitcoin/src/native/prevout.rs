//! Previous-output resolution: turns a `getblock` verbosity-2 result into
//! self-contained [`crate::native::block::RawBlock`] evidence.
//!
//! Verbosity 2 names each input's outpoint but not the value or address it
//! spends. Outputs created earlier in the same block are resolved by the
//! parser; every other outpoint is fetched once with `getrawtransaction`
//! (at most four in flight) and written into its input as a compact
//! `prevout` of value and address. Historical scripts are validated and
//! discarded, so the evidence of one block stays bounded.

use std::collections::{BTreeMap, BTreeSet};

use bitcoin::{Transaction as NativeTransaction, TxIn};
use futures_util::{StreamExt, stream};
use serde_json::{Value, json};

use crate::native::{
    Address, Error, Network, TransactionId,
    block::{Outpoint, Output, Transaction},
    json::Object,
    rpc::Client,
};

/// Every non-coinbase input consumes at least a 36-byte outpoint, a one-byte
/// empty-script length, and a four-byte sequence: 164 weight units. A
/// 4,000,000-WU block therefore spends at most 24,390 external outputs; this
/// looser ceiling is complete for every consensus-valid block.
const MAX_EXTERNAL_PREVOUTS: usize = 25_000;
const MAX_IN_FLIGHT: usize = 4;
const MAX_ADDRESS_BYTES: usize = 128;
const MAX_PREVOUT_JSON_BYTES: usize = 192;
const MAX_PREVOUT_TOTAL_BYTES: usize = MAX_EXTERNAL_PREVOUTS * MAX_PREVOUT_JSON_BYTES;

/// A block's transactions decoded from their consensus bytes, with every
/// outpoint they spend from outside the block.
struct Prevouts {
    transactions: Vec<(TransactionId, NativeTransaction)>,
    external: External,
}

/// The outpoints a block spends from outside itself, by transaction.
#[derive(Default)]
struct External {
    outputs: BTreeMap<TransactionId, BTreeSet<u32>>,
    count: usize,
}

impl<C: json_rpc::Client> Client<C> {
    /// `block` with every external input's compact `prevout` written in and
    /// any node-supplied `prevout` removed.
    pub(crate) async fn resolve_prevouts(&self, block: Vec<u8>) -> Result<Vec<u8>, Error> {
        let mut value: Value = serde_json::from_slice(&block)
            .map_err(|_| Error::invalid_reply("Bitcoin block result is not valid JSON"))?;
        let prevouts = Prevouts::from_block(&value)?;
        let external = prevouts.external.outputs.clone();
        let requests = external
            .into_iter()
            .map(|(id, indexes)| self.previous_outputs(id, indexes));
        let mut requests = stream::iter(requests).buffer_unordered(MAX_IN_FLIGHT);
        let mut resolved = BTreeMap::new();
        while let Some(result) = requests.next().await {
            let (id, outputs) = result?;
            resolved.insert(id, outputs);
        }
        prevouts.write(&mut value, &resolved)?;
        serde_json::to_vec(&value)
            .map_err(|_| Error::invalid_reply("Bitcoin enriched block JSON could not be encoded"))
    }
}

impl<C: json_rpc::Client> Client<C> {
    /// Outputs `indexes` of the confirmed transaction `id`.
    async fn previous_outputs(
        &self,
        id: TransactionId,
        indexes: BTreeSet<u32>,
    ) -> Result<(TransactionId, BTreeMap<u32, Output>), Error> {
        let transaction = self.confirmed_transaction(id).await?;
        let network = *self.network();
        let outputs = indexes
            .into_iter()
            .map(|index| Ok((index, Prevouts::output(&transaction, id, index, network)?)))
            .collect::<Result<_, Error>>()?;
        Ok((id, outputs))
    }
}

impl Prevouts {
    fn from_block(block: &Value) -> Result<Self, Error> {
        let block = Object::new(block, "Bitcoin block result", Error::invalid_reply)?;
        let values = block.array("tx", "Bitcoin block transactions")?;
        let mut prevouts = Self {
            transactions: Vec::with_capacity(values.len()),
            external: External::default(),
        };
        let mut ids = BTreeSet::new();
        for value in values {
            let object = Object::new(value, "Bitcoin block transaction", Error::invalid_reply)?;
            let id = object
                .string("txid", "Bitcoin block transaction ID")?
                .parse::<TransactionId>()
                .map_err(|_| object.fail("Bitcoin block transaction ID is invalid"))?;
            let transaction = Transaction::decode(&object, id)?;
            Self::check_claims(&object, &transaction)?;
            if !ids.insert(id) {
                return Err(object.fail("Bitcoin block contains duplicate transaction IDs"));
            }
            prevouts.transactions.push((id, transaction));
        }
        prevouts.collect_external(&ids)?;
        Ok(prevouts)
    }

    /// Verbosity-2 input claims must match the consensus inputs before any
    /// previous transaction is requested.
    fn check_claims(object: &Object<'_>, transaction: &NativeTransaction) -> Result<(), Error> {
        let inputs = object.array("vin", "Bitcoin transaction inputs")?;
        if inputs.len() != transaction.input.len() {
            return Err(
                object.fail("Bitcoin transaction input count does not match its consensus bytes")
            );
        }
        for (index, (claim, input)) in inputs.iter().zip(&transaction.input).enumerate() {
            let claim = Object::new(claim, "Bitcoin transaction input", Error::invalid_reply)?;
            Self::check_input(&claim, input, transaction.is_coinbase(), index)?;
        }
        Ok(())
    }

    fn check_input(
        claim: &Object<'_>,
        input: &TxIn,
        coinbase: bool,
        index: usize,
    ) -> Result<(), Error> {
        if input.previous_output.is_null() {
            if !coinbase || claim.get("coinbase").and_then(Value::as_str).is_none() {
                return Err(claim.fail("Bitcoin null input is not a valid coinbase input"));
            }
            return Ok(());
        }
        let id = claim
            .string("txid", "Bitcoin input previous transaction ID")?
            .parse::<TransactionId>()
            .map_err(|_| claim.fail("Bitcoin input previous transaction ID is invalid"))?;
        let output_index = claim.u32("vout", "Bitcoin input output index")?;
        let claimed = Outpoint {
            transaction_id: id,
            output_index,
        };
        if claimed != Outpoint::from(input.previous_output) {
            return Err(claim.fail(format!(
                "Bitcoin input {index} outpoint does not match its consensus bytes"
            )));
        }
        Ok(())
    }

    /// Records every outpoint not created earlier in the block.
    fn collect_external(&mut self, ids: &BTreeSet<TransactionId>) -> Result<(), Error> {
        let mut earlier = BTreeSet::new();
        for (id, transaction) in &self.transactions {
            for input in transaction
                .input
                .iter()
                .filter(|input| !input.previous_output.is_null())
            {
                let outpoint = Outpoint::from(input.previous_output);
                if earlier.contains(&outpoint) {
                    continue;
                }
                if ids.contains(&outpoint.transaction_id) {
                    return Err(Error::invalid_reply(
                        "Bitcoin block transaction spends an output that was not created \
                         earlier in the block",
                    ));
                }
                self.external.record(outpoint)?;
            }
            earlier.extend(Self::created(*id, transaction));
        }
        let budget = self.external.count.checked_mul(MAX_PREVOUT_JSON_BYTES);
        if budget.is_none_or(|budget| budget > MAX_PREVOUT_TOTAL_BYTES) {
            return Err(Error::invalid_block(
                "Bitcoin compact prevout data exceeds its aggregate bound",
            ));
        }
        Ok(())
    }

    /// The outpoints `transaction` creates.
    fn created(
        id: TransactionId,
        transaction: &NativeTransaction,
    ) -> impl Iterator<Item = Outpoint> + '_ {
        (0_u32..)
            .zip(&transaction.output)
            .map(move |(output_index, _)| Outpoint {
                transaction_id: id,
                output_index,
            })
    }

    /// The value and address of output `index` of `transaction`.
    fn output(
        transaction: &NativeTransaction,
        id: TransactionId,
        index: u32,
        network: Network,
    ) -> Result<Output, Error> {
        let output = usize::try_from(index)
            .ok()
            .and_then(|position| transaction.output.get(position))
            .ok_or_else(|| {
                Error::invalid_reply(format!(
                    "Bitcoin previous transaction {id} has no output {index}"
                ))
            })?;
        let output = Output::from_native(output, network);
        output
            .address
            .as_ref()
            .map(Self::check_address)
            .transpose()?;
        Ok(output)
    }

    /// Replaces every input's `prevout`: external inputs get their compact
    /// resolved output, same-block inputs get none.
    fn write(
        &self,
        block: &mut Value,
        resolved: &BTreeMap<TransactionId, BTreeMap<u32, Output>>,
    ) -> Result<(), Error> {
        let values = block["tx"]
            .as_array_mut()
            .ok_or_else(|| Error::invalid_reply("Bitcoin block transactions must be an array"))?;
        let mut earlier = BTreeSet::new();
        for ((id, transaction), value) in self.transactions.iter().zip(values) {
            let inputs = value["vin"].as_array_mut().ok_or_else(|| {
                Error::invalid_reply("Bitcoin transaction inputs must be an array")
            })?;
            for (claim, input) in inputs.iter_mut().zip(&transaction.input) {
                Self::write_input(claim, input, &earlier, resolved)?;
            }
            earlier.extend(Self::created(*id, transaction));
        }
        Ok(())
    }

    fn write_input(
        claim: &mut Value,
        input: &TxIn,
        earlier: &BTreeSet<Outpoint>,
        resolved: &BTreeMap<TransactionId, BTreeMap<u32, Output>>,
    ) -> Result<(), Error> {
        let Some(claim) = claim.as_object_mut() else {
            return Err(Error::invalid_reply(
                "Bitcoin transaction input must be an object",
            ));
        };
        // Verbosity 2 has no `prevout`; drop any, so parsing sees only ours.
        claim.remove("prevout");
        let outpoint = Outpoint::from(input.previous_output);
        if input.previous_output.is_null() || earlier.contains(&outpoint) {
            return Ok(());
        }
        let output = resolved
            .get(&outpoint.transaction_id)
            .and_then(|outputs| outputs.get(&outpoint.output_index))
            .ok_or_else(|| {
                Error::invalid_reply("Bitcoin external previous output was not resolved")
            })?;
        claim.insert("prevout".to_owned(), Self::compact(output)?);
        Ok(())
    }

    /// `{"value_satoshis": …, "address": …}`, within its per-input bound.
    fn compact(output: &Output) -> Result<Value, Error> {
        output
            .address
            .as_ref()
            .map(Self::check_address)
            .transpose()?;
        let prevout = json!({
            "value_satoshis": output.value.units(),
            "address": output.address.as_ref().map(Address::as_str),
        });
        if prevout.to_string().len() > MAX_PREVOUT_JSON_BYTES {
            return Err(Error::invalid_block(
                "Bitcoin compact prevout JSON exceeds its per-input bound",
            ));
        }
        Ok(prevout)
    }

    fn check_address(address: &Address) -> Result<(), Error> {
        let text = address.as_str();
        if text.len() > MAX_ADDRESS_BYTES || !text.bytes().all(|byte| byte.is_ascii_alphanumeric())
        {
            return Err(Error::invalid_block(
                "Bitcoin canonical prevout address exceeds the compact data bound",
            ));
        }
        Ok(())
    }
}

impl External {
    fn record(&mut self, outpoint: Outpoint) -> Result<(), Error> {
        let indexes = self.outputs.entry(outpoint.transaction_id).or_default();
        if indexes.contains(&outpoint.output_index) {
            return Err(Error::invalid_block(
                "Bitcoin block spends the same external outpoint more than once",
            ));
        }
        if self.count >= MAX_EXTERNAL_PREVOUTS {
            return Err(Error::invalid_block(format!(
                "Bitcoin block exceeds the {MAX_EXTERNAL_PREVOUTS} external-prevout safety bound"
            )));
        }
        indexes.insert(outpoint.output_index);
        self.count += 1;
        Ok(())
    }
}
