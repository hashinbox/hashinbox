//! [`Model`]: Sepolia's rules for the mock chain — accounts, one ERC-20,
//! EIP-1559 fees — and every answer, rendered from recorded replies.

use std::collections::BTreeMap;

use ::rpc::{Answer, Failure, RawJson};
use alloy_consensus::{Transaction as _, TxEnvelope, transaction::SignerRecoverable as _};
use alloy_eips::Decodable2718 as _;
use alloy_primitives::{Address, B256, Bytes, Signature, U256, hex, keccak256};
use base::testing::{Duplicate, Model as _, Submitted, View};
use serde_json::{Value, json};

use super::{recording, render};

/// Sepolia's chain ID.
pub(super) const CHAIN_ID: u64 = 11_155_111;
/// Gas of an ether transfer.
const TRANSFER_GAS: u64 = 21_000;
/// `transfer(address,uint256)`.
const TRANSFER_SELECTOR: [u8; 4] = [0xa9, 0x05, 0x9c, 0xbb];

/// Every account's ether and nonce, and every holder's token balance.
#[derive(Clone, Debug, Default)]
pub struct State {
    accounts: BTreeMap<Address, (U256, u64)>,
    tokens: BTreeMap<Address, U256>,
}

impl State {
    /// `address` holds `ether` wei and `tokens` token units.
    pub(super) fn fund(&mut self, address: Address, ether: U256, tokens: U256) {
        self.accounts.entry(address).or_default().0 = ether;
        self.tokens.insert(address, tokens);
    }

    pub(super) fn nonce(&self, address: &Address) -> u64 {
        self.accounts.get(address).map_or(0, |account| account.1)
    }

    fn balance(&self, address: &Address) -> U256 {
        self.accounts
            .get(address)
            .map_or(U256::ZERO, |account| account.0)
    }

    fn token(&self, address: &Address) -> U256 {
        self.tokens.get(address).copied().unwrap_or_default()
    }

    fn pay(&mut self, from: Address, to: Address, amount: U256) {
        self.accounts.entry(from).or_default().0 -= amount;
        self.accounts.entry(to).or_default().0 += amount;
    }
}

/// One EIP-1559 transaction: signed and submitted, or the faucet's own
/// (unsigned, the model's).
#[derive(Clone, Debug)]
pub struct Tx {
    pub(super) hash: B256,
    pub(super) from: Address,
    pub(super) to: Address,
    pub(super) nonce: u64,
    pub(super) value: U256,
    pub(super) input: Bytes,
    pub(super) gas: u64,
    pub(super) max_fee: u128,
    pub(super) priority: u128,
    pub(super) signature: Option<Signature>,
}

/// One receipt: status, gas, price and `Transfer` logs (contract, from,
/// to, amount); ether moves log from the EIP-7708 system address.
#[derive(Clone, Debug)]
pub struct Outcome {
    pub(super) status: bool,
    pub(super) gas_used: u64,
    pub(super) price: u128,
    pub(super) logs: Vec<(Address, Address, Address, U256)>,
}

/// A block's hash and its parent's.
#[derive(Clone, Debug)]
pub struct Seal {
    pub(super) hash: B256,
    pub(super) parent: B256,
}

/// Sepolia, as recorded: its base fee, tip, token gas and decimals.
#[derive(Debug)]
pub struct Model {
    pub(super) token: Address,
    pub(super) base_fee: u128,
    pub(super) priority: u128,
    /// What a token transfer uses, as the recorded receipt says.
    token_gas: u64,
}

/// The address ether moves log from (EIP-7708), as recorded.
pub(super) const SYSTEM: Address =
    alloy_primitives::address!("fffffffffffffffffffffffffffffffffffffffe");

impl Model {
    /// Sepolia's USDC and fees, read from the recordings.
    pub(super) fn new(token: Address) -> Self {
        let block = recording("eth_getBlockByNumber.full");
        let receipt = recording("eth_getTransactionReceipt");
        let tip = recording("eth_maxPriorityFeePerGas");
        Self {
            token,
            base_fee: quantity(&block.result()["baseFeePerGas"]),
            priority: quantity(tip.result()),
            token_gas: u64::try_from(quantity(&receipt.result()["gasUsed"])).unwrap_or(0),
        }
    }

    /// The fee cap every faucet and wallet transaction pays.
    pub(super) const fn max_fee(&self) -> u128 {
        2 * self.base_fee + self.priority
    }

