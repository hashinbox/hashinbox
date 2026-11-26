//! [`Transaction`]: a Tron transaction and its base form, kept together.
//! Built for sending (its `raw_data` protobuf encoded here, never taken
//! from a node) and signed, or read from a block.

use alloy_primitives::hex;
use anyhow::{Context as _, bail, ensure};
use base::{
    Addressed as _, Asset, Decimal, Leg, Networked as _, Output, Reason, Transacted, TransactionId,
    TransactionState,
};
use crypto::PrivateKey as _;
use num_bigint::BigUint;
use serde::Deserialize;
use serde_json::Value;

use crate::{Account, Address, KeyPair, Network, Tokens, trc20};

/// How long after its reference block a transaction can land: 3 minutes.
/// The reference is the solidified block, about a minute old, so about two
/// minutes from signing.
const EXPIRATION_MS: u64 = 180_000;

/// A block's least time after its parent: one slot.
const SLOT_MS: u64 = 3_000;

/// The block a transaction references (TaPoS), so it lands on that chain
/// only, and from whose time its expiration counts.
#[derive(Clone, Debug)]
pub(crate) struct Reference {
    /// Its number.
    pub(crate) number: u64,
    /// Its id: the number then the header's hash.
    pub(crate) id: [u8; 32],
    /// Unix milliseconds.
    pub(crate) timestamp: u64,
}

impl Reference {
    /// When a transaction on it expires, in Unix milliseconds.
    pub(crate) const fn expiration(&self) -> u64 {
        self.timestamp + EXPIRATION_MS
    }

    /// The last height a transaction on it can land at: blocks are at
    /// least a slot apart, so any block above it is past the expiration.
    pub(crate) const fn expires_height(&self) -> u64 {
        self.number + EXPIRATION_MS / SLOT_MS
    }
}

/// What a transaction calls: TRX to `to`, or `data` (a TRC-20 `transfer`)
/// on `contract`.
#[derive(Clone, Debug)]
pub(crate) enum Call {
    /// A `TransferContract` of `amount` sun.
    Transfer {
        /// The recipient.
        to: Account,
        /// Sun.
        amount: u64,
    },
    /// A `TriggerSmartContract` with no TRX.
    Trigger {
        /// The token.
        contract: Account,
        /// The ABI call.
        data: Vec<u8>,
    },
}

/// `Transaction.raw` (protocol.proto), protobuf: `owner` making `call` on
/// `reference`, made at `timestamp` (Unix milliseconds, which keeps two
/// equal payments apart), burning at most `fee_limit` sun of energy.
pub(crate) fn raw(
    owner: &Account,
    call: &Call,
    reference: &Reference,
    timestamp: u64,
    fee_limit: Option<u64>,
) -> Vec<u8> {
    let mut value = Vec::new();
    bytes(&mut value, 1, &owner.prefixed());
    let (kind, name) = match call {
        Call::Transfer { to, amount } => {
            bytes(&mut value, 2, &to.prefixed());
            number(&mut value, 3, *amount);
            (1, "TransferContract")
        }
        Call::Trigger { contract, data } => {
            bytes(&mut value, 2, &contract.prefixed());
            bytes(&mut value, 4, data);
            (31, "TriggerSmartContract")
        }
    };
    let mut any = Vec::new();
    bytes(
        &mut any,
        1,
        format!("type.googleapis.com/protocol.{name}").as_bytes(),
    );
    bytes(&mut any, 2, &value);
    let mut contract = Vec::new();
    number(&mut contract, 1, kind);
    bytes(&mut contract, 2, &any);
    let mut raw = Vec::new();
    bytes(&mut raw, 1, &reference.number.to_be_bytes()[6..]);
    bytes(&mut raw, 4, &reference.id[8..16]);
    number(&mut raw, 8, reference.expiration());
    bytes(&mut raw, 11, &contract);
    number(&mut raw, 14, timestamp);
    number(&mut raw, 18, fee_limit.unwrap_or(0));
    raw
}

