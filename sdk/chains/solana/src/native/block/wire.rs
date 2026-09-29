//! The `getBlock` JSON shape (`json` encoding, full transactions). Private:
//! only [`super::RawBlock`] reads it.

use serde::Deserialize;
use serde_json::{Value, value::RawValue};

use crate::native::Error;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Reply {
    pub(super) blockhash: String,
    pub(super) previous_blockhash: String,
    pub(super) parent_slot: u64,
    /// Kept undecoded: the header is checked when the block is fetched, the
    /// transactions only when it is parsed.
    transactions: Vec<Box<RawValue>>,
    pub(super) block_time: Option<i64>,
    pub(super) block_height: Option<u64>,
}

impl Reply {
    pub(super) fn decode(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }

    pub(super) fn transactions(&self) -> Result<Vec<TransactionWire>, Error> {
        self.transactions
            .iter()
            .map(|raw| {
                serde_json::from_str(raw.get()).map_err(|_| {
                    Error::invalid_block("Solana block transactions have an invalid RPC shape")
                })
            })
            .collect()
    }
}

#[derive(Deserialize)]
pub(super) struct TransactionWire {
    pub(super) transaction: SignedWire,
    pub(super) meta: Option<MetaWire>,
    pub(super) version: Value,
}

#[derive(Deserialize)]
pub(super) struct SignedWire {
    pub(super) signatures: Vec<String>,
    pub(super) message: MessageWire,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MessageWire {
    pub(super) header: HeaderWire,
    pub(super) account_keys: Vec<String>,
    pub(super) instructions: Vec<InstructionWire>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct HeaderWire {
    pub(super) num_required_signatures: u8,
    pub(super) num_readonly_signed_accounts: u8,
    pub(super) num_readonly_unsigned_accounts: u8,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct InstructionWire {
    pub(super) program_id_index: u8,
    pub(super) accounts: Vec<u8>,
    pub(super) data: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MetaWire {
    pub(super) err: Value,
    pub(super) fee: u64,
    pub(super) pre_balances: Vec<u64>,
    pub(super) post_balances: Vec<u64>,
    pub(super) inner_instructions: Option<Vec<InnerWire>>,
    pub(super) loaded_addresses: Option<LoadedWire>,
    /// Absent when the node recorded no token balances for the transaction.
    pub(super) pre_token_balances: Option<Vec<TokenWire>>,
    pub(super) post_token_balances: Option<Vec<TokenWire>>,
}

/// One account's SPL token balance on one side of execution. It names the
/// owning wallet, so a position is attributed without tracking the associated
/// token account's own address.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TokenWire {
    pub(super) account_index: u8,
    pub(super) mint: String,
    /// Absent on a node that does not record the owning wallet.
    pub(super) owner: Option<String>,
    #[serde(rename = "uiTokenAmount")]
    pub(super) amount: AmountWire,
}

#[derive(Deserialize)]
pub(super) struct AmountWire {
    /// The raw unit count, which the node reports as decimal text. `None`
    /// when the program exposes no plain amount, as a Token-2022
    /// confidential balance need not.
    #[serde(rename = "amount", default, deserialize_with = "AmountWire::decode")]
    pub(super) units: Option<u64>,
}

impl AmountWire {
    fn decode<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Option::<Value>::deserialize(deserializer)?;
        Ok(value
            .as_ref()
            .and_then(Value::as_str)
            .and_then(|text| text.parse::<u64>().ok()))
    }
}

#[derive(Deserialize)]
pub(super) struct InnerWire {
    pub(super) index: u8,
    pub(super) instructions: Vec<InstructionWire>,
}

#[derive(Deserialize)]
pub(super) struct LoadedWire {
    pub(super) writable: Vec<String>,
    pub(super) readonly: Vec<String>,
}
