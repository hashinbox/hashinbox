use std::collections::BTreeMap;

use base::{Identified, MovementKind};
use bitcoin::{OutPoint, Transaction as NativeTransaction, TxIn, consensus, hex::FromHex};
use serde_json::Value;

use super::{Outpoint, Output};
use crate::native::{Address, Error, Network, Satoshi, TransactionId, json::Object};

/// One parsed transaction as included in a block, with every spent output resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transaction {
    id: TransactionId,
    fee: Option<Fee>,
    /// Inputs in consensus order; each non-coinbase input carries its previous output.
    pub inputs: Vec<Input>,
    /// Outputs in consensus order.
    pub outputs: Vec<Output>,
    /// Whether this is the block's coinbase transaction.
    pub coinbase: bool,
}

/// One transaction input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Input {
    /// The spent output; the null outpoint for a coinbase input.
    pub outpoint: Outpoint,
    /// The consensus sequence number.
    pub sequence: u32,
    /// The spent output's value and address. `None` for a coinbase input,
    /// which spends nothing, and for every input of a transaction read
    /// outside a block, whose previous outputs are unknown.
    pub previous: Option<Output>,
}

/// The network fee paid by one transaction: resolved inputs minus outputs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fee {
    /// Satoshis paid.
    pub amount: Satoshi,
    /// The one address every input spends from; `None` when inputs are mixed
    /// or have no address.
    pub payer: Option<Address>,
}

/// One value change inside a [`Transaction`]: an input spent or an output created.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Movement {
    /// Stable within the block: `<txid>:vin:<n>` or `<txid>:vout:<n>`.
    pub id: String,
    /// [`MovementKind::Input`] or [`MovementKind::Output`].
    pub kind: MovementKind,
    /// Satoshis moved.
    pub amount: Satoshi,
    /// The spending address of an input.
    pub from: Option<Address>,
    /// The receiving address of an output.
    pub to: Option<Address>,
}

impl Transaction {
    /// The transaction ID.
    #[must_use]
    pub const fn id(&self) -> &TransactionId {
        &self.id
    }

    /// Always true: a Bitcoin block contains only valid transactions.
    #[must_use]
    pub const fn succeeded(&self) -> bool {
        true
    }

    /// `None` for the coinbase transaction, which pays no fee.
    #[must_use]
    pub const fn fee(&self) -> Option<&Fee> {
        self.fee.as_ref()
    }

    /// Every spent previous output, then every created output.
    #[must_use]
    pub fn movements(&self) -> Vec<Movement> {
        let spent = self.inputs.iter().enumerate().filter_map(|(index, input)| {
            let previous = input.previous.as_ref()?;
            Some(Movement {
                id: format!("{}:vin:{index}", self.id),
                kind: MovementKind::Input,
                amount: previous.value,
                from: previous.address.clone(),
                to: None,
            })
        });
        let created = self
            .outputs
            .iter()
            .enumerate()
            .map(|(index, output)| Movement {
                id: format!("{}:vout:{index}", self.id),
                kind: MovementKind::Output,
                amount: output.value,
                from: None,
                to: output.address.clone(),
            });
        spent.chain(created).collect()
    }

    /// Parses one verbosity-2 transaction whose non-coinbase inputs spend
    /// either `earlier` outputs of the same block or a resolved `prevout`.
    pub(super) fn parse(
        value: &Value,
        height: u64,
        network: Network,
        earlier: &BTreeMap<Outpoint, Output>,
    ) -> Result<Self, Error> {
        let object = Object::new(value, "Bitcoin transaction", Error::invalid_block)?;
        let id = object
            .string("txid", "Bitcoin transaction ID")?
            .parse::<TransactionId>()
            .map_err(|_| object.fail("Bitcoin transaction ID is invalid"))?;
        let native = Self::decode(&object, id)?;
        let coinbase = native.is_coinbase();
        let context = Context {
            height,
            network,
            coinbase,
            earlier,
        };
        let claims = object.array("vin", "Bitcoin transaction inputs")?;
        if claims.len() != native.input.len() {
            return Err(
                object.fail("Bitcoin transaction input count does not match its consensus bytes")
            );
        }
        let inputs = claims
            .iter()
            .zip(&native.input)
            .enumerate()
            .map(|(index, (claim, input))| context.input(claim, input, index))
            .collect::<Result<Vec<_>, _>>()?;
        let outputs = Self::outputs(&object, &native, network)?;
        let fee = (!coinbase)
            .then(|| Fee::paid(&inputs, &outputs))
            .transpose()?;
        Ok(Self {
            id,
            fee,
            inputs,
            outputs,
            coinbase,
        })
    }

    /// A transaction outside any block: previous outputs and fee unknown.
    pub(crate) fn unconfirmed(native: &NativeTransaction, network: Network) -> Self {
        Self {
            id: TransactionId::from(native.compute_txid()),
            fee: None,
            inputs: native
                .input
                .iter()
                .map(|input| Input {
                    outpoint: Outpoint::from(input.previous_output),
                    sequence: input.sequence.to_consensus_u32(),
                    previous: None,
                })
                .collect(),
            outputs: native
                .output
                .iter()
                .map(|output| Output::from_native(output, network))
                .collect(),
            coinbase: native.is_coinbase(),
        }
    }