/// A varint field; zero is left out, as proto3 does.
fn number(out: &mut Vec<u8>, field: u64, value: u64) {
    if value != 0 {
        varint(out, field << 3);
        varint(out, value);
    }
}

/// A length-delimited field.
fn bytes(out: &mut Vec<u8>, field: u64, value: &[u8]) {
    varint(out, (field << 3) | 2);
    varint(out, value.len() as u64);
    out.extend_from_slice(value);
}

#[allow(clippy::cast_possible_truncation, reason = "seven bits at a time")]
fn varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// A Tron transaction: its `raw_data` protobuf (none for one read from a
/// block) and its base form (id, state, order, legs, bytes). Legs never
/// change after construction; [`Transaction::sign`] sets the bytes, and the
/// id, the SHA-256 of `raw_data`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transaction {
    raw: Option<Vec<u8>>,
    base: base::Transaction,
}

impl Transaction {
    /// Pending and unsigned: `raw` sent by `from` to pay `output`. Its legs
    /// are the transfer and the worst-case `fee` (sun), which is all a
    /// sender knows before it is mined.
    #[must_use]
    pub(crate) fn pending(
        raw: Vec<u8>,
        from: &Address,
        output: &Output<Address>,
        fee: u64,
    ) -> Self {
        let network = from.network().clone();
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
                amount: Decimal::from(fee),
            },
        ];
        Self::new(
            Some(raw),
            TransactionId::new(network, String::new()),
            TransactionState::Pending,
            None,
            legs,
        )
    }

    /// A mined transaction: its object in `getblockbynum` and its entry in
    /// `gettransactioninfobyblocknum`. Legs are of TRX (a
    /// `TransferContract`, a contract call's `call_value`), of the
    /// allowlisted `tokens` (their `Transfer` logs), and the TRX burned
    /// (`fee`: bandwidth, energy, an account created) of its owner. TRX a
    /// contract moves inside it, TRC-10, staking and resource delegation
    /// are not seen. A failed transaction keeps only its fee.
    ///
    /// # Errors
    ///
    /// When a field the legs need does not decode, or the info is of
    /// another transaction.
    pub(crate) fn from_block(
        listed: &Value,
        info: &Value,
        order: u64,
        network: &Network,
        tokens: &Tokens,
    ) -> anyhow::Result<Self> {
        let listed = Listed::deserialize(listed).context("Tron transaction")?;
        let info = Info::deserialize(info).context("Tron transaction info")?;
        ensure!(
            listed.id == info.id,
            "Tron transaction info is of another transaction; retry"
        );
        let succeeded = info.result.as_deref() != Some("FAILED");
        let legs = legs(&listed, &info, succeeded, network, tokens)?;
        let state = if succeeded {
            TransactionState::Succeeded
        } else {
            TransactionState::Reverted(Some(Reason::Reverted))
        };
        Ok(Self::new(
            None,
            TransactionId::new(network.into(), listed.id),
            state,
            Some(order),
            legs,
        ))
    }

    fn new(
        raw: Option<Vec<u8>>,
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
            expires_height: None,
        };
        Self { raw, base }
    }

    /// Signs with `key`: the id is the SHA-256 of `raw_data`, the signature
    /// over it 65 bytes, `r ‖ s ‖ 27 + recovery id`; the bytes are the
    /// `Transaction` protobuf.
    ///
    /// # Errors
    ///
    /// For a mined transaction, one `key` does not pay for, or a failed
    /// signature.
    pub(crate) fn sign(&mut self, key: &KeyPair) -> anyhow::Result<()> {
        let Some(raw) = &self.raw else {
            bail!("a mined Tron transaction cannot be signed again");
        };
        let payer = self.base.legs.iter().find_map(|leg| match leg {
            Leg::Fee { address, .. } => Some(address),
            _ => None,
        });
        ensure!(
            payer == Some(key.address()),
            "Tron transaction does not belong to the signing key"
        );
        let id = crypto::sha256(raw);
        let mut signature = key.key.sign(&id)?;
        if let Some(recovery) = signature.last_mut() {
            *recovery += 27;
        }
        let mut signed = Vec::new();
        bytes(&mut signed, 1, raw);
        bytes(&mut signed, 2, &signature);
        self.base.bytes = signed;
        self.base.id.id = hex::encode(id);
        Ok(())
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

/// The fields of a block's transaction the legs need.
#[derive(Deserialize)]
struct Listed {
    #[serde(rename = "txID")]
    id: String,
    raw_data: ListedRaw,
}

#[derive(Deserialize)]
struct ListedRaw {
    #[serde(default)]
    contract: Vec<Contract>,
}

/// Its one contract: the type and the fields of its parameter, protobuf
/// JSON (addresses `41…` hex, zero fields left out).
#[derive(Deserialize)]
struct Contract {
    #[serde(rename = "type")]
    kind: String,
    parameter: Parameter,
}

#[derive(Deserialize)]
struct Parameter {
    value: Fields,
}

#[derive(Deserialize)]
struct Fields {
    owner_address: String,
    to_address: Option<String>,
    contract_address: Option<String>,
    amount: Option<u64>,
    call_value: Option<u64>,
}

/// The fields of a transaction's info the legs need.
#[derive(Deserialize)]
struct Info {
    id: String,
    /// Sun burned, everything included; left out when zero.
    #[serde(default)]
    fee: u64,
    /// `FAILED` when it failed; left out when it succeeded.
    result: Option<String>,
    #[serde(default)]
    log: Vec<Log>,
}

/// One event log: its contract (20 bytes, hex, no `41`), topics and data.
#[derive(Deserialize)]
struct Log {
    address: String,
    #[serde(default)]
    topics: Vec<String>,
    #[serde(default)]
    data: String,
}

/// The legs of a mined transaction, in order:
///
/// 1. TRX it moves: a `TransferContract`'s `amount`, or a contract call's
///    `call_value`, `Spent` by the owner, `Received` by the recipient.
/// 2. Each allowlisted token `Transfer` log: `Spent` and `Received`; a
///    mint (from zero) is `Minted`, a burn (to zero) `Burned`.
/// 3. The fee, the TRX it burned, of the owner.
///
/// A failed transaction moved nothing but its fee.
fn legs(
    listed: &Listed,
    info: &Info,
    succeeded: bool,
    network: &Network,
    tokens: &Tokens,
) -> anyhow::Result<Vec<Leg>> {
    let Some(contract) = listed.raw_data.contract.first() else {
        return Ok(Vec::new());
    };
    let address = |account| base::Address::from(Address::new(account, network));
    let fields = &contract.parameter.value;
    let owner = Account::from_hex(&fields.owner_address)?;
    let trx = Asset::native(network.into());
    let mut legs = Vec::new();
    let moved = match contract.kind.as_str() {
        "TransferContract" => fields.to_address.as_deref().zip(fields.amount),
        "TriggerSmartContract" => fields.contract_address.as_deref().zip(fields.call_value),
        _ => None,
    };
    if let Some((to, amount)) = moved.filter(|(_, amount)| succeeded && *amount != 0) {
        let amount = Decimal::from(amount);
        legs.push(Leg::Spent {
            address: address(owner),
            asset: trx.clone(),
            amount: amount.clone(),
            coin: None,
        });
        legs.push(Leg::Received {
            address: address(Account::from_hex(to)?),
            asset: trx.clone(),
            amount,
            coin: None,
        });
    }
    for log in info.log.iter().filter(|_| succeeded) {
        let Ok(bytes) = hex::decode(&log.address) else {
            continue;
        };
        let Ok(token) = <[u8; 20]>::try_from(bytes).map(Account) else {
            continue;
        };
        if !tokens.contains(&token) {
            continue;
        }
        let Some((from, to, amount)) = trc20::transfer_log(&log.topics, &log.data) else {
            continue;
        };
        let zero = Account([0; 20]);
        if amount == BigUint::ZERO || (from == zero && to == zero) {
            continue;
        }
        let asset = Asset {
            network: network.into(),
            id: token.to_string(),
        };
        let amount = Decimal::from_atomic(amount, 0);
        if from == zero {
            legs.push(Leg::Minted {
                address: address(to),
                asset,
                amount,
            });
        } else if to == zero {
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
    if info.fee != 0 {
        legs.push(Leg::Fee {
            address: address(owner),
            asset: trx,
            amount: Decimal::from(info.fee),
        });
    }
    Ok(legs)
}

#[cfg(test)]
mod tests {
    use alloy_primitives::hex;

    use super::{Call, Reference, raw};
    use crate::Account;

    fn account(text: &str) -> Account {
        Account::from_hex(text).unwrap()
    }

    /// Nile block 73043095's first two transactions, as the node wrote
    /// them: encoded from their fields, each is its `raw_data_hex`, and its
    /// id the SHA-256 of that.
    #[test]
    fn raw_data_is_the_nodes_protobuf_and_its_hash_the_id() {
        let reference = |number: u64, hash: &str, expiration: u64| {
            let mut id = [0; 32];
            id[..8].copy_from_slice(&number.to_be_bytes());
            hex::decode_to_slice(hash, &mut id[8..16]).unwrap();
            Reference {
                number,
                id,
                timestamp: expiration - super::EXPIRATION_MS,
            }
        };
        let data = "a9059cbb000000000000000000000000df76bd95259da7466dce0ef7e53c1e754d6f1518\
                    00000000000000000000000000000000000000000000000000000000000f4240";
        let usdt = raw(
            &account("41780aa35c3d216844f2f3d7bdfdae2926b09647ae"),
            &Call::Trigger {
                contract: account("41eca9bc828a3005b9a3b909f2cc5c2a54794de05f"),
                data: hex::decode(data).unwrap(),
            },
            &reference(0x8c95, "a524211c9e71f044", 1_795_673_133_000),
            1_795_673_073_000,
            Some(225_000_000),
        );
        assert_eq!(
            hex::encode(&usdt),
            "0a028c952208a524211c9e71f04440c89fd6b3a1345aae01081f12a9010a31747970652e676f6f\
             676c65617069732e636f6d2f70726f746f636f6c2e54726967676572536d617274436f6e7472\
             61637412740a1541780aa35c3d216844f2f3d7bdfdae2926b09647ae121541eca9bc828a3005\
             b9a3b909f2cc5c2a54794de05f2244a9059cbb000000000000000000000000df76bd95259da7\
             466dce0ef7e53c1e754d6f151800000000000000000000000000000000000000000000000000\
             000000000f424070e8cad2b3a1349001c0f4a46b"
        );
        assert_eq!(
            hex::encode(crypto::sha256(&usdt)),
            "da7b851f39e4d2271ec579987f1fdeae3f92df367f9ed012744d65be41f33ecb"
        );
        let trx = raw(
            &account("41ba324bde91f75574cf1cc7093f28a9fca6a20409"),
            &Call::Transfer {
                to: account("416046f7ec9d8eebd997c0f3870f1e20196de522eb"),
                amount: 986_000,
            },
            &reference(0x8c84, "ea59ae19ec08f285", 1_795_673_377_351),
            1_795_673_076_374_467_800,
            None,
        );
        assert_eq!(
            hex::encode(&trx),
            "0a028c842208ea59ae19ec08f28540c794e5b3a1345a67080112630a2d747970652e676f6f67\
             6c65617069732e636f6d2f70726f746f636f6c2e5472616e73666572436f6e74726163741232\
             0a1541ba324bde91f75574cf1cc7093f28a9fca6a204091215416046f7ec9d8eebd997c0f387\
             0f1e20196de522eb1890973c70d8e9a1d0e2e4e0f518"
        );
        assert_eq!(
            hex::encode(crypto::sha256(&trx)),
            "be3074e5348f857989c742b15a3bbbeed01dfe4e53d78f5eb36e5a4ba5698780"
        );
    }
}