    /// The faucet's transaction of `value` ether or (with `tokens`) token
    /// units to `to`.
    pub(super) fn faucet(&self, state: &State, to: Address, value: U256, tokens: bool) -> Tx {
        let nonce = state.nonce(&super::FAUCET);
        let (target, value, input, gas) = if tokens {
            let input = crate::erc20::transfer(to, value);
            (self.token, U256::ZERO, Bytes::from(input), self.token_gas)
        } else {
            (to, value, Bytes::new(), TRANSFER_GAS)
        };
        Tx {
            hash: keccak256([b"faucet".as_slice(), &nonce.to_be_bytes()].concat()),
            from: super::FAUCET,
            to: target,
            nonce,
            value,
            input,
            gas,
            max_fee: self.max_fee(),
            priority: self.priority,
            signature: None,
        }
    }

    /// The recipient and amount of a token `transfer` call.
    fn transfer(input: &[u8]) -> Option<(Address, U256)> {
        let rest = input.strip_prefix(TRANSFER_SELECTOR.as_slice())?;
        let (to, amount) = (rest.get(..32)?, rest.get(32..64)?);
        Some((Address::from_slice(&to[12..]), U256::from_be_slice(amount)))
    }

    /// `eth_sendRawTransaction`, refusing as the recorded node does: bytes,
    /// chain, then funds; `already known`, `nonce too low` and the fee cap
    /// are modeled (an empty key cannot provoke them).
    fn send(&self, view: &mut View<'_, Self>, params: &Value) -> Answer {
        let bytes = params[0]
            .as_str()
            .and_then(|text| hex::decode(text).ok())
            .unwrap_or_default();
        let Ok(TxEnvelope::Eip1559(signed)) = TxEnvelope::decode_2718(&mut bytes.as_slice()) else {
            return Err(recording("eth_sendRawTransaction.invalid").failure());
        };
        if signed.tx().chain_id != CHAIN_ID {
            return Err(recording("eth_sendRawTransaction.chain").failure());
        }
        let envelope = TxEnvelope::Eip1559(signed.clone());
        let Ok(from) = envelope.recover_signer() else {
            return Err(recording("eth_sendRawTransaction.invalid").failure());
        };
        let native = signed.tx();
        let tx = Tx {
            hash: *signed.hash(),
            from,
            to: native.to().unwrap_or_default(),
            nonce: native.nonce,
            value: native.value,
            input: native.input.clone(),
            gas: native.gas_limit,
            max_fee: native.max_fee_per_gas,
            priority: native.max_priority_fee_per_gas,
            signature: Some(*signed.signature()),
        };
        let answer = Ok(raw(&json!(hex::encode_prefixed(tx.hash))));
        if view.known(&self.id(&tx)) {
            return match view.duplicate() {
                Duplicate::Accept => answer,
                Duplicate::Reject => Err(failure(-32_000, "already known".to_owned())),
            };
        }
        let state = view.head_state();
        if tx.nonce < state.nonce(&from) {
            let message = format!(
                "nonce too low: next nonce {}, tx nonce {}",
                state.nonce(&from),
                tx.nonce
            );
            return Err(failure(-32_003, message));
        }
        let cost = U256::from(tx.gas) * U256::from(tx.max_fee) + tx.value;
        let balance = state.balance(&from);
        if balance < cost {
            let recorded = recording("eth_sendRawTransaction.funds").failure();
            let values = [balance, cost, cost - balance].map(|value| value.to_string());
            return Err(failure(recorded.code, fill(&recorded.message, &values)));
        }
        if tx.max_fee < self.base_fee {
            let message = "max fee per gas less than block base fee".to_owned();
            return Err(failure(-32_000, message));
        }
        if view.submit(tx) == Submitted::Known {
            return Err(failure(-32_000, "already known".to_owned()));
        }
        answer
    }

    /// `eth_estimateGas` of an ether or token transfer; a token transfer
    /// beyond the sender's balance reverts as recorded.
    fn estimate(&self, view: &View<'_, Self>, params: &Value) -> Answer {
        let call = &params[0];
        let data = call["data"]
            .as_str()
            .and_then(|text| hex::decode(text).ok())
            .unwrap_or_default();
        let Some((_, amount)) = Self::transfer(&data) else {
            return Ok(raw(&json!(format!("{TRANSFER_GAS:#x}"))));
        };
        let from: Address = call["from"]
            .as_str()
            .unwrap_or_default()
            .parse()
            .unwrap_or_default();
        if view.pending().token(&from) < amount {
            return Err(recording("eth_estimateGas.reverted").failure());
        }
        Ok(raw(recording("eth_estimateGas").result()))
    }

    /// The block `tag` names on this node: `latest`, `finalized`, or a height.
    fn height(view: &View<'_, Self>, tag: &Value) -> Option<u64> {
        match tag.as_str()? {
            "latest" | "pending" | "safe" => Some(view.head()),
            "finalized" => view.finalized(),
            "earliest" => Some(0),
            text => u64::from_str_radix(text.trim_start_matches("0x"), 16).ok(),
        }
    }