    /// Decodes the `hex` consensus bytes and checks they hash to `expected`.
    pub(crate) fn decode(
        object: &Object<'_>,
        expected: TransactionId,
    ) -> Result<NativeTransaction, Error> {
        let raw = Vec::<u8>::from_hex(object.string("hex", "Bitcoin raw transaction")?)
            .map_err(|_| object.fail("Bitcoin transaction hex is invalid"))?;
        let native: NativeTransaction = consensus::deserialize(&raw)
            .map_err(|_| object.fail("Bitcoin transaction consensus bytes are invalid"))?;
        if TransactionId::from(native.compute_txid()) != expected {
            return Err(object.fail("Bitcoin transaction ID does not match its consensus bytes"));
        }
        Ok(native)
    }

    fn outputs(
        object: &Object<'_>,
        native: &NativeTransaction,
        network: Network,
    ) -> Result<Vec<Output>, Error> {
        let claims = object.array("vout", "Bitcoin transaction outputs")?;
        if claims.len() != native.output.len() {
            return Err(
                object.fail("Bitcoin transaction output count does not match its consensus bytes")
            );
        }
        claims
            .iter()
            .zip(&native.output)
            .enumerate()
            .map(|(index, (claim, output))| {
                Output::parse(claim, output, index)?;
                Ok(Output::from_native(output, network))
            })
            .collect()
    }
}

/// What every input of one transaction is checked against.
struct Context<'a> {
    height: u64,
    network: Network,
    coinbase: bool,
    earlier: &'a BTreeMap<Outpoint, Output>,
}

impl Context<'_> {
    fn input(&self, claim: &Value, native: &TxIn, index: usize) -> Result<Input, Error> {
        let object = Object::new(claim, "Bitcoin transaction input", Error::invalid_block)?;
        let sequence = native.sequence.to_consensus_u32();
        if native.previous_output.is_null() {
            if !self.coinbase || object.get("coinbase").and_then(Value::as_str).is_none() {
                return Err(object.fail("Bitcoin null input is not a valid coinbase input"));
            }
            return Ok(Input {
                outpoint: Outpoint::from(native.previous_output),
                sequence,
                previous: None,
            });
        }
        let outpoint = Outpoint::claimed(&object, &native.previous_output, index)?;
        let previous = self.previous(&object, outpoint, index)?;
        Ok(Input {
            outpoint,
            sequence,
            previous: Some(previous),
        })
    }

    fn previous(
        &self,
        object: &Object<'_>,
        outpoint: Outpoint,
        index: usize,
    ) -> Result<Output, Error> {
        let local = self.earlier.get(&outpoint);
        let Some(prevout) = object.get("prevout").filter(|value| value.is_object()) else {
            return local.cloned().ok_or_else(|| {
                object.fail(format!(
                    "Bitcoin input {index} has no resolved previous output"
                ))
            });
        };
        let prevout = Object::new(prevout, "Bitcoin prevout", Error::invalid_block)?;
        let resolved = Output::parse_previous(&prevout, self.height, self.network)?;
        let conflicts = local.is_some_and(|local| {
            local.value != resolved.value || local.address != resolved.address
        });
        if conflicts {
            return Err(object.fail(format!(
                "Bitcoin input {index} prevout conflicts with an earlier same-block output"
            )));
        }
        Ok(resolved)
    }
}

impl Outpoint {
    fn claimed(object: &Object<'_>, native: &OutPoint, index: usize) -> Result<Self, Error> {
        let transaction_id = object
            .string("txid", "Bitcoin input previous transaction ID")?
            .parse::<TransactionId>()
            .map_err(|_| object.fail("Bitcoin transaction ID is invalid"))?;
        let output_index = object.u32("vout", "Bitcoin input output index")?;
        let claimed = Self {
            transaction_id,
            output_index,
        };
        if claimed != Self::from(*native) {
            return Err(object.fail(format!(
                "Bitcoin input {index} outpoint does not match its consensus bytes"
            )));
        }
        Ok(claimed)
    }
}

impl From<OutPoint> for Outpoint {
    fn from(outpoint: OutPoint) -> Self {
        Self {
            transaction_id: TransactionId::from(outpoint.txid),
            output_index: outpoint.vout,
        }
    }
}

impl Fee {
    fn paid(inputs: &[Input], outputs: &[Output]) -> Result<Self, Error> {
        let spent = inputs.iter().filter_map(|input| input.previous.as_ref());
        let input_total = Self::total(spent.clone(), "input")?;
        let output_total = Self::total(outputs.iter(), "output")?;
        let amount = input_total.checked_sub(output_total).ok_or_else(|| {
            Error::invalid_block("Bitcoin transaction outputs exceed its resolved inputs")
        })?;
        let mut owners = spent.map(|previous| previous.address.as_ref());
        let first = owners.next().flatten();
        let payer = first.filter(|payer| owners.all(|owner| owner == Some(*payer)));
        Ok(Self {
            amount: Satoshi::from_atomic(amount),
            payer: payer.cloned(),
        })
    }

    fn total<'a>(mut outputs: impl Iterator<Item = &'a Output>, side: &str) -> Result<u64, Error> {
        outputs.try_fold(0_u64, |total, output| {
            total.checked_add(output.value.units()).ok_or_else(|| {
                Error::invalid_block(format!("Bitcoin transaction {side} value overflowed u64"))
            })
        })
    }
}

impl Identified for Transaction {
    type Id = TransactionId;

    fn id(&self) -> &TransactionId {
        &self.id
    }
}