    fn block(&self, view: &View<'_, Self>, params: &Value) -> Value {
        let full = params[1].as_bool().unwrap_or(false);
        Self::height(view, &params[0])
            .filter(|height| *height <= view.head())
            .and_then(|height| view.block(height))
            .map_or(Value::Null, |block| render::block(self, block, full))
    }

    fn receipts(&self, view: &View<'_, Self>, params: &Value) -> Value {
        let wanted = &params[0];
        let found = view.blocks().find(|block| {
            json!(hex::encode_prefixed(block.seal.hash)) == *wanted
                || json!(format!("{:#x}", block.height)) == *wanted
        });
        match found {
            Some(block) if view.served(block.height) => render::receipts(block),
            // Below the node's floor: `null`, as the recorded node answers.
            _ => Value::Null,
        }
    }

    fn transaction(&self, view: &View<'_, Self>, params: &Value, receipt: bool) -> Value {
        let wanted = params[0].as_str().unwrap_or_default();
        for block in view.blocks() {
            let at = block.txs.iter().position(|(tx, _)| self.id(tx) == wanted);
            let Some(at) = at else {
                continue;
            };
            if receipt && !view.served(block.height) {
                return Value::Null;
            }
            if receipt {
                return render::receipt(block, at);
            }
            return render::mined(block, at);
        }
        let pending = view.mempool().iter().find(|tx| self.id(tx) == wanted);
        match pending {
            Some(tx) if !receipt => render::pending(tx),
            _ => Value::Null,
        }
    }

    /// `eth_getLogs` of `fromBlock..=toBlock` on this node: the logs of
    /// the filter's `address` (one or a list) whose topics match position
    /// by position (`null` any, a list any of it).
    fn logs(view: &View<'_, Self>, params: &Value) -> Value {
        let filter = &params[0];
        let bound = |field: &str| Self::height(view, &filter[field]);
        let from = bound("fromBlock").unwrap_or(view.head());
        let to = bound("toBlock").unwrap_or(view.head()).min(view.head());
        let addresses: Vec<Value> = match &filter["address"] {
            Value::Array(all) => all.clone(),
            Value::Null => Vec::new(),
            one => vec![one.clone()],
        };
        let wanted = |log: &Value| {
            let address = addresses.is_empty() || addresses.contains(&log["address"]);
            let topics = filter["topics"].as_array().map_or(&[][..], Vec::as_slice);
            let matches = topics.iter().enumerate().all(|(at, topic)| match topic {
                Value::Null => true,
                Value::Array(any) => any.contains(&log["topics"][at]),
                one => *one == log["topics"][at],
            });
            address && matches
        };
        let mut logs = Vec::new();
        for block in view
            .blocks()
            .filter(|block| (from..=to).contains(&block.height))
        {
            logs.extend(render::logs(block).into_iter().filter(|log| wanted(log)));
        }
        Value::Array(logs)
    }

    fn nonce(view: &View<'_, Self>, params: &Value) -> Value {
        let address: Address = params[0]
            .as_str()
            .unwrap_or_default()
            .parse()
            .unwrap_or_default();
        let state = if params[1] == "pending" {
            view.pending()
        } else {
            view.head_state()
        };
        json!(format!("{:#x}", state.nonce(&address)))
    }

    /// The state at the block `tag` names on this node; none above its head.
    fn state_at<'v>(view: &'v View<'_, Self>, tag: &Value) -> Option<&'v State> {
        let height = Self::height(view, tag).filter(|height| *height <= view.head())?;
        view.block(height).map(|block| &block.state)
    }

    /// `eth_getBalance` of an account at a block: its ether there.
    fn balance(view: &View<'_, Self>, params: &Value) -> Value {
        let owner: Address = params[0]
            .as_str()
            .unwrap_or_default()
            .parse()
            .unwrap_or_default();
        let wei =
            Self::state_at(view, &params[1]).map_or(U256::ZERO, |state| state.balance(&owner));
        json!(format!("{wei:#x}"))
    }

    /// The token's `decimals()`, as recorded, and `balanceOf(owner)` at a
    /// block.
    fn call(&self, view: &View<'_, Self>, params: &Value) -> Value {
        let decimals = hex::encode_prefixed(crate::erc20::decimals());
        let to: Address = params[0]["to"]
            .as_str()
            .unwrap_or_default()
            .parse()
            .unwrap_or_default();
        if to != self.token {
            return json!("0x");
        }
        if params[0]["data"] == decimals {
            return recording("eth_call.decimals").result().clone();
        }
        let data: Bytes = params[0]["data"]
            .as_str()
            .unwrap_or_default()
            .parse()
            .unwrap_or_default();
        let Some(owner) = data.get(16..36).map(Address::from_slice) else {
            return json!("0x");
        };
        if *data != crate::erc20::balance_of(owner) {
            return json!("0x");
        }
        let units =
            Self::state_at(view, &params[1]).map_or(U256::ZERO, |state| state.token(&owner));
        json!(hex::encode_prefixed(units.to_be_bytes::<32>()))
    }
}

impl base::testing::Model for Model {
    type State = State;
    type Tx = Tx;
    type Outcome = Outcome;
    type Seal = Seal;

    fn id(&self, tx: &Tx) -> String {
        hex::encode_prefixed(tx.hash)
    }

    /// Includes `tx` when its nonce is next and its sender can pay the
    /// worst case; a token transfer beyond the balance reverts (fee paid).
    fn apply(&self, state: &mut State, tx: &Tx) -> Option<Outcome> {
        let cost = U256::from(tx.gas) * U256::from(tx.max_fee) + tx.value;
        let payable = state.balance(&tx.from) >= cost && tx.max_fee >= self.base_fee;
        if tx.nonce != state.nonce(&tx.from) || !payable {
            return None;
        }
        let price = tx.max_fee.min(self.base_fee + tx.priority);
        let mut outcome = Outcome {
            status: true,
            gas_used: if tx.input.is_empty() {
                TRANSFER_GAS
            } else {
                self.token_gas
            },
            price,
            logs: Vec::new(),
        };
        let account = state.accounts.entry(tx.from).or_default();
        account.0 -= U256::from(outcome.gas_used) * U256::from(price);
        account.1 += 1;
        let transfer = Self::transfer(&tx.input).filter(|_| tx.to == self.token);
        if let Some((to, amount)) = transfer {
            if state.token(&tx.from) < amount {
                outcome.status = false;
                return Some(outcome);
            }
            *state.tokens.entry(tx.from).or_default() -= amount;
            *state.tokens.entry(to).or_default() += amount;
            outcome.logs.push((self.token, tx.from, to, amount));
        }
        if !tx.value.is_zero() {
            state.pay(tx.from, tx.to, tx.value);
            outcome.logs.insert(0, (SYSTEM, tx.from, tx.to, tx.value));
        }
        Some(outcome)
    }

    fn seal(&self, parent: Option<&Seal>, height: u64, branch: u64) -> Seal {
        let parent = parent.map_or(B256::ZERO, |seal| seal.hash);
        let hash = keccak256(
            [
                parent.as_slice(),
                &height.to_be_bytes(),
                &branch.to_be_bytes(),
            ]
            .concat(),
        );
        Seal { hash, parent }
    }

    fn answer(&self, view: &mut View<'_, Self>, method: &str, params: &Value) -> Answer {
        let result = match method {
            "eth_chainId" => recording("eth_chainId").result().clone(),
            "eth_blockNumber" => json!(format!("{:#x}", view.head())),
            "eth_getBlockByNumber" => self.block(view, params),
            "eth_getBlockReceipts" => self.receipts(view, params),
            "eth_getLogs" => Self::logs(view, params),
            "eth_getTransactionReceipt" => self.transaction(view, params, true),
            "eth_getTransactionByHash" => self.transaction(view, params, false),
            "eth_getTransactionCount" => Self::nonce(view, params),
            "eth_maxPriorityFeePerGas" => json!(format!("{:#x}", self.priority)),
            "eth_call" => self.call(view, params),
            "eth_getBalance" => Self::balance(view, params),
            "eth_estimateGas" => return self.estimate(view, params),
            "eth_sendRawTransaction" => return self.send(view, params),
            _ => return Err(recording("method_not_found").failure()),
        };
        Ok(raw(&result))
    }
}

fn raw(value: &Value) -> RawJson {
    RawJson::from_serializable(value).expect("a JSON value serializes")
}

fn failure(code: i64, message: String) -> Failure {
    Failure {
        code,
        message,
        data: None,
    }
}

/// `template` with its numbers replaced by `values`, in order: one wording
/// (`have B want C`, or `balance B, tx cost C, overshot O`) whichever
/// client was recorded.
fn fill(template: &str, values: &[String]) -> String {
    let mut out = String::new();
    let mut values = values.iter();
    let mut digits = false;
    for character in template.chars() {
        let digit = character.is_ascii_digit();
        if digit && !digits {
            out.push_str(values.next().map_or("", String::as_str));
        }
        if !digit {
            out.push(character);
        }
        digits = digit;
    }
    out
}

fn quantity(value: &Value) -> u128 {
    value
        .as_str()
        .and_then(|text| u128::from_str_radix(text.trim_start_matches("0x"), 16).ok())
        .unwrap_or(0)
}
